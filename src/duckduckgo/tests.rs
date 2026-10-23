use std::time::Duration;

use super::*;

fn context() -> RequestContext {
    RequestContext::new(Duration::from_secs(30))
}

fn base() -> Url {
    Url::parse("https://links.duckduckgo.com/d.js?o=json&dp=private").unwrap()
}

fn results(body: &str) -> Result<Vec<DiscoveredResult>, Error> {
    match web_results(body, &base(), &context(), &Limits::default())? {
        WebResponse::Results(results) => Ok(results),
        WebResponse::Challenge(_) => panic!("expected results"),
    }
}

#[test]
fn supported_results_need_titles_and_urls_but_snippets_can_be_missing_or_empty() {
    let found = results(r#"{"results":[
        {"u":"https://example.com/guide","t":"<b>Rust</b> &amp; HTML","a":"Parse <b>small</b> pages."},
        {"u":"https://example.com/api","t":"API","da":true},
        {"u":"https://example.com/faq","t":"FAQ","a":" "}
    ]}"#).unwrap();
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
                false
            ),
            ("https://example.com/api", "API", "", false),
            ("https://example.com/faq", "FAQ", "", false),
        ]
    );
    for body in [
        r#"{"results":[{"u":"https://example.com/","t":" "}]}"#,
        r#"{"results":[{"t":"Missing URL"}]}"#,
        r#"{"results":[{"u":"https://example.com/"}]}"#,
        r#"{"results":[{"u":"https://example.com/","t":"Title","a":4}]}"#,
    ] {
        assert_eq!(results(body).err().unwrap().code, ErrorCode::UpstreamFailed);
    }
}

#[test]
fn trusted_redirects_preserve_queries_dedupe_destinations_and_exclude_ads() {
    let found = results(r##"{"ads":[{"u":"https://ads.example/","t":"Outside organic results"}],"results":[
        {"ad":true,"u":"https://ads.example/","t":"Ad"},
        {"u":"https://duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Ffind%3Fq%3Da%2520b%26mode%3Dall%23first&rut=ignored","t":"First","a":"Original snippet."},
        {"u":"https://example.com/find?q=a%20b&mode=all#second","t":"Duplicate"},
        {"u":"https://tracker.example.com/l/?uddg=https%3A%2F%2Fhidden.example%2F","t":"Untrusted wrapper"},
        {"n":"/d.js?next=ignored"}
    ]}"##).unwrap();
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
                "Original snippet."
            ),
            (
                "https://tracker.example.com/l/?uddg=https%3A%2F%2Fhidden.example%2F",
                "Untrusted wrapper",
                ""
            ),
        ]
    );
}

#[test]
fn pagination_sentinels_allow_metadata_without_discarding_organic_results() {
    let found = results(
        r#"{"results":[
        {"u":"https://example.com/guide","t":"Guide","a":"Useful page."},
        {"n":"/d.js?next=1","t":"EOF","a":"","metadata":{"end":true}}
    ]}"#,
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
        vec![("https://example.com/guide", "Guide", "Useful page.")]
    );
    for body in [
        r#"{"results":[{"n":"/d.js?next=1","t":"EOF","a":""}]}"#,
        r#"{"results":[{"n":"https://links.duckduckgo.com/d.js?next=1","t":null,"a":4}]}"#,
    ] {
        assert!(results(body).unwrap().is_empty());
    }
    for body in [
        r#"{"results":[{"n":"","t":"EOF","a":""}]}"#,
        r#"{"results":[{"n":"https://example.com/d.js?next=1","t":"EOF","a":""}]}"#,
        r#"{"results":[{"n":"/unrelated?next=1","t":"EOF","a":""}]}"#,
        r#"{"results":[{"n":"/d.js?next=1"},{}]}"#,
        r#"{"results":[{"n":"/d.js?next=1"},{"unrelated":true}]}"#,
    ] {
        assert_eq!(results(body).err().unwrap().code, ErrorCode::UpstreamFailed);
    }
}

