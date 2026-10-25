use super::*;

fn metadata_request() -> ContentsRequest {
    ContentsRequest {
        urls: vec!["https://docs.rs/reqwest/".into()],
        include_text: false,
        ..ContentsRequest::default()
    }
}

fn search_request() -> SearchRequest {
    SearchRequest {
        query: "reqwest ClientBuilder".into(),
        ..SearchRequest::default()
    }
}

fn provider_state() -> ProviderState {
    ProviderState {
        ready: Some(Instant::now()),
        failure_cooldown: false,
        backoff_secs: 0,
    }
}

const WEB_BOOTSTRAP: &str = "<link id='deep_preload_link' rel='preload' as='script' href='https://links.duckduckgo.com/d.js?q=test&amp;dp=server-token'>";
const WEB_CHALLENGE: &str = "let jsa = 7; let multiply = function(num) { return num * 3; }; jsa = multiply(jsa); DDG.deep.initialize('/d.js?o=json&jsa=' + jsa);";
const WEB_RESULTS: &str = r#"{"results":[{"u":"https://www.rust-lang.org/","t":"<b>Rust</b> &amp; safety","a":"A systems language","n":"/d.js?next=ignored"}]}"#;

fn http_response(status: u16, body: &'static str, retry: Option<u64>) -> reqwest::Response {
    let mut response = http::Response::builder()
        .status(status)
        .header("Content-Type", "application/javascript");
    if let Some(seconds) = retry {
        response = response.header("Retry-After", seconds.to_string());
    }
    response.body(reqwest::Body::from(body)).unwrap().into()
}

#[tokio::test]
async fn script_202_arithmetic_uses_the_production_reader_parser_and_one_guarded_follow() {
    let scry = Scry::new(Config::default()).unwrap();
    let ctx = scry.context();
    let mut calls = Vec::new();
    let mut responses = std::collections::VecDeque::from([
        (200, WEB_BOOTSTRAP),
        (202, WEB_CHALLENGE),
        (200, WEB_RESULTS),
    ]);
    let discovered = scry
        .discover_with("rust".into(), &ctx, |url, script| {
            assert!(duckduckgo::is_web_url(&url, script));
            assert!(scry.inner.provider.try_lock().is_err());
            calls.push((url.to_string(), script));
            let (status, body) = responses.pop_front().expect("Unexpected continuation");
            let mut response = http_response(status, body, None);
            if !script {
                response
                    .headers_mut()
                    .insert("Content-Type", http::HeaderValue::from_static("text/html"));
            }
            scry.inner
                .fetcher
                .read_response(response, url, Some(script), &ctx)
        })
        .await
        .unwrap();
    assert!(responses.is_empty());
    assert_eq!(calls.len(), 3);
    assert_eq!(
        calls[2],
        (
            "https://links.duckduckgo.com/d.js?o=json&jsa=21".into(),
            true
        )
    );
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].url.as_str(), "https://www.rust-lang.org/");
    assert_eq!(discovered[0].title, "Rust & safety");
    assert_eq!(discovered[0].snippet, "A systems language");
    assert!(!discovered[0].truncated);
    let provider = scry.inner.provider.try_lock().unwrap();
    assert_eq!(provider.backoff_secs, 0);
    assert!(!provider.failure_cooldown);
}

#[tokio::test]
async fn script_202_captured_wrapper_follows_the_literal_answer_once() {
    const CHALLENGE: &str = include_str!("../duckduckgo/challenge/fixtures/window_arithmetic.js");
    let scry = Scry::new(Config::default()).unwrap();
    let ctx = scry.context();
    let mut calls = Vec::new();
    let mut responses = std::collections::VecDeque::from([
        (200, WEB_BOOTSTRAP),
        (202, CHALLENGE),
        (200, WEB_RESULTS),
    ]);
    let discovered = scry
        .discover_with("test".into(), &ctx, |url, script| {
            assert!(duckduckgo::is_web_url(&url, script));
            assert!(scry.inner.provider.try_lock().is_err());
            assert_eq!(scry.inner.pipeline.available_permits(), 7);
            calls.push((url.to_string(), script));
            let (status, body) = responses.pop_front().expect("Unexpected continuation");
            let mut response = http_response(status, body, None);
            if !script {
                response
                    .headers_mut()
                    .insert("Content-Type", http::HeaderValue::from_static("text/html"));
            }
            let read = scry
                .inner
                .fetcher
                .read_response(response, url, Some(script), &ctx);
            async move {
                let page = read.await?;
                if status == 202 {
                    assert_eq!(page.as_text(), Some(CHALLENGE));
                    assert_eq!(
                        page.status_error.as_ref().unwrap().code,
                        ErrorCode::UpstreamBlocked
                    );
                }
                Ok(page)
            }
        })
        .await
        .unwrap();
    assert!(responses.is_empty());
    assert_eq!(
        calls,
        [
            ("https://duckduckgo.com/?q=test&t=h_&ia=web".into(), false),
            (
                "https://links.duckduckgo.com/d.js?o=json&q=test&dp=server-token".into(),
                true
            ),
            (
                "https://links.duckduckgo.com/d.js?o=json&jsa=28352".into(),
                true
            ),
        ]
    );
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].url.as_str(), "https://www.rust-lang.org/");
    assert_eq!(discovered[0].title, "Rust & safety");
    assert_eq!(discovered[0].snippet, "A systems language");
    assert!(!discovered[0].truncated);
    let provider = scry.inner.provider.try_lock().unwrap();
    assert_eq!(provider.backoff_secs, 0);
    assert!(!provider.failure_cooldown);
    assert_eq!(scry.inner.requests.available_permits(), 20);
    assert_eq!(scry.inner.pipeline.available_permits(), 8);
    assert_eq!(scry.inner.parsers.available_permits(), 2);
}

#[tokio::test(start_paused = true)]
async fn script_202_captured_wrapper_keeps_status_at_parser_wait_deadline_without_continuation() {
    const CHALLENGE: &str = include_str!("../duckduckgo/challenge/fixtures/window_arithmetic.js");
    let scry = Scry::new(Config::default()).unwrap();
    let ctx = scry.context();
    let mut held = None;
    let mut calls = Vec::new();
    let mut responses = std::collections::VecDeque::from([
        (200, WEB_BOOTSTRAP),
        (202, CHALLENGE),
        (200, WEB_RESULTS),
    ]);
    let error = scry
        .discover_with("test".into(), &ctx, |url, script| {
            assert!(duckduckgo::is_web_url(&url, script));
            assert!(scry.inner.provider.try_lock().is_err());
            assert_eq!(scry.inner.pipeline.available_permits(), 7);
            calls.push((url.to_string(), script));
            let (status, body) = responses.pop_front().expect("Unexpected continuation");
            if status == 202 {
                held = Some(
                    scry.inner
                        .parsers
                        .clone()
                        .try_acquire_many_owned(scry.config().limits.max_parsers as u32)
                        .unwrap(),
                );
            }
            let mut response = http_response(status, body, None);
            if !script {
                response
                    .headers_mut()
                    .insert("Content-Type", http::HeaderValue::from_static("text/html"));
            }
            let read = scry
                .inner
                .fetcher
                .read_response(response, url, Some(script), &ctx);
            async move {
                let page = read.await?;
                if status == 202 {
                    assert_eq!(page.as_text(), Some(CHALLENGE));
                    assert_eq!(
                        page.status_error.as_ref().unwrap().code,
                        ErrorCode::UpstreamBlocked
                    );
                }
                Ok(page)
            }
        })
        .await
        .err()
        .expect("Captured challenge bypassed the shared parser limiter");
    assert!(Instant::now() >= ctx.deadline);
    assert!(Instant::now() <= ctx.deadline + Duration::from_millis(1));
    assert_eq!(
        calls,
        [
            ("https://duckduckgo.com/?q=test&t=h_&ia=web".into(), false),
            (
                "https://links.duckduckgo.com/d.js?o=json&q=test&dp=server-token".into(),
                true
            ),
        ]
    );
    assert_eq!(responses.len(), 1);
    assert_eq!(error.code, ErrorCode::UpstreamBlocked);
    assert_eq!(error.retry_after_secs, Some(60));
    assert_eq!(
        error.message,
        "Upstream returned HTTP 202. scry applies a DuckDuckGo retry cooldown"
    );
    let provider = scry.inner.provider.try_lock().unwrap();
    assert_eq!(provider.backoff_secs, 60);
    assert!(provider.failure_cooldown);
    assert_eq!(scry.inner.pipeline.available_permits(), 8);
    assert_eq!(scry.inner.parsers.available_permits(), 0);
    drop(held.unwrap());
    assert_eq!(scry.inner.parsers.available_permits(), 2);
}

