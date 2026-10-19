use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::StreamExt;
use rmcp::{
    RoleServer, ServiceExt,
    model::{
        ClientJsonRpcMessage, ClientNotification, GetExtensions, RequestId, ServerJsonRpcMessage,
    },
    service::QuitReason,
    transport::{Transport, async_rw::AsyncRwTransport},
};
use scry::{Error, ErrorCode, Scry};
use tokio::sync::Semaphore;
use tokio_util::{
    bytes::Bytes,
    codec::{FramedRead, LinesCodec, LinesCodecError},
    io::StreamReader,
    sync::CancellationToken,
};

struct State {
    error: Mutex<Option<Error>>,
    stop: CancellationToken,
}

impl State {
    fn fail(&self, error: Error) {
        self.error
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_or_insert(error);
        self.stop.cancel();
    }
}

pub async fn serve(service: Scry, shutdown: CancellationToken) -> Result<(), Error> {
    let maximum = service.config().limits.max_request_bytes;
    let state = Arc::new(State {
        error: Mutex::new(None),
        stop: shutdown.child_token(),
    });
    let input_state = state.clone();
    let input = FramedRead::new(tokio::io::stdin(), LinesCodec::new_with_max_length(maximum)).map(
        move |line| {
            let result = match line {
                Ok(mut line) => {
                    let valid = serde_json::from_str::<ClientJsonRpcMessage>(&line)
                        .map_err(|_| {
                            Error::new(ErrorCode::InvalidInput, "Malformed MCP input frame")
                        })
                        .and_then(|_| crate::mcp::validate_legacy_message(line.as_bytes()));
                    valid.map(|()| {
                        line.push('\n');
                        Bytes::from(line)
                    })
                }
                Err(LinesCodecError::MaxLineLengthExceeded) => Err(Error::new(
                    ErrorCode::TooLarge,
                    "MCP input frame exceeds max_request_bytes",
                )),
                Err(LinesCodecError::Io(_)) => Err(Error::new(
                    ErrorCode::InvalidInput,
                    "Cannot read a UTF-8 MCP input frame",
                )),
            };
            result.map_err(|error| {
                let message = error.message.clone();
                input_state.fail(error);
                io::Error::new(io::ErrorKind::InvalidData, message)
            })
        },
    );
    let transport = BoundedTransport {
        inner: AsyncRwTransport::new_server(StreamReader::new(input), tokio::io::stdout()),
        state: state.clone(),
        requests: Arc::new(Mutex::new(HashMap::new())),
        notifications: Arc::new(Semaphore::new(32)),
    };
    let session = async {
        let running = match crate::mcp::Mcp::new(service, state.stop.clone())
            .serve_with_ct(transport, state.stop.clone())
            .await
        {
            Ok(running) => running,
            Err(_) if state.stop.is_cancelled() => return Ok(()),
            Err(_) => {
                return Err(Error::new(
                    ErrorCode::InvalidInput,
                    "MCP initialization failed",
                ));
            }
        };
        match running.waiting().await {
            Ok(QuitReason::Closed | QuitReason::Cancelled) => Ok(()),
            _ => Err(Error::new(ErrorCode::Internal, "MCP service failed")),
        }
    };
    tokio::pin!(session);
    let result = tokio::select! {
        biased;
        result = &mut session => result,
        _ = state.stop.cancelled() => {
            tokio::time::timeout(Duration::from_secs(1), &mut session)
                .await
                .unwrap_or(Ok(()))
        }
    };
    state.stop.cancel();
    match state
        .error
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
    {
        Some(error) => Err(error),
        None => result,
    }
}

struct BoundedTransport<T> {
    inner: T,
    state: Arc<State>,
    requests: Arc<Mutex<HashMap<RequestId, bool>>>,
    notifications: Arc<Semaphore>,
}

impl<T: Transport<RoleServer, Error = io::Error>> BoundedTransport<T> {
    fn reserve(&self, message: &mut ClientJsonRpcMessage) -> Result<(), Error> {
        let mut requests = self
            .requests
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match message {
            ClientJsonRpcMessage::Request(request) => {
                if let RequestId::String(id) = &request.id
                    && id.len() > 1024
                {
                    return Err(Error::new(
                        ErrorCode::TooLarge,
                        "MCP request ID exceeds 1024 bytes",
                    ));
                }
                if requests.contains_key(&request.id) {
                    return Err(Error::new(
                        ErrorCode::InvalidInput,
                        "MCP request ID is already active or was cancelled",
                    ));
                }
                if requests.len() >= 32 {
                    return Err(Error::new(
                        ErrorCode::Overloaded,
                        "MCP pending request ID limit reached",
                    ));
                }
                requests.insert(request.id.clone(), false);
            }
            ClientJsonRpcMessage::Notification(notification) => {
                let permit = self
                    .notifications
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| {
                        Error::new(
                            ErrorCode::Overloaded,
                            "MCP pending notification limit reached",
                        )
                    })?;
                if let ClientNotification::CancelledNotification(cancelled) =
                    &notification.notification
                    && let Some(id) = &cancelled.params.request_id
                    && let Some(retired) = requests.get_mut(id)
                {
                    *retired = true;
                }
                notification
                    .notification
                    .extensions_mut()
                    .insert(Arc::new(permit));
            }
            _ => {}
        }
        Ok(())
    }
}

impl<T: Transport<RoleServer, Error = io::Error>> Transport<RoleServer> for BoundedTransport<T> {
    type Error = io::Error;

    fn send(
        &mut self,
        message: ServerJsonRpcMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let id = match &message {
            ServerJsonRpcMessage::Response(response) => Some(response.id.clone()),
            ServerJsonRpcMessage::Error(error) => error.id.clone(),
            _ => None,
        };
        let send = self.inner.send(message);
        let state = self.state.clone();
        let requests = self.requests.clone();
        async move {
            let result = tokio::select! {
                biased;
                _ = state.stop.cancelled() => return Ok(()),
                _ = tokio::time::sleep(Duration::from_secs(10)) => {
                    let message = "MCP stdout write deadline exceeded (10 seconds)";
                    state.fail(Error::new(ErrorCode::Timeout, message));
                    return Err(io::Error::new(io::ErrorKind::TimedOut, message));
                }
                result = send => result,
            };
            if let Err(error) = result {
                state.fail(Error::new(ErrorCode::Internal, "Cannot write MCP stdout"));
                return Err(error);
            }
            if let Some(id) = id {
                let mut requests = requests.lock().unwrap_or_else(|error| error.into_inner());
                if requests.get(&id) == Some(&false) {
                    requests.remove(&id);
                }
            }
            Ok(())
        }
    }

    async fn receive(&mut self) -> Option<ClientJsonRpcMessage> {
        let Some(mut message) = self.inner.receive().await else {
            self.state.stop.cancel();
            return None;
        };
        if let Err(error) = self.reserve(&mut message) {
            self.state.fail(error);
            return None;
        }
        Some(message)
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.inner.close().await
    }
}
