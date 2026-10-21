use super::*;

const IMAGE_CHALLENGE: &str = "<form id='challenge-form' action='/anomaly.js?token=body-secret'>\
    <div class='anomaly-modal__puzzle'>\
    <input type='checkbox' class='anomaly-modal__check' name='image-check_secret'>\
    <img class='anomaly-modal__image' src='/assets/anomaly/images/challenge/secret.jpg'>\
    </div></form>";

async fn diagnose(
    limits: Limits,
    status: &str,
    headers: &str,
    chunked: bool,
    body: &[u8],
) -> Error {
    let fetcher = fetcher(limits);
    let mut diagnosed = None;
    for (url, script) in [
        ("https://duckduckgo.com/?q=request-secret", false),
        ("https://links.duckduckgo.com/d.js?q=request-secret", true),
    ] {
        let chunks = [body];
        let (response, mut server, ctx) = response(
            &fetcher,
            status,
            headers,
            chunked,
            if body.is_empty() { &[] } else { &chunks },
        )
        .await;
        let error = provider::check_response(
            &fetcher,
            response,
            &Url::parse(url).unwrap(),
            Some(script),
            &ctx,
        )
        .await
        .unwrap_err();
        server.finish().await;
        if let Some(previous) = &diagnosed {
            assert_eq!(previous, &error);
        }
        diagnosed = Some(error);
    }
    diagnosed.unwrap()
}

#[tokio::test]
async fn script_202_reader_preserves_status_on_media_charset_size_and_transfer_failures() {
    let oversized = vec![b'x'; 65537];
    let expanding = vec![0x80; 32768];
    for (headers, body, limits) in [
        ("", b"let jsa = 7;".as_slice(), Limits::default()),
        (
            "Content-Type: text/plain\r\n",
            b"x".as_slice(),
            Limits::default(),
        ),
        (
            "Content-Type: text/javascript\r\nContent-Encoding: gzip\r\n",
            b"x".as_slice(),
            Limits::default(),
        ),
        (
            "Content-Type: text/javascript; charset=unknown\r\n",
            b"x".as_slice(),
            Limits::default(),
        ),
        (
            "Content-Type: text/javascript\r\nContent-Type: text/html\r\n",
            b"x".as_slice(),
            Limits::default(),
        ),
        (
            "Content-Type: text/javascript\r\nContent-Length: 65537\r\n",
            b"".as_slice(),
            Limits::default(),
        ),
        (
            "Content-Type: text/javascript\r\nContent-Length: 1000\r\n",
            b"x".as_slice(),
            Limits::default(),
        ),
        (
            "Content-Type: text/javascript\r\n",
            oversized.as_slice(),
            Limits::default(),
        ),
        (
            "Content-Type: text/javascript; charset=windows-1252\r\n",
            expanding.as_slice(),
            Limits::default(),
        ),
        (
            "Content-Type: text/javascript\r\n",
            b"".as_slice(),
            Limits::default(),
        ),
        (
            "Content-Type: text/javascript\r\n",
            b"123456789".as_slice(),
            Limits {
                max_download_bytes: 8,
                ..Limits::default()
            },
        ),
        (
            "Content-Type: text/javascript; charset=windows-1252\r\n",
            b"\x80\x80".as_slice(),
            Limits {
                max_decoded_bytes: 5,
                ..Limits::default()
            },
        ),
    ] {
        let fetcher = fetcher(limits);
        let chunks = [body];
        let (response, mut server, ctx) = response(
            &fetcher,
            "202 Accepted",
            &format!("{headers}Retry-After: 777\r\n"),
            false,
            if body.is_empty() { &[] } else { &chunks },
        )
        .await;
        let error = fetcher
            .read_response(
                response,
                Url::parse("https://links.duckduckgo.com/d.js").unwrap(),
                Some(true),
                &ctx,
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::UpstreamBlocked, "{headers}");
        assert_eq!(error.retry_after_secs, Some(777));
        assert_eq!(error.message, "Upstream returned HTTP 202");
        server.finish().await;
    }
}

