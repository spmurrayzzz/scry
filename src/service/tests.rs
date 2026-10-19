use super::*;

fn metadata_request() -> ContentsRequest {
    ContentsRequest {
        urls: vec!["https://docs.rs/reqwest/".into()],
        include_text: false,
        ..ContentsRequest::default()
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
