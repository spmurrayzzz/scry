use scry::{Config, ContentsRequest, ErrorCode, ItemStatus, Limits, Scry, SearchRequest};
use serde_json::json;

#[test]
fn public_request_json_defaults_and_strict_camel_case_contract() {
    let search: SearchRequest = serde_json::from_value(json!({"query": "builder"})).unwrap();
    assert_eq!(search.search_type, "auto");
    assert_eq!(search.num_results, 10);
    assert!(!search.include_text);
    assert!(!search.include_highlights);
    assert!(search.include_domains.is_empty());
    assert!(search.exclude_domains.is_empty());
    assert!(search.start_published_date.is_none());
    assert!(search.end_published_date.is_none());
    assert_eq!(
        serde_json::to_value(&search).unwrap(),
        serde_json::to_value(SearchRequest {
            query: "builder".into(),
            ..SearchRequest::default()
        })
        .unwrap()
    );
    let contents: ContentsRequest =
        serde_json::from_value(json!({"ids": ["https://docs.rs/"]})).unwrap();
    assert!(contents.include_text);
    assert!(!contents.include_highlights);
    assert!(contents.urls.is_empty());
    assert_eq!(contents.ids, ["https://docs.rs/"]);
    let defaults: ContentsRequest = serde_json::from_value(json!({})).unwrap();
    assert_eq!(
        serde_json::to_value(defaults).unwrap(),
        serde_json::to_value(ContentsRequest::default()).unwrap()
    );
    for value in [
        json!({}),
        json!({"query": "builder", "include_text": true}),
        json!({"query": "builder", "numResults": -1}),
        json!({"query": "builder", "unexpected": true}),
    ] {
        assert!(
            serde_json::from_value::<SearchRequest>(value.clone()).is_err(),
            "{value}"
        );
    }
    assert!(serde_json::from_value::<ContentsRequest>(json!({"url": "https://docs.rs/"})).is_err());
    let explicit: ContentsRequest = serde_json::from_value(json!({
        "urls": ["https://docs.rs/"], "includeText": false, "includeHighlights": true
    }))
    .unwrap();
    assert!(!explicit.include_text);
    assert!(explicit.include_highlights);
}

#[tokio::test]
async fn metadata_contents_normalizes_source_ids_deduplicates_in_order_and_preserves_queries() {
    let scry = Scry::new(Config::default()).unwrap();
    let request: ContentsRequest = serde_json::from_value(json!({
        "urls": [
            "HTTPS://DOCS.RS.:443/reqwest/?q=one#first",
            "https://docs.rs/reqwest/?q=two",
            "https://docs.rs/reqwest/?a=1&b=2",
            "https://bücher.de/docs#section"
        ],
        "ids": [
            "https://docs.rs/reqwest/?q=one#other",
            "https://docs.rs/reqwest/?b=2&a=1",
            "https://xn--bcher-kva.de/docs"
        ],
        "includeText": false
    }))
    .unwrap();
    let response = scry.contents(request).await.unwrap();
    let expected = [
        "https://docs.rs/reqwest/?q=one",
        "https://docs.rs/reqwest/?q=two",
        "https://docs.rs/reqwest/?a=1&b=2",
        "https://xn--bcher-kva.de/docs",
        "https://docs.rs/reqwest/?b=2&a=1",
    ];
    assert_eq!(response.results.len(), expected.len());
    assert!(
        response
            .warnings
            .iter()
            .any(|warning| warning.contains("not fetched"))
    );
    for (result, expected) in response.results.iter().zip(expected) {
        assert_eq!(result.id, expected);
        assert_eq!(result.url, expected);
        assert_eq!(result.status, ItemStatus::Ok);
        assert!(result.error.is_none());
        assert!(result.text.is_none());
        assert!(result.title.is_none());
        assert!(result.snippet.is_none());
        assert!(result.highlights.is_empty());
        assert!(result.final_url.is_none());
        assert!(result.content_origin.is_none());
        assert!(!result.truncated);
    }
    let ids_only = scry
        .contents(ContentsRequest {
            ids: response
                .results
                .iter()
                .map(|result| result.id.clone())
                .collect(),
            include_text: false,
            ..ContentsRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(ids_only).unwrap(),
        serde_json::to_value(response).unwrap()
    );
}

#[tokio::test]
async fn metadata_contents_keeps_valid_items_and_explicit_item_errors_and_checks_combined_count() {
    let scry = Scry::new(Config::default()).unwrap();
    let inputs = [
        "https://docs.rs/reqwest/",
        "opaque-provider-id",
        "http://127.0.0.1/",
        "ftp://docs.rs/",
        "https://www.rust-lang.org/",
    ];
    let response = scry
        .contents(ContentsRequest {
            urls: inputs[..3].iter().map(|input| (*input).into()).collect(),
            ids: inputs[3..].iter().map(|input| (*input).into()).collect(),
            include_text: false,
            ..ContentsRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(response.results.len(), inputs.len());
    for (index, expected_error) in [
        None,
        Some(ErrorCode::InvalidInput),
        Some(ErrorCode::BlockedDestination),
        Some(ErrorCode::InvalidInput),
        None,
    ]
    .into_iter()
    .enumerate()
    {
        let result = &response.results[index];
        assert_eq!(result.id, inputs[index]);
        assert_eq!(result.url, inputs[index]);
        assert_eq!(
            result.error.as_ref().map(|error| error.code),
            expected_error
        );
        assert_eq!(
            result.status,
            if expected_error.is_some() {
                ItemStatus::Error
            } else {
                ItemStatus::Ok
            }
        );
        assert!(result.text.is_none());
        assert!(result.content_origin.is_none());
    }
    for (url_count, id_count) in [(0, 0), (20, 1), (1, 20), (0, 21)] {
        let request = ContentsRequest {
            urls: vec!["https://docs.rs/".into(); url_count],
            ids: vec!["https://docs.rs/".into(); id_count],
            include_text: false,
            ..ContentsRequest::default()
        };
        assert_eq!(
            scry.contents(request).await.unwrap_err().code,
            ErrorCode::InvalidInput
        );
    }
}

#[test]
fn pdf_page_limit_enforces_documented_bounds() {
    assert_eq!(Limits::default().max_pdf_pages, 200);
    for value in [1, 200, 5000] {
        let config = Config {
            limits: Limits {
                max_pdf_pages: value,
                ..Limits::default()
            },
        };
        assert!(config.validate().is_ok(), "{value}");
    }
    for value in [0, 5001] {
        let config = Config {
            limits: Limits {
                max_pdf_pages: value,
                ..Limits::default()
            },
        };
        let error = config.validate().unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidInput, "{value}");
    }
}
