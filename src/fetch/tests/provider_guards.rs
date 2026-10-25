use super::*;

fn redirect(status: u16, locations: &[&str]) -> Response {
    let mut response = http::Response::builder().status(status);
    for location in locations {
        response = response.header(LOCATION, *location);
    }
    Response::from(response.body("").unwrap())
}

#[tokio::test]
async fn provider_entry_checks_scope_and_url_limits_before_dns_or_fetch_admission() {
    let fetcher = fetcher(Limits::default());
    fetcher.semaphore.close();
    for (input, script) in [
        ("https://html.duckduckgo.com/html/", false),
        ("https://example.org/", false),
        ("https://duckduckgo.com/d.js", false),
        ("https://links.duckduckgo.com/", true),
        ("https://links.duckduckgo.com/anomaly.js", true),
        ("https://links.duckduckgo.com/d.js/", true),
        ("https://links.duckduckgo.com.example.org/d.js", true),
        ("https://links.duckduckgo.com:8443/d.js", true),
        ("http://links.duckduckgo.com/d.js", true),
        ("https://user:secret@links.duckduckgo.com/d.js", true),
        ("https://links.duckduckgo.com/d.js#fragment", true),
    ] {
        let error = fetcher
            .get_provider(
                Url::parse(input).unwrap(),
                script,
                &RequestContext::new(BUDGET),
            )
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, ErrorCode::BlockedDestination, "{input}");
    }
    for (input, script) in [
        ("https://duckduckgo.com/?q=test", false),
        ("https://links.duckduckgo.com/d.js?o=json&q=test", true),
        ("https://duckduckgo.com/d.js?o=json&q=test", true),
    ] {
        assert_eq!(
            fetcher
                .get_provider(
                    Url::parse(input).unwrap(),
                    script,
                    &RequestContext::new(BUDGET)
                )
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::Overloaded
        );
    }
    let constrained = fetcher_with_url_limit(40);
    assert_eq!(
        constrained
            .get_provider(
                Url::parse(&format!("https://duckduckgo.com/?q={}", "x".repeat(40))).unwrap(),
                false,
                &RequestContext::new(BUDGET),
            )
            .await
            .err()
            .unwrap()
            .code,
        ErrorCode::InvalidInput
    );
}

fn fetcher_with_url_limit(max_url_bytes: usize) -> Fetcher {
    fetcher(Limits {
        max_url_bytes,
        ..Limits::default()
    })
}

#[test]
fn provider_redirects_follow_all_supported_statuses_with_scope_on_each_hop() {
    let fetcher = fetcher(Limits::default());
    let filter = DomainFilter::default();
    for status in [301, 302, 303, 307, 308] {
        for (script, initial, location, expected) in [
            (
                false,
                "https://duckduckgo.com/?q=test",
                "/?q=next",
                "https://duckduckgo.com/?q=next",
            ),
            (
                true,
                "https://links.duckduckgo.com/d.js?o=json",
                "https://duckduckgo.com/d.js?o=json&v=next",
                "https://duckduckgo.com/d.js?o=json&v=next",
            ),
            (
                true,
                "https://duckduckgo.com/d.js?o=json",
                "//links.duckduckgo.com/d.js?o=json&v=next",
                "https://links.duckduckgo.com/d.js?o=json&v=next",
            ),
        ] {
            let initial = Url::parse(initial).unwrap();
            let next = fetcher
                .redirect_target(
                    &redirect(status, &[location]),
                    &initial,
                    &filter,
                    Some(script),
                    0,
                )
                .unwrap()
                .unwrap();
            assert_eq!(next.as_str(), expected);
            let error = fetcher
                .redirect_target(
                    &redirect(status, &["https://example.org/d.js"]),
                    &next,
                    &filter,
                    Some(script),
                    1,
                )
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::BlockedDestination);
            assert!(
                fetcher
                    .redirect_target(&redirect(200, &[]), &next, &filter, Some(script), 1)
                    .unwrap()
                    .is_none()
            );
        }
    }
}

