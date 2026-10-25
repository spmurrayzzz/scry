use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use encoding_rs::{CoderResult, Decoder, Encoding, UTF_8};
use html5gum::{DefaultEmitter, Token, Tokenizer};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{
    ACCEPT, ACCEPT_ENCODING, CONNECTION, CONTENT_ENCODING, CONTENT_TYPE, LOCATION, RETRY_AFTER,
};
use reqwest::{Client, Response, StatusCode};
use tokio::sync::Semaphore;
use tokio::time::Instant;
use url::{Host, Url};

use crate::api::{Error, ErrorCode};
use crate::config::Limits;
use crate::context::RequestContext;
use crate::policy::{DomainFilter, parse_url, validate_ip};

mod dns;
mod provider;

#[cfg(test)]
mod tests;

const MAX_DNS_ANSWERS: usize = 64;
const MAX_CHARSET_SNIFF_BYTES: usize = 1024;

#[derive(Clone)]
pub(crate) struct Fetcher {
    limits: Limits,
    dns: dns::Dns,
    semaphore: Arc<Semaphore>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PageKind {
    Html,
    Plain,
    Pdf,
}

#[derive(Debug)]
pub(crate) enum PageBody {
    Html(String),
    Plain(String),
    Pdf(Vec<u8>),
}

#[derive(Debug)]
pub(crate) struct FetchedPage {
    pub final_url: Url,
    pub body: PageBody,
    pub status_error: Option<Error>,
}

impl FetchedPage {
    #[cfg(test)]
    pub(crate) fn as_text(&self) -> Option<&str> {
        match &self.body {
            PageBody::Html(text) | PageBody::Plain(text) => Some(text),
            PageBody::Pdf(_) => None,
        }
    }

    pub(crate) fn into_parts(self) -> Result<(String, Url, Option<Error>), Error> {
        let text = match self.body {
            PageBody::Html(text) | PageBody::Plain(text) => text,
            PageBody::Pdf(_) => {
                return Err(Error::new(
                    ErrorCode::Internal,
                    "Provider responses cannot be PDF pages",
                ));
            }
        };
        Ok((text, self.final_url, self.status_error))
    }
}

impl Fetcher {
    pub(crate) fn new(limits: Limits) -> Result<Self, Error> {
        limits.validate()?;
        Ok(Self {
            dns: dns::Dns::new(&limits)?,
            semaphore: Arc::new(Semaphore::new(limits.max_fetches)),
            limits,
        })
    }

    pub(crate) async fn get(
        &self,
        url: Url,
        filter: &DomainFilter,
        ctx: &RequestContext,
    ) -> Result<FetchedPage, Error> {
        self.fetch(url, filter, None, ctx).await
    }

    pub(crate) async fn get_provider(
        &self,
        url: Url,
        script: bool,
        ctx: &RequestContext,
    ) -> Result<FetchedPage, Error> {
        self.fetch(url, &DomainFilter::default(), Some(script), ctx)
            .await
    }

    async fn fetch(
        &self,
        url: Url,
        filter: &DomainFilter,
        provider: Option<bool>,
        parent: &RequestContext,
    ) -> Result<FetchedPage, Error> {
        self.fetch_with(
            url,
            filter,
            provider,
            parent,
            |url: Url, addresses, ctx: RequestContext| async move {
                if let Some(script) = provider {
                    provider::send(self, &url, addresses, script, &ctx).await
                } else {
                    let client = self.client(&url, addresses, &ctx)?;
                    client
                        .get(url.clone())
                        .header(
                            ACCEPT,
                            "text/html, application/xhtml+xml, text/plain, text/markdown, text/x-markdown, application/json, application/pdf",
                        )
                        .header(ACCEPT_ENCODING, "identity")
                        .header(CONNECTION, "close")
                        .send()
                        .await
                        .map_err(network_error)
                }
            },
        )
        .await
    }

