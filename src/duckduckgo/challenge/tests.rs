use std::time::Duration;

use super::*;
use crate::api::ErrorCode;
use crate::duckduckgo::{WebResponse, web_results};

const CAPTURED_SYNTAX: &str = include_str!("fixtures/window_arithmetic.js");

fn context() -> RequestContext {
    RequestContext::new(Duration::from_secs(30))
}

fn base() -> Url {
    Url::parse("https://links.duckduckgo.com/d.js?o=json&dp=opaque").unwrap()
}

fn target(body: &str) -> Result<Url, Error> {
    match web_results(body, &base(), &context(), &Limits::default())? {
        WebResponse::Challenge(url) => Ok(url),
        WebResponse::Results(_) => panic!("expected a challenge"),
    }
}

#[test]
fn sanitized_captured_window_challenge_has_the_literal_expected_answer() {
    assert_eq!(
        target(CAPTURED_SYNTAX).unwrap().as_str(),
        "https://links.duckduckgo.com/d.js?o=json&jsa=28352"
    );
}

#[test]
fn captured_wrapper_also_accepts_the_calculation_without_try_catch() {
    let body = CAPTURED_SYNTAX
        .replace("try {", "")
        .replace("} catch (e) { jsa = -1; }", "");
    for body in [body.clone(), body.replace(", false", "")] {
        assert_eq!(
            target(&body).unwrap().as_str(),
            "https://links.duckduckgo.com/d.js?o=json&jsa=28352"
        );
    }
}

#[test]
fn captured_challenge_preserves_the_opaque_server_query_prefix() {
    for prefix in [
        "/d.js?o=json&dp=A%2fb+X&v=opaque%2526%3D&jsa=",
        "https://links.duckduckgo.com/d.js?o=json&dp=A%2fb+X&v=opaque%2526%3D&jsa=",
    ] {
        let body = CAPTURED_SYNTAX.replace("/d.js?o=json&jsa=", prefix);
        assert_eq!(
            target(&body).unwrap().as_str(),
            "https://links.duckduckgo.com/d.js?o=json&dp=A%2fb+X&v=opaque%2526%3D&jsa=28352"
        );
    }
}

#[test]
fn captured_wrapper_rejects_unknown_catch_initialize_return_and_wrapper_code() {
    for (original, replacement) in [
        ("jsa = -1;", "jsa = SECRET();"),
        ("jsa = -1;", "jsa = -1; SECRET();"),
        ("jsa = -1;", "SECRET(); jsa = -1;"),
        ("jsa = -1;", "jsa = 0;"),
        ("jsa = -1;", "return -1;"),
        ("catch (e)", "finally"),
        (", false);", ", true);"),
        (", false);", ", false, SECRET());"),
        (", false);", ", false, false);"),
        (" + jsa, false", " + SECRET(), false"),
        ("isJsaChallenge: true", "isJsaChallenge: SECRET()"),
        ("isJsaChallenge: true", "isJsaChallenge: false"),
        (
            "isJsaChallenge: true",
            "isJsaChallenge: true, extra: SECRET()",
        ),
        ("return {isJsaChallenge: true};", "return SECRET();"),
        ("return {isJsaChallenge: true};", ""),
        (
            "return {isJsaChallenge: true};",
            "SECRET(); return {isJsaChallenge: true};",
        ),
        ("window.execDeep", "window.SECRET"),
        (
            "window.execDeep = function()",
            "window.execDeep = function(SECRET)",
        ),
        ("let jsa = 7;", "SECRET(); let jsa = 7;"),
        ("try {", "try { SECRET();"),
        ("};\njsa = third", "};\nSECRET(); jsa = third"),
        ("\n};", "\n}; SECRET();"),
        ("\n};", "\n}();"),
    ] {
        assert!(CAPTURED_SYNTAX.contains(original));
        let body = CAPTURED_SYNTAX.replace(original, replacement);
        let error = target(&body).expect_err("unknown syntax must fail");
        assert_eq!(error.code, ErrorCode::UpstreamBlocked);
        assert!(!error.message.contains("SECRET"));
    }
}

