use std::time::Duration;

use super::*;

fn results(html: &str) -> Result<Vec<DiscoveredResult>, Error> {
    search_results(
        html,
        &Url::parse("https://html.duckduckgo.com/html/").unwrap(),
        &RequestContext::new(Duration::from_secs(30)),
        &Limits::default(),
    )
}

#[test]
fn supported_results_need_titles_and_urls_but_snippets_can_be_missing_or_empty() {
    let found = results(
        "<div class='result'><a class='result__a' href='https://example.com/guide'>\
         <b>Rust</b> &amp; HTML</a><a class='result__snippet'>Parse <b>small</b> pages.</a></div>\
         <div class='result'><a class='result__a' href='https://example.com/api'>API</a></div>\
         <div class='result'><a class='result__a' href='https://example.com/faq'>FAQ</a>\
         <div class='result__snippet'> </div></div>",
    )
    .unwrap();
    let items: Vec<_> = found
        .iter()
        .map(|item| {
            (
                item.url.as_str(),
                item.title.as_str(),
                item.snippet.as_str(),
                item.truncated,
            )
        })
        .collect();
    assert_eq!(
        items,
        vec![
            (
                "https://example.com/guide",
                "Rust & HTML",
                "Parse small pages.",
                false,
            ),
            ("https://example.com/api", "API", "", false),
            ("https://example.com/faq", "FAQ", "", false),
        ]
    );
    for html in [
        "<div class='result'><a class='result__a' href='https://example.com/'> </a></div>",
        "<div class='result'><a class='result__a'>Missing URL</a></div>",
    ] {
        assert_eq!(
            results(html)
                .err()
                .expect("a title and URL are both required")
                .code,
            ErrorCode::UpstreamFailed,
            "{html}"
        );
    }
}

#[test]
fn trusted_redirects_preserve_queries_dedupe_destinations_and_exclude_ads() {
    let found = results(
        "<div class='result result--ad'><a class='result__a' href='https://ads.example/'>Ad</a></div>\
         <div class='result'><a class='result__a' \
         href='//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Ffind%3Fq%3Da%2520b%26mode%3Dall%23first&amp;rut=ignored'>\
         First</a><div class='result__snippet'>Original snippet.</div></div>\
         <div class='result'><a class='result__a' \
         href='https://example.com/find?q=a%20b&amp;mode=all#second'>Duplicate</a></div>\
         <div class='result'><div class='result--ad'><a class='result__a' \
         href='https://ads.example/embedded'>Embedded ad</a></div>\
         <a class='result__a' href='https://tracker.example/l/?uddg=https%3A%2F%2Fhidden.example%2F'>\
         Untrusted wrapper</a></div>",
    )
    .unwrap();
    let items: Vec<_> = found
        .iter()
        .map(|item| {
            (
                item.url.as_str(),
                item.title.as_str(),
                item.snippet.as_str(),
            )
        })
        .collect();
    assert_eq!(
        items,
        vec![
            (
                "https://example.com/find?q=a%20b&mode=all",
                "First",
                "Original snippet.",
            ),
            (
                "https://tracker.example/l/?uddg=https%3A%2F%2Fhidden.example%2F",
                "Untrusted wrapper",
                "",
            ),
        ]
    );
}

#[test]
fn explicit_no_results_is_distinct_from_unknown_layout_and_real_challenges() {
    assert!(
        results("<div class='no-results__message'>No results found for this query.</div>")
            .unwrap()
            .is_empty()
    );
    for html in [
        "<main>No results found for this query.</main>",
        "<div class='result'><h2>New layout without a supported link</h2></div>",
    ] {
        assert_eq!(
            results(html)
                .err()
                .expect("unrecognized markup must not become an empty success")
                .code,
            ErrorCode::UpstreamFailed,
            "{html}"
        );
    }
    let error = results(
        "<div class='no-results'>No results found.</div>\
         <form action='//duckduckgo.com/anomaly.js'><p>Verify you are human.</p></form>",
    )
    .err()
    .expect("a challenge must take precedence over an empty-results marker");
    assert_eq!(error.code, ErrorCode::UpstreamBlocked);
}