#[tokio::test]
async fn script_202_unknown_image_json_and_repeated_bodies_preserve_status_retry_and_cooldown() {
    for scripts in [
        vec![(202, "let jsa = 7; arbitraryCode();")],
        vec![(
            202,
            "<form id='challenge-form'><div class='anomaly-modal__puzzle'><input type='checkbox' class='anomaly-modal__check' name='image-check_secret'><img class='anomaly-modal__image'></div></form>",
        )],
        vec![(202, WEB_RESULTS)],
        vec![(202, r#"{"results":[]}"#)],
        vec![(202, "{malformed")],
        vec![(
            202,
            "let jsa = 7; let multiply = function(num) { return num * 3; }; jsa = multiply(jsa); DDG.deep.initialize('https://example.org/d.js?jsa=' + jsa);",
        )],
        vec![(202, WEB_CHALLENGE), (202, WEB_CHALLENGE)],
        vec![(200, WEB_CHALLENGE), (202, WEB_CHALLENGE)],
        vec![(200, WEB_CHALLENGE), (202, WEB_RESULTS)],
        vec![(200, WEB_CHALLENGE), (202, "unknown")],
    ] {
        let scry = Scry::new(Config::default()).unwrap();
        let ctx = scry.context();
        let expected_calls = scripts.len() + 1;
        let mut responses = std::collections::VecDeque::from(scripts);
        let mut calls = 0;
        let error = scry
            .discover_with("test".into(), &ctx, |url, script| {
                calls += 1;
                let response = if script {
                    let (status, body) =
                        responses.pop_front().expect("Unexpected challenge follow");
                    let retry = if calls == 2 && body == WEB_CHALLENGE {
                        0
                    } else {
                        777
                    };
                    http_response(status, body, (status == 202).then_some(retry))
                } else {
                    let mut response = http_response(200, WEB_BOOTSTRAP, None);
                    response
                        .headers_mut()
                        .insert("Content-Type", http::HeaderValue::from_static("text/html"));
                    response
                };
                scry.inner
                    .fetcher
                    .read_response(response, url, Some(script), &ctx)
            })
            .await
            .err()
            .unwrap();
        assert_eq!(calls, expected_calls);
        assert!(responses.is_empty());
        assert_eq!(error.code, ErrorCode::UpstreamBlocked);
        assert_eq!(error.retry_after_secs, Some(777));
        assert_eq!(
            error.message,
            "Upstream returned HTTP 202. scry applies a DuckDuckGo retry cooldown"
        );
        let provider = scry.inner.provider.try_lock().unwrap();
        assert_eq!(provider.backoff_secs, 60);
        assert!(provider.failure_cooldown);
        drop(provider);
        assert_eq!(
            scry.search(search_request()).await.unwrap_err().code,
            ErrorCode::RateLimited
        );
    }
}

#[tokio::test]
async fn script_202_retry_after_waits_only_when_it_fits_and_cancellation_wins() {
    for (retry, cancel, expected_calls) in [
        (0, false, 3),
        (1, false, 3),
        (30, false, 2),
        (777, false, 2),
        (u64::MAX, false, 2),
        (1, true, 2),
    ] {
        let scry = Scry::new(Config::default()).unwrap();
        let ctx = scry.context();
        let mut calls = 0;
        let mut challenge_at = None;
        let mut continuation_at = None;
        let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
        let mut discovery = Box::pin(scry.discover_with("test".into(), &ctx, |url, script| {
            let stage = calls;
            calls += 1;
            let (status, body) = [
                (200, WEB_BOOTSTRAP),
                (202, WEB_CHALLENGE),
                (200, WEB_RESULTS),
            ][stage];
            if stage == 1 {
                challenge_at = Some(Instant::now());
                tokio::time::pause();
                sent.send(()).unwrap();
            } else if stage == 2 {
                continuation_at = Some(Instant::now());
            }
            let mut response = http_response(status, body, (status == 202).then_some(retry));
            if !script {
                response
                    .headers_mut()
                    .insert("Content-Type", http::HeaderValue::from_static("text/html"));
            }
            scry.inner
                .fetcher
                .read_response(response, url, Some(script), &ctx)
        }));
        if cancel {
            tokio::select! {
                biased;
                _ = received.recv() => {},
                _ = discovery.as_mut() => panic!("Discovery completed before cancellation"),
            }
            ctx.cancellation.cancel();
        }
        let result = discovery.await;
        assert_eq!(calls, expected_calls);
        if expected_calls == 3 {
            assert_eq!(result.unwrap().len(), 1);
            assert!(continuation_at.unwrap() - challenge_at.unwrap() >= Duration::from_secs(retry));
            assert!(!scry.inner.provider.try_lock().unwrap().failure_cooldown);
        } else if cancel {
            assert_eq!(result.err().unwrap().code, ErrorCode::Cancelled);
            assert!(!scry.inner.provider.try_lock().unwrap().failure_cooldown);
        } else {
            let error = result.err().unwrap();
            assert_eq!(error.code, ErrorCode::UpstreamBlocked);
            assert_eq!(error.retry_after_secs, Some(retry.max(60)));
            assert!(error.message.starts_with("Upstream returned HTTP 202"));
            assert!(scry.inner.provider.try_lock().unwrap().failure_cooldown);
        }
        tokio::time::resume();
    }
}

#[tokio::test]
async fn script_202_parser_limits_preserve_the_known_status() {
    let scry = Scry::new(Config {
        limits: Limits {
            max_html_tokens: 30,
            ..Limits::default()
        },
    })
    .unwrap();
    let ctx = scry.context();
    let mut calls = 0;
    let error = scry
        .discover_with("test".into(), &ctx, |url, script| {
            calls += 1;
            let mut response = http_response(
                if script { 202 } else { 200 },
                if script { WEB_CHALLENGE } else { WEB_BOOTSTRAP },
                script.then_some(777),
            );
            if !script {
                response
                    .headers_mut()
                    .insert("Content-Type", http::HeaderValue::from_static("text/html"));
            }
            scry.inner
                .fetcher
                .read_response(response, url, Some(script), &ctx)
        })
        .await
        .err()
        .unwrap();
    assert_eq!(calls, 2);
    assert_eq!(error.code, ErrorCode::UpstreamBlocked);
    assert_eq!(error.retry_after_secs, Some(777));
    assert!(error.message.starts_with("Upstream returned HTTP 202"));
}

#[tokio::test]
async fn script_202_waits_for_the_existing_parser_limiter_and_keeps_status_at_deadline() {
    for cancel in [false, true] {
        let scry = Scry::new(Config::default()).unwrap();
        let ctx = scry.context();
        let mut held = None;
        let mut calls = 0;
        let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
        let mut discovery = Box::pin(scry.discover_with("test".into(), &ctx, |url, script| {
            calls += 1;
            if script {
                held = Some(
                    scry.inner
                        .parsers
                        .clone()
                        .try_acquire_many_owned(scry.config().limits.max_parsers as u32)
                        .unwrap(),
                );
                sent.send(()).unwrap();
            }
            let mut response = http_response(
                if script { 202 } else { 200 },
                if script { WEB_CHALLENGE } else { WEB_BOOTSTRAP },
                script.then_some(777),
            );
            if !script {
                response
                    .headers_mut()
                    .insert("Content-Type", http::HeaderValue::from_static("text/html"));
            }
            scry.inner
                .fetcher
                .read_response(response, url, Some(script), &ctx)
        }));
        tokio::select! {
            biased;
            _ = received.recv() => {},
            _ = discovery.as_mut() => panic!("Parser limiter was bypassed"),
        }
        assert!(futures_util::poll!(discovery.as_mut()).is_pending());
        tokio::time::pause();
        if cancel {
            ctx.cancellation.cancel();
        }
        tokio::time::advance(ctx.deadline - Instant::now()).await;
        let error = discovery.await.err().unwrap();
        assert_eq!(calls, 2);
        assert_eq!(
            error.code,
            if cancel {
                ErrorCode::Cancelled
            } else {
                ErrorCode::UpstreamBlocked
            }
        );
        if !cancel {
            assert_eq!(error.retry_after_secs, Some(777));
            assert!(error.message.starts_with("Upstream returned HTTP 202"));
        }
        assert_eq!(
            scry.inner.provider.try_lock().unwrap().failure_cooldown,
            !cancel
        );
        drop(held);
        assert_eq!(
            scry.inner.pipeline.available_permits(),
            scry.config().limits.max_fetches
        );
        tokio::time::resume();
    }
}

#[tokio::test]
async fn provider_web_workflow_keeps_one_lock_and_pipeline_through_results_or_one_challenge() {
    for challenge in [false, true] {
        let scry = Scry::new(Config::default()).unwrap();
        let ctx = scry.context();
        let mut calls = Vec::new();
        let mut bodies = std::collections::VecDeque::from(vec![WEB_BOOTSTRAP]);
        if challenge {
            bodies.push_back(WEB_CHALLENGE);
        }
        bodies.push_back(WEB_RESULTS);
        let discovered = scry
            .discover_with("rust & tls".into(), &ctx, |url, script| {
                assert!(scry.inner.provider.try_lock().is_err());
                assert_eq!(
                    scry.inner.pipeline.available_permits(),
                    scry.config().limits.max_fetches - 1
                );
                calls.push((url.to_string(), script));
                std::future::ready(Ok(crate::fetch::FetchedPage {
                    final_url: url,
                    status_error: None,
                    body: crate::fetch::PageBody::Html(
                        bodies
                            .pop_front()
                            .expect("Unexpected protocol request")
                            .into(),
                    ),
                }))
            })
            .await
            .unwrap();
        assert!(bodies.is_empty());
        let mut expected = vec![
            (
                "https://duckduckgo.com/?q=rust+%26+tls&t=h_&ia=web".to_owned(),
                false,
            ),
            (
                "https://links.duckduckgo.com/d.js?o=json&q=test&dp=server-token".to_owned(),
                true,
            ),
        ];
        if challenge {
            expected.push((
                "https://links.duckduckgo.com/d.js?o=json&jsa=21".to_owned(),
                true,
            ));
        }
        assert_eq!(calls, expected);
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].url.as_str(), "https://www.rust-lang.org/");
        assert_eq!(discovered[0].title, "Rust & safety");
        assert_eq!(discovered[0].snippet, "A systems language");
        assert!(!discovered[0].truncated);
        let provider = scry.inner.provider.try_lock().unwrap();
        assert_eq!(provider.backoff_secs, 0);
        assert!(!provider.failure_cooldown);
        assert_eq!(
            scry.inner.pipeline.available_permits(),
            scry.config().limits.max_fetches
        );
        assert_eq!(
            scry.inner.parsers.available_permits(),
            scry.config().limits.max_parsers
        );
    }
}