#[test]
fn padded_short_result_sets_keep_all_usable_results() {
    let found = results(
        r#"{"results":[
        {"u":"https://example.com/guide","t":"<b>Guide</b>","a":"First page."},
        {"u":"https://example.com/reference","t":"Reference","a":"Second page."},
        {"n":"/d.js?next=sample"},{},{},{},{}
    ]}"#,
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
            ("https://example.com/guide", "Guide", "First page.", false),
            (
                "https://example.com/reference",
                "Reference",
                "Second page.",
                false
            ),
        ]
    );
}

#[test]
fn explicit_no_results_marker_accepts_empty_padding_but_not_result_urls() {
    for body in [
        r#"{"results":[{"t":"DEEP_SIMPLE_NO_RESULTS","u":"","a":""}]}"#,
        r#"{"deep_answers":{},"results":[
            {"a":"","ae":null,"c":"","d":"","h":0,"i":"","k":"","m":0,"o":0,"p":"","t":"DEEP_SIMPLE_NO_RESULTS","u":""},
            {},{},{},{},{},{}
        ]}"#,
    ] {
        assert!(results(body).unwrap().is_empty());
    }
    let found = results(
        r#"{"results":[{"u":"https://example.com/guide","t":"DEEP_SIMPLE_NO_RESULTS","a":"A real page."}]}"#,
    )
    .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].url.as_str(), "https://example.com/guide");
    assert_eq!(found[0].title, "DEEP_SIMPLE_NO_RESULTS");
    assert_eq!(found[0].snippet, "A real page.");
}

#[test]
fn padding_does_not_hide_invalid_markers_or_rows() {
    for body in [
        r#"{"results":[{"t":"UNKNOWN_NO_RESULTS","u":"","a":""},{}]}"#,
        r#"{"results":[{"t":"DEEP_SIMPLE_NO_RESULTS","a":""},{}]}"#,
        r#"{"results":[{"t":"DEEP_SIMPLE_NO_RESULTS","u":null,"a":""},{}]}"#,
        r#"{"results":[{"t":"DEEP_SIMPLE_NO_RESULTS","u":"invalid","a":""},{}]}"#,
        r#"{"results":[{"t":"DEEP_SIMPLE_NO_RESULTS","u":""},{}]}"#,
        r#"{"results":[{"t":"DEEP_SIMPLE_NO_RESULTS","u":"","a":null},{}]}"#,
        r#"{"results":[{"t":"DEEP_SIMPLE_NO_RESULTS","u":"","a":"Not empty"},{}]}"#,
        r#"{"results":[{}, {"t":"DEEP_SIMPLE_NO_RESULTS","u":"","a":""}]}"#,
        r#"{"results":[{"t":"DEEP_SIMPLE_NO_RESULTS","u":"","a":""},{},null]}"#,
        r#"{"results":[{"t":"DEEP_SIMPLE_NO_RESULTS","u":"","a":""},{},{"unknown":true}]}"#,
        r#"{"results":[{"u":"https://example.com/","t":"Guide"},{"n":"/d.js?next=sample"},{},null]}"#,
        r#"{"results":[{"u":"https://example.com/","t":"Guide"},{"n":"/d.js?next=sample"},{},{"unknown":true}]}"#,
        r#"{"results":[{"u":"https://example.com/","t":"Guide"},{"n":"https://example.com/d.js?next=sample"},{}]}"#,
        r#"{"results":[{"u":"https://example.com/","t":"Guide"},{},{"n":"/d.js?next=sample"},{}]}"#,
        r#"{"results":[{"u":"https://example.com/","t":" "},{"n":"/d.js?next=sample"},{}]}"#,
        r#"{"results":[{"u":"https://127.0.0.1/","t":"Private"},{"n":"/d.js?next=sample"},{}]}"#,
    ] {
        assert_eq!(
            results(body)
                .err()
                .expect("invalid response must fail")
                .code,
            ErrorCode::UpstreamFailed,
            "{body}"
        );
    }
}