    async fn fetch_with<F: Future<Output = Result<Response, Error>>>(
        &self,
        url: Url,
        filter: &DomainFilter,
        provider: Option<bool>,
        parent: &RequestContext,
        send: impl FnMut(Url, Vec<SocketAddr>, RequestContext) -> F,
    ) -> Result<FetchedPage, Error> {
        let mut ctx = parent.child();
        ctx.deadline = ctx
            .deadline
            .min(Instant::now() + Duration::from_secs(self.limits.fetch_timeout_secs));
        ctx.check()?;
        let url = self.checked_url(url, filter, provider)?;
        let _permit = self.semaphore.clone().try_acquire_owned().map_err(|_| {
            Error::new(
                ErrorCode::Overloaded,
                "All outbound fetch slots are busy; try again later",
            )
        })?;
        tokio::select! {
            biased;
            _ = ctx.cancellation.cancelled() => Err(Error::cancelled()),
            result = self.follow_with(url, filter, provider, &ctx, send) => result,
            _ = tokio::time::sleep_until(ctx.deadline) => Err(Error::timeout()),
        }
    }

    async fn follow_with<F: Future<Output = Result<Response, Error>>>(
        &self,
        mut url: Url,
        filter: &DomainFilter,
        provider: Option<bool>,
        ctx: &RequestContext,
        mut send: impl FnMut(Url, Vec<SocketAddr>, RequestContext) -> F,
    ) -> Result<FetchedPage, Error> {
        for redirects in 0..=self.limits.max_redirects {
            ctx.check()?;
            let addresses = self
                .resolve(&url, ctx)
                .await
                .map_err(|error| ctx.check().err().unwrap_or(error))?;
            ctx.check()?;
            let response = send(url.clone(), addresses, ctx.clone())
                .await
                .map_err(|error| ctx.check().err().unwrap_or(error))?;
            if provider.is_none() || response.status().is_redirection() {
                ctx.check()?;
            }
            if let Some(target) =
                self.redirect_target(&response, &url, filter, provider, redirects)?
            {
                ctx.check()?;
                drop(response);
                url = target;
                continue;
            }
            return self.read_response(response, url, provider, ctx).await;
        }
        Err(Error::new(ErrorCode::Internal, "Redirect state is invalid"))
    }

    pub(crate) async fn read_response(
        &self,
        response: Response,
        url: Url,
        provider: Option<bool>,
        ctx: &RequestContext,
    ) -> Result<FetchedPage, Error> {
        let status_error = if provider == Some(true)
            && crate::duckduckgo::is_web_url(&url, true)
            && response.status() == StatusCode::ACCEPTED
        {
            check_status(&response).err()
        } else {
            None
        };
        let body = if let Some(error) = &status_error {
            PageBody::Plain(provider::read_continuation(self, response, error, ctx).await?)
        } else {
            let response = provider::check_response(self, response, &url, provider, ctx).await?;
            ctx.check()?;
            let (kind, encoding) = response_kind_for(&response, provider)?;
            if response
                .content_length()
                .is_some_and(|size| size > self.limits.max_download_bytes as u64)
            {
                return Err(download_too_large());
            }
            match kind {
                PageKind::Html | PageKind::Plain => {
                    let text = self.read_text(response, kind, encoding, ctx).await?;
                    ctx.check()?;
                    if kind == PageKind::Html {
                        PageBody::Html(text)
                    } else {
                        PageBody::Plain(text)
                    }
                }
                PageKind::Pdf => {
                    let bytes = self.read_pdf(response, ctx).await?;
                    ctx.check()?;
                    PageBody::Pdf(bytes)
                }
            }
        };
        Ok(FetchedPage {
            final_url: url,
            body,
            status_error,
        })
    }

    fn checked_url(
        &self,
        url: Url,
        filter: &DomainFilter,
        provider: Option<bool>,
    ) -> Result<Url, Error> {
        if let Some(script) = provider
            && !crate::duckduckgo::is_web_url(&url, script)
        {
            return Err(Error::new(
                ErrorCode::BlockedDestination,
                "DuckDuckGo request URL is outside the permitted provider endpoint",
            ));
        }
        let url = parse_url(url.as_str(), &self.limits)?;
        filter.check(&url)?;
        Ok(url)
    }