#[tokio::test]
async fn provider_web_quoted_queries_accept_padded_short_and_empty_results() {
    const SHORT_RESULTS: &str = r#"{"results":[
        {"u":"https://example.com/guide","t":"Guide","a":"First page."},
        {"u":"https://example.com/reference","t":"Reference","a":"Second page."},
        {"n":"/d.js?next=sample"},{},{},{},{}
    ]}"#;
    const NO_RESULTS: &str = r#"{"results":[
        {"t":"DEEP_SIMPLE_NO_RESULTS","u":"","a":""},{},{},{},{},{},{}
    ]}"#;
    for (query, expected_url, body, expected_count) in [
        (
            "catalog \"blue lamp\" \"desk\"",
            "https://duckduckgo.com/?q=catalog+%22blue+lamp%22+%22desk%22&t=h_&ia=web",
            SHORT_RESULTS,
            2,
        ),
        (
            "\"sample\" \"rare-component\"",
            "https://duckduckgo.com/?q=%22sample%22+%22rare-component%22&t=h_&ia=web",
            NO_RESULTS,
            0,
        ),
        (
            "\"demo-tool\" \"sample-flag\" catalog",
            "https://duckduckgo.com/?q=%22demo-tool%22+%22sample-flag%22+catalog&t=h_&ia=web",
            NO_RESULTS,
            0,
        ),
    ] {
        let scry = Scry::new(Config::default()).unwrap();
        let ctx = scry.context();
        let mut calls = Vec::new();
        let mut bodies = std::collections::VecDeque::from([WEB_BOOTSTRAP, body]);
        let discovered = scry
            .discover_with(query.into(), &ctx, |url, script| {
                calls.push((url.to_string(), script));
                std::future::ready(Ok(crate::fetch::FetchedPage {
                    final_url: url,
                    status_error: None,
                    body: crate::fetch::PageBody::Html(
                        bodies
                            .pop_front()
                            .expect("Unexpected protocol request")
                            .into(),
                    ),
                }))
            })
            .await
            .unwrap();
        assert!(bodies.is_empty());
        assert_eq!(
            calls,
            [
                (expected_url.to_owned(), false),
                (
                    "https://links.duckduckgo.com/d.js?o=json&q=test&dp=server-token".to_owned(),
                    true,
                ),
            ]
        );
        assert_eq!(discovered.len(), expected_count);
        let provider = scry.inner.provider.try_lock().unwrap();
        assert_eq!(provider.backoff_secs, 0);
        assert!(!provider.failure_cooldown);
    }
}

#[tokio::test]
async fn provider_web_rejects_image_unknown_and_repeated_challenges_without_more_requests() {
    for bodies in [
        vec!["<form id='challenge-form'><p>Verify you are human</p></form>"],
        vec![WEB_BOOTSTRAP, "<form id='challenge-form'></form>"],
        vec![WEB_BOOTSTRAP, "let jsa = 7; arbitraryCode();"],
        vec![WEB_BOOTSTRAP, WEB_CHALLENGE, WEB_CHALLENGE],
    ] {
        let scry = Scry::new(Config::default()).unwrap();
        let ctx = scry.context();
        let expected_calls = bodies.len();
        let mut bodies = std::collections::VecDeque::from(bodies);
        let mut calls = 0;
        let error = scry
            .discover_with("test".into(), &ctx, |url, _| {
                calls += 1;
                std::future::ready(Ok(crate::fetch::FetchedPage {
                    final_url: url,
                    status_error: None,
                    body: crate::fetch::PageBody::Html(
                        bodies
                            .pop_front()
                            .expect("Unexpected challenge follow")
                            .into(),
                    ),
                }))
            })
            .await
            .err()
            .unwrap();
        assert_eq!(calls, expected_calls);
        assert!(bodies.is_empty());
        assert_eq!(error.code, ErrorCode::UpstreamBlocked);
        assert_eq!(error.retry_after_secs, Some(60));
        let provider = scry.inner.provider.try_lock().unwrap();
        assert_eq!(provider.backoff_secs, 60);
        assert!(provider.failure_cooldown);
        drop(provider);
        assert_eq!(
            scry.search(search_request()).await.unwrap_err().code,
            ErrorCode::RateLimited
        );
    }
}

#[tokio::test]
async fn provider_web_every_fetch_stage_finishes_known_status_and_retry_even_at_deadline() {
    for failure_stage in 0..3 {
        for code in [
            ErrorCode::UpstreamBlocked,
            ErrorCode::RateLimited,
            ErrorCode::UpstreamFailed,
        ] {
            let scry = Scry::new(Config::default()).unwrap();
            let ctx = scry.context();
            let mut calls = 0;
            let error = scry
                .discover_with("test".into(), &ctx, |url, _| {
                    let stage = calls;
                    calls += 1;
                    let deadline = ctx.deadline;
                    async move {
                        if stage == failure_stage {
                            tokio::time::pause();
                            tokio::time::advance(
                                deadline.saturating_duration_since(Instant::now()),
                            )
                            .await;
                            Err(Error {
                                code,
                                message: "Upstream returned an HTTP error".into(),
                                retry_after_secs: Some(777),
                            })
                        } else {
                            Ok(crate::fetch::FetchedPage {
                                final_url: url,
                                status_error: None,
                                body: crate::fetch::PageBody::Html(
                                    [WEB_BOOTSTRAP, WEB_CHALLENGE][stage].into(),
                                ),
                            })
                        }
                    }
                })
                .await
                .err()
                .unwrap();
            assert_eq!(calls, failure_stage + 1);
            assert_eq!(error.code, code);
            assert_eq!(error.retry_after_secs, Some(777));
            assert!(error.message.starts_with("Upstream returned an HTTP error"));
            let provider = scry.inner.provider.try_lock().unwrap();
            assert!(provider.failure_cooldown);
            assert_eq!(
                provider.backoff_secs,
                if code == ErrorCode::UpstreamFailed {
                    0
                } else {
                    60
                }
            );
            assert_eq!(
                provider.ready,
                Some(Instant::now() + Duration::from_secs(777))
            );
            assert_eq!(
                scry.inner.pipeline.available_permits(),
                scry.config().limits.max_fetches
            );
            tokio::time::resume();
        }
    }
}

