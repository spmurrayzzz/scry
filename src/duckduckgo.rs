use std::collections::HashSet;

use url::Url;

use crate::api::{Error, ErrorCode};
use crate::config::Limits;
use crate::context::RequestContext;
use crate::extract::{bounded_plain_text, challenge_screen, parse, plain_text, safe_url};

pub(crate) struct DiscoveredResult {
    pub url: Url,
    pub title: String,
    pub snippet: String,
    pub truncated: bool,
}

fn destination(base: &Url, href: &str, limits: &Limits) -> Option<Url> {
    let mut url = safe_url(base, href, limits)?;
    let trusted = matches!(
        url.host_str(),
        Some(
            "duckduckgo.com" | "www.duckduckgo.com" | "html.duckduckgo.com" | "lite.duckduckgo.com"
        )
    ) && matches!(url.port(), None | Some(80) | Some(443));
    if trusted && matches!(url.path(), "/l" | "/l/") {
        let mut targets = url.query_pairs().filter(|(key, _)| key == "uddg");
        let (_, target) = targets.next()?;
        if targets.next().is_some() || target.len() > limits.max_url_bytes {
            return None;
        }
        let absolute = Url::parse(target.trim()).ok()?;
        if !matches!(absolute.scheme(), "http" | "https") {
            return None;
        }
        let target = safe_url(&url, &target, limits)?;
        url = target;
    }
    url.set_fragment(None);
    Some(url)
}

pub(crate) fn search_results(
    html: &str,
    base: &Url,
    ctx: &RequestContext,
    limits: &Limits,
) -> Result<Vec<DiscoveredResult>, Error> {
    ctx.check()?;
    if html.len() > limits.max_decoded_bytes {
        return Err(Error::new(
            ErrorCode::TooLarge,
            "Decoded search HTML exceeds the byte limit",
        ));
    }
    let doc = parse(html, ctx, limits)?;
    if challenge_screen(&doc, base, ctx, limits)? {
        return Err(Error::new(
            ErrorCode::UpstreamBlocked,
            "DuckDuckGo returned an access challenge",
        ));
    }
    let mut results = Vec::new();
    let mut seen = HashSet::new();
    let mut recognized_empty = false;
    let mut result_items = 0usize;
    let mut result_links = 0usize;
    let mut remaining = limits.max_text_chars;
    let mut index = 1;
    while index < doc.nodes.len() {
        ctx.check()?;
        let node = &doc.nodes[index];
        if node.class("result--ad") || matches!(node.tag.as_str(), "script" | "style" | "template")
        {
            index = node.end;
            continue;
        }
        if node.class("no-results")
            || node.class("no-results__message")
            || node.attr("id") == "no-results"
        {
            let message = plain_text(&doc, index, 2000, false, ctx)?.to_lowercase();
            recognized_empty |=
                message.contains("no results") || message.contains("no more results");
            index = node.end;
            continue;
        }
        index += 1;
    }
    index = 1;
    while index < doc.nodes.len() {
        ctx.check()?;
        let node = &doc.nodes[index];
        if node.class("result--ad") || matches!(node.tag.as_str(), "script" | "style" | "template")
        {
            index = node.end;
            continue;
        }
        if !node.class("result") {
            index += 1;
            continue;
        }
        result_items += 1;
        let mut link = None;
        let mut snippet = None;
        let mut child = index + 1;
        while child < node.end {
            ctx.check()?;
            let descendant = &doc.nodes[child];
            if descendant.class("result--ad")
                || matches!(descendant.tag.as_str(), "script" | "style" | "template")
            {
                child = descendant.end;
                continue;
            }
            if link.is_none() && descendant.tag == "a" && descendant.class("result__a") {
                link = Some(child);
            }
            if snippet.is_none() && descendant.class("result__snippet") {
                snippet = Some(child);
            }
            child += 1;
        }
        if let Some(link) = link {
            result_links += 1;
            let link_node = &doc.nodes[link];
            if !link_node.attr("href").is_empty()
                && let Some(url) = destination(base, link_node.attr("href"), limits)
            {
                let (title, title_truncated) =
                    bounded_plain_text(&doc, link, remaining.min(1024), false, ctx)?;
                let available = remaining.saturating_sub(title.chars().count());
                let (snippet, snippet_truncated) = snippet
                    .map(|i| bounded_plain_text(&doc, i, available.min(4096), false, ctx))
                    .transpose()?
                    .unwrap_or_default();
                if !title.is_empty() && seen.insert(url.as_str().to_owned()) {
                    remaining = available.saturating_sub(snippet.chars().count());
                    results.push(DiscoveredResult {
                        url,
                        title,
                        snippet,
                        truncated: title_truncated || snippet_truncated,
                    });
                    if results.len() == 20 || remaining == 0 {
                        break;
                    }
                }
            }
        }
        index = node.end;
    }
    ctx.check()?;
    if results.is_empty() && !(recognized_empty && result_links == 0) {
        let message = if result_items > 0 {
            "DuckDuckGo returned result containers without usable titles and URLs"
        } else {
            "DuckDuckGo returned an unrecognized results layout"
        };
        return Err(Error::new(ErrorCode::UpstreamFailed, message));
    }
    Ok(results)
}