#[tokio::test]
async fn script_202_reader_only_defers_the_scoped_script_status_and_keeps_the_body_private() {
    let fetcher = fetcher(Limits::default());
    for (url, mode, status, content_type, accepted) in [
        (
            "https://links.duckduckgo.com/d.js",
            Some(true),
            "202 Accepted",
            "text/javascript",
            true,
        ),
        (
            "https://duckduckgo.com/d.js",
            Some(true),
            "202 Accepted",
            "application/javascript",
            true,
        ),
        (
            "https://links.duckduckgo.com/d.js",
            Some(true),
            "202 Accepted",
            "text/html",
            true,
        ),
        (
            "https://links.duckduckgo.com/d.js",
            Some(true),
            "202 Accepted",
            "application/json",
            true,
        ),
        (
            "https://links.duckduckgo.com/d.js",
            None,
            "202 Accepted",
            "text/html",
            false,
        ),
        (
            "https://duckduckgo.com/",
            Some(false),
            "202 Accepted",
            "text/html",
            false,
        ),
        (
            "https://example.org/d.js",
            Some(true),
            "202 Accepted",
            "text/javascript",
            false,
        ),
        (
            "https://links.duckduckgo.com/d.js",
            Some(true),
            "403 Forbidden",
            "text/javascript",
            false,
        ),
    ] {
        let (response, mut server, ctx) = response(
            &fetcher,
            status,
            &format!("Content-Type: {content_type}\r\nRetry-After: 17\r\n"),
            true,
            &[b"body-secret"],
        )
        .await;
        let result = fetcher
            .read_response(response, Url::parse(url).unwrap(), mode, &ctx)
            .await;
        let error = if accepted {
            let page = result.unwrap();
            assert_eq!(page.html, "body-secret");
            assert_eq!(page.final_url.as_str(), url);
            page.status_error.unwrap()
        } else {
            result.unwrap_err()
        };
        assert_eq!(error.code, ErrorCode::UpstreamBlocked);
        assert_eq!(error.retry_after_secs, Some(17));
        assert!(
            error
                .message
                .starts_with(&format!("Upstream returned HTTP {}", &status[..3]))
        );
        assert!(!error.message.contains("body-secret"));
        server.finish().await;
    }
}

#[tokio::test]
async fn script_202_body_read_reserves_time_and_never_replaces_known_status_with_timeout() {
    let fetcher = fetcher(Limits::default());
    for remaining_ms in [0, 50, 250, 499, 500, 501, 1000, 30000] {
        let (response, mut server, mut ctx, release) = stalled_response(&fetcher).await;
        tokio::time::pause();
        let start = Instant::now();
        ctx.deadline = start + Duration::from_millis(remaining_ms);
        let mut reading = Box::pin(fetcher.read_response(
            response,
            Url::parse("https://links.duckduckgo.com/d.js").unwrap(),
            Some(true),
            &ctx,
        ));
        if remaining_ms > 500 {
            assert!(futures_util::poll!(reading.as_mut()).is_pending());
            tokio::time::advance(Duration::from_millis(249)).await;
            assert!(futures_util::poll!(reading.as_mut()).is_pending());
            tokio::time::advance(Duration::from_millis(1)).await;
        }
        let error = reading.await.unwrap_err();
        let elapsed = Instant::now() - start;
        if remaining_ms > 500 {
            assert!((Duration::from_millis(250)..=Duration::from_millis(251)).contains(&elapsed));
        } else {
            assert_eq!(elapsed, Duration::ZERO);
        }
        assert_eq!(error.code, ErrorCode::UpstreamBlocked);
        assert_eq!(error.message, "Upstream returned HTTP 202");
        assert_eq!(error.retry_after_secs, Some(17));
        if remaining_ms > 0 {
            assert!(Instant::now() < ctx.deadline);
        }
        tokio::time::resume();
        release.send(()).unwrap();
        server.finish().await;
    }
}