#[tokio::test]
async fn provider_web_stage_cancellation_completes_without_adding_or_resetting_backoff() {
    for cancellation_stage in 0..3 {
        let scry = Scry::new(Config::default()).unwrap();
        scry.inner.provider.lock().await.backoff_secs = 120;
        let ctx = scry.context();
        let mut calls = 0;
        let error = scry
            .discover_with("test".into(), &ctx, |url, _| {
                let stage = calls;
                calls += 1;
                std::future::ready(if stage == cancellation_stage {
                    ctx.cancellation.cancel();
                    Err(Error::cancelled())
                } else {
                    Ok(crate::fetch::FetchedPage {
                        final_url: url,
                        status_error: None,
                        body: crate::fetch::PageBody::Html(
                            [WEB_BOOTSTRAP, WEB_CHALLENGE][stage].into(),
                        ),
                    })
                })
            })
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, ErrorCode::Cancelled);
        assert_eq!(calls, cancellation_stage + 1);
        let provider = scry.inner.provider.try_lock().unwrap();
        assert_eq!(provider.backoff_secs, 120);
        assert!(!provider.failure_cooldown);
        assert_eq!(
            scry.inner.pipeline.available_permits(),
            scry.config().limits.max_fetches
        );
    }
}

#[tokio::test(start_paused = true)]
async fn provider_web_parser_wait_cancellation_releases_the_pipeline_and_provider_lock() {
    let scry = Scry::new(Config::default()).unwrap();
    let _parsers = scry
        .inner
        .parsers
        .clone()
        .acquire_many_owned(scry.config().limits.max_parsers as u32)
        .await
        .unwrap();
    let ctx = scry.context();
    let mut pending = Box::pin(scry.discover_with("test".into(), &ctx, |url, _| {
        std::future::ready(Ok(crate::fetch::FetchedPage {
            final_url: url,
            status_error: None,
            body: crate::fetch::PageBody::Html(WEB_BOOTSTRAP.into()),
        }))
    }));
    assert!(futures_util::poll!(pending.as_mut()).is_pending());
    assert!(scry.inner.provider.try_lock().is_err());
    assert_eq!(
        scry.inner.pipeline.available_permits(),
        scry.config().limits.max_fetches - 1
    );
    ctx.cancellation.cancel();
    assert_eq!(pending.await.err().unwrap().code, ErrorCode::Cancelled);
    assert!(scry.inner.provider.try_lock().is_ok());
    assert_eq!(
        scry.inner.pipeline.available_permits(),
        scry.config().limits.max_fetches
    );
}