    fn redirect_target(
        &self,
        response: &Response,
        url: &Url,
        filter: &DomainFilter,
        provider: Option<bool>,
        redirects: usize,
    ) -> Result<Option<Url>, Error> {
        if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            if redirects == self.limits.max_redirects {
                return Err(Error::new(
                    ErrorCode::UpstreamFailed,
                    "Redirect limit exceeded",
                ));
            }
            let locations = response.headers().get_all(LOCATION);
            let mut locations = locations.iter();
            let location = locations
                .next()
                .and_then(|value| value.to_str().ok())
                .filter(|value| !value.is_empty() && value.len() <= self.limits.max_url_bytes)
                .ok_or_else(|| {
                    Error::new(
                        ErrorCode::UpstreamFailed,
                        "Redirect has no valid Location URL",
                    )
                })?;
            if locations.next().is_some() {
                return Err(Error::new(
                    ErrorCode::UpstreamFailed,
                    "Redirect has multiple Location headers",
                ));
            }
            let authority = location
                .strip_prefix("//")
                .or_else(|| location.split_once("://").map(|(_, rest)| rest))
                .map(|rest| rest.split(['/', '?', '#']).next().unwrap_or_default());
            if location
                .chars()
                .any(|c| c.is_control() || c.is_whitespace())
                || location.contains('\\')
                || authority
                    .is_some_and(|authority| authority.is_empty() || authority.contains('@'))
            {
                return Err(Error::new(
                    ErrorCode::InvalidInput,
                    "Invalid redirect URL syntax",
                ));
            }
            let target = url.join(location).map_err(|_| {
                Error::new(
                    ErrorCode::UpstreamFailed,
                    "Redirect Location URL cannot be resolved",
                )
            })?;
            let target = self.checked_url(target, filter, provider)?;
            if url.scheme() == "https" && target.scheme() != "https" {
                return Err(Error::new(
                    ErrorCode::BlockedDestination,
                    "HTTPS redirects to HTTP are not allowed",
                ));
            }
            return Ok(Some(target));
        }
        Ok(None)
    }

    async fn resolve(&self, url: &Url, ctx: &RequestContext) -> Result<Vec<SocketAddr>, Error> {
        let port = url
            .port_or_known_default()
            .ok_or_else(|| Error::new(ErrorCode::InvalidInput, "URL has no supported port"))?;
        let host = match url.host() {
            Some(Host::Ipv4(ip)) => {
                validate_ip(ip.into())?;
                return Ok(vec![SocketAddr::new(ip.into(), port)]);
            }
            Some(Host::Ipv6(ip)) => {
                validate_ip(ip.into())?;
                return Ok(vec![SocketAddr::new(ip.into(), port)]);
            }
            Some(Host::Domain(host)) => host,
            None => return Err(Error::new(ErrorCode::InvalidInput, "URL has no hostname")),
        };
        let lookup = self.dns.lookup(host, &self.limits, ctx).await?;
        ctx.check()?;
        if lookup.as_lookup().answers().len() > MAX_DNS_ANSWERS {
            return Err(Error::new(
                ErrorCode::DnsFailed,
                "DNS answer count exceeds the safety limit",
            ));
        }
        let mut addresses = Vec::new();
        for ip in lookup.iter() {
            validate_ip(ip)?;
            let address = SocketAddr::new(ip, port);
            if !addresses.contains(&address) {
                addresses.push(address);
            }
        }
        if addresses.is_empty() {
            return Err(Error::new(
                ErrorCode::DnsFailed,
                "DNS lookup returned no usable addresses",
            ));
        }
        Ok(addresses)
    }

    fn client(
        &self,
        url: &Url,
        addresses: Vec<SocketAddr>,
        ctx: &RequestContext,
    ) -> Result<Client, Error> {
        let remaining = ctx.remaining()?;
        Client::builder()
            .dns_resolver(FrozenDns {
                host: url.host_str().unwrap_or_default().to_owned(),
                addresses,
            })
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .referer(false)
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .http1_only()
            .http1_max_headers(64)
            .pool_max_idle_per_host(0)
            .timeout(remaining)
            .connect_timeout(remaining.min(Duration::from_secs(self.limits.connect_timeout_secs)))
            .user_agent("scry/0.1")
            .build()
            .map_err(|_| {
                Error::new(
                    ErrorCode::Internal,
                    "Cannot create the outbound HTTP client",
                )
            })
    }