#[tokio::test]
async fn script_202_body_cancellation_wins_over_status_and_expired_inspection() {
    let fetcher = fetcher(Limits::default());
    for (remaining_ms, already_cancelled) in [(30000, false), (30000, true), (0, true)] {
        let (response, mut server, mut ctx, release) = stalled_response(&fetcher).await;
        tokio::time::pause();
        ctx.deadline = Instant::now() + Duration::from_millis(remaining_ms);
        if already_cancelled {
            ctx.cancellation.cancel();
        }
        let mut reading = Box::pin(fetcher.read_response(
            response,
            Url::parse("https://links.duckduckgo.com/d.js").unwrap(),
            Some(true),
            &ctx,
        ));
        if !already_cancelled {
            assert!(futures_util::poll!(reading.as_mut()).is_pending());
            ctx.cancellation.cancel();
        }
        tokio::time::advance(Duration::from_millis(250)).await;
        assert_eq!(reading.await.unwrap_err().code, ErrorCode::Cancelled);
        tokio::time::resume();
        release.send(()).unwrap();
        server.finish().await;
    }
}

#[tokio::test]
async fn provider_diagnostics_classify_structure_without_inferring_captcha_from_status() {
    for (body, label) in [
        (IMAGE_CHALLENGE, "image CAPTCHA"),
        (
            "<form id='challenge-form'><p>Verify you are human.</p></form>",
            "access challenge",
        ),
        (
            "<form action='//duckduckgo.com/anomaly.js'></form>",
            "access challenge",
        ),
        ("<div class='anomaly-modal'></div>", "access challenge"),
        ("<p>Accepted</p>", "unclassified"),
        (
            "<p>CAPTCHA challenge-form anomaly-modal__puzzle anomaly-modal__image</p>",
            "unclassified",
        ),
        (
            "<!-- <form id='challenge-form'></form> -->\
             <script>const html = '<form id=challenge-form></form>';</script>\
             <pre>&lt;form id=challenge-form&gt;</pre>",
            "unclassified",
        ),
        (
            "<form id='challenge-form'></form><div class='anomaly-modal__puzzle'>\
             <input type=checkbox class='anomaly-modal__check' name='image-check_secret'>\
             <img class='anomaly-modal__image'></div>",
            "access challenge",
        ),
        (
            "<form id='challenge-form'><div class='anomaly-modal__puzzle'>\
             <img class='anomaly-modal__image'></div></form>",
            "access challenge",
        ),
    ] {
        let error = diagnose(
            Limits::default(),
            "202 Accepted",
            "Content-Type: text/html\r\nRetry-After: 17\r\n",
            true,
            body.as_bytes(),
        )
        .await;
        assert_eq!(error.code, ErrorCode::UpstreamBlocked);
        assert_eq!(error.retry_after_secs, Some(17));
        assert_eq!(
            error.message,
            format!("Upstream returned HTTP 202; DuckDuckGo response: {label}")
        );
    }
}

#[tokio::test]
async fn provider_diagnostics_preserve_status_codes_and_numeric_retry_after_only() {
    for (status, code, retry) in [
        ("401 Unauthorized", ErrorCode::UpstreamBlocked, Some(17)),
        ("403 Forbidden", ErrorCode::UpstreamBlocked, Some(17)),
        ("429 Too Many Requests", ErrorCode::RateLimited, Some(17)),
        (
            "503 Service Unavailable",
            ErrorCode::UpstreamFailed,
            Some(17),
        ),
        ("500 Internal Server Error", ErrorCode::UpstreamFailed, None),
    ] {
        let error = diagnose(
            Limits::default(),
            status,
            "Content-Type: text/html\r\nRetry-After: 17\r\n",
            false,
            IMAGE_CHALLENGE.as_bytes(),
        )
        .await;
        assert_eq!(error.code, code);
        assert_eq!(error.retry_after_secs, retry);
        assert_eq!(
            error.message,
            format!(
                "Upstream returned HTTP {}; DuckDuckGo response: image CAPTCHA",
                &status[..3]
            )
        );
    }
}