#[test]
fn captured_wrapper_retains_destination_checks() {
    for prefix in [
        "https://evil.com/d.js?SECRET=&jsa=",
        "http://links.duckduckgo.com/d.js?SECRET=&jsa=",
        "https://links.duckduckgo.com:444/d.js?SECRET=&jsa=",
        "https://links.duckduckgo.com.evil.com/d.js?SECRET=&jsa=",
        "https://user:SECRET@links.duckduckgo.com/d.js?jsa=",
        "/anomaly.js?SECRET=&jsa=",
        "/d.js?SECRET=#&jsa=",
        "/d.js?SECRET= &jsa=",
    ] {
        let body = CAPTURED_SYNTAX.replace("/d.js?o=json&jsa=", prefix);
        let error = target(&body).expect_err("unsafe destinations must fail");
        assert_eq!(error.code, ErrorCode::UpstreamBlocked);
        assert!(!error.message.contains("SECRET"));
        assert!(!error.message.contains(prefix));
    }
}

#[test]
fn captured_catch_never_supplies_a_result_for_invalid_arithmetic() {
    for (original, replacement) in [
        ("let jsa = 7;", "let jsa = 9007199254740991;"),
        ("num * 7", "num * 9007199254740991"),
        ("jsa = third(jsa);", "jsa = unknown(jsa);"),
        ("jsa = third(jsa);", "jsa = third(jsa); throw 1;"),
    ] {
        let body = CAPTURED_SYNTAX.replace(original, replacement);
        assert_eq!(
            target(&body).expect_err("fallback must not run").code,
            ErrorCode::UpstreamBlocked
        );
    }
}

