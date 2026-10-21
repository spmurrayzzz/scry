use super::*;

async fn provider_response(
    status: &str,
    headers: &str,
    chunked: bool,
    chunks: &[&[u8]],
    limits: &Limits,
) -> (Response, WireServer, RequestContext) {
    let server = WireServer::start(status, headers, chunked, chunks).await;
    let ctx = RequestContext::new(BUDGET);
    let url = Url::parse(&format!(
        "http://pinned.invalid:{}/page",
        server.address.port()
    ))
    .unwrap();
    let response = provider::send(
        &fetcher(limits.clone()),
        &url,
        vec![server.address],
        false,
        &ctx,
    )
    .await
    .unwrap();
    (response, server, ctx)
}

fn header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    request.split("\r\n\r\n").next()?.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then_some(value.trim())
    })
}

#[tokio::test]
async fn wire_requests_preserve_pinned_host_and_scoped_provider_headers() {
    let limits = Limits::default();
    for script in [false, true] {
        let mut server = WireServer::start(
            "200 OK",
            "Content-Type: text/html\r\nContent-Length: 11\r\n",
            false,
            &[b"<p>test</p>"],
        )
        .await;
        let url = Url::parse(&format!(
            "http://pinned.invalid:{}/page",
            server.address.port()
        ))
        .unwrap();
        let ctx = RequestContext::new(BUDGET);
        let response = provider::send(
            &fetcher(limits.clone()),
            &url,
            vec![server.address],
            script,
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.content_length(), Some(11));
        let encoding = response_encoding(&response).unwrap();
        assert_eq!(
            fetcher(limits.clone())
                .read_html(response, encoding, &ctx)
                .await
                .unwrap(),
            "<p>test</p>"
        );
        let request = server.finish().await;
        let (headers, body) = request.split_once("\r\n\r\n").unwrap();
        assert_eq!(headers.lines().next(), Some("GET /page HTTP/1.1"));
        assert!(body.is_empty());
        assert!(header(&request, "content-type").is_none());
        assert_eq!(
            header(&request, "host"),
            Some(format!("pinned.invalid:{}", server.address.port()).as_str())
        );
        assert!(
            header(&request, "user-agent")
                .unwrap()
                .contains("Chrome/149.")
        );
        for (name, value) in [
            ("accept-encoding", "identity"),
            ("accept-language", "en-US,en;q=0.9"),
            ("connection", "close"),
            (
                "accept",
                if script {
                    "*/*"
                } else {
                    "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"
                },
            ),
            ("sec-fetch-dest", if script { "script" } else { "document" }),
            (
                "sec-fetch-mode",
                if script { "no-cors" } else { "navigate" },
            ),
            ("sec-fetch-site", if script { "same-site" } else { "none" }),
        ] {
            assert_eq!(header(&request, name), Some(value), "{name}");
        }
        assert_eq!(
            header(&request, "sec-fetch-user"),
            if script { None } else { Some("?1") }
        );
        assert_eq!(
            header(&request, "referer"),
            script.then_some("https://duckduckgo.com/")
        );
        for name in [
            "cookie",
            "authorization",
            "proxy-authorization",
            "sec-ch-ua",
            "sec-ch-ua-mobile",
            "sec-ch-ua-platform",
            "priority",
            "upgrade-insecure-requests",
        ] {
            assert!(header(&request, name).is_none(), "{name}");
        }
    }
}

#[tokio::test]
async fn ordinary_client_does_not_select_provider_headers_from_a_duckduckgo_hostname() {
    let fetcher = fetcher(Limits::default());
    for hostname in ["duckduckgo.com", "links.duckduckgo.com"] {
        let mut server = WireServer::start(
            "200 OK",
            "Content-Type: text/html\r\n",
            false,
            &[b"<p>page</p>"],
        )
        .await;
        let url = Url::parse(&format!(
            "http://{hostname}:{}/d.js?o=json",
            server.address.port()
        ))
        .unwrap();
        let ctx = RequestContext::new(BUDGET);
        let response = fetcher
            .client(&url, vec![server.address], &ctx)
            .unwrap()
            .get(url)
            .send()
            .await
            .unwrap();
        let encoding = response_encoding_for(&response, false).unwrap();
        assert_eq!(
            fetcher.read_html(response, encoding, &ctx).await.unwrap(),
            "<p>page</p>"
        );
        let request = server.finish().await;
        assert_eq!(header(&request, "user-agent"), Some("scry/0.1"));
        for forbidden in [
            "referer",
            "accept-language",
            "sec-fetch-dest",
            "sec-fetch-mode",
            "sec-fetch-site",
            "sec-fetch-user",
        ] {
            assert!(
                header(&request, forbidden).is_none(),
                "{hostname}: {forbidden}"
            );
        }
    }
}