#[tokio::test]
async fn provider_diagnostics_keep_original_error_on_format_length_and_parser_failures() {
    let oversized = "x".repeat(32 * 1024 + 1);
    let expanding = vec![0x80; 16 * 1024];
    let deep = format!("{}{}", "<div>".repeat(33), "</div>".repeat(33));
    for (headers, body, limits) in [
        ("", IMAGE_CHALLENGE.as_bytes(), Limits::default()),
        (
            "Content-Type: text/plain\r\n",
            IMAGE_CHALLENGE.as_bytes(),
            Limits::default(),
        ),
        (
            "Content-Type: application/json\r\n",
            IMAGE_CHALLENGE.as_bytes(),
            Limits::default(),
        ),
        (
            "Content-Type: text/javascript\r\n",
            IMAGE_CHALLENGE.as_bytes(),
            Limits::default(),
        ),
        (
            "Content-Type: text/html\r\nContent-Encoding: gzip\r\n",
            IMAGE_CHALLENGE.as_bytes(),
            Limits::default(),
        ),
        (
            "Content-Type: text/html; charset=unknown\r\n",
            IMAGE_CHALLENGE.as_bytes(),
            Limits::default(),
        ),
        (
            "Content-Type: text/html\r\nContent-Type: application/xhtml+xml\r\n",
            IMAGE_CHALLENGE.as_bytes(),
            Limits::default(),
        ),
        (
            "Content-Type: text/html\r\nContent-Length: 32769\r\n",
            b"".as_slice(),
            Limits::default(),
        ),
        (
            "Content-Type: text/html\r\nContent-Length: 1000\r\n",
            IMAGE_CHALLENGE.as_bytes(),
            Limits::default(),
        ),
        (
            "Content-Type: text/html\r\n",
            oversized.as_bytes(),
            Limits::default(),
        ),
        (
            "Content-Type: text/html; charset=windows-1252\r\n",
            expanding.as_slice(),
            Limits::default(),
        ),
        (
            "Content-Type: text/html; charset=windows-1252\r\n",
            b"\x80\x80".as_slice(),
            Limits {
                max_decoded_bytes: 5,
                ..Limits::default()
            },
        ),
        (
            "Content-Type: text/html\r\n",
            IMAGE_CHALLENGE.as_bytes(),
            Limits {
                max_download_bytes: 8,
                ..Limits::default()
            },
        ),
        (
            "Content-Type: text/html\r\n",
            IMAGE_CHALLENGE.as_bytes(),
            Limits {
                max_html_tokens: 2,
                ..Limits::default()
            },
        ),
        (
            "Content-Type: text/html\r\n",
            deep.as_bytes(),
            Limits::default(),
        ),
        (
            "Content-Type: text/html\r\n",
            b"".as_slice(),
            Limits::default(),
        ),
    ] {
        let error = diagnose(
            limits,
            "202 Accepted",
            &format!("{headers}Retry-After: 17\r\n"),
            false,
            body,
        )
        .await;
        assert_eq!(error.code, ErrorCode::UpstreamBlocked, "{headers}");
        assert_eq!(error.retry_after_secs, Some(17));
        assert_eq!(error.message, "Upstream returned HTTP 202", "{headers}");
    }
}

#[tokio::test]
async fn provider_diagnostics_do_not_leak_response_or_request_data() {
    let body = format!("{IMAGE_CHALLENGE}<p>body-secret query-secret token-secret</p>");
    let error = diagnose(
        Limits::default(),
        "403 arbitrary-reason-secret",
        "Content-Type: text/html\r\nRetry-After: arbitrary-retry-secret\r\n\
         Set-Cookie: cookie-secret=value-secret\r\nServer: safe-looking-secret\r\n\
         X-Request-Id: request-id-secret\r\nLocation: https://secret.invalid/path\r\n",
        false,
        body.as_bytes(),
    )
    .await;
    assert_eq!(
        serde_json::to_value(error).unwrap(),
        serde_json::json!({
            "code": "upstream_blocked",
            "message": "Upstream returned HTTP 403; DuckDuckGo response: image CAPTCHA"
        })
    );
}

