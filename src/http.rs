use std::{
    convert::Infallible,
    io,
    net::{Ipv6Addr, SocketAddr},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use axum::{
    Json, Router,
    body::{Body, Bytes, to_bytes},
    extract::{Request, State},
    http::{StatusCode, header, uri::Authority},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::stream;
use hyper::server::conn::http1;
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use scry::{ContentsRequest, Error, ErrorCode, ErrorResponse, Scry, SearchRequest};
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
    task::JoinSet,
    time::Sleep,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct AppState {
    service: Scry,
    shutdown: CancellationToken,
    hosts: Arc<Vec<String>>,
    token: Option<Arc<Vec<u8>>>,
    operations: Arc<Semaphore>,
}

pub async fn serve(
    service: Scry,
    bind: SocketAddr,
    allow_hosts: Vec<String>,
    shutdown: CancellationToken,
) -> Result<(), Error> {
    let token = match std::env::var("SCRY_AUTH_TOKEN") {
        Ok(value)
            if !value.is_empty()
                && value.len() <= 4096
                && value.bytes().all(|b| b.is_ascii_graphic()) =>
        {
            Some(Arc::new(value.into_bytes()))
        }
        Ok(_) | Err(std::env::VarError::NotUnicode(_)) => {
            return Err(Error::new(
                ErrorCode::InvalidInput,
                "SCRY_AUTH_TOKEN must contain 1 to 4096 visible ASCII characters",
            ));
        }
        Err(std::env::VarError::NotPresent) => None,
    };
    if !bind.ip().is_loopback() && token.is_none() {
        return Err(Error::new(
            ErrorCode::InvalidInput,
            "A non-loopback listener requires SCRY_AUTH_TOKEN",
        ));
    }
    let extra_hosts = allow_hosts
        .into_iter()
        .map(|host| normalize_host(&host))
        .collect::<Result<Vec<_>, _>>()?;
    let listener = TcpListener::bind(bind)
        .await
        .map_err(|_| Error::new(ErrorCode::Internal, "Cannot bind the HTTP listener"))?;
    let address = listener
        .local_addr()
        .map_err(|_| Error::new(ErrorCode::Internal, "Cannot read the listener address"))?;
    let mut hosts = vec![format!("localhost:{}", address.port())];
    if address.ip().is_loopback() {
        hosts.push(address.to_string());
    } else {
        hosts.push(format!("127.0.0.1:{}", address.port()));
    }
    hosts.extend(extra_hosts);
    hosts.sort();
    hosts.dedup();
    let (mcp, sessions) = crate::mcp::transport(service.clone(), shutdown.clone(), hosts.clone());
    let state = AppState {
        operations: Arc::new(Semaphore::new(service.config().limits.max_requests)),
        service,
        shutdown: shutdown.clone(),
        hosts: Arc::new(hosts),
        token,
    };
    let app = Router::new()
        .route("/search", post(search))
        .route("/contents", post(contents))
        .route("/health", get(health))
        .nest_service("/mcp", mcp)
        .fallback(|| async {
            rejection(
                StatusCode::NOT_FOUND,
                ErrorCode::InvalidInput,
                "Unknown route",
            )
        })
        .method_not_allowed_fallback(|| async {
            rejection(
                StatusCode::METHOD_NOT_ALLOWED,
                ErrorCode::InvalidInput,
                "Method not allowed",
            )
        })
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(state, protect));
    let mut reap = tokio::spawn(sessions.maintain(shutdown.clone()));
    let mut connections = JoinSet::new();
    eprintln!("scry listening on {address}; MCP at /mcp");
    let result = loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break Ok(()),
            _ = connections.join_next(), if !connections.is_empty() => {},
            accepted = listener.accept(), if connections.len() < 32 => {
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(_) => break Err(Error::new(ErrorCode::Internal, "HTTP listener failed")),
                };
                let app = app.clone();
                let shutdown = shutdown.clone();
                connections.spawn(async move {
                    let mut builder = http1::Builder::new();
                    builder.timer(TokioTimer::new()).header_read_timeout(Duration::from_secs(10));
                    let connection = builder.serve_connection(
                        TokioIo::new(BoundedConnection { inner: stream, write_deadline: None }),
                        TowerToHyperService::new(app),
                    );
                    tokio::pin!(connection);
                    tokio::select! {
                        biased;
                        _ = shutdown.cancelled() => {
                            connection.as_mut().graceful_shutdown();
                            let _ = connection.await;
                        }
                        _ = &mut connection => {},
                    }
                });
            }
        }
    };
    drop(listener);
    shutdown.cancel();
    let drain = async {
        while connections.join_next().await.is_some() {}
        let _ = (&mut reap).await;
    };
    if tokio::time::timeout(Duration::from_secs(5), drain)
        .await
        .is_err()
    {
        connections.shutdown().await;
        reap.abort();
        let _ = reap.await;
    }
    result
}