    async fn read_pdf(
        &self,
        mut response: Response,
        ctx: &RequestContext,
    ) -> Result<Vec<u8>, Error> {
        let expected_length = response.content_length();
        let mut downloaded = 0usize;
        let mut bytes: Vec<u8> = Vec::new();
        loop {
            let chunk = response.chunk().await.map_err(network_error)?;
            ctx.check()?;
            let last = chunk.is_none();
            if last && expected_length.is_some_and(|length| length != downloaded as u64) {
                return Err(Error::new(
                    ErrorCode::UpstreamFailed,
                    "Upstream closed an incomplete response",
                ));
            }
            if let Some(chunk) = chunk {
                let new_len = downloaded
                    .checked_add(chunk.len())
                    .ok_or_else(download_too_large)?;
                if new_len > self.limits.max_download_bytes {
                    return Err(download_too_large());
                }
                if new_len > bytes.capacity() {
                    let capacity = bytes
                        .capacity()
                        .saturating_mul(2)
                        .max(new_len)
                        .min(self.limits.max_download_bytes);
                    bytes
                        .try_reserve_exact(capacity - bytes.len())
                        .map_err(|_| {
                            Error::new(
                                ErrorCode::Overloaded,
                                "Cannot allocate memory for the PDF page",
                            )
                        })?;
                }
                bytes.extend_from_slice(&chunk);
                downloaded = new_len;
            } else {
                break;
            }
        }
        if bytes.is_empty() {
            return Err(Error::new(
                ErrorCode::UnsupportedContent,
                "Upstream returned an empty page",
            ));
        }
        Ok(bytes)
    }

    async fn read_text(
        &self,
        response: Response,
        kind: PageKind,
        encoding: Option<&'static Encoding>,
        ctx: &RequestContext,
    ) -> Result<String, Error> {
        Self::read_text_bounded(response, kind, encoding, &self.limits, ctx).await
    }

    async fn read_text_bounded(
        mut response: Response,
        kind: PageKind,
        encoding: Option<&'static Encoding>,
        limits: &Limits,
        ctx: &RequestContext,
    ) -> Result<String, Error> {
        let expected_length = response.content_length();
        let mut downloaded = 0usize;
        let mut prefix = [0u8; MAX_CHARSET_SNIFF_BYTES];
        let mut prefix_len = 0;
        let mut decoder = None;
        let mut text = String::new();
        loop {
            let chunk = response.chunk().await.map_err(network_error)?;
            ctx.check()?;
            let last = chunk.is_none();
            if last && expected_length.is_some_and(|length| length != downloaded as u64) {
                return Err(Error::new(
                    ErrorCode::UpstreamFailed,
                    "Upstream closed an incomplete response",
                ));
            }
            let mut input = chunk.as_deref().unwrap_or_default();
            downloaded = downloaded
                .checked_add(input.len())
                .ok_or_else(download_too_large)?;
            if downloaded > limits.max_download_bytes {
                return Err(download_too_large());
            }
            if decoder.is_none() {
                let take = input.len().min(prefix.len() - prefix_len);
                prefix[prefix_len..prefix_len + take].copy_from_slice(&input[..take]);
                prefix_len += take;
                input = &input[take..];
                if prefix_len < prefix.len() && !last {
                    continue;
                }
                let mut selected =
                    text_encoding(&prefix[..prefix_len], encoding, kind, ctx)?.new_decoder();
                decode(
                    &mut selected,
                    &prefix[..prefix_len],
                    false,
                    &mut text,
                    limits,
                    ctx,
                )?;
                decoder = Some(selected);
            }
            let decoder = decoder.as_mut().unwrap();
            for input in input.chunks(8192) {
                decode(decoder, input, false, &mut text, limits, ctx)?;
            }
            if last {
                decode(decoder, &[], true, &mut text, limits, ctx)?;
                break;
            }
        }
        if text.trim().is_empty() {
            return Err(Error::new(
                ErrorCode::UnsupportedContent,
                "Upstream returned an empty page",
            ));
        }
        Ok(text)
    }
}

struct FrozenDns {
    host: String,
    addresses: Vec<SocketAddr>,
}

impl Resolve for FrozenDns {
    fn resolve(&self, name: Name) -> Resolving {
        if !name.as_str().eq_ignore_ascii_case(&self.host) {
            return Box::pin(async {
                Err(std::io::Error::other("Unvalidated DNS name is not allowed").into())
            });
        }
        let addresses: Addrs = Box::new(self.addresses.clone().into_iter());
        Box::pin(async move { Ok(addresses) })
    }
}

