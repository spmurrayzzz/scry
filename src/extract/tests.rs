use std::time::Duration;

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

use super::*;

fn context() -> RequestContext {
    RequestContext::new(Duration::from_secs(30))
}

fn base() -> Url {
    Url::parse("https://example.com/start/page").unwrap()
}

fn page(html: &str) -> ExtractedPage {
    extract(html, &base(), None, &context(), &Limits::default()).unwrap()
}

fn markdown_text(markdown: &str) -> String {
    Parser::new(markdown)
        .filter_map(|event| match event {
            Event::Text(text) | Event::Code(text) => Some(text.into_string()),
            Event::SoftBreak | Event::HardBreak => Some(" ".to_owned()),
            _ => None,
        })
        .collect()
}

fn code_blocks(markdown: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut in_code = false;
    for event in Parser::new(markdown) {
        match event {
            Event::Start(Tag::CodeBlock(_)) => {
                blocks.push(String::new());
                in_code = true;
            }
            Event::End(TagEnd::CodeBlock) => in_code = false,
            Event::Text(text) if in_code => blocks.last_mut().unwrap().push_str(&text),
            _ => {}
        }
    }
    blocks
}

#[test]
fn code_preserves_indentation_newlines_and_literal_markup() {
    let page = page(
        "<pre><code>  let value = &lt;tag&gt;;\n    ``` *literal*\n</code></pre>\
         <p>Use <code>&lt;x&gt; *literal* `tick`</code> now.</p>",
    );
    let inline: Vec<_> = Parser::new(&page.text)
        .filter_map(|event| match event {
            Event::Code(text) => Some(text.into_string()),
            _ => None,
        })
        .collect();
    assert_eq!(
        (code_blocks(&page.text), inline),
        (
            vec!["  let value = <tag>;\n    ``` *literal*\n".to_owned()],
            vec!["<x> *literal* `tick`".to_owned()],
        ),
        "{}",
        page.text
    );
    assert!(!page.truncated);
}

#[test]
fn literal_html_is_text_not_executable_markdown_html() {
    let page = page(
        "<p>Literal &lt;script&gt;alert(1)&lt;/script&gt; &amp; **bold**.\
         <script>alert(2)</script> End.</p>",
    );
    for event in Parser::new(&page.text) {
        assert!(
            !matches!(
                event,
                Event::Html(_) | Event::InlineHtml(_) | Event::Start(Tag::Strong | Tag::Emphasis)
            ),
            "unexpected Markdown event: {event:?}"
        );
    }
    assert_eq!(
        markdown_text(&page.text),
        "Literal <script>alert(1)</script> & **bold**. End."
    );
}

#[test]
fn nested_ordered_list_tail_belongs_to_outer_item() {
    let page = page("<ol><li>Outer<ol><li>Inner</li></ol>Tail</li><li>Next</li></ol>");
    let mut path = Vec::new();
    let mut starts = Vec::new();
    let mut labels = Vec::new();
    for event in Parser::new(&page.text) {
        match event {
            Event::Start(Tag::List(start)) => {
                starts.push(start);
                path.push(0);
            }
            Event::End(TagEnd::List(_)) => {
                path.pop().unwrap();
            }
            Event::Start(Tag::Item) => *path.last_mut().unwrap() += 1,
            Event::Text(text) => labels.push((path.clone(), text.into_string())),
            _ => {}
        }
    }
    assert_eq!(starts, vec![Some(1), Some(1)], "{}", page.text);
    assert_eq!(
        labels,
        vec![
            (vec![1], "Outer".to_owned()),
            (vec![1, 1], "Inner".to_owned()),
            (vec![1], "Tail".to_owned()),
            (vec![2], "Next".to_owned()),
        ],
        "{}",
        page.text
    );
}

#[test]
fn links_use_document_base_and_unsafe_hrefs_remain_only_labels() {
    let page = page(
        "<head><base href='https://docs.example/guide/'></head><main><p>\
         <a href='../api?x=1&amp;y=2#method'>API</a> \
         <a href='//reference.example/path'>Reference</a> \
         <a href='javascript:alert(1)'>Script</a> \
         <a href='data:text/html,unsafe'>Data</a> \
         <a href='https://user:pass@example.com/'>Credentials</a></p></main>",
    );
    let links: Vec<_> = Parser::new(&page.text)
        .filter_map(|event| match event {
            Event::Start(Tag::Link { dest_url, .. }) => Some(dest_url.into_string()),
            _ => None,
        })
        .collect();
    assert_eq!(
        links,
        vec![
            "https://docs.example/api?x=1&y=2#method",
            "https://reference.example/path",
        ]
    );
    assert_eq!(
        markdown_text(&page.text),
        "API Reference Script Data Credentials"
    );
}