fn normalize_host(value: &str) -> Result<String, Error> {
    let authority = value.parse::<Authority>().map_err(|_| {
        Error::new(
            ErrorCode::InvalidInput,
            "Expected an exact HOST[:PORT] authority",
        )
    })?;
    let host = authority.host();
    let valid_host = match host.strip_prefix('[') {
        Some(ip) => ip
            .strip_suffix(']')
            .is_some_and(|ip| ip.parse::<Ipv6Addr>().is_ok()),
        None => !host.is_empty() && !host.contains(['[', ']', ':']),
    };
    let suffix = &value[host.len()..];
    let valid_port = suffix.is_empty()
        || suffix.strip_prefix(':').is_some_and(|port| {
            !port.is_empty()
                && port.bytes().all(|byte| byte.is_ascii_digit())
                && port.parse::<u16>().is_ok()
        });
    if value.len() > 1024
        || value.contains('@')
        || value.contains('*')
        || !valid_host
        || !valid_port
    {
        return Err(Error::new(
            ErrorCode::InvalidInput,
            "Expected an exact HOST[:PORT] authority",
        ));
    }
    Ok(value.to_ascii_lowercase())
}

async fn protect(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if request.headers().contains_key(header::ORIGIN) {
        return rejection(
            StatusCode::FORBIDDEN,
            ErrorCode::InvalidInput,
            "Origin headers are not allowed",
        );
    }
    let mut host_values = request.headers().get_all(header::HOST).iter();
    let host = host_values
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| normalize_host(value).ok());
    if host_values.next().is_some() || !host.as_ref().is_some_and(|host| state.hosts.contains(host))
    {
        return rejection(
            StatusCode::FORBIDDEN,
            ErrorCode::InvalidInput,
            "Host is not allowed",
        );
    }
    if let Some(authority) = request.uri().authority()
        && normalize_host(authority.as_str()).ok().as_ref() != host.as_ref()
    {
        return rejection(
            StatusCode::FORBIDDEN,
            ErrorCode::InvalidInput,
            "Request authority does not match Host",
        );
    }
    if let Some(expected) = &state.token {
        let mut values = request.headers().get_all(header::AUTHORIZATION).iter();
        let provided = values
            .next()
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
            .map(|(_, token)| token)
            .unwrap_or("");
        let matches = provided.as_bytes().ct_eq(expected.as_slice());
        if values.next().is_some() || !bool::from(matches) {
            let mut response = rejection(
                StatusCode::UNAUTHORIZED,
                ErrorCode::InvalidInput,
                "Bearer authentication required",
            );
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                axum::http::HeaderValue::from_static("Bearer"),
            );
            return response;
        }
    }
    if state.shutdown.is_cancelled() {
        return rejection(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Cancelled,
            "Server is shutting down",
        );
    }
    let is_mcp = request.uri().path() == "/mcp" || request.uri().path().starts_with("/mcp/");
    if is_mcp && let Err(error) = crate::mcp::validate_protocol_headers(request.headers()) {
        return (StatusCode::BAD_REQUEST, Json(ErrorResponse { error })).into_response();
    }
    let maximum = state.service.config().limits.max_request_bytes;
    if request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > maximum as u64)
    {
        return rejection(
            StatusCode::PAYLOAD_TOO_LARGE,
            ErrorCode::TooLarge,
            "Request body exceeds the byte limit",
        );
    }
    let (parts, body) = request.into_parts();
    let bytes = tokio::select! {
        biased;
        _ = state.shutdown.cancelled() => return rejection(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Cancelled, "Server is shutting down"),
        result = tokio::time::timeout(Duration::from_secs(10), to_bytes(body, maximum)) => match result {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(_)) => return rejection(StatusCode::PAYLOAD_TOO_LARGE, ErrorCode::TooLarge, "Cannot read request within the body limit"),
            Err(_) => return rejection(StatusCode::REQUEST_TIMEOUT, ErrorCode::Timeout, "Request body deadline exceeded"),
        },
    };
    if is_mcp && let Err(error) = crate::mcp::validate_legacy_message(&bytes) {
        return (StatusCode::BAD_REQUEST, Json(ErrorResponse { error })).into_response();
    }
    let mut response = next
        .run(Request::from_parts(parts, Body::from(bytes)))
        .await;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        axum::http::HeaderValue::from_static("nosniff"),
    );
    response
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({"status": "ok", "version": env!("CARGO_PKG_VERSION")}))
}