#[tokio::test]
async fn provider_diagnostics_skip_contents_pages_and_urls_outside_scoped_endpoints() {
    let fetcher = fetcher(Limits::default());
    for (url, mode) in [
        ("https://duckduckgo.com/", None),
        ("https://links.duckduckgo.com/d.js", None),
        ("http://duckduckgo.com/", Some(false)),
        ("https://duckduckgo.com:8443/", Some(false)),
        ("https://duckduckgo.com/d.js", Some(false)),
        ("https://duckduckgo.com/", Some(true)),
        ("https://html.duckduckgo.com/html/", Some(false)),
        ("https://links.duckduckgo.com/anomaly.js", Some(true)),
        ("https://example.org/d.js", Some(true)),
        ("https://links.duckduckgo.com.example.org/d.js", Some(true)),
    ] {
        let (response, mut server, ctx) = response(
            &fetcher,
            "403 Forbidden",
            "Content-Type: text/html\r\nRetry-After: 17\r\n",
            false,
            &[IMAGE_CHALLENGE.as_bytes()],
        )
        .await;
        let error =
            provider::check_response(&fetcher, response, &Url::parse(url).unwrap(), mode, &ctx)
                .await
                .unwrap_err();
        assert_eq!(error.code, ErrorCode::UpstreamBlocked);
        assert_eq!(error.message, "Upstream returned HTTP 403");
        assert_eq!(error.retry_after_secs, Some(17));
        server.finish().await;
    }
}

#[tokio::test]
async fn provider_diagnostics_leave_successful_bodies_for_the_normal_parser() {
    let fetcher = fetcher(Limits::default());
    let (response, mut server, ctx) = response(
        &fetcher,
        "200 OK",
        "Content-Type: text/html\r\n",
        true,
        &[IMAGE_CHALLENGE.as_bytes()],
    )
    .await;
    let response = provider::check_response(
        &fetcher,
        response,
        &Url::parse("https://links.duckduckgo.com/d.js").unwrap(),
        Some(true),
        &ctx,
    )
    .await
    .unwrap();
    let encoding = response_encoding(&response).unwrap();
    assert_eq!(
        fetcher.read_html(response, encoding, &ctx).await.unwrap(),
        IMAGE_CHALLENGE
    );
    server.finish().await;
}

async fn stalled_response(
    fetcher: &Fetcher,
) -> (
    Response,
    WireServer,
    RequestContext,
    tokio::sync::oneshot::Sender<()>,
) {
    let (release, gate) = tokio::sync::oneshot::channel();
    let server = WireServer::start_with_gate(
        "202 Accepted",
        "Content-Type: text/html\r\nContent-Length: 1000\r\nRetry-After: 17\r\n",
        false,
        &[IMAGE_CHALLENGE.as_bytes()],
        Some(gate),
    )
    .await;
    let ctx = RequestContext::new(Duration::from_secs(30));
    let url = Url::parse(&format!("http://pinned.invalid:{}/", server.address.port())).unwrap();
    let response = fetcher
        .client(&url, vec![server.address], &ctx)
        .unwrap()
        .get(url)
        .send()
        .await
        .unwrap();
    (response, server, ctx, release)
}

#[tokio::test]
async fn provider_diagnostics_leave_a_reserve_before_the_parent_deadline() {
    let fetcher = fetcher(Limits::default());
    let url = Url::parse("https://links.duckduckgo.com/d.js").unwrap();
    for remaining_ms in [0, 50, 250, 499, 500, 501, 1000, 30000] {
        let (response, mut server, mut ctx, release) = stalled_response(&fetcher).await;
        tokio::time::pause();
        let start = Instant::now();
        ctx.deadline = start + Duration::from_millis(remaining_ms);
        let mut checking = Box::pin(provider::check_response(
            &fetcher,
            response,
            &url,
            Some(true),
            &ctx,
        ));
        if remaining_ms > 500 {
            assert!(futures_util::poll!(checking.as_mut()).is_pending());
            tokio::time::advance(Duration::from_millis(249)).await;
            assert!(futures_util::poll!(checking.as_mut()).is_pending());
            tokio::time::advance(Duration::from_millis(1)).await;
        }
        let error = checking.await.unwrap_err();
        let elapsed = Instant::now() - start;
        if remaining_ms > 500 {
            assert!((Duration::from_millis(250)..=Duration::from_millis(251)).contains(&elapsed));
        } else {
            assert_eq!(elapsed, Duration::ZERO);
        }
        assert_eq!(error.code, ErrorCode::UpstreamBlocked);
        assert_eq!(error.message, "Upstream returned HTTP 202");
        assert_eq!(error.retry_after_secs, Some(17));
        if remaining_ms > 0 {
            assert!(ctx.remaining().unwrap() >= Duration::from_millis(remaining_ms.min(250)));
        }
        tokio::time::resume();
        release.send(()).unwrap();
        server.finish().await;
    }
}

