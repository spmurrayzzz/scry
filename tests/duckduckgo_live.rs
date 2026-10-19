use std::error::Error;
use std::io::Read;
use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use reqwest::{StatusCode, Url, redirect};
use scraper::{ElementRef, Html, Selector};

fn text(element: ElementRef<'_>) -> String {
    element
        .text()
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
#[ignore = "makes one live request to DuckDuckGo"]
fn duckduckgo_returns_search_results() -> Result<(), Box<dyn Error>> {
    let endpoint = Url::parse("https://html.duckduckgo.com/html/")?;
    let client = Client::builder()
        .user_agent(concat!(
            "scry/",
            env!("CARGO_PKG_VERSION"),
            " (search test)"
        ))
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .redirect(redirect::Policy::none())
        .no_proxy()
        .build()?;

    let started = Instant::now();
    let response = client
        .post(endpoint.clone())
        .header("Accept", "text/html")
        .header("Accept-Encoding", "identity")
        .form(&[
            ("q", "reqwest ClientBuilder documentation"),
            ("kl", "us-en"),
            ("b", ""),
        ])
        .send()?;
    let status = response.status();
    let max_bytes = 2 * 1024 * 1024;
    let mut body = Vec::new();
    response.take(max_bytes + 1).read_to_end(&mut body)?;
    assert!(body.len() as u64 <= max_bytes, "response exceeds 2 MiB");
    println!(
        "DuckDuckGo: HTTP {status}, {} bytes, {:?}",
        body.len(),
        started.elapsed()
    );

    let html = String::from_utf8(body)?;
    let document = Html::parse_document(&html);
    let challenge = Selector::parse("#challenge-form, .anomaly-modal, form[action*='anomaly.js']")?;
    assert!(
        document.select(&challenge).next().is_none(),
        "DuckDuckGo returned a bot challenge (HTTP {status})"
    );
    assert_eq!(status, StatusCode::OK, "DuckDuckGo did not return HTTP 200");

    let items = Selector::parse(".result:not(.result--ad)")?;
    let links = Selector::parse("a.result__a")?;
    let snippets = Selector::parse(".result__snippet")?;
    let mut results = Vec::new();

    for item in document.select(&items) {
        let Some(link) = item.select(&links).next() else {
            continue;
        };
        let title = text(link);
        let href = link.value().attr("href").ok_or("result has no URL")?;
        let mut url = endpoint.join(href)?;
        if matches!(
            url.host_str(),
            Some("duckduckgo.com" | "html.duckduckgo.com")
        ) {
            let target = url
                .query_pairs()
                .find(|(key, _)| key == "uddg")
                .map(|(_, value)| value.into_owned())
                .ok_or("DuckDuckGo result link has no destination")?;
            url = Url::parse(&target)?;
        }
        let snippet = item.select(&snippets).next().map(text).unwrap_or_default();
        assert!(!title.is_empty(), "result has no title");
        assert!(!snippet.is_empty(), "result has no snippet: {url}");
        assert!(matches!(url.scheme(), "http" | "https"));
        assert!(url.host_str().is_some());
        assert!(url.username().is_empty() && url.password().is_none());
        results.push((title, url, snippet));
        if results.len() == 5 {
            break;
        }
    }

    for (index, (title, url, snippet)) in results.iter().enumerate() {
        println!("\n{}. {title}\n{url}\n{snippet}", index + 1);
    }
    assert!(!results.is_empty(), "no parseable search results returned");
    assert!(
        results.iter().any(
            |(_, url, _)| url.host_str() == Some("docs.rs") && url.path().contains("/reqwest/")
        ),
        "no docs.rs/reqwest documentation found in the first five results"
    );
    Ok(())
}