#[test]
fn provider_redirects_reject_unsafe_origins_paths_syntax_and_ambiguous_locations() {
    let fetcher = fetcher(Limits::default());
    let filter = DomainFilter::default();
    let base = Url::parse("https://links.duckduckgo.com/d.js?o=json").unwrap();
    for (location, code) in [
        (
            "http://links.duckduckgo.com/d.js",
            ErrorCode::BlockedDestination,
        ),
        (
            "https://links.duckduckgo.com:8443/d.js",
            ErrorCode::BlockedDestination,
        ),
        ("https://127.0.0.1/d.js", ErrorCode::BlockedDestination),
        ("https://example.org/d.js", ErrorCode::BlockedDestination),
        (
            "https://links.duckduckgo.com/",
            ErrorCode::BlockedDestination,
        ),
        ("/anomaly.js", ErrorCode::BlockedDestination),
        ("/d.js#fragment", ErrorCode::BlockedDestination),
        (
            "https://user:secret@links.duckduckgo.com/d.js",
            ErrorCode::InvalidInput,
        ),
        (
            "https://@links.duckduckgo.com/d.js",
            ErrorCode::InvalidInput,
        ),
        ("/d.js?q=has space", ErrorCode::InvalidInput),
        ("/d.js\\unsafe", ErrorCode::InvalidInput),
    ] {
        assert_eq!(
            fetcher
                .redirect_target(&redirect(301, &[location]), &base, &filter, Some(true), 0)
                .unwrap_err()
                .code,
            code,
            "{location}"
        );
    }
    for locations in [vec![], vec![""], vec!["/d.js", "/d.js?second"]] {
        assert_eq!(
            fetcher
                .redirect_target(&redirect(302, &locations), &base, &filter, Some(true), 0)
                .unwrap_err()
                .code,
            ErrorCode::UpstreamFailed
        );
    }
    let oversized = format!("/d.js?q={}", "x".repeat(fetcher.limits.max_url_bytes));
    assert_eq!(
        fetcher
            .redirect_target(&redirect(302, &[&oversized]), &base, &filter, Some(true), 0)
            .unwrap_err()
            .code,
        ErrorCode::UpstreamFailed
    );
    let constrained = fetcher_with_url_limit(50);
    assert_eq!(
        constrained
            .redirect_target(
                &redirect(302, &["?q=12345678901234567890"]),
                &base,
                &filter,
                Some(true),
                0
            )
            .unwrap_err()
            .code,
        ErrorCode::InvalidInput
    );
    assert_eq!(
        fetcher
            .redirect_target(
                &redirect(308, &["/d.js"]),
                &base,
                &filter,
                Some(true),
                fetcher.limits.max_redirects
            )
            .unwrap_err()
            .code,
        ErrorCode::UpstreamFailed
    );
}

#[test]
fn ordinary_redirects_keep_domain_filter_and_downgrade_checks() {
    let fetcher = fetcher(Limits::default());
    let base = Url::parse("https://example.org/").unwrap();
    let filter = DomainFilter::new(&["example.org".into()], &[], &Limits::default()).unwrap();
    assert_eq!(
        fetcher
            .redirect_target(
                &redirect(302, &["https://duckduckgo.com/"]),
                &base,
                &filter,
                None,
                0
            )
            .unwrap_err()
            .code,
        ErrorCode::DomainFiltered
    );
    assert_eq!(
        fetcher
            .redirect_target(
                &redirect(302, &["http://example.org/"]),
                &base,
                &filter,
                None,
                0
            )
            .unwrap_err()
            .code,
        ErrorCode::BlockedDestination
    );
}

#[tokio::test]
async fn script_media_exception_is_scoped_and_keeps_the_shared_bounded_decoder() {
    let fetcher = fetcher(Limits::default());
    for kind in [
        "application/javascript",
        "text/javascript",
        "application/x-javascript",
    ] {
        let (response, mut server, ctx) = response(
            &fetcher,
            "200 OK",
            &format!("Content-Type: {kind}; charset=utf-8\r\n"),
            true,
            &[b"{\"results\":[]}"],
        )
        .await;
        assert_eq!(
            response_kind_for(&response, None).unwrap_err().code,
            ErrorCode::UnsupportedContent
        );
        let (kind, encoding) = response_kind_for(&response, Some(true)).unwrap();
        assert_eq!(
            fetcher
                .read_text(response, kind, encoding, &ctx)
                .await
                .unwrap(),
            "{\"results\":[]}"
        );
        server.finish().await;
    }
    let (json_response, mut server, ctx) = response(
        &fetcher,
        "200 OK",
        "Content-Type: application/json; charset=utf-8\r\n",
        true,
        &[b"{\"results\":[]}"],
    )
    .await;
    let (kind, encoding) = response_kind_for(&json_response, None).unwrap();
    assert_eq!(
        fetcher
            .read_text(json_response, kind, encoding, &ctx)
            .await
            .unwrap(),
        "{\"results\":[]}"
    );
    server.finish().await;
    for headers in [
        "Content-Type: text/plain\r\n",
        "Content-Type: application/json\r\nContent-Encoding: gzip\r\n",
        "Content-Type: application/json\r\nContent-Type: text/javascript\r\n",
        "Content-Type: application/json; charset=unknown\r\n",
    ] {
        let (response, mut server, _) = response(&fetcher, "200 OK", headers, true, &[b"{}"]).await;
        assert_eq!(
            response_kind_for(&response, Some(true)).unwrap_err().code,
            ErrorCode::UnsupportedContent
        );
        server.finish().await;
    }
    let bounded = super::fetcher(Limits {
        max_download_bytes: 2,
        ..Limits::default()
    });
    let (response, mut server, ctx) = response(
        &bounded,
        "200 OK",
        "Content-Type: application/json\r\n",
        true,
        &[b"{}x"],
    )
    .await;
    let (kind, encoding) = response_kind_for(&response, Some(true)).unwrap();
    assert_eq!(
        bounded
            .read_text(response, kind, encoding, &ctx)
            .await
            .unwrap_err()
            .code,
        ErrorCode::TooLarge
    );
    server.finish().await;
}