#[tokio::test]
async fn client_rejects_unpinned_host_without_resolver_fallback() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let url = Url::parse(&format!("http://pinned.invalid:{}/", address.port())).unwrap();
    let ctx = RequestContext::new(BUDGET);
    let client = fetcher(Limits::default())
        .client(&url, vec![address], &ctx)
        .unwrap();
    let unexpected = format!("http://unexpected.invalid:{}/", address.port());
    let error = timeout(BUDGET, client.get(unexpected).send())
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.is_connect());
    assert!(!error.is_timeout());
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    let mut rejected_name = false;
    while let Some(error) = source {
        rejected_name |= error
            .to_string()
            .contains("Unvalidated DNS name is not allowed");
        source = error.source();
    }
    assert!(rejected_name, "{error:?}");
    let mut accepted = Box::pin(listener.accept());
    assert!(futures_util::poll!(accepted.as_mut()).is_pending());
}

#[tokio::test]
async fn provider_responses_preserve_redirects_errors_and_retry_after_without_following() {
    for (status, expected_status, expected_error) in [
        ("302 Found", 302, ErrorCode::UpstreamFailed),
        ("307 Temporary Redirect", 307, ErrorCode::UpstreamFailed),
        ("202 Accepted", 202, ErrorCode::UpstreamBlocked),
        ("403 Forbidden", 403, ErrorCode::UpstreamBlocked),
        ("429 Too Many Requests", 429, ErrorCode::RateLimited),
        ("503 Service Unavailable", 503, ErrorCode::UpstreamFailed),
    ] {
        let (response, mut server, _) = provider_response(
            status,
            "Content-Type: text/html\r\nRetry-After: 7\r\nLocation: https://unexpected.invalid/\r\n",
            false,
            &[],
            &Limits::default(),
        )
        .await;
        assert_eq!(response.status().as_u16(), expected_status);
        assert_eq!(response.headers()[LOCATION], "https://unexpected.invalid/");
        let error = check_status(&response).unwrap_err();
        assert_eq!(error.code, expected_error);
        assert_eq!(
            error.retry_after_secs,
            if matches!(expected_status, 302 | 307) {
                None
            } else {
                Some(7)
            }
        );
        server.finish().await;
    }
}

#[tokio::test]
async fn provider_bodies_keep_download_decoding_and_encoding_limits() {
    for chunked in [false, true] {
        for (headers, chunks, download_limit, decoded_limit, expected) in [
            (
                "Content-Type: text/html\r\n",
                vec![b"abc".as_slice(), b"de"],
                5,
                20,
                Ok("abcde"),
            ),
            (
                "Content-Type: text/html\r\n",
                vec![b"abc".as_slice(), b"def"],
                5,
                20,
                Err(ErrorCode::TooLarge),
            ),
            (
                "Content-Type: text/html; charset=windows-1252\r\n",
                vec![b"\x80".as_slice(), b"\x80"],
                2,
                6,
                Ok("€€"),
            ),
            (
                "Content-Type: text/html; charset=windows-1252\r\n",
                vec![b"\x80".as_slice(), b"\x80"],
                2,
                5,
                Err(ErrorCode::TooLarge),
            ),
            (
                "Content-Type: text/html\r\nContent-Encoding: gzip\r\n",
                vec![b"not-gzip".as_slice()],
                20,
                20,
                Err(ErrorCode::UnsupportedContent),
            ),
            (
                "Content-Type: text/html\r\nContent-Type: application/xhtml+xml\r\n",
                vec![b"<p>x</p>".as_slice()],
                20,
                20,
                Err(ErrorCode::UnsupportedContent),
            ),
            (
                "Content-Type: application/pdf\r\n",
                vec![b"%PDF".as_slice()],
                20,
                20,
                Err(ErrorCode::UnsupportedContent),
            ),
        ] {
            let limits = Limits {
                max_download_bytes: download_limit,
                max_decoded_bytes: decoded_limit,
                ..Limits::default()
            };
            let (response, mut server, ctx) =
                provider_response("200 OK", headers, chunked, &chunks, &limits).await;
            let result = match response_encoding(&response) {
                Ok(encoding) => fetcher(limits).read_html(response, encoding, &ctx).await,
                Err(error) => Err(error),
            };
            match expected {
                Ok(text) => assert_eq!(result.unwrap(), text),
                Err(code) => assert_eq!(result.unwrap_err().code, code, "{headers}"),
            }
            server.finish().await;
        }
    }
}