#[test]
fn unusable_titles_preserve_results_and_do_not_hide_usable_duplicates() {
    let found = results(
        r#"{"results":[
        {"u":"https://example.com/first","t":"First","a":"Keep this."},
        {"u":"https://example.com/second","t":"","a":"Discard this."},
        {"u":"https://example.com/second","t":"<script>Hidden</script>","a":"Still unusable."},
        {"u":"https://example.com/second","t":"Second","a":"Usable duplicate."},
        {"u":"https://example.com/third","t":" &#32; ","a":"Whitespace title."},
        {"u":"https://example.com/second","t":"Later duplicate","a":"Ignored."}
    ]}"#,
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
            ("https://example.com/first", "First", "Keep this."),
            ("https://example.com/second", "Second", "Usable duplicate."),
        ]
    );
    for body in [
        r#"{"results":[{"u":"https://example.com/","t":""}]}"#,
        r#"{"results":[{"u":"https://example.com/","t":"<script>Hidden</script>"},{"n":"/d.js?next=1","t":"EOF","a":""}]}"#,
    ] {
        assert_eq!(results(body).err().unwrap().code, ErrorCode::UpstreamFailed);
    }
}

#[test]
fn explicit_no_results_is_distinct_from_unknown_layout_and_real_challenges() {
    assert!(results(r#"{"results":[]}"#).unwrap().is_empty());
    for body in [
        "{}",
        "[]",
        r#"{"results":null}"#,
        r#"{"results":{}}"#,
        r#"{"results":[{}]}"#,
        r#"{"results":[{"n":4}]}"#,
        r#"{"results":[{"new_layout":"unknown"}]}"#,
        r#"{"results":[{"u":"https://example.com/","t":"Good"},{}]}"#,
        r#"{"results":[]"#,
    ] {
        let code = results(body).err().expect("unknown input must fail").code;
        assert!(matches!(
            code,
            ErrorCode::UpstreamFailed | ErrorCode::UpstreamBlocked
        ));
    }
    let error =
        results("<form action='//duckduckgo.com/anomaly.js'><p>Verify you are human.</p></form>")
            .err()
            .unwrap();
    assert_eq!(error.code, ErrorCode::UpstreamBlocked);
}

#[test]
fn website_query_is_encoded_and_provider_targets_are_strict() {
    assert_eq!(
        web_url("rust & HTML/é").unwrap().as_str(),
        "https://duckduckgo.com/?q=rust+%26+HTML%2F%C3%A9&t=h_&ia=web"
    );
    for (target, script, expected) in [
        ("https://duckduckgo.com/?q=test", false, true),
        ("https://links.duckduckgo.com/d.js?dp=secret", true, true),
        ("https://duckduckgo.com:443/d.js", true, true),
        ("https://links.duckduckgo.com/", false, true),
        ("http://duckduckgo.com/d.js", true, false),
        ("https://duckduckgo.com:444/d.js", true, false),
        ("https://duckduckgo.com.evil.com/d.js", true, false),
        ("https://www.duckduckgo.com/d.js", true, false),
        ("https://html.duckduckgo.com/d.js", true, false),
        ("https://user:pass@duckduckgo.com/d.js", true, false),
        ("https://duckduckgo.com/d.js#", true, false),
        ("https://duckduckgo.com/d.js/", true, false),
        ("https://duckduckgo.com/anomaly.js", true, false),
        ("https://duckduckgo.com/d.js", false, false),
        ("https://duckduckgo.com/", true, false),
    ] {
        assert_eq!(
            is_web_url(&Url::parse(target).unwrap(), script),
            expected,
            "{target}"
        );
    }
}

fn preload(html: &str) -> Result<Url, Error> {
    preload_url(
        html,
        &web_url("rust").unwrap(),
        &context(),
        &Limits::default(),
    )
}

#[test]
fn preload_extracts_server_url_without_reencoding_opaque_parameters() {
    let url = preload("<html><head><link id='deep_preload_link' rel='preload' as='script' \
        href='https://links.duckduckgo.com/d.js?q=a%20b&amp;t=D&amp;vqd=opaque%2bValue&amp;dp=A%2FB+%3d&amp;x=1&amp;x=2'></head></html>").unwrap();
    assert_eq!(
        url.as_str(),
        "https://links.duckduckgo.com/d.js?o=json&q=a%20b&t=D&vqd=opaque%2bValue&dp=A%2FB+%3d&x=1&x=2"
    );
    assert_eq!(
        preload("<link id=deep_preload_link href='/d.js?dp=opaque'>")
            .unwrap()
            .as_str(),
        "https://duckduckgo.com/d.js?o=json&dp=opaque"
    );
    assert_eq!(
        preload("<link id=deep_preload_link href='//links.duckduckgo.com/d.js?dp=opaque'>")
            .unwrap()
            .as_str(),
        "https://links.duckduckgo.com/d.js?o=json&dp=opaque"
    );
}

#[test]
fn preload_requires_one_real_link_and_challenges_take_precedence() {
    for html in [
        "<main>Normal page</main>",
        "<a id=deep_preload_link href='/d.js?dp=opaque'>not a link element</a>",
        "<script>\"<link id=deep_preload_link href='/d.js?dp=opaque'>\"</script>",
        "<template><link id=deep_preload_link href='/d.js?dp=opaque'></template>",
        "<link id=deep_preload_link href='/d.js?dp=opaque'><link id=deep_preload_link href='/d.js?dp=opaque'>",
        "<link id=deep_preload_link href='/d.js?o=json&dp=opaque'>",
        "<link id=deep_preload_link href='/d.js?%6f=js&dp=opaque'>",
        "<link id=deep_preload_link href='/d.js?o=json&o=other'>",
        "<link id=deep_preload_link href='/d.js'>",
    ] {
        assert!(preload(html).is_err(), "{html}");
    }
    let error = preload(
        "<link id=deep_preload_link href='/d.js?dp=opaque'>\
        <form action='/anomaly.js'><p>Select all squares containing a duck</p></form>",
    )
    .err()
    .unwrap();
    assert_eq!(error.code, ErrorCode::UpstreamBlocked);
}

#[test]
fn unsafe_preloads_are_rejected_without_disclosing_tokens() {
    for target in [
        "https://evil.com/d.js?dp=SECRET",
        "http://links.duckduckgo.com/d.js?dp=SECRET",
        "https://links.duckduckgo.com:444/d.js?dp=SECRET",
        "https://user:SECRET@links.duckduckgo.com/d.js",
        "https://@links.duckduckgo.com/d.js?dp=SECRET",
        "https://127.0.0.1/d.js?dp=SECRET",
        "https://duckduckgo.com/anomaly.js?dp=SECRET",
        "/d.js/?dp=SECRET",
        "/d.js?dp=SECRET#fragment",
        "/d.js?dp=SECRET#",
        "/d.js?dp=SEC RET",
        "/d.js?dp=SEC\\RET",
        "javascript:SECRET",
    ] {
        let error = preload(&format!("<link id=deep_preload_link href='{target}'>"))
            .err()
            .unwrap();
        assert_eq!(error.code, ErrorCode::UpstreamBlocked);
        assert!(!error.message.contains("SECRET"));
        assert!(!error.message.contains(target));
    }
}

#[test]
fn unsafe_result_urls_do_not_discard_usable_results() {
    let found = results(r#"{"results":[
        {"u":"https://127.0.0.1/secret","t":"Private"},
        {"u":"javascript:secret","t":"Script"},
        {"u":"","t":"Empty"},
        {"u":"not-a-url","t":"Relative"},
        {"u":"/d.js?dp=secret","t":"Provider script"},
        {"u":"https://@example.com/","t":"Empty credentials"},
        {"u":"https://user:secret@example.com/","t":"Credentials"},
        {"u":"https://example.com:444/","t":"Port"},
        {"u":"https://duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2F&uddg=https%3A%2F%2Fother.com%2F","t":"Ambiguous"},
        {"u":"https://example.com/public","t":"Public"}
    ]}"#).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].url.as_str(), "https://example.com/public");
    let error = results(r#"{"results":[{"u":"https://127.0.0.1/SECRET","t":"SECRET"}]}"#)
        .err()
        .unwrap();
    assert_eq!(error.code, ErrorCode::UpstreamFailed);
    assert!(!error.message.contains("SECRET"));
}

#[test]
fn results_have_a_global_text_budget_and_twenty_item_cap() {
    let limits = Limits {
        max_text_chars: 8,
        ..Limits::default()
    };
    let body = r#"{"results":[{"u":"https://example.com/1","t":"Title","a":"Body long"},{"u":"https://example.com/2","t":"Omitted"}]}"#;
    let WebResponse::Results(found) = web_results(body, &base(), &context(), &limits).unwrap()
    else {
        panic!("results")
    };
    assert_eq!(found.len(), 1);
    assert_eq!(
        (found[0].title.as_str(), found[0].snippet.as_str()),
        ("Title", "Bod")
    );
    assert!(found[0].truncated);
    let rows: Vec<_> = (0..21)
        .map(|i| serde_json::json!({"u": format!("https://example.com/{i}"), "t": "Title"}))
        .collect();
    let found = results(&serde_json::json!({"results": rows}).to_string()).unwrap();
    assert_eq!(found.len(), 20);
    assert!(found[19].truncated);
}

#[test]
fn individual_text_fields_are_bounded_and_hidden_markup_is_not_rendered() {
    let body = serde_json::json!({"results":[{
        "u": "https://example.com/",
        "t": "x".repeat(1025),
        "a": "<script>SECRET</script>".to_owned() + &"y".repeat(4097),
    }]})
    .to_string();
    let found = results(&body).unwrap();
    assert_eq!(found[0].title.chars().count(), 1024);
    assert_eq!(found[0].snippet.chars().count(), 4096);
    assert!(found[0].truncated);
    assert!(!found[0].snippet.contains("SECRET"));
    let error = results(r#"{"results":[{"u":"SECRET","t": }]}"#)
        .err()
        .unwrap();
    assert_eq!(error.code, ErrorCode::UpstreamFailed);
    assert!(!error.message.contains("SECRET"));
}

#[test]
fn parser_bounds_cover_bytes_json_depth_tokens_and_html_fragments() {
    let body = r#"{"results":[{"u":"https://example.com/","t":"<b><i>Title</i></b>"}]}"#;
    for limits in [
        Limits {
            max_decoded_bytes: body.len() - 1,
            ..Limits::default()
        },
        Limits {
            max_html_depth: 1,
            ..Limits::default()
        },
        Limits {
            max_html_tokens: 3,
            ..Limits::default()
        },
    ] {
        assert_eq!(
            web_results(body, &base(), &context(), &limits)
                .err()
                .unwrap()
                .code,
            ErrorCode::TooLarge
        );
    }
    let limits = Limits {
        max_html_depth: 3,
        ..Limits::default()
    };
    assert_eq!(web_results(r#"{"results":[{"u":"https://example.com/","t":"<b><i><em><strong>Deep</strong></em></i></b>"}]}"#,
        &base(), &context(), &limits).err().unwrap().code, ErrorCode::TooLarge);
    let limits = Limits {
        max_url_bytes: 30,
        ..Limits::default()
    };
    assert!(
        preload_url(
            "<link id=deep_preload_link href='/d.js?dp=long-opaque-token'>",
            &web_url("rust").unwrap(),
            &context(),
            &limits
        )
        .is_err()
    );
}

#[tokio::test(start_paused = true)]
async fn cancellation_and_deadlines_apply_to_both_parsers() {
    for cancelled in [true, false] {
        let ctx = context();
        if cancelled {
            ctx.cancellation.cancel();
        } else {
            tokio::time::advance(Duration::from_secs(30)).await;
        }
        let expected = if cancelled {
            ErrorCode::Cancelled
        } else {
            ErrorCode::Timeout
        };
        assert_eq!(
            web_results(r#"{"results":[]}"#, &base(), &ctx, &Limits::default())
                .err()
                .unwrap()
                .code,
            expected
        );
        assert_eq!(
            preload_url(
                "<link id=deep_preload_link href='/d.js?dp=opaque'>",
                &web_url("rust").unwrap(),
                &ctx,
                &Limits::default()
            )
            .err()
            .unwrap()
            .code,
            expected
        );
    }
}
