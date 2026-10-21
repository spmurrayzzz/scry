use std::collections::HashSet;

use serde_json::Value;
use url::Url;

use crate::api::{Error, ErrorCode};
use crate::config::Limits;
use crate::context::RequestContext;
use crate::extract::{bounded_plain_text, challenge_screen, parse, safe_url};
use crate::policy;

mod challenge;
#[cfg(test)]
mod tests;

pub(crate) struct DiscoveredResult {
    pub url: Url,
    pub title: String,
    pub snippet: String,
    pub truncated: bool,
}

pub(crate) enum WebResponse {
    Results(Vec<DiscoveredResult>),
    Challenge(Url),
}

fn blocked() -> Error {
    Error::new(
        ErrorCode::UpstreamBlocked,
        "DuckDuckGo returned an unsupported or unsafe access challenge",
    )
}

fn invalid_results() -> Error {
    Error::new(
        ErrorCode::UpstreamFailed,
        "DuckDuckGo returned an unrecognized or unusable results response",
    )
}

pub(crate) fn web_url(query: &str) -> Result<Url, Error> {
    let mut url = Url::parse("https://duckduckgo.com/").map_err(|_| invalid_results())?;
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("t", "h_")
        .append_pair("ia", "web");
    Ok(url)
}

pub(crate) fn is_web_url(url: &Url, script: bool) -> bool {
    url.scheme() == "https"
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && matches!(
            url.host_str(),
            Some("duckduckgo.com" | "links.duckduckgo.com")
        )
        && url.path() == if script { "/d.js" } else { "/" }
}

fn provider_target(target: &str, base: &Url, limits: &Limits) -> Result<Url, Error> {
    if target.is_empty()
        || target.len() > limits.max_url_bytes
        || target.contains(['\\', '#'])
        || target.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(blocked());
    }
    let absolute = if target.starts_with("//") {
        format!("https:{target}")
    } else if Url::parse(target).is_ok() {
        target.to_owned()
    } else {
        base.join(target).map_err(|_| blocked())?.to_string()
    };
    let url = policy::parse_url(&absolute, limits).map_err(|_| blocked())?;
    if !is_web_url(&url, true) {
        return Err(blocked());
    }
    Ok(url)
}

pub(crate) fn preload_url(
    html: &str,
    base: &Url,
    ctx: &RequestContext,
    limits: &Limits,
) -> Result<Url, Error> {
    ctx.check()?;
    if !is_web_url(base, false) {
        return Err(blocked());
    }
    let doc = parse(html, ctx, limits)?;
    if challenge_screen(&doc, base, ctx, limits)? {
        return Err(blocked());
    }
    let mut href = None;
    let mut index = 1;
    while index < doc.nodes.len() {
        ctx.check()?;
        let node = &doc.nodes[index];
        if matches!(
            node.tag.as_str(),
            "script" | "style" | "template" | "noscript" | "svg" | "math"
        ) {
            index = node.end;
            continue;
        }
        if node.tag == "link"
            && node.attr("id") == "deep_preload_link"
            && href.replace(node.attr("href")).is_some()
        {
            return Err(invalid_results());
        }
        index += 1;
    }
    let mut url = provider_target(href.ok_or_else(invalid_results)?, base, limits)?;
    if url.query_pairs().any(|(name, _)| name == "o") {
        return Err(invalid_results());
    }
    let query = match url.query() {
        Some(query) if !query.is_empty() => format!("o=json&{query}"),
        _ => return Err(invalid_results()),
    };
    url.set_query(Some(&query));
    let url = provider_target(url.as_str(), base, limits)?;
    ctx.check()?;
    Ok(url)
}

fn destination(base: &Url, href: &str, limits: &Limits) -> Option<Url> {
    if href.is_empty()
        || href.chars().any(|c| c.is_control() || c.is_whitespace())
        || href.contains('\\')
    {
        return None;
    }
    let relative = !href.starts_with("//") && Url::parse(href).is_err();
    let mut url = if href.starts_with("//") {
        policy::parse_url(&format!("{}:{href}", base.scheme()), limits).ok()?
    } else if Url::parse(href).is_ok() {
        policy::parse_url(href, limits).ok()?
    } else {
        safe_url(base, href, limits)?
    };
    let trusted = matches!(
        url.host_str(),
        Some(
            "duckduckgo.com" | "www.duckduckgo.com" | "html.duckduckgo.com" | "lite.duckduckgo.com"
        )
    ) && matches!(url.port(), None | Some(80) | Some(443));
    let wrapper = trusted && matches!(url.path(), "/l" | "/l/");
    if relative && !wrapper {
        return None;
    }
    if wrapper {
        let mut targets = url.query_pairs().filter(|(key, _)| key == "uddg");
        let (_, target) = targets.next()?;
        if targets.next().is_some() || target.len() > limits.max_url_bytes {
            return None;
        }
        let absolute = Url::parse(&target).ok()?;
        if !matches!(absolute.scheme(), "http" | "https") {
            return None;
        }
        url = policy::parse_url(&target, limits).ok()?;
    }
    policy::parse_url(url.as_str(), limits).ok()
}