#[test]
fn table_cells_are_separate_and_highlights_are_contiguous_source_text() {
    let page = extract(
        "<main><table><tr><th>Planet</th><th>Moons</th></tr>\
         <tr><td>Mars</td><td>Two</td></tr></table>\
         <p>Red dust settles slowly.</p></main>",
        &base(),
        Some("Mars"),
        &context(),
        &Limits::default(),
    )
    .unwrap();
    let mut rows = Vec::new();
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut in_table = false;
    let mut in_item = false;
    let mut in_paragraph = false;
    let mut trailing_paragraph = String::new();
    for event in Parser::new_ext(&page.text, Options::ENABLE_TABLES) {
        match event {
            Event::Start(Tag::Table(_)) => in_table = true,
            Event::End(TagEnd::Table) => in_table = false,
            Event::End(TagEnd::TableCell) => cells.push(std::mem::take(&mut cell)),
            Event::End(TagEnd::TableHead | TagEnd::TableRow) => {
                rows.push(std::mem::take(&mut cells));
            }
            Event::Start(Tag::Item) => {
                in_item = true;
                cell.clear();
            }
            Event::End(TagEnd::Item) => {
                rows.push(
                    cell.split('|')
                        .map(str::trim)
                        .filter(|cell| !cell.is_empty())
                        .map(str::to_owned)
                        .collect::<Vec<_>>(),
                );
                in_item = false;
            }
            Event::Start(Tag::Paragraph) => in_paragraph = true,
            Event::End(TagEnd::Paragraph) => in_paragraph = false,
            Event::Text(text) | Event::Code(text) => {
                if in_table || in_item {
                    cell.push_str(&text);
                } else if in_paragraph {
                    trailing_paragraph.push_str(&text);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if in_table || in_item {
                    cell.push(' ');
                } else if in_paragraph {
                    trailing_paragraph.push(' ');
                }
            }
            _ => {}
        }
    }
    assert_eq!(
        page.highlights,
        vec!["Planet Moons Mars Two Red dust settles slowly."]
    );
    assert!(!page.truncated);
    assert_eq!(
        trailing_paragraph, "Red dust settles slowly.",
        "{}",
        page.text
    );
    assert_eq!(
        rows,
        vec![vec!["Planet", "Moons"], vec!["Mars", "Two"]],
        "{}",
        page.text
    );
}

#[test]
fn omitted_head_and_self_closing_foreign_elements_do_not_hide_article_content() {
    for html in [
        "<head><title>Field guide</title><nav>Menu</nav>\
         <main><article><h1>Field guide</h1>\
         <p>Read the gauge before starting.</p></article></main><footer>Legal</footer>",
        "<head><title>Field guide</title><body><svg><script/><style/><path/></svg>\
         <nav>Menu</nav><article><h1>Field guide</h1>\
         <p>Read the gauge before starting.</p></article><footer>Legal</footer>",
    ] {
        let page = page(html);
        assert_eq!(page.title.as_deref(), Some("Field guide"), "{html}");
        assert_eq!(
            markdown_text(&page.text),
            "Field guideRead the gauge before starting.",
            "{html}"
        );
        assert_eq!(
            page.highlights,
            vec!["Field guide Read the gauge before starting."],
            "{html}"
        );
        assert!(!page.truncated, "{html}");
    }
}

#[test]
fn real_access_screens_are_rejected_but_documented_error_names_are_not() {
    for html in [
        "<h1>Access Denied</h1><p>You don't have permission to access this page.</p>",
        "<form action='/anomaly.js'><p>Confirm you are human.</p></form>",
        "<div id='challenge-form'><p>Verify you are human.</p></div>",
    ] {
        let error = extract(html, &base(), None, &context(), &Limits::default())
            .err()
            .expect("an access screen must fail");
        assert_eq!(error.code, ErrorCode::UpstreamBlocked, "{html}");
    }
    let page = page(
        "<head><title>Access Denied</title></head><main><h1>Access Denied</h1>\
         <p>The Access Denied response can indicate a missing role. \
         Inspect anomaly.js requests in the network log.</p>\
         <pre>&lt;form action='/anomaly.js'&gt;</pre>\
         <div hidden><form action='/anomaly.js'></form></div></main>",
    );
    assert!(markdown_text(&page.text).contains(
        "The Access Denied response can indicate a missing role. Inspect anomaly.js requests in the network log."
    ));
}

#[test]
fn login_screens_are_distinct_from_docs_with_embedded_or_hidden_login_forms() {
    for html in [
        "<title>Sign in</title><main><h1>Sign in</h1><form>\
         <label>Email<input type='email'></label><input type='password'></form></main>",
        "<h1>Log in</h1><form><input autocomplete='username' name='email'>\
         <button>Continue</button></form>",
    ] {
        let error = extract(html, &base(), None, &context(), &Limits::default())
            .err()
            .expect("a login screen must fail");
        assert_eq!(error.code, ErrorCode::UnsupportedContent, "{html}");
    }
    for html in [
        "<title>Authentication guide</title><main><article><h1>Authentication guide</h1>\
         <p>Use a passkey to sign in without a password.</p></article>\
         <form aria-label='Sign in'><input type='password'></form></main>",
        "<title>Sign in</title><main><h1>Sign in</h1>\
         <p>Use a passkey to sign in without a password.</p>\
         <div hidden><form aria-label='Sign in'><input type='password'></form></div>\
         <form style='display: none'><input type='password'></form></main>",
    ] {
        let page = page(html);
        assert!(
            markdown_text(&page.text).contains("Use a passkey to sign in without a password."),
            "{html}"
        );
        assert!(!page.truncated, "{html}");
    }
}

#[test]
fn truncation_preserves_complete_code_and_never_clips_markdown_syntax() {
    let ctx = context();
    let limits = Limits::default();
    let doc = parse("<pre>x</pre><p>Afterword.</p>", &ctx, &limits).unwrap();
    let (text, truncated) = render(&doc, 0, &base(), 9, &ctx, &limits).unwrap();
    assert_eq!(code_blocks(&text), vec!["x\n"], "{text}");
    assert!(text.chars().count() <= 9, "{text}");
    assert!(truncated);

    let html = "<p>é &lt;img src=x&gt; **bold** [click](javascript:alert(1)) \\ end</p>";
    let literal = "é <img src=x> **bold** [click](javascript:alert(1)) \\ end";
    let doc = parse(html, &ctx, &limits).unwrap();
    for cap in 1..=64 {
        let (text, truncated) = render(&doc, 0, &base(), cap, &ctx, &limits).unwrap();
        assert!(truncated, "cap={cap}: {text}");
        assert!(text.chars().count() <= cap, "cap={cap}: {text}");
        let retained = markdown_text(&text);
        assert!(literal.starts_with(&retained), "cap={cap}: {text}");
        if cap >= 48 {
            assert!(
                retained.starts_with("é <img src=x> **bold**"),
                "cap={cap}: {text}"
            );
            assert!(retained.chars().count() >= cap / 2, "cap={cap}: {text}");
        }
        for event in Parser::new(&text) {
            assert!(
                !matches!(
                    event,
                    Event::Html(_)
                        | Event::InlineHtml(_)
                        | Event::Code(_)
                        | Event::Start(Tag::Link { .. } | Tag::Emphasis | Tag::Strong)
                ),
                "cap={cap}: {event:?} in {text}"
            );
        }
    }
    let blocks = format!(
        "<p>Read the gauge before starting.</p><p>{}</p>",
        "Further measurements require a steady hand. ".repeat(8)
    );
    let doc = parse(&blocks, &ctx, &limits).unwrap();
    for cap in [48, 80, 128] {
        let (text, truncated) = render(&doc, 0, &base(), cap, &ctx, &limits).unwrap();
        assert!(truncated, "cap={cap}: {text}");
        assert!(text.chars().count() <= cap, "cap={cap}: {text}");
        assert!(
            markdown_text(&text).starts_with("Read the gauge before starting."),
            "cap={cap}: {text}"
        );
    }
    let page = extract(
        html,
        &base(),
        None,
        &ctx,
        &Limits {
            max_text_chars: 48,
            ..limits
        },
    )
    .unwrap();
    let total = page.text.chars().count()
        + page.title.as_ref().map_or(0, |title| title.chars().count())
        + page
            .highlights
            .iter()
            .map(|s| s.chars().count())
            .sum::<usize>();
    assert!(total <= 48, "{total} characters");
    assert!(page.truncated);
    let retained = markdown_text(&page.text);
    assert!(literal.starts_with(&retained), "{}", page.text);
    assert!(retained.starts_with("é <img src=x>"), "{}", page.text);
}

#[test]
fn parser_bounds_depth_tokens_and_even_discarded_long_attributes() {
    let ctx = context();
    let depth = Limits {
        max_html_depth: 2,
        ..Limits::default()
    };
    assert!(parse("<div><p>Visible</p></div>", &ctx, &depth).is_ok());
    assert_eq!(
        parse("<div><section><p>Visible</p></section></div>", &ctx, &depth)
            .err()
            .expect("excess nesting must fail")
            .code,
        ErrorCode::TooLarge
    );
    let tokens = Limits {
        max_html_tokens: 3,
        ..Limits::default()
    };
    assert!(parse("<p>one</p>", &ctx, &tokens).is_ok());
    assert_eq!(
        parse("<p>one</p><p>two</p>", &ctx, &tokens)
            .err()
            .expect("excess tokens must fail")
            .code,
        ErrorCode::TooLarge
    );
    let limits = Limits::default();
    assert!(
        parse(
            &format!("<p title='{}'>Visible</p>", "x".repeat(16_384)),
            &ctx,
            &limits
        )
        .is_ok()
    );
    let html = format!("<p title='{}'>Visible</p>", "x".repeat(16_385));
    assert_eq!(
        parse(&html, &ctx, &limits)
            .err()
            .expect("excess attribute bytes must fail")
            .code,
        ErrorCode::TooLarge
    );
}

#[test]
fn parser_bounds_raw_token_bytes_even_for_discarded_comments() {
    let ctx = context();
    let limits = Limits::default();
    let accepted = format!("<!--{}--><p>Visible</p>", "x".repeat(65_520 - 7));
    let doc = parse(&accepted, &ctx, &limits).unwrap();
    assert!(doc.nodes.iter().any(|node| node.text == "Visible"));
    let oversized = format!("<!--{}--><p>Visible</p>", "x".repeat(65_537 - 7));
    assert_eq!(
        parse(&oversized, &ctx, &limits)
            .err()
            .expect("excess raw comment token bytes must fail")
            .code,
        ErrorCode::TooLarge
    );
}

#[test]
fn cooperative_reader_observes_cancellation_after_reading_begins() {
    let ctx = context();
    let input = vec![b'x'; 8192];
    let consumed = Cell::new(0);
    let count = Cell::new(0);
    let mut reader = CheckedReader {
        input: &input,
        ctx: &ctx,
        consumed: &consumed,
        count: &count,
        max_tokens: Limits::default().max_html_tokens,
        unchecked: 0,
    };
    assert_eq!(reader.read_byte().unwrap(), Some(b'x'));
    assert_eq!(consumed.get(), 1);
    ctx.cancellation.cancel();
    let error = (0..4096)
        .find_map(|_| reader.read_byte().err())
        .expect("the reader must check cancellation within the next 4 KiB");
    assert_eq!(error.code, ErrorCode::Cancelled);
    assert!(!reader.input.is_empty());
}

#[test]
fn plain_extraction_keeps_text_verbatim_without_html_parsing() {
    let page = extract_plain(
        "a < b && c > d\n```rust\nlet x = 1;\n```\n",
        None,
        &context(),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(page.title, None);
    assert_eq!(page.text, "a < b && c > d\n```rust\nlet x = 1;\n```\n");
    assert_eq!(
        page.highlights,
        vec!["a < b && c > d\n```rust\nlet x = 1;\n```".to_owned()]
    );
    assert!(!page.truncated);
}

#[test]
fn plain_extraction_preserves_leading_indentation() {
    let page = extract_plain(
        "    indented code\nsecond\n",
        None,
        &context(),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(page.text, "    indented code\nsecond\n");
    assert!(!page.truncated);
}

#[test]
fn plain_extraction_truncates_at_the_text_limit() {
    let limits = Limits {
        max_text_chars: 20,
        ..Limits::default()
    };
    let source = "0123456789012345678901234567890";
    let page = extract_plain(source, None, &context(), &limits).unwrap();
    assert_eq!(page.highlights, vec!["01234".to_owned()]);
    assert_eq!(page.text, "012345678901234");
    assert!(page.truncated);
}

#[test]
fn plain_extraction_rejects_blank_text() {
    let error = extract_plain("   \n\t", None, &context(), &Limits::default())
        .err()
        .unwrap();
    assert_eq!(error.code, ErrorCode::ExtractionFailed);
}