#[tokio::test]
async fn ordinary_fetch_prioritizes_cancellation_then_deadline_over_a_ready_dns_failure() {
    use hickory_resolver::config::{ConnectionConfig, NameServerConfig, ResolverConfig};

    for (expired, cancelled) in [(false, false), (true, false), (true, true)] {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = socket.local_addr().unwrap();
        let (sent, received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut seen = [false; 2];
            for _ in 0..2 {
                let mut data = [0u8; 4096];
                let (length, peer) = socket.recv_from(&mut data).await.unwrap();
                let name = b"\x07example\x03org\0";
                let type_offset = 12 + name.len();
                let question_end = type_offset + 4;
                assert!(length >= question_end);
                assert_eq!(&data[12..type_offset], name);
                let kind = u16::from_be_bytes([data[type_offset], data[type_offset + 1]]);
                let index = match kind {
                    1 => 0,
                    28 => 1,
                    _ => panic!("Unexpected DNS query type: {kind}"),
                };
                assert!(!seen[index]);
                seen[index] = true;
                data[2] = 0x81;
                data[3] = 0x83;
                data[6..12].fill(0);
                socket.send_to(&data[..question_end], peer).await.unwrap();
            }
            assert_eq!(seen, [true, true]);
            sent.send(()).unwrap();
        });
        let mut connection = ConnectionConfig::udp();
        connection.port = address.port();
        let config = ResolverConfig::from_name_servers(vec![NameServerConfig::new(
            address.ip(),
            true,
            vec![connection],
        )]);
        let mut fetcher = fetcher(Limits {
            fetch_timeout_secs: 1,
            ..Limits::default()
        });
        fetcher.dns = dns::Dns::with_reader(1, move || Ok(config.clone()));
        let ctx = RequestContext::new(Duration::from_secs(30));
        let filter = DomainFilter::default();
        let mut fetching =
            Box::pin(fetcher.get(Url::parse("https://example.org/").unwrap(), &filter, &ctx));
        tokio::select! {
            biased;
            result = received => result.unwrap(),
            _ = fetching.as_mut() => panic!("Fetch finished before the DNS fixture replied"),
            _ = tokio::time::sleep(BUDGET) => panic!("DNS fixture exceeded its budget"),
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        tokio::time::pause();
        if expired {
            tokio::time::advance(Duration::from_secs(2)).await;
        }
        if cancelled {
            ctx.cancellation.cancel();
        }
        let error = fetching.await.err().unwrap();
        assert_eq!(
            error.code,
            if cancelled {
                ErrorCode::Cancelled
            } else if expired {
                ErrorCode::Timeout
            } else {
                ErrorCode::DnsFailed
            }
        );
        assert!(ctx.deadline - Instant::now() > Duration::from_secs(25));
        tokio::time::resume();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn provider_diagnostics_prioritize_cancellation_over_status_and_inspection_timeout() {
    let fetcher = fetcher(Limits::default());
    let url = Url::parse("https://duckduckgo.com/").unwrap();
    for (remaining_ms, already_cancelled) in [(30000, false), (30000, true), (50, true)] {
        let (response, mut server, mut ctx, release) = stalled_response(&fetcher).await;
        tokio::time::pause();
        ctx.deadline = Instant::now() + Duration::from_millis(remaining_ms);
        if already_cancelled {
            ctx.cancellation.cancel();
        }
        let mut checking = Box::pin(provider::check_response(
            &fetcher,
            response,
            &url,
            Some(false),
            &ctx,
        ));
        if !already_cancelled {
            assert!(futures_util::poll!(checking.as_mut()).is_pending());
            ctx.cancellation.cancel();
        }
        tokio::time::advance(Duration::from_millis(250)).await;
        let error = checking.await.unwrap_err();
        assert_eq!(error, Error::cancelled());
        tokio::time::resume();
        release.send(()).unwrap();
        server.finish().await;
    }
}