fn json_bounds(body: &str, ctx: &RequestContext, limits: &Limits) -> Result<(), Error> {
    let mut quoted = false;
    let mut escaped = false;
    let mut depth = 0usize;
    let mut tokens = 0usize;
    for (index, byte) in body.bytes().enumerate() {
        if index % 4096 == 0 {
            ctx.check()?;
        }
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
            continue;
        }
        match byte {
            b'"' => {
                quoted = true;
                tokens += 1;
            }
            b'{' | b'[' => {
                depth += 1;
                tokens += 1;
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            b',' | b':' => tokens += 1,
            _ => {}
        }
        if depth > limits.max_html_depth || tokens > limits.max_html_tokens {
            return Err(Error::new(
                ErrorCode::TooLarge,
                "Search JSON exceeds the parsing limits",
            ));
        }
    }
    ctx.check()
}

fn text(
    html: &str,
    cap: usize,
    ctx: &RequestContext,
    limits: &Limits,
    tokens: &mut usize,
) -> Result<(String, bool), Error> {
    let mut bounded = limits.clone();
    bounded.max_html_tokens = bounded.max_html_tokens.saturating_sub(*tokens);
    let doc = parse(html, ctx, &bounded)?;
    *tokens = tokens.saturating_add(doc.nodes.len().saturating_sub(1));
    bounded_plain_text(&doc, 0, cap, false, ctx)
}

pub(crate) fn web_results(
    body: &str,
    base: &Url,
    ctx: &RequestContext,
    limits: &Limits,
) -> Result<WebResponse, Error> {
    ctx.check()?;
    if !is_web_url(base, true) {
        return Err(blocked());
    }
    if body.len() > limits.max_decoded_bytes {
        return Err(Error::new(
            ErrorCode::TooLarge,
            "Decoded search response exceeds the byte limit",
        ));
    }
    if !body.trim_start().starts_with('{') {
        return challenge::solve(body, base, ctx, limits).map(WebResponse::Challenge);
    }
    json_bounds(body, ctx, limits)?;
    let data: Value = serde_json::from_str(body).map_err(|_| invalid_results())?;
    ctx.check()?;
    let rows = data
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(invalid_results)?;
    let mut results: Vec<DiscoveredResult> = Vec::new();
    let mut seen = HashSet::new();
    let mut remaining = limits.max_text_chars;
    let mut tokens = 0;
    let mut organic = 0;
    for row in rows {
        ctx.check()?;
        let row = row.as_object().ok_or_else(invalid_results)?;
        if ["ad", "is_ad", "isAd"]
            .iter()
            .any(|key| row.get(*key).and_then(Value::as_bool) == Some(true))
        {
            continue;
        }
        if !row.contains_key("u")
            && row
                .get("n")
                .and_then(Value::as_str)
                .is_some_and(|n| provider_target(n, base, limits).is_ok())
        {
            continue;
        }
        let href = row
            .get("u")
            .and_then(Value::as_str)
            .ok_or_else(invalid_results)?;
        let title_html = row
            .get("t")
            .and_then(Value::as_str)
            .ok_or_else(invalid_results)?;
        let snippet_html = match row.get("a") {
            None => "",
            Some(value) => value.as_str().ok_or_else(invalid_results)?,
        };
        organic += 1;
        let Some(url) = destination(base, href, limits) else {
            continue;
        };
        if seen.contains(url.as_str()) {
            continue;
        }
        if results.len() == 20 || remaining == 0 {
            if let Some(last) = results.last_mut() {
                last.truncated = true;
            }
            continue;
        }
        let (title, title_truncated) =
            text(title_html, remaining.min(1024), ctx, limits, &mut tokens)?;
        if title.is_empty() {
            continue;
        }
        remaining = remaining.saturating_sub(title.chars().count());
        let (snippet, snippet_truncated) =
            text(snippet_html, remaining.min(4096), ctx, limits, &mut tokens)?;
        remaining = remaining.saturating_sub(snippet.chars().count());
        seen.insert(url.as_str().to_owned());
        results.push(DiscoveredResult {
            url,
            title,
            snippet,
            truncated: title_truncated || snippet_truncated,
        });
    }
    if results.is_empty() && organic > 0 {
        return Err(invalid_results());
    }
    ctx.check()?;
    Ok(WebResponse::Results(results))
}