fn check_status(response: &Response) -> Result<(), Error> {
    let status = response.status();
    let code = match status.as_u16() {
        200 => return Ok(()),
        202 | 401 | 403 => ErrorCode::UpstreamBlocked,
        429 => ErrorCode::RateLimited,
        _ => ErrorCode::UpstreamFailed,
    };
    let mut error = Error::new(code, format!("Upstream returned HTTP {}", status.as_u16()));
    if matches!(
        status,
        StatusCode::ACCEPTED
            | StatusCode::UNAUTHORIZED
            | StatusCode::FORBIDDEN
            | StatusCode::TOO_MANY_REQUESTS
            | StatusCode::SERVICE_UNAVAILABLE
    ) {
        error.retry_after_secs = response.headers().get(RETRY_AFTER).and_then(|value| {
            let value = value.to_str().ok()?.trim();
            if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
                return value.parse::<u64>().ok();
            }
            let date = httpdate::parse_http_date(value).ok()?;
            let delay = date.duration_since(SystemTime::now()).unwrap_or_default();
            Some(
                delay
                    .as_secs()
                    .saturating_add(u64::from(delay.subsec_nanos() != 0)),
            )
        });
    }
    Err(error)
}

fn response_kind_for(
    response: &Response,
    provider: Option<bool>,
) -> Result<(PageKind, Option<&'static Encoding>), Error> {
    let script = provider == Some(true);
    let unsupported = match provider {
        None => "Only HTML, XHTML, plain text, markdown, JSON, and PDF pages are supported",
        Some(true) => "Provider script responses must be HTML, XHTML, JSON, or JavaScript",
        Some(false) => "Provider website responses must be HTML or XHTML",
    };
    for header in response.headers().get_all(CONTENT_ENCODING) {
        let value = header.to_str().map_err(|_| unsupported_encoding())?;
        if value
            .split(',')
            .any(|encoding| !encoding.trim().eq_ignore_ascii_case("identity"))
        {
            return Err(unsupported_encoding());
        }
    }
    let types = response.headers().get_all(CONTENT_TYPE);
    let mut types = types.iter();
    let content_type = types
        .next()
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            Error::new(
                ErrorCode::UnsupportedContent,
                "Page must declare a Content-Type",
            )
        })?;
    if types.next().is_some() {
        return Err(Error::new(
            ErrorCode::UnsupportedContent,
            "Page has multiple Content-Type headers",
        ));
    }
    let mut parts = content_type.split(';');
    let kind = parts
        .next()
        .and_then(|kind| {
            let kind = kind.trim().to_ascii_lowercase();
            match kind.as_str() {
                "text/html" | "application/xhtml+xml" => Some(PageKind::Html),
                "application/json" => Some(PageKind::Plain),
                "application/pdf" if !script => Some(PageKind::Pdf),
                "text/plain" | "text/markdown" | "text/x-markdown" if !script => {
                    Some(PageKind::Plain)
                }
                "application/javascript" | "text/javascript" | "application/x-javascript"
                    if script =>
                {
                    Some(PageKind::Plain)
                }
                _ => None,
            }
        })
        .ok_or_else(|| Error::new(ErrorCode::UnsupportedContent, unsupported))?;
    if provider.is_some()
        && !match kind {
            PageKind::Html => true,
            PageKind::Plain => script,
            PageKind::Pdf => false,
        }
    {
        return Err(Error::new(ErrorCode::UnsupportedContent, unsupported));
    }
    let mut encoding = None;
    for part in parts {
        if let Some((key, value)) = part.trim().split_once('=')
            && key.trim().eq_ignore_ascii_case("charset")
        {
            if encoding.is_some() {
                return Err(Error::new(
                    ErrorCode::UnsupportedContent,
                    "Page declares multiple character sets",
                ));
            }
            let value = value.trim();
            let label = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(value);
            encoding = Some(
                Encoding::for_label(label.as_bytes())
                    .filter(|encoding| *encoding != encoding_rs::REPLACEMENT)
                    .ok_or_else(|| {
                        Error::new(
                            ErrorCode::UnsupportedContent,
                            "Page declares an unsupported character set",
                        )
                    })?,
            );
        }
    }
    Ok((kind, encoding))
}

