use std::net::SocketAddr;
use std::time::Duration;

use reqwest::Response;
use url::Url;

use super::{Fetcher, PageKind, network_error, response_kind_for};
use crate::api::{Error, ErrorCode};
use crate::config::Limits;
use crate::context::RequestContext;
use crate::extract::{challenge_screen, parse};

const DIAGNOSTIC_BYTES: usize = 32 * 1024;
const DIAGNOSTIC_BUDGET: Duration = Duration::from_millis(250);

pub(super) async fn read_continuation(
    fetcher: &Fetcher,
    response: Response,
    error: &Error,
    ctx: &RequestContext,
) -> Result<String, Error> {
    if ctx.cancellation.is_cancelled() {
        return Err(Error::cancelled());
    }
    let now = tokio::time::Instant::now();
    if ctx.deadline.saturating_duration_since(now) <= DIAGNOSTIC_BUDGET * 2 {
        return Err(error.clone());
    }
    let mut body_ctx = ctx.child();
    body_ctx.deadline = now + DIAGNOSTIC_BUDGET;
    let limits = Limits {
        max_download_bytes: fetcher.limits.max_download_bytes.min(65536),
        max_decoded_bytes: fetcher.limits.max_decoded_bytes.min(65536),
        ..fetcher.limits.clone()
    };
    let reading = async {
        let (kind, encoding) = response_kind_for(&response, Some(true))?;
        if response
            .content_length()
            .is_some_and(|size| size > limits.max_download_bytes as u64)
        {
            return Err(super::download_too_large());
        }
        Fetcher::read_text_bounded(response, kind, encoding, &limits, &body_ctx).await
    };
    let result = tokio::select! {
        biased;
        _ = ctx.cancellation.cancelled() => Err(Error::cancelled()),
        _ = tokio::time::sleep_until(body_ctx.deadline) => Err(error.clone()),
        result = reading => result.map_err(|_| error.clone()),
    };
    if ctx.cancellation.is_cancelled() {
        Err(Error::cancelled())
    } else {
        result
    }
}

pub(super) async fn check_response(
    fetcher: &Fetcher,
    response: Response,
    url: &Url,
    provider: Option<bool>,
    ctx: &RequestContext,
) -> Result<Response, Error> {
    let mut error = match super::check_status(&response) {
        Ok(()) => return Ok(response),
        Err(error) => error,
    };
    if !provider.is_some_and(|script| crate::duckduckgo::is_web_url(url, script)) {
        return Err(error);
    }
    if ctx.cancellation.is_cancelled() {
        return Err(Error::cancelled());
    }
    let now = tokio::time::Instant::now();
    if ctx.deadline.saturating_duration_since(now) <= DIAGNOSTIC_BUDGET * 2 {
        return Err(error);
    }
    let mut diagnostic_ctx = ctx.child();
    diagnostic_ctx.deadline = now + DIAGNOSTIC_BUDGET;
    let classification = tokio::select! {
        biased;
        _ = ctx.cancellation.cancelled() => return Err(Error::cancelled()),
        _ = tokio::time::sleep_until(diagnostic_ctx.deadline) => None,
        result = classify(response, url, &fetcher.limits, &diagnostic_ctx) => result.ok(),
    };
    if ctx.cancellation.is_cancelled() {
        return Err(Error::cancelled());
    }
    if let Some(classification) = classification {
        error.message.push_str(classification);
    }
    Err(error)
}

async fn classify(
    response: Response,
    url: &Url,
    limits: &Limits,
    ctx: &RequestContext,
) -> Result<&'static str, Error> {
    let (kind, encoding) = response_kind_for(&response, None)?;
    if kind != PageKind::Html {
        return Err(Error::new(
            ErrorCode::UnsupportedContent,
            "Only HTML and XHTML pages are supported",
        ));
    }
    let limits = Limits {
        max_download_bytes: limits.max_download_bytes.min(DIAGNOSTIC_BYTES),
        max_decoded_bytes: limits.max_decoded_bytes.min(DIAGNOSTIC_BYTES),
        max_html_tokens: limits.max_html_tokens.min(4096),
        max_html_depth: limits.max_html_depth.min(32),
        ..limits.clone()
    };
    if response
        .content_length()
        .is_some_and(|size| size > limits.max_download_bytes as u64)
    {
        return Err(super::download_too_large());
    }
    let html = Fetcher::read_text_bounded(response, kind, encoding, &limits, ctx).await?;
    let doc = parse(&html, ctx, &limits)?;
    if !challenge_screen(&doc, url, ctx, &limits)? {
        return Ok("; DuckDuckGo response: unclassified");
    }
    for (index, form) in doc.nodes.iter().enumerate() {
        ctx.check()?;
        if form.tag != "form" || form.attr("id") != "challenge-form" {
            continue;
        }
        let mut puzzle = false;
        let mut checkbox = false;
        let mut image = false;
        for node in &doc.nodes[index + 1..form.end] {
            ctx.check()?;
            puzzle |= node.class("anomaly-modal__puzzle");
            checkbox |= node.tag == "input"
                && node.attr("type").eq_ignore_ascii_case("checkbox")
                && node.class("anomaly-modal__check")
                && node.attr("name").starts_with("image-check_");
            image |= node.tag == "img" && node.class("anomaly-modal__image");
        }
        if puzzle && checkbox && image {
            return Ok("; DuckDuckGo response: image CAPTCHA");
        }
    }
    Ok("; DuckDuckGo response: access challenge")
}

pub(super) async fn send(
    fetcher: &Fetcher,
    url: &Url,
    addresses: Vec<SocketAddr>,
    script: bool,
    ctx: &RequestContext,
) -> Result<Response, Error> {
    let client = fetcher.client(url, addresses, ctx)?;
    let request = client.get(url.clone())
        .header("User-Agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36")
        .header("Accept-Encoding", "identity")
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Connection", "close");
    let request = if script {
        request
            .header("Accept", "*/*")
            .header("Sec-Fetch-Dest", "script")
            .header("Sec-Fetch-Mode", "no-cors")
            .header("Sec-Fetch-Site", "same-site")
            .header("Referer", "https://duckduckgo.com/")
    } else {
        request
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header("Sec-Fetch-Dest", "document")
            .header("Sec-Fetch-Mode", "navigate")
            .header("Sec-Fetch-Site", "none")
            .header("Sec-Fetch-User", "?1")
    };
    request.send().await.map_err(network_error)
}
