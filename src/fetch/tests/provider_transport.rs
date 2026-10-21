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
        None,
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

#[test]
fn provider_headers_match_only_the_https_provider_origin() {
    for (input, expected) in [
        ("https://html.duckduckgo.com/html/", true),
        ("https://HTML.DUCKDUCKGO.COM:443/other", true),
        ("http://html.duckduckgo.com/html/", false),
        ("https://html.duckduckgo.com:8443/html/", false),
        ("https://duckduckgo.com/html/", false),
        ("https://lite.duckduckgo.com/lite/", false),
        ("https://html.duckduckgo.com.example.org/", false),
        ("https://sub.html.duckduckgo.com/html/", false),
        ("https://example.org/?host=html.duckduckgo.com", false),
    ] {
        assert_eq!(
            provider::matches(&Url::parse(input).unwrap()),
            expected,
            "{input}"
        );
    }
}

#[tokio::test]
async fn wire_requests_preserve_pinned_host_form_and_provider_headers() {
    let limits = Limits::default();
    let form = [("q", "rust & tls"), ("kl", "us-en"), ("b", "")];
    for form in [None, Some(form.as_slice())] {
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
            form,
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
        if form.is_some() {
            assert_eq!(headers.lines().next(), Some("POST /page HTTP/1.1"));
            assert_eq!(body, "q=rust+%26+tls&kl=us-en&b=");
            assert_eq!(
                header(&request, "content-type"),
                Some("application/x-www-form-urlencoded")
            );
        } else {
            assert_eq!(headers.lines().next(), Some("GET /page HTTP/1.1"));
            assert!(body.is_empty());
        }
        assert_eq!(
            header(&request, "host"),
            Some(format!("pinned.invalid:{}", server.address.port()).as_str())
        );
        assert!(
            header(&request, "user-agent")
                .unwrap()
                .contains("Chrome/149.")
        );
        assert_eq!(
            header(&request, "accept"),
            Some("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
        );
        for (name, value) in [
            ("accept-encoding", "identity"),
            ("accept-language", "en-US,en;q=0.9"),
            ("sec-fetch-dest", "document"),
            ("sec-fetch-mode", "navigate"),
            ("sec-fetch-site", "none"),
            ("sec-fetch-user", "?1"),
        ] {
            assert_eq!(header(&request, name), Some(value), "{name}");
        }
        for name in [
            "cookie",
            "authorization",
            "proxy-authorization",
            "referer",
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
        None,
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
    let url = Url::parse("https://html.duckduckgo.com/html/").unwrap();
    for cancelled in [false, true] {
        let ctx = RequestContext::new(if cancelled { BUDGET } else { Duration::ZERO });
        if cancelled {
            ctx.cancellation.cancel();
        }
        let error = provider::send(&fetcher(Limits::default()), &url, Vec::new(), None, &ctx)
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
    let response = Response::from(
        http::Response::builder()
            .header(CONTENT_TYPE, "text/html")
            .header("Content-Length", "5")
            .body(reqwest::Body::wrap(EarlyEofBody { sent: false }))
            .unwrap(),
    );
    assert_eq!(response.content_length(), Some(5));
    let encoding = response_encoding(&response).unwrap();
    let error = fetcher(Limits::default())
        .read_html(response, encoding, &RequestContext::new(BUDGET))
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::UpstreamFailed);
    assert_eq!(error.message, "Upstream closed an incomplete HTML response");
}