#[tokio::test]
async fn provider_body_timeout_keeps_its_timeout_classification() {
    let (release, gate) = tokio::sync::oneshot::channel();
    let mut server = WireServer::start_with_gate(
        "200 OK",
        "Content-Type: text/html\r\nContent-Length: 32\r\n",
        false,
        &[],
        Some(gate),
    )
    .await;
    let url = Url::parse(&format!("http://pinned.invalid:{}/", server.address.port())).unwrap();
    let limits = Limits::default();
    let request_ctx = RequestContext::new(Duration::from_secs(1));
    let response = provider::send(
        &fetcher(limits.clone()),
        &url,
        vec![server.address],
        true,
        &request_ctx,
    )
    .await
    .unwrap();
    let encoding = response_encoding(&response).unwrap();
    let body_ctx = RequestContext::new(Duration::from_secs(30));
    let fetcher = fetcher(limits);
    tokio::time::pause();
    let mut reading = Box::pin(fetcher.read_html(response, encoding, &body_ctx));
    assert!(futures_util::poll!(reading.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(2)).await;
    assert_eq!(reading.await.unwrap_err().code, ErrorCode::Timeout);
    assert!(body_ctx.remaining().unwrap() > Duration::from_secs(20));
    release.send(()).unwrap();
    server.finish().await;
}

#[tokio::test(start_paused = true)]
async fn expired_or_cancelled_context_stops_provider_before_connecting() {
    let url = Url::parse("https://duckduckgo.com/").unwrap();
    for cancelled in [false, true] {
        let ctx = RequestContext::new(if cancelled { BUDGET } else { Duration::ZERO });
        if cancelled {
            ctx.cancellation.cancel();
        }
        let error = provider::send(&fetcher(Limits::default()), &url, Vec::new(), false, &ctx)
            .await
            .unwrap_err();
        assert_eq!(
            error.code,
            if cancelled {
                ErrorCode::Cancelled
            } else {
                ErrorCode::Timeout
            }
        );
    }
}

struct EarlyEofBody {
    sent: bool,
}

impl hyper::body::Body for EarlyEofBody {
    type Data = hyper::body::Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        if self.sent {
            std::task::Poll::Ready(None)
        } else {
            self.sent = true;
            std::task::Poll::Ready(Some(Ok(hyper::body::Frame::data(
                hyper::body::Bytes::from_static(b"abc"),
            ))))
        }
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        hyper::body::SizeHint::with_exact(if self.sent { 2 } else { 5 })
    }
}

#[tokio::test]
async fn decoder_rejects_eof_with_declared_bytes_still_missing() {
    for script in [false, true] {
        let response = Response::from(
            http::Response::builder()
                .header(
                    CONTENT_TYPE,
                    if script {
                        "application/json"
                    } else {
                        "text/html"
                    },
                )
                .header("Content-Length", "5")
                .body(reqwest::Body::wrap(EarlyEofBody { sent: false }))
                .unwrap(),
        );
        assert_eq!(response.content_length(), Some(5));
        let encoding = response_encoding_for(&response, script).unwrap();
        let error = fetcher(Limits::default())
            .read_html(response, encoding, &RequestContext::new(BUDGET))
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::UpstreamFailed);
        assert_eq!(error.message, "Upstream closed an incomplete HTML response");
    }
}
