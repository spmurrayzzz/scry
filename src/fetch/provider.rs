use std::net::SocketAddr;

use reqwest::Response;
use url::Url;

use super::{Fetcher, network_error};
use crate::api::Error;
use crate::context::RequestContext;

pub(super) fn matches(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some("html.duckduckgo.com")
        && url.port_or_known_default() == Some(443)
}

pub(super) async fn send(
    fetcher: &Fetcher,
    url: &Url,
    addresses: Vec<SocketAddr>,
    form: Option<&[(&str, &str)]>,
    ctx: &RequestContext,
) -> Result<Response, Error> {
    let client = fetcher.client(url, addresses, ctx)?;
    let request = if let Some(form) = form {
        client.post(url.clone()).form(form)
    } else {
        client.get(url.clone())
    };
    request
        .header("User-Agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36")
        .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
        .header("Accept-Encoding", "identity")
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Sec-Fetch-Dest", "document")
        .header("Sec-Fetch-Mode", "navigate")
        .header("Sec-Fetch-Site", "none")
        .header("Sec-Fetch-User", "?1")
        .send()
        .await
        .map_err(network_error)
}