async fn search(State(state): State<AppState>, request: Request) -> Response {
    let request = match json_request::<SearchRequest>(request).await {
        Ok(request) => request,
        Err((status, error)) => return (status, Json(ErrorResponse { error })).into_response(),
    };
    operation(state, move |service, ctx| async move {
        service.search_with_context(request, ctx).await
    })
}

async fn contents(State(state): State<AppState>, request: Request) -> Response {
    let request = match json_request::<ContentsRequest>(request).await {
        Ok(request) => request,
        Err((status, error)) => return (status, Json(ErrorResponse { error })).into_response(),
    };
    operation(state, move |service, ctx| async move {
        service.contents_with_context(request, ctx).await
    })
}

async fn json_request<T: serde::de::DeserializeOwned>(
    request: Request,
) -> Result<T, (StatusCode, Error)> {
    let json = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"));
    if !json {
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Error::new(
                ErrorCode::InvalidInput,
                "Content-Type must be application/json",
            ),
        ));
    }
    let bytes = to_bytes(request.into_body(), 1048576).await.map_err(|_| {
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            Error::new(ErrorCode::TooLarge, "Request body exceeds the byte limit"),
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Error::new(
                ErrorCode::InvalidInput,
                "Invalid request JSON or unknown request fields",
            ),
        )
    })
}

fn operation<F, Fut>(state: AppState, run: F) -> Response
where
    F: FnOnce(Scry, scry::RequestContext) -> Fut + Send + 'static,
    Fut: Future<Output = Result<scry::Response, Error>> + Send + 'static,
{
    let permit = match state.operations.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return rejection(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::Overloaded,
                "Too many active REST operations",
            );
        }
    };
    let ctx = crate::request_context(&state.service, state.shutdown.child_token());
    let guard = ctx.cancellation.clone().drop_guard();
    let maximum = state.service.config().limits.max_response_bytes;
    let body = stream::once(async move {
        let _permit = permit;
        let _guard = guard;
        let result = tokio::select! {
            biased;
            _ = ctx.cancellation.cancelled() => Err(Error::cancelled()),
            result = run(state.service, ctx.clone()) => result,
        };
        Ok::<Bytes, Infallible>(Bytes::from(crate::response_bytes(result, maximum)))
    });
    (
        [(header::CONTENT_TYPE, "application/json")],
        Body::from_stream(body),
    )
        .into_response()
}

fn rejection(status: StatusCode, code: ErrorCode, message: &str) -> Response {
    (
        status,
        Json(ErrorResponse {
            error: Error::new(code, message),
        }),
    )
        .into_response()
}

struct BoundedConnection {
    inner: TcpStream,
    write_deadline: Option<Pin<Box<Sleep>>>,
}

impl BoundedConnection {
    fn check_write_deadline(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        let deadline = self
            .write_deadline
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(Duration::from_secs(10))));
        if deadline.as_mut().poll(cx).is_ready() {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Write deadline exceeded",
            ))
        } else {
            Ok(())
        }
    }
}

impl AsyncRead for BoundedConnection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for BoundedConnection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.check_write_deadline(cx)?;
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.check_write_deadline(cx)?;
        let result = Pin::new(&mut self.inner).poll_flush(cx);
        if result.is_ready() {
            self.write_deadline = None;
        }
        result
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.check_write_deadline(cx)?;
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