#[test]
fn multiplication_and_all_four_browser_lengths_are_applied_in_source_order() {
    let body = "let jsa = 7;
        let times = function(num) { return num * 3; };
        let paragraph = function(num) { let e = document.createElement('div'); e.innerHTML = `<p><div></p><p></div`; return num + e.innerHTML.length; };
        let list = function(num) { const x = document.createElement(\"div\"); x.innerHTML = `<li><div></li><li></div`; return num + x.innerHTML.length; };
        let nested = function(num) { var element = document.createElement('div'); element.innerHTML = `<div><div></div><div></div`; return num + element.innerHTML.length; };
        let breaks = function(num) { let d = document.createElement('div'); d.innerHTML = `<br><div></br><br></div`; return num + d.innerHTML.length; };
        jsa = times(jsa); jsa = paragraph(jsa); jsa = list(jsa);
        jsa = times(jsa); jsa = nested(jsa); jsa = breaks(jsa);
        DDG.deep.initialize('/d.js?o=json&dp=A%2fb+X&jsa=' + jsa);";
    assert_eq!(
        target(body).unwrap().as_str(),
        "https://links.duckduckgo.com/d.js?o=json&dp=A%2fb+X&jsa=302"
    );
}

#[test]
fn each_known_html_length_has_an_independent_literal_answer() {
    for (snippet, expected) in [
        (
            "<p><div></p><p></div",
            "https://links.duckduckgo.com/d.js?jsa=42",
        ),
        (
            "<li><div></li><li></div",
            "https://links.duckduckgo.com/d.js?jsa=39",
        ),
        (
            "<div><div></div><div></div",
            "https://links.duckduckgo.com/d.js?jsa=43",
        ),
        (
            "<br><div></br><br></div",
            "https://links.duckduckgo.com/d.js?jsa=33",
        ),
    ] {
        let body = format!(
            "let jsa = 10; let add = function(num) {{ let e = document.createElement('div'); e.innerHTML = `{snippet}`; return num + e.innerHTML.length; }}; jsa = add(jsa); DDG.deep.initialize('/d.js?jsa=' + jsa);"
        );
        assert_eq!(target(&body).unwrap().as_str(), expected);
    }
}

#[test]
fn comments_and_inert_iife_scaffolding_preserve_the_known_calculation() {
    let program = "let /* seed */ jsa = 7;
        let times = function(num) { return num /* factor */ * 3; };
        jsa = times(jsa); // apply once
        DDG.deep.initialize('/d.js?jsa=' + jsa);";
    for (prefix, suffix) in [
        ("/* challenge */", "/* done */"),
        ("// challenge\n", "// done"),
        ("; 'use strict';", ";"),
        ("(function() {", "})();"),
        ("(function() {", "}());"),
        ("!function() {", "}();"),
        ("(() => {", "})();"),
        ("(() => {", "}());"),
        (
            "/* challenge */ (function() { 'use strict';",
            "; })(); // done",
        ),
    ] {
        let body = format!("{prefix}{program}{suffix}");
        assert_eq!(
            target(&body).unwrap().as_str(),
            "https://links.duckduckgo.com/d.js?jsa=21",
            "{prefix} ... {suffix}"
        );
    }
}

#[test]
fn parenthesized_multiplication_has_the_same_literal_answer() {
    for expression in ["(num * 3)", "((num * 3))"] {
        let body = format!(
            "let jsa = 7; let times = function(num) {{ return {expression}; }}; jsa = times(jsa); DDG.deep.initialize('/d.js?jsa=' + jsa);"
        );
        assert_eq!(
            target(&body).unwrap().as_str(),
            "https://links.duckduckgo.com/d.js?jsa=21"
        );
    }
}

#[test]
fn parenthesized_and_reordered_known_additions_have_literal_answers() {
    for (snippet, expected) in [
        (
            "<p><div></p><p></div",
            "https://links.duckduckgo.com/d.js?jsa=39",
        ),
        (
            "<li><div></li><li></div",
            "https://links.duckduckgo.com/d.js?jsa=36",
        ),
        (
            "<div><div></div><div></div",
            "https://links.duckduckgo.com/d.js?jsa=40",
        ),
        (
            "<br><div></br><br></div",
            "https://links.duckduckgo.com/d.js?jsa=30",
        ),
    ] {
        for expression in [
            "(num + e.innerHTML.length)",
            "((num + e.innerHTML.length))",
            "e.innerHTML.length + num",
            "(e.innerHTML.length + num)",
            "((e.innerHTML.length + num))",
        ] {
            let body = format!(
                "let jsa = 7; let add = function(num) {{ let e = document.createElement('div'); e.innerHTML = `{snippet}`; return {expression}; }}; jsa = add(jsa); DDG.deep.initialize('/d.js?jsa=' + jsa);"
            );
            assert_eq!(target(&body).unwrap().as_str(), expected);
        }
    }
}

#[test]
fn scaffolding_never_hides_unknown_code_or_changes_target_checks() {
    let program = "let jsa = 7; let times = function(num) { return (num * 3); }; jsa = times(jsa); DDG.deep.initialize('/d.js?jsa=' + jsa);";
    for (prefix, suffix) in [
        ("unknown();", ""),
        ("", "unknown();"),
        ("(function() { unknown();", "})();"),
        ("(function() {", "unknown(); })();"),
        ("(() => {", "jsa = unknown(jsa); })();"),
        ("if (false) {", "}"),
        ("function neverCalled() {", "}"),
        ("(function() {", "});"),
        ("(function() {", "})(SECRET);"),
        ("(function(SECRET) {", "})();"),
        ("(() => {", "})() + SECRET;"),
        ("/*", ""),
        ("/*", "*/"),
        ("//", ""),
    ] {
        let error = target(&format!("{prefix}{program}{suffix}")).err().unwrap();
        assert_eq!(error.code, ErrorCode::UpstreamBlocked);
        assert!(!error.message.contains("SECRET"));
    }
    for expression in [
        "(num * 3) + 1",
        "(num * 3, SECRET())",
        "(num * 3; SECRET())",
        "(num * 3",
        "num * 3)",
    ] {
        let body = program.replace("(num * 3)", expression);
        assert_eq!(
            target(&body).err().unwrap().code,
            ErrorCode::UpstreamBlocked
        );
    }
    let body = format!("/* challenge */ (() => {{ {program} }})();")
        .replace("'/d.js?jsa='", "'https://example.com/d.js?SECRET='");
    let error = target(&body).err().unwrap();
    assert_eq!(error.code, ErrorCode::UpstreamBlocked);
    assert!(!error.message.contains("SECRET"));
}

#[test]
fn zero_and_exact_safe_integer_bound_are_supported() {
    for (body, expected) in [
        (
            "let jsa = 0; let m = function(num) { return num * 999; }; jsa = m(jsa); DDG.deep.initialize('/d.js?jsa=' + jsa);",
            "https://links.duckduckgo.com/d.js?jsa=0",
        ),
        (
            "let jsa = 9007199254740991; let m = function(num) { return num * 1; }; jsa = m(jsa); DDG.deep.initialize('https://duckduckgo.com/d.js?jsa=' + jsa);",
            "https://duckduckgo.com/d.js?jsa=9007199254740991",
        ),
    ] {
        assert_eq!(target(body).unwrap().as_str(), expected);
    }
}

#[test]
fn relative_challenge_targets_use_the_source_links_origin() {
    let body = "let jsa = 2; let m = function(num) { return num * 3; }; jsa = m(jsa); DDG.deep.initialize('/d.js?jsa=' + jsa);";
    let initial = Url::parse("https://duckduckgo.com/d.js?dp=opaque").unwrap();
    assert_eq!(
        solve(body, &initial, &context(), &Limits::default())
            .unwrap()
            .as_str(),
        "https://links.duckduckgo.com/d.js?jsa=6"
    );
}

#[test]
fn malformed_unknown_ambiguous_or_truncated_programs_are_blocked() {
    for body in [
        "let jsa = 2; DDG.deep.initialize('/d.js?jsa=' + jsa);",
        "let jsa = 2; jsa = missing(jsa); DDG.deep.initialize('/d.js?jsa=' + jsa);",
        "let jsa = 2; let m = function(num) { return num / 2; }; jsa = m(jsa); DDG.deep.initialize('/d.js?jsa=' + jsa);",
        "let jsa = 2; let m = function(num) { return num * 2 + 1; }; jsa = m(jsa); DDG.deep.initialize('/d.js?jsa=' + jsa);",
        "let jsa = 2; let m = function(num) { fetch('SECRET'); return num * 2; }; jsa = m(jsa); DDG.deep.initialize('/d.js?jsa=' + jsa);",
        "let jsa = 2; let m = function(num) { return num * 2; }; let m = function(num) { return num * 3; }; jsa = m(jsa); DDG.deep.initialize('/d.js?jsa=' + jsa);",
        "let jsa = 2; let jsa = 3; let m = function(num) { return num * 2; }; jsa = m(jsa); DDG.deep.initialize('/d.js?jsa=' + jsa);",
        "let jsa = 2; let m = function(num) { return num * 2; }; jsa = m(jsa); DDG.deep.initialize('/d.js?jsa=' + jsa); DDG.deep.initialize('/d.js?jsa=' + jsa);",
        "let jsa = 2; let m = function(num) { return num * 2; }; jsa = m(jsa); DDG.deep.initialize('/d.js?jsa=' + jsa",
        "let jsa = 2; let m = function(num) { return num * 2; }; while (true) { jsa = m(jsa); } DDG.deep.initialize('/d.js?jsa=' + jsa);",
        "let jsa = 2; let m = function(num) { return num * 2; }; for (;;) { jsa = m(jsa); } DDG.deep.initialize('/d.js?jsa=' + jsa);",
        "let jsa = 2; let m = function(num) { let e = document.createElement('div'); e.innerHTML = `<p>Unknown</p>`; return num + e.innerHTML.length; }; jsa = m(jsa); DDG.deep.initialize('/d.js?jsa=' + jsa);",
        "let jsa = 2; let m = function(num) { let e = document.createElement('div'); e.innerHTML = `<p><div></p><p></div`; return num + e.outerHTML.length; }; jsa = m(jsa); DDG.deep.initialize('/d.js?jsa=' + jsa);",
        "let jsa = 2; let m = function(num) { let e = document.createElement('div'); e.innerHTML = `${SECRET}`; return num + e.innerHTML.length; }; jsa = m(jsa); DDG.deep.initialize('/d.js?jsa=' + jsa);",
    ] {
        let error = target(body).expect_err("unsupported code must fail");
        assert_eq!(error.code, ErrorCode::UpstreamBlocked);
        assert!(!error.message.contains("SECRET"));
    }
}

#[test]
fn numeric_overflow_and_unbounded_operations_are_blocked() {
    for body in [
        "let jsa = 9007199254740992; let m = function(num) { return num * 1; }; jsa = m(jsa); DDG.deep.initialize('/d.js?jsa=' + jsa);",
        "let jsa = 9007199254740991; let m = function(num) { let e = document.createElement('div'); e.innerHTML = `<p><div></p><p></div`; return num + e.innerHTML.length; }; jsa = m(jsa); DDG.deep.initialize('/d.js?jsa=' + jsa);",
        "let jsa = 9007199254740991; let m = function(num) { return num * 2; }; jsa = m(jsa); DDG.deep.initialize('/d.js?jsa=' + jsa);",
        "let jsa = 9007199254740991; let m = function(num) { return num * 9007199254740991; }; jsa = m(jsa); DDG.deep.initialize('/d.js?jsa=' + jsa);",
        "let jsa = 999999999999999999999999999; let m = function(num) { return num * 1; }; jsa = m(jsa); DDG.deep.initialize('/d.js?jsa=' + jsa);",
        "let jsa = -1; let m = function(num) { return num * 1; }; jsa = m(jsa); DDG.deep.initialize('/d.js?jsa=' + jsa);",
        "let jsa = 1; let m = function(num) { return num * 1e9; }; jsa = m(jsa); DDG.deep.initialize('/d.js?jsa=' + jsa);",
    ] {
        assert_eq!(target(body).err().unwrap().code, ErrorCode::UpstreamBlocked);
    }
    let mut body = "let jsa = 1; let m = function(num) { return num * 1; };".to_owned();
    body.push_str(&"jsa = m(jsa);".repeat(129));
    body.push_str("DDG.deep.initialize('/d.js?jsa=' + jsa);");
    assert_eq!(
        target(&body).err().unwrap().code,
        ErrorCode::UpstreamBlocked
    );
    let mut body = "let jsa = 1;".to_owned();
    for i in 0..65 {
        body.push_str(&format!("let m{i} = function(num) {{ return num * 1; }};"));
    }
    body.push_str("jsa = m0(jsa); DDG.deep.initialize('/d.js?jsa=' + jsa);");
    assert_eq!(
        target(&body).err().unwrap().code,
        ErrorCode::UpstreamBlocked
    );
}

#[test]
fn unsafe_challenge_targets_never_leave_the_provider_boundary_or_leak_secrets() {
    for prefix in [
        "https://evil.com/d.js?SECRET=",
        "http://links.duckduckgo.com/d.js?SECRET=",
        "https://links.duckduckgo.com:444/d.js?SECRET=",
        "https://links.duckduckgo.com.evil.com/d.js?SECRET=",
        "https://user:SECRET@links.duckduckgo.com/d.js?jsa=",
        "https://@links.duckduckgo.com/d.js?SECRET=",
        "https://127.0.0.1/d.js?SECRET=",
        "/anomaly.js?SECRET=",
        "/d.js/?SECRET=",
        "/d.js?SECRET=#",
        "/d.js?SECRET= ",
        "javascript:SECRET",
    ] {
        let body = format!(
            "let jsa = 2; let m = function(num) {{ return num * 3; }}; jsa = m(jsa); DDG.deep.initialize('{prefix}' + jsa);"
        );
        let error = target(&body).err().unwrap();
        assert_eq!(error.code, ErrorCode::UpstreamBlocked);
        assert!(!error.message.contains("SECRET"));
        assert!(!error.message.contains(prefix));
    }
}

#[test]
fn challenge_byte_and_token_budgets_are_finite() {
    let body = format!("{}let jsa = 1;", " ".repeat(65536));
    assert_eq!(
        target(&body).err().unwrap().code,
        ErrorCode::UpstreamBlocked
    );
    let body = "let jsa = 2; let m = function(num) { return num * 3; }; jsa = m(jsa); DDG.deep.initialize('/d.js?jsa=' + jsa);";
    let oversized_comment = format!("/*{}*/{body}", "x".repeat(MAX_BYTES));
    assert_eq!(
        target(&oversized_comment).err().unwrap().code,
        ErrorCode::UpstreamBlocked
    );
    let deeply_grouped = body.replace(
        "num * 3",
        &format!("{}num * 3{}", "(".repeat(129), ")".repeat(129)),
    );
    assert_eq!(
        target(&deeply_grouped).err().unwrap().code,
        ErrorCode::UpstreamBlocked
    );
    let length_body = "let jsa = 2; let m = function(num) { let e = document.createElement('div'); e.innerHTML = `<p><div></p><p></div`; return num + e.innerHTML.length; }; jsa = m(jsa); DDG.deep.initialize('/d.js?jsa=' + jsa);";
    for (body, limits) in [
        (
            body,
            Limits {
                max_html_tokens: 10,
                ..Limits::default()
            },
        ),
        (
            length_body,
            Limits {
                max_html_depth: 1,
                ..Limits::default()
            },
        ),
        (
            body,
            Limits {
                max_decoded_bytes: 10,
                ..Limits::default()
            },
        ),
        (
            body,
            Limits {
                max_url_bytes: 20,
                ..Limits::default()
            },
        ),
    ] {
        assert!(web_results(body, &base(), &context(), &limits).is_err());
    }
}

#[tokio::test(start_paused = true)]
async fn challenge_parser_respects_cancellation_and_deadline() {
    let body = "let jsa = 2; let m = function(num) { return num * 3; }; jsa = m(jsa); DDG.deep.initialize('/d.js?jsa=' + jsa);";
    let ctx = context();
    ctx.cancellation.cancel();
    assert_eq!(
        solve(body, &base(), &ctx, &Limits::default())
            .err()
            .unwrap()
            .code,
        ErrorCode::Cancelled
    );
    let ctx = context();
    tokio::time::advance(Duration::from_secs(30)).await;
    assert_eq!(
        solve(body, &base(), &ctx, &Limits::default())
            .err()
            .unwrap()
            .code,
        ErrorCode::Timeout
    );
}