fn text_encoding(
    prefix: &[u8],
    encoding: Option<&'static Encoding>,
    kind: PageKind,
    ctx: &RequestContext,
) -> Result<&'static Encoding, Error> {
    let prefix = &prefix[..prefix.len().min(MAX_CHARSET_SNIFF_BYTES)];
    if let Some((encoding, _)) = Encoding::for_bom(prefix) {
        return Ok(encoding);
    }
    if let Some(encoding) = encoding {
        return Ok(encoding);
    }
    if kind == PageKind::Plain {
        return Ok(UTF_8);
    }
    let mut emitter = DefaultEmitter::default();
    emitter.naively_switch_states(true);
    for token in Tokenizer::new_with_emitter(prefix, emitter) {
        ctx.check()?;
        let Token::StartTag(tag) = token.unwrap_or_else(|never| match never {}) else {
            continue;
        };
        if tag.name.as_slice() != b"meta" {
            continue;
        }
        let label = tag
            .attributes
            .get(b"charset".as_slice())
            .map(|value| value.as_slice())
            .or_else(|| {
                let pragma = tag.attributes.get(b"http-equiv".as_slice())?;
                if !pragma.eq_ignore_ascii_case(b"content-type") {
                    return None;
                }
                let content = tag.attributes.get(b"content".as_slice())?;
                let content = std::str::from_utf8(content).ok()?;
                content.split(';').find_map(|part| {
                    let (key, value) = part.split_once('=')?;
                    if !key.trim_ascii().eq_ignore_ascii_case("charset") {
                        return None;
                    }
                    let value = value.trim_ascii();
                    let label = value
                        .strip_prefix('"')
                        .and_then(|value| value.strip_suffix('"'))
                        .or_else(|| {
                            value
                                .strip_prefix('\'')
                                .and_then(|value| value.strip_suffix('\''))
                        })
                        .unwrap_or(value);
                    Some(label.as_bytes())
                })
            });
        let Some(encoding) = label
            .filter(|label| label.is_ascii())
            .and_then(Encoding::for_label_no_replacement)
        else {
            continue;
        };
        return Ok(
            if encoding == encoding_rs::UTF_16LE || encoding == encoding_rs::UTF_16BE {
                UTF_8
            } else if encoding == encoding_rs::X_USER_DEFINED {
                encoding_rs::WINDOWS_1252
            } else {
                encoding
            },
        );
    }
    Ok(UTF_8)
}

fn decode(
    decoder: &mut Decoder,
    mut input: &[u8],
    last: bool,
    html: &mut String,
    limits: &Limits,
    ctx: &RequestContext,
) -> Result<(), Error> {
    loop {
        ctx.check()?;
        let mut output = [0u8; 8192];
        let (result, read, written, _) = decoder.decode_to_utf8(input, &mut output, last);
        let required = html
            .len()
            .checked_add(written)
            .filter(|size| *size <= limits.max_decoded_bytes)
            .ok_or_else(|| {
                Error::new(ErrorCode::TooLarge, "Decoded page exceeds the byte limit")
            })?;
        if required > html.capacity() {
            let capacity = html
                .capacity()
                .saturating_mul(2)
                .max(required)
                .min(limits.max_decoded_bytes);
            html.try_reserve_exact(capacity - html.len()).map_err(|_| {
                Error::new(
                    ErrorCode::Overloaded,
                    "Cannot allocate memory for decoded page",
                )
            })?;
        }
        html.push_str(std::str::from_utf8(&output[..written]).map_err(|_| {
            Error::new(
                ErrorCode::Internal,
                "Character decoder returned invalid UTF-8",
            )
        })?);
        input = &input[read..];
        if result == CoderResult::InputEmpty {
            return Ok(());
        }
    }
}

fn network_error(error: reqwest::Error) -> Error {
    if error.is_timeout() {
        Error::timeout()
    } else if error.is_connect() {
        Error::new(
            ErrorCode::UpstreamFailed,
            "Cannot connect to the upstream server or complete TLS",
        )
    } else {
        Error::new(
            ErrorCode::UpstreamFailed,
            "Upstream HTTP request or body transfer failed",
        )
    }
}

fn download_too_large() -> Error {
    Error::new(
        ErrorCode::TooLarge,
        "Downloaded page exceeds the byte limit",
    )
}

fn unsupported_encoding() -> Error {
    Error::new(
        ErrorCode::UnsupportedContent,
        "Server must return identity Content-Encoding",
    )
}