#[tokio::test(start_paused = true)]
async fn provider_state_blocked_recoveries_double_from_sixty_seconds_and_cap_at_nine_hundred() {
    for code in [ErrorCode::UpstreamBlocked, ErrorCode::RateLimited] {
        let mut provider = provider_state();
        for seconds in [60, 120, 240, 480, 900, 900] {
            provider.started();
            let started = Instant::now();
            tokio::time::advance(Duration::from_millis(250)).await;
            let mut result = Err(Error::new(code, "Provider refused the request"));
            provider.finished(&mut result);
            let error = result.err().unwrap();
            assert_eq!(error.code, code);
            assert_eq!(error.retry_after_secs, Some(seconds));
            assert!(error.message.starts_with("Provider refused the request"));
            assert!(
                error
                    .message
                    .contains("scry applies a DuckDuckGo retry cooldown")
            );
            assert_eq!(provider.backoff_secs, seconds);
            assert!(provider.failure_cooldown);
            assert_eq!(
                provider.ready,
                Some(started + Duration::from_millis(250) + Duration::from_secs(seconds))
            );
            tokio::time::advance(Duration::from_secs(seconds)).await;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn provider_state_success_resets_backoff_but_keeps_start_spacing() {
    let mut provider = provider_state();
    for seconds in [60, 120] {
        provider.started();
        provider.finished(&mut Err(Error::new(ErrorCode::UpstreamBlocked, "Blocked")));
        tokio::time::advance(Duration::from_secs(seconds)).await;
    }
    assert_eq!(provider.backoff_secs, 120);
    provider.started();
    let next = Instant::now() + Duration::from_secs(1);
    let mut success = Ok(Vec::new());
    provider.finished(&mut success);
    assert!(success.is_ok());
    assert_eq!(provider.backoff_secs, 0);
    assert!(!provider.failure_cooldown);
    assert_eq!(provider.ready, Some(next));
    tokio::time::advance(Duration::from_secs(1)).await;
    provider.started();
    let mut blocked = Err(Error::new(ErrorCode::UpstreamBlocked, "Blocked again"));
    provider.finished(&mut blocked);
    assert_eq!(blocked.err().unwrap().retry_after_secs, Some(60));
    assert_eq!(provider.backoff_secs, 60);
}

#[tokio::test(start_paused = true)]
async fn provider_state_retry_after_precedence_includes_zero_overflow_and_nonblocked_errors() {
    for code in [
        ErrorCode::UpstreamBlocked,
        ErrorCode::RateLimited,
        ErrorCode::UpstreamFailed,
    ] {
        for retry in [None, Some(0), Some(30), Some(1200), Some(u64::MAX)] {
            let blocked = code != ErrorCode::UpstreamFailed;
            let expected = match (blocked, retry) {
                (true, None | Some(0) | Some(30)) => Some(60),
                (_, retry) => retry,
            };
            let mut provider = provider_state();
            provider.started();
            let started = Instant::now();
            let mut result = Err(Error {
                code,
                message: "Upstream returned HTTP 503".into(),
                retry_after_secs: retry,
            });
            provider.finished(&mut result);
            let error = result.err().unwrap();
            assert_eq!(error.code, code);
            assert_eq!(error.retry_after_secs, expected);
            assert_eq!(provider.backoff_secs, if blocked { 60 } else { 0 });
            assert_eq!(provider.failure_cooldown, expected.is_some_and(|n| n > 0));
            match expected {
                Some(u64::MAX) => assert!(provider.ready.is_none()),
                Some(seconds) if seconds > 0 => {
                    assert_eq!(provider.ready, Some(started + Duration::from_secs(seconds)));
                    tokio::time::advance(Duration::from_secs(seconds)).await;
                    provider.started();
                    let mut recovery = Err(Error::new(ErrorCode::UpstreamBlocked, "Blocked"));
                    provider.finished(&mut recovery);
                    assert_eq!(
                        recovery.err().unwrap().retry_after_secs,
                        Some(if blocked { 120 } else { 60 })
                    );
                }
                _ => {
                    assert_eq!(provider.ready, Some(started + Duration::from_secs(1)));
                    assert_eq!(error.message, "Upstream returned HTTP 503");
                }
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn provider_state_other_errors_and_cancellation_do_not_add_or_reset_backoff() {
    for code in [
        ErrorCode::InvalidInput,
        ErrorCode::Unsupported,
        ErrorCode::BlockedDestination,
        ErrorCode::DomainFiltered,
        ErrorCode::DnsFailed,
        ErrorCode::UpstreamFailed,
        ErrorCode::Timeout,
        ErrorCode::Cancelled,
        ErrorCode::TooLarge,
        ErrorCode::UnsupportedContent,
        ErrorCode::ExtractionFailed,
        ErrorCode::Overloaded,
        ErrorCode::Internal,
    ] {
        for backoff_secs in [0, 120] {
            for retry_after_secs in [None, Some(0)] {
                let mut provider = provider_state();
                provider.backoff_secs = backoff_secs;
                provider.started();
                let ready = provider.ready;
                let original = Error {
                    code,
                    message: "Request failed".into(),
                    retry_after_secs,
                };
                let mut result = Err(original.clone());
                provider.finished(&mut result);
                assert_eq!(result.err().unwrap(), original);
                assert_eq!(provider.ready, ready);
                assert_eq!(provider.backoff_secs, backoff_secs);
                assert!(!provider.failure_cooldown);
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn provider_parser_challenge_enters_failure_cooldown_without_changing_its_error_code() {
    let scry = Scry::new(Config::default()).unwrap();
    scry.inner.pipeline.close();
    let mut discovered = duckduckgo::preload_url(
        "<form action='//duckduckgo.com/anomaly.js'><p>Verify you are human.</p></form>",
        &Url::parse("https://duckduckgo.com/").unwrap(),
        &scry.context(),
        &scry.config().limits,
    )
    .map(|_| Vec::new());
    assert_eq!(
        discovered.as_ref().err().unwrap().code,
        ErrorCode::UpstreamBlocked
    );
    {
        let mut provider = scry.inner.provider.lock().await;
        provider.started();
        provider.finished(&mut discovered);
    }
    let error = discovered.err().unwrap();
    assert_eq!(error.code, ErrorCode::UpstreamBlocked);
    assert_eq!(error.retry_after_secs, Some(60));
    assert!(error.message.contains("access challenge"));
    let started = Instant::now();
    let rejected = scry.search(search_request()).await.unwrap_err();
    assert_eq!(rejected.code, ErrorCode::RateLimited);
    assert_eq!(rejected.retry_after_secs, Some(60));
    assert_eq!(Instant::now(), started);
}

#[tokio::test(start_paused = true)]
async fn provider_cooldown_is_shared_only_by_clones_rejects_fast_rounds_up_and_expires() {
    let scry = Scry::new(Config::default()).unwrap();
    scry.inner.pipeline.close();
    let clone = scry.clone();
    let separate = Scry::new(Config::default()).unwrap();
    separate.inner.pipeline.close();
    {
        let mut provider = scry.inner.provider.lock().await;
        provider.started();
        provider.finished(&mut Err(Error::new(ErrorCode::UpstreamBlocked, "Blocked")));
    }
    let started = Instant::now();
    for (elapsed_ms, remaining) in [(0, 60), (250, 60), (1000, 59), (58000, 1)] {
        tokio::time::advance(Duration::from_millis(elapsed_ms)).await;
        for service in [&scry, &clone] {
            let before = Instant::now();
            let error = service
                .search_with_context(
                    search_request(),
                    RequestContext::new(Duration::from_secs(300)),
                )
                .await
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::RateLimited);
            assert_eq!(error.retry_after_secs, Some(remaining));
            assert_eq!(Instant::now(), before);
            assert_eq!(
                service.inner.requests.available_permits(),
                service.config().limits.max_requests
            );
        }
    }
    let error = separate.search(search_request()).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::Internal);
    assert_eq!(error.message, "Pipeline limiter closed");
    assert_eq!(separate.inner.provider.lock().await.backoff_secs, 0);
    assert_eq!(scry.inner.provider.lock().await.backoff_secs, 60);
    tokio::time::advance(Duration::from_millis(750)).await;
    assert_eq!(Instant::now() - started, Duration::from_secs(60));
    for service in [&scry, &clone] {
        let error = service.search(search_request()).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Internal);
        assert_eq!(error.message, "Pipeline limiter closed");
        assert_eq!(error.retry_after_secs, None);
    }
}

#[tokio::test(start_paused = true)]
async fn provider_unrepresentable_retry_after_rejects_without_waiting_or_panicking() {
    let scry = Scry::new(Config::default()).unwrap();
    scry.inner.pipeline.close();
    {
        let mut provider = scry.inner.provider.lock().await;
        provider.started();
        provider.finished(&mut Err(Error {
            code: ErrorCode::UpstreamFailed,
            message: "Upstream returned HTTP 503".into(),
            retry_after_secs: Some(u64::MAX),
        }));
        assert!(provider.ready.is_none());
    }
    let started = Instant::now();
    let error = scry.search(search_request()).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::RateLimited);
    assert_eq!(error.retry_after_secs, Some(u64::MAX));
    assert_eq!(Instant::now(), started);
    assert!(scry.inner.provider.try_lock().is_ok());
    assert_eq!(
        scry.inner.requests.available_permits(),
        scry.config().limits.max_requests
    );
}

#[tokio::test(start_paused = true)]
async fn provider_spacing_waits_one_second_only_when_the_deadline_permits() {
    for deadline_ms in [999, 1000, 1001, 2000] {
        let scry = Scry::new(Config::default()).unwrap();
        scry.inner.pipeline.close();
        scry.inner.provider.lock().await.started();
        let started = Instant::now();
        let mut pending = Box::pin(scry.search_with_context(
            search_request(),
            RequestContext::new(Duration::from_millis(deadline_ms)),
        ));
        if deadline_ms <= 1000 {
            let error = pending.await.unwrap_err();
            assert_eq!(error.code, ErrorCode::RateLimited);
            assert_eq!(error.retry_after_secs, Some(1));
            assert_eq!(Instant::now(), started);
        } else {
            assert!(futures_util::poll!(pending.as_mut()).is_pending());
            assert!(scry.inner.provider.try_lock().is_err());
            tokio::time::advance(Duration::from_millis(999)).await;
            assert!(futures_util::poll!(pending.as_mut()).is_pending());
            tokio::time::advance(Duration::from_millis(1)).await;
            let error = pending.await.unwrap_err();
            assert_eq!(error.code, ErrorCode::Internal);
            assert_eq!(error.message, "Pipeline limiter closed");
            assert_eq!(Instant::now() - started, Duration::from_secs(1));
        }
        let provider = scry.inner.provider.lock().await;
        assert!(!provider.failure_cooldown);
        assert_eq!(provider.backoff_secs, 0);
        assert_eq!(
            scry.inner.requests.available_permits(),
            scry.config().limits.max_requests
        );
    }
}

#[tokio::test(start_paused = true)]
async fn provider_expired_cooldown_recovery_requests_serialize_behind_the_real_pipeline() {
    let scry = Scry::new(Config {
        limits: Limits {
            max_requests: 2,
            max_fetches: 1,
            ..Limits::default()
        },
    })
    .unwrap();
    let _pipeline = scry.inner.pipeline.clone().acquire_owned().await.unwrap();
    {
        let mut provider = scry.inner.provider.lock().await;
        provider.started();
        provider.finished(&mut Err(Error::new(ErrorCode::UpstreamBlocked, "Blocked")));
    }
    tokio::time::advance(Duration::from_secs(60)).await;
    let clone = scry.clone();
    let mut first = Box::pin(scry.search(search_request()));
    assert!(futures_util::poll!(first.as_mut()).is_pending());
    assert!(scry.inner.provider.try_lock().is_err());
    let mut second = Box::pin(clone.search(search_request()));
    assert!(futures_util::poll!(second.as_mut()).is_pending());
    assert_eq!(scry.inner.requests.available_permits(), 0);
    assert_eq!(
        scry.search(search_request()).await.unwrap_err().code,
        ErrorCode::Overloaded
    );
    drop(first);
    assert_eq!(scry.inner.requests.available_permits(), 1);
    assert!(futures_util::poll!(second.as_mut()).is_pending());
    assert!(scry.inner.provider.try_lock().is_err());
    scry.inner.pipeline.close();
    let error = second.await.unwrap_err();
    assert_eq!(error.code, ErrorCode::Internal);
    assert_eq!(error.message, "Pipeline limiter closed");
    assert_eq!(scry.inner.requests.available_permits(), 2);
    let provider = scry.inner.provider.try_lock().unwrap();
    assert_eq!(provider.backoff_secs, 60);
}

#[tokio::test(start_paused = true)]
async fn provider_mutex_waiters_observe_the_blocked_recovery_cooldown_before_pipeline_io() {
    let scry = Scry::new(Config::default()).unwrap();
    scry.inner.pipeline.close();
    let mut provider = scry.inner.provider.lock().await;
    provider.started();
    provider.finished(&mut Err(Error::new(ErrorCode::UpstreamBlocked, "Blocked")));
    tokio::time::advance(Duration::from_secs(60)).await;
    provider.started();
    let clone = scry.clone();
    let mut first = Box::pin(scry.search(search_request()));
    let mut second = Box::pin(clone.search(search_request()));
    assert!(futures_util::poll!(first.as_mut()).is_pending());
    assert!(futures_util::poll!(second.as_mut()).is_pending());
    provider.finished(&mut Err(Error::new(
        ErrorCode::UpstreamBlocked,
        "Blocked recovery",
    )));
    drop(provider);
    let started = Instant::now();
    for pending in [first, second] {
        let error = pending.await.unwrap_err();
        assert_eq!(error.code, ErrorCode::RateLimited);
        assert_eq!(error.retry_after_secs, Some(120));
        assert_eq!(Instant::now(), started);
    }
    assert_eq!(
        scry.inner.requests.available_permits(),
        scry.config().limits.max_requests
    );
}

#[tokio::test(start_paused = true)]
async fn provider_wait_cancellation_and_dropping_release_admission_and_mutex_without_backoff() {
    for phase in ["mutex", "spacing", "pipeline"] {
        for cancel in [true, false] {
            let scry = Scry::new(Config {
                limits: Limits {
                    max_requests: 1,
                    max_fetches: 1,
                    ..Limits::default()
                },
            })
            .unwrap();
            let _pipeline = scry.inner.pipeline.clone().acquire_owned().await.unwrap();
            let mut provider = scry.inner.provider.lock().await;
            if phase == "spacing" {
                provider.started();
            }
            let ready = provider.ready;
            let held = if phase == "mutex" {
                Some(provider)
            } else {
                drop(provider);
                None
            };
            let parent = RequestContext::new(Duration::from_secs(10));
            let mut pending = Box::pin(scry.search_with_context(search_request(), parent.clone()));
            assert!(futures_util::poll!(pending.as_mut()).is_pending());
            assert_eq!(scry.inner.requests.available_permits(), 0);
            assert!(scry.inner.provider.try_lock().is_err());
            let started = Instant::now();
            if cancel {
                parent.cancellation.cancel();
                assert_eq!(pending.await.unwrap_err().code, ErrorCode::Cancelled);
            } else {
                drop(pending);
                assert!(!parent.cancellation.is_cancelled());
            }
            drop(held);
            assert_eq!(Instant::now(), started);
            assert_eq!(scry.inner.requests.available_permits(), 1);
            {
                let provider = scry.inner.provider.try_lock().unwrap();
                assert_eq!(provider.ready, ready);
                assert_eq!(provider.backoff_secs, 0);
                assert!(!provider.failure_cooldown);
            }
            scry.inner.pipeline.close();
            let error = scry.search(search_request()).await.unwrap_err();
            assert_eq!(error.code, ErrorCode::Internal);
            assert_eq!(error.message, "Pipeline limiter closed");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn provider_mutex_and_pipeline_wait_deadlines_release_admission_without_backoff() {
    for hold_mutex in [true, false] {
        let scry = Scry::new(Config {
            limits: Limits {
                request_timeout_secs: 2,
                max_fetches: 1,
                ..Limits::default()
            },
        })
        .unwrap();
        let _pipeline = scry.inner.pipeline.clone().acquire_owned().await.unwrap();
        let held = if hold_mutex {
            Some(scry.inner.provider.lock().await)
        } else {
            None
        };
        let parent = RequestContext::new(Duration::from_secs(120));
        let started = Instant::now();
        let mut pending = Box::pin(scry.search_with_context(search_request(), parent.clone()));
        assert!(futures_util::poll!(pending.as_mut()).is_pending());
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(pending.await.unwrap_err().code, ErrorCode::Timeout);
        drop(held);
        assert_eq!(Instant::now() - started, Duration::from_secs(2));
        assert!(!parent.cancellation.is_cancelled());
        assert_eq!(
            scry.inner.requests.available_permits(),
            scry.config().limits.max_requests
        );
        let provider = scry.inner.provider.try_lock().unwrap();
        assert_eq!(provider.backoff_secs, 0);
        assert!(!provider.failure_cooldown);
    }
}

#[tokio::test]
async fn search_validation_rejects_unsupported_and_invalid_requests_before_pipeline_io() {
    let scry = Scry::new(Config::default()).unwrap();
    scry.inner.pipeline.close();
    let base = SearchRequest {
        query: "reqwest ClientBuilder".into(),
        ..SearchRequest::default()
    };
    let mut cases = Vec::new();
    for query in [
        String::new(),
        " \t\n".into(),
        "a".repeat(500),
        "é".repeat(500),
    ] {
        cases.push((
            SearchRequest {
                query,
                ..base.clone()
            },
            ErrorCode::InvalidInput,
        ));
    }
    for num_results in [0, 21, usize::MAX] {
        cases.push((
            SearchRequest {
                num_results,
                ..base.clone()
            },
            ErrorCode::InvalidInput,
        ));
    }
    for search_type in ["neural", "semantic", "AUTO", ""] {
        cases.push((
            SearchRequest {
                search_type: search_type.into(),
                ..base.clone()
            },
            ErrorCode::Unsupported,
        ));
    }
    cases.push((
        SearchRequest {
            start_published_date: Some("2025-01-01".into()),
            ..base.clone()
        },
        ErrorCode::Unsupported,
    ));
    cases.push((
        SearchRequest {
            end_published_date: Some("2025-12-31".into()),
            ..base.clone()
        },
        ErrorCode::Unsupported,
    ));
    for include_domains in [vec!["https://docs.rs".into()], vec!["docs.rs".into(); 21]] {
        cases.push((
            SearchRequest {
                include_domains,
                ..base.clone()
            },
            ErrorCode::InvalidInput,
        ));
    }
    for (request, expected) in cases {
        let error = scry.search(request.clone()).await.unwrap_err();
        assert_eq!(error.code, expected, "{request:?}");
    }
    for search_type in ["auto", "keyword"] {
        let error = scry
            .search(SearchRequest {
                query: "é".repeat(499),
                search_type: search_type.into(),
                ..base.clone()
            })
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(error.message.contains("Pipeline limiter closed"));
    }
    let constrained = Scry::new(Config {
        limits: Limits {
            max_query_chars: 3,
            max_request_bytes: 256,
            ..Limits::default()
        },
    })
    .unwrap();
    constrained.inner.pipeline.close();
    assert_eq!(
        constrained
            .search(SearchRequest {
                query: "four".into(),
                ..base.clone()
            })
            .await
            .unwrap_err()
            .code,
        ErrorCode::InvalidInput
    );
    assert_eq!(
        constrained
            .search(SearchRequest {
                query: "ok".into(),
                exclude_domains: vec![format!("{}.org", "a".repeat(63)); 4],
                ..base
            })
            .await
            .unwrap_err()
            .code,
        ErrorCode::TooLarge
    );
}

#[tokio::test(start_paused = true)]
async fn contents_coordinator_clamps_long_parent_deadline_while_real_pipeline_is_held() {
    let scry = Scry::new(Config {
        limits: Limits {
            request_timeout_secs: 3,
            max_fetches: 1,
            ..Limits::default()
        },
    })
    .unwrap();
    let _pipeline = scry.inner.pipeline.clone().acquire_owned().await.unwrap();
    let parent = RequestContext::new(Duration::from_secs(120));
    let request = ContentsRequest {
        urls: vec![
            "https://docs.rs/reqwest/".into(),
            "https://www.rust-lang.org/".into(),
        ],
        ..ContentsRequest::default()
    };
    let started = Instant::now();
    let mut pending = Box::pin(scry.contents_with_context(request, parent.clone()));
    assert!(futures_util::poll!(pending.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(futures_util::poll!(pending.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(1)).await;
    let response = pending.await.unwrap();
    assert_eq!(Instant::now() - started, Duration::from_secs(3));
    assert_eq!(response.results.len(), 2);
    for result in response.results {
        assert_eq!(result.status, ItemStatus::Error);
        assert_eq!(result.error.unwrap().code, ErrorCode::Timeout);
        assert!(result.text.is_none());
        assert!(result.final_url.is_none());
        assert!(result.content_origin.is_none());
    }
    assert!(!parent.cancellation.is_cancelled());
    assert_eq!(parent.remaining().unwrap(), Duration::from_secs(117));
    assert_eq!(
        scry.inner.requests.available_permits(),
        scry.config().limits.max_requests
    );
}

#[tokio::test(start_paused = true)]
async fn cancellation_interrupts_pipeline_wait_and_releases_real_request_admission() {
    let scry = Scry::new(Config {
        limits: Limits {
            max_requests: 1,
            max_fetches: 1,
            ..Limits::default()
        },
    })
    .unwrap();
    let _pipeline = scry.inner.pipeline.clone().acquire_owned().await.unwrap();
    let parent = RequestContext::new(Duration::from_secs(120));
    let mut pending = Box::pin(scry.contents_with_context(
        ContentsRequest {
            urls: metadata_request().urls,
            ..ContentsRequest::default()
        },
        parent.clone(),
    ));
    assert!(futures_util::poll!(pending.as_mut()).is_pending());
    assert_eq!(scry.inner.requests.available_permits(), 0);
    let cancelled_at = Instant::now();
    parent.cancellation.cancel();
    assert_eq!(pending.await.unwrap_err().code, ErrorCode::Cancelled);
    assert_eq!(Instant::now(), cancelled_at);
    assert_eq!(scry.inner.requests.available_permits(), 1);
    assert_eq!(
        scry.contents(metadata_request()).await.unwrap().results[0].status,
        ItemStatus::Ok
    );
    assert_eq!(
        scry.contents_with_context(metadata_request(), parent)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Cancelled
    );
    let expired = RequestContext::new(Duration::ZERO);
    assert_eq!(
        scry.contents_with_context(metadata_request(), expired)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Timeout
    );
}

#[tokio::test(start_paused = true)]
async fn clones_share_admission_and_dropping_waiting_operation_releases_it() {
    let scry = Scry::new(Config {
        limits: Limits {
            max_requests: 1,
            max_fetches: 1,
            ..Limits::default()
        },
    })
    .unwrap();
    let clone = scry.clone();
    let _pipeline = scry.inner.pipeline.clone().acquire_owned().await.unwrap();
    let parent = RequestContext::new(Duration::from_secs(120));
    let mut pending = Box::pin(scry.contents_with_context(
        ContentsRequest {
            urls: metadata_request().urls,
            ..ContentsRequest::default()
        },
        parent.clone(),
    ));
    assert!(futures_util::poll!(pending.as_mut()).is_pending());
    assert_eq!(
        clone.contents(metadata_request()).await.unwrap_err().code,
        ErrorCode::Overloaded
    );
    assert_eq!(
        clone
            .search(SearchRequest::default())
            .await
            .unwrap_err()
            .code,
        ErrorCode::Overloaded
    );
    drop(pending);
    assert!(!parent.cancellation.is_cancelled());
    assert_eq!(
        clone.contents(metadata_request()).await.unwrap().results[0].status,
        ItemStatus::Ok
    );
    assert_eq!(scry.inner.requests.available_permits(), 1);
}

#[tokio::test(start_paused = true)]
async fn default_twenty_request_slots_are_shared_by_clones_and_both_operations_and_released() {
    assert_eq!(Limits::default().max_requests, 20);
    assert_eq!(Config::default().limits.max_requests, 20);
    for release in ["complete", "cancel", "drop"] {
        let scry = Scry::new(Config::default()).unwrap();
        let clone = scry.clone();
        assert_eq!(scry.config().limits.max_fetches, 8);
        let pipeline = scry
            .inner
            .pipeline
            .clone()
            .try_acquire_many_owned(8)
            .unwrap();
        let first_ctx = RequestContext::new(Duration::from_secs(1));
        let started = Instant::now();
        let mut pending = Vec::new();
        for index in 0..20 {
            let service = if index % 2 == 0 {
                scry.clone()
            } else {
                clone.clone()
            };
            let ctx = if index == 0 {
                first_ctx.clone()
            } else {
                service.context()
            };
            let mut operation = Box::pin(async move {
                if index % 2 == 0 {
                    service
                        .contents_with_context(
                            ContentsRequest {
                                urls: metadata_request().urls,
                                ..ContentsRequest::default()
                            },
                            ctx,
                        )
                        .await
                } else {
                    service.search_with_context(search_request(), ctx).await
                }
            });
            assert!(futures_util::poll!(operation.as_mut()).is_pending());
            assert_eq!(scry.inner.requests.available_permits(), 19 - index);
            pending.push(operation);
        }
        for service in [&scry, &clone] {
            assert_eq!(
                service.contents(metadata_request()).await.unwrap_err().code,
                ErrorCode::Overloaded
            );
            assert_eq!(
                service.search(search_request()).await.unwrap_err().code,
                ErrorCode::Overloaded
            );
        }
        let first = pending.remove(0);
        match release {
            "complete" => {
                tokio::time::advance(Duration::from_secs(1)).await;
                let response = first.await.unwrap();
                assert_eq!(response.results.len(), 1);
                assert_eq!(response.results[0].status, ItemStatus::Error);
                assert_eq!(
                    response.results[0].error.as_ref().unwrap().code,
                    ErrorCode::Timeout
                );
                assert!(response.results[0].final_url.is_none());
                assert!(response.results[0].text.is_none());
            }
            "cancel" => {
                first_ctx.cancellation.cancel();
                assert_eq!(first.await.unwrap_err().code, ErrorCode::Cancelled);
            }
            "drop" => drop(first),
            _ => unreachable!(),
        }
        assert_eq!(scry.inner.requests.available_permits(), 1);
        assert_eq!(first_ctx.cancellation.is_cancelled(), release == "cancel");
        let response = clone.contents(metadata_request()).await.unwrap();
        assert_eq!(response.results.len(), 1);
        assert_eq!(response.results[0].status, ItemStatus::Ok);
        assert_eq!(scry.inner.requests.available_permits(), 1);
        let mut replacement = Box::pin(clone.contents(ContentsRequest {
            urls: metadata_request().urls,
            ..ContentsRequest::default()
        }));
        assert!(futures_util::poll!(replacement.as_mut()).is_pending());
        assert_eq!(scry.inner.requests.available_permits(), 0);
        assert_eq!(
            scry.search(search_request()).await.unwrap_err().code,
            ErrorCode::Overloaded
        );
        drop(replacement);
        drop(pending);
        assert_eq!(scry.inner.requests.available_permits(), 20);
        assert_eq!(clone.inner.requests.available_permits(), 20);
        assert!(scry.inner.provider.try_lock().is_ok());
        assert_eq!(scry.inner.parsers.available_permits(), 2);
        assert_eq!(scry.inner.pipeline.available_permits(), 0);
        drop(pipeline);
        assert_eq!(scry.inner.pipeline.available_permits(), 8);
        assert_eq!(
            Instant::now() - started,
            Duration::from_secs(u64::from(release == "complete"))
        );
    }
}

#[tokio::test(start_paused = true)]
async fn collect_coordinator_keeps_metadata_and_later_filter_failures_while_first_job_waits() {
    let scry = Scry::new(Config {
        limits: Limits {
            max_fetches: 1,
            ..Limits::default()
        },
    })
    .unwrap();
    let _pipeline = scry.inner.pipeline.clone().acquire_owned().await.unwrap();
    let mut first = metadata("https://docs.rs/reqwest/".into());
    first.title = Some("Provider title".into());
    first.snippet = Some("Provider excerpt".into());
    first.highlights.push("Provider excerpt".into());
    first.content_origin = Some(ContentOrigin::ProviderSnippet);
    let completed = metadata("https://docs.rs/tokio/".into());
    let mut filtered = metadata("https://www.rust-lang.org/".into());
    filtered.title = Some("Provider-only metadata".into());
    let bare = metadata("https://blog.rust-lang.org/".into());
    let original = [first, completed, filtered, bare];
    let jobs = [0, 2, 3]
        .into_iter()
        .map(|index| (index, Url::parse(&original[index].url).unwrap()))
        .collect();
    let filter = DomainFilter::new(&[], &["rust-lang.org".into()], &Limits::default()).unwrap();
    let ctx = RequestContext::new(Duration::from_secs(4));
    let response = scry
        .collect(
            Response {
                results: original.to_vec(),
                warnings: Vec::new(),
            },
            jobs,
            &filter,
            ContentOptions {
                query: None,
                include_text: true,
                include_highlights: true,
            },
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(response.results.len(), original.len());
    for (result, original) in response.results.iter().zip(&original) {
        assert_eq!(result.id, original.id);
        assert_eq!(result.url, original.url);
        assert_eq!(result.title, original.title);
        assert_eq!(result.snippet, original.snippet);
        assert_eq!(result.highlights, original.highlights);
        assert_eq!(result.content_origin, original.content_origin);
        assert!(result.text.is_none());
        assert!(result.final_url.is_none());
    }
    assert_eq!(response.results[0].status, ItemStatus::Partial);
    assert_eq!(
        response.results[0].error.as_ref().unwrap().code,
        ErrorCode::Timeout
    );
    assert_eq!(
        serde_json::to_value(&response.results[1]).unwrap(),
        serde_json::to_value(&original[1]).unwrap()
    );
    for index in [2, 3] {
        assert_eq!(
            response.results[index].error.as_ref().unwrap().code,
            ErrorCode::DomainFiltered
        );
    }
    assert_eq!(response.results[2].status, ItemStatus::Partial);
    assert_eq!(response.results[3].status, ItemStatus::Error);
    assert!(!ctx.cancellation.is_cancelled());
}

#[tokio::test]
async fn response_budget_counts_serialized_bytes_preserves_metadata_and_omits_whole_markdown() {
    let markdown = "# ClientBuilder\n\nUse `Client::builder()`.\n\n```rust\nlet client = reqwest::Client::new();\n```\n";
    let results: Vec<_> = (0..3)
        .map(|index| {
            let mut result = metadata(format!("https://docs.rs/reqwest/{index}"));
            result.final_url = Some(format!("https://docs.rs/reqwest/latest/{index}"));
            result.title = Some(format!("ClientBuilder {index} é 🦀"));
            result.snippet = Some("Quoted \"builder\" with \\ escapes\n".into());
            result.highlights = vec!["Useful builder methods".into()];
            result.content_origin = Some(ContentOrigin::Live);
            result.text = Some(markdown.repeat(if index == 0 { 1 } else { 50 }));
            result
        })
        .collect();
    let response = Response {
        results,
        warnings: Vec::new(),
    };
    let mut metadata_only = response.clone();
    for result in &mut metadata_only.results {
        result.text = None;
    }
    let limit = serde_json::to_vec(&metadata_only).unwrap().len() + markdown.len() + 256;
    assert!(serde_json::to_vec(&response).unwrap().len() > limit);
    let limits = Limits {
        max_response_bytes: limit,
        ..Limits::default()
    };
    let ctx = RequestContext::new(Duration::from_secs(10));
    let bounded = bound_response(response.clone(), &limits, &ctx).unwrap();
    assert!(serde_json::to_vec(&bounded).unwrap().len() <= limit);
    assert_eq!(bounded.results.len(), response.results.len());
    assert!(
        bounded
            .warnings
            .iter()
            .any(|warning| warning.contains("omitted") && warning.contains("size limit"))
    );
    assert_eq!(bounded.results[0].text.as_deref(), Some(markdown));
    assert!(bounded.results.iter().any(|result| result.text.is_none()));
    for (result, original) in bounded.results.iter().zip(&response.results) {
        match &result.text {
            Some(text) => assert_eq!(Some(text), original.text.as_ref()),
            None => assert!(result.truncated),
        }
        let mut actual = serde_json::to_value(result).unwrap();
        let mut expected = serde_json::to_value(original).unwrap();
        for field in ["text", "truncated"] {
            actual.as_object_mut().unwrap().remove(field);
            expected.as_object_mut().unwrap().remove(field);
        }
        assert_eq!(actual, expected);
    }
    assert_eq!(
        bound_response(
            metadata_only,
            &Limits {
                max_response_bytes: 1,
                ..limits
            },
            &ctx,
        )
        .unwrap_err()
        .code,
        ErrorCode::TooLarge
    );
}

#[test]
fn provider_query_applies_domain_operators_or_warns_without_cutting_the_base_query() {
    let mut request = SearchRequest {
        query: "reqwest ClientBuilder".into(),
        include_domains: vec!["DOCS.RS.".into(), "BÜCHER.DE".into()],
        exclude_domains: vec!["private.docs.rs".into()],
        ..SearchRequest::default()
    };
    let mut warnings = Vec::new();
    assert_eq!(
        provider_query(&request, 499, &mut warnings),
        "(reqwest ClientBuilder) (site:docs.rs OR site:xn--bcher-kva.de) -site:private.docs.rs"
    );
    assert!(warnings.is_empty());
    request.query = "é".repeat(499);
    assert_eq!(provider_query(&request, 499, &mut warnings), request.query);
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("limit") && warning.contains("locally"))
    );
    warnings.clear();
    request.query = "builder".into();
    request.include_domains = vec!["[2606:4700:4700::1111]".into()];
    request.exclude_domains.clear();
    assert_eq!(provider_query(&request, 499, &mut warnings), "builder");
    assert!(warnings.iter().any(|warning| warning.contains("locally")));
}

struct KindCase {
    media: &'static str,
    body: Vec<u8>,
    expected: String,
    exact: bool,
    title: Option<&'static str>,
}

fn media_response(media: &str, body: reqwest::Body) -> reqwest::Response {
    http::Response::builder()
        .status(200)
        .header("Content-Type", media)
        .body(body)
        .unwrap()
        .into()
}

#[tokio::test]
async fn enrich_routes_all_supported_body_kinds() {
    let scry = Scry::new(Config::default()).unwrap();
    let ctx = scry.context();
    let fetcher = scry.inner.fetcher.clone();
    let options = ContentOptions {
        query: None,
        include_text: true,
        include_highlights: true,
    };
    let html = "<html><head><title>Doc</title></head><body><p>hello html</p></body></html>";
    let cases = vec![
        KindCase {
            media: "text/html",
            body: html.as_bytes().to_vec(),
            expected: "hello html".to_owned(),
            exact: false,
            title: Some("Doc"),
        },
        KindCase {
            media: "text/plain",
            body: b"hello plain".to_vec(),
            expected: "hello plain".to_owned(),
            exact: true,
            title: None,
        },
        KindCase {
            media: "text/markdown",
            body: b"# hello md".to_vec(),
            expected: "# hello md".to_owned(),
            exact: true,
            title: None,
        },
        KindCase {
            media: "text/x-markdown",
            body: b"# hello xmd".to_vec(),
            expected: "# hello xmd".to_owned(),
            exact: true,
            title: None,
        },
        KindCase {
            media: "application/json",
            body: b"{\"k\":\"v\"}".to_vec(),
            expected: "{\"k\":\"v\"}".to_owned(),
            exact: true,
            title: None,
        },
    ];
    for KindCase {
        media,
        body,
        expected,
        exact,
        title,
    } in cases
    {
        let url = Url::parse("https://example.com/page").unwrap();
        let result = scry
            .enrich_with(
                metadata(url.as_str().to_owned()),
                url,
                &DomainFilter::default(),
                options,
                &ctx,
                |url| {
                    let ctx = &ctx;
                    let response = media_response(media, reqwest::Body::from(body.clone()));
                    let fetcher = fetcher.clone();
                    async move { fetcher.read_response(response, url, None, ctx).await }
                },
            )
            .await;
        assert_eq!(result.status, ItemStatus::Ok, "{media}");
        assert_eq!(result.content_origin, Some(ContentOrigin::Live), "{media}");
        assert_eq!(result.title.as_deref(), title, "{media}");
        let text = result.text.as_deref().unwrap();
        if exact {
            assert_eq!(text, expected, "{media}");
        } else {
            assert!(text.contains(&expected), "{media}: {text}");
        }
    }
}

#[tokio::test]
async fn enrich_failures_preserve_search_metadata() {
    let scry = Scry::new(Config::default()).unwrap();
    let ctx = scry.context();
    let fetcher = scry.inner.fetcher.clone();
    let options = ContentOptions {
        query: None,
        include_text: true,
        include_highlights: false,
    };
    for (media, body) in [("application/octet-stream", &b"binary"[..])] {
        let url = Url::parse("https://example.com/page").unwrap();
        let mut result = metadata(url.as_str().to_owned());
        result.title = Some("Result title".into());
        result.snippet = Some("Result snippet".into());
        let result = scry
            .enrich_with(
                result,
                url,
                &DomainFilter::default(),
                options,
                &ctx,
                |url| {
                    let ctx = &ctx;
                    let response = media_response(media, reqwest::Body::from(body.to_vec()));
                    let fetcher = fetcher.clone();
                    async move { fetcher.read_response(response, url, None, ctx).await }
                },
            )
            .await;
        assert_eq!(result.status, ItemStatus::Partial, "{media}");
        assert_eq!(
            result.error.as_ref().unwrap().code,
            ErrorCode::UnsupportedContent,
            "{media}"
        );
        assert_eq!(result.snippet.as_deref(), Some("Result snippet"), "{media}");
        assert_eq!(result.title.as_deref(), Some("Result title"), "{media}");
        assert!(result.text.is_none(), "{media}");
    }
}

#[tokio::test]
async fn enrich_cancellation_returns_promptly_and_keeps_permits_accounted() {
    let scry = Scry::new(Config::default()).unwrap();
    let ctx = scry.context();
    let gate = Arc::new(tokio::sync::Notify::new());
    let gated = gate.clone();
    let fetcher = scry.inner.fetcher.clone();
    let url = Url::parse("https://example.com/page").unwrap();
    let filter = DomainFilter::default();
    let mut pending = Box::pin(scry.enrich_with(
        metadata(url.as_str().to_owned()),
        url,
        &filter,
        ContentOptions {
            query: None,
            include_text: true,
            include_highlights: false,
        },
        &ctx,
        |url| {
            let ctx = &ctx;
            let gate = gated.clone();
            let fetcher = fetcher.clone();
            async move {
                gate.notified().await;
                let response = media_response("text/plain", reqwest::Body::from("blocked"));
                fetcher.read_response(response, url, None, ctx).await
            }
        },
    ));
    assert!(futures_util::poll!(pending.as_mut()).is_pending());
    let max_fetches = scry.config().limits.max_fetches;
    assert_eq!(scry.inner.pipeline.available_permits(), max_fetches - 1);
    ctx.cancellation.cancel();
    let result = pending.await;
    assert_eq!(result.status, ItemStatus::Error);
    assert_eq!(result.error.unwrap().code, ErrorCode::Cancelled);
    assert_eq!(scry.inner.pipeline.available_permits(), max_fetches);
}
