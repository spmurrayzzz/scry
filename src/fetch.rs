use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use encoding_rs::{CoderResult, Decoder, Encoding, UTF_8};
use hickory_resolver::config::{LookupIpStrategy, ResolveHosts, ResolverConfig, ResolverOpts};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::{Resolver, TokioResolver};
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

mod provider;

#[cfg(test)]
mod tests;

const MAX_DNS_ANSWERS: usize = 64;
const MAX_CHARSET_SNIFF_BYTES: usize = 1024;

#[derive(Clone)]
pub(crate) struct Fetcher {
    limits: Limits,
    resolver: TokioResolver,
    semaphore: Arc<Semaphore>,
}

pub(crate) struct FetchedPage {
    pub final_url: Url,
    pub html: String,
}

impl Fetcher {
    pub(crate) fn new(limits: Limits) -> Result<Self, Error> {
        limits.validate()?;
        let (config, _) = hickory_resolver::system_conf::read_system_conf().map_err(|_| {
            Error::new(
                ErrorCode::DnsFailed,
                "Cannot read system DNS server configuration",
            )
        })?;
        if config.name_servers.is_empty() || config.name_servers.len() > 8 {
            return Err(Error::new(
                ErrorCode::DnsFailed,
                "System DNS configuration must contain between one and eight servers",
            ));
        }
        let mut options = ResolverOpts::default();
        options.timeout = Duration::from_secs(limits.dns_timeout_secs);
        options.attempts = 0;
        options.ip_strategy = LookupIpStrategy::Ipv4AndIpv6;
        options.cache_size = 0;
        options.use_hosts_file = ResolveHosts::Never;
        options.num_concurrent_reqs = 1;
        options.max_active_requests = limits.max_fetches.saturating_mul(2);
        options.positive_max_ttl = Some(Duration::ZERO);
        options.negative_max_ttl = Some(Duration::ZERO);
        let resolver = Resolver::builder_with_config(
            ResolverConfig::from_name_servers(config.name_servers),
            TokioRuntimeProvider::default(),
        )
        .with_options(options)
        .build()
        .map_err(|_| Error::new(ErrorCode::DnsFailed, "Cannot initialize the DNS resolver"))?;
        Ok(Self {
            semaphore: Arc::new(Semaphore::new(limits.max_fetches)),
            limits,
            resolver,
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

    pub(crate) async fn post_form(
        &self,
        url: Url,
        form: &[(&str, &str)],
        ctx: &RequestContext,
    ) -> Result<FetchedPage, Error> {
        self.fetch(url, &DomainFilter::default(), Some(form), ctx)
            .await
    }

    async fn fetch(
        &self,
        url: Url,
        filter: &DomainFilter,
        form: Option<&[(&str, &str)]>,
        parent: &RequestContext,
    ) -> Result<FetchedPage, Error> {
        let mut ctx = parent.child();
        ctx.deadline = ctx
            .deadline
            .min(Instant::now() + Duration::from_secs(self.limits.fetch_timeout_secs));
        ctx.check()?;
        let url = parse_url(url.as_str(), &self.limits)?;
        filter.check(&url)?;
        if let Some(form) = form {
            self.check_form(form)?;
        }
        let _permit = self.semaphore.clone().try_acquire_owned().map_err(|_| {
            Error::new(
                ErrorCode::Overloaded,
                "All outbound fetch slots are busy; try again later",
            )
        })?;
        tokio::select! {
            biased;
            _ = ctx.cancellation.cancelled() => Err(Error::cancelled()),
            _ = tokio::time::sleep_until(ctx.deadline) => Err(Error::timeout()),
            result = self.follow(url, filter, form, &ctx) => result,
        }
    }

    async fn follow(
        &self,
        mut url: Url,
        filter: &DomainFilter,
        mut form: Option<&[(&str, &str)]>,
        ctx: &RequestContext,
    ) -> Result<FetchedPage, Error> {
        let provider_request = form.is_some() && provider::matches(&url);
        for redirects in 0..=self.limits.max_redirects {
            ctx.check()?;
            let addresses = self.resolve(&url, ctx).await?;
            ctx.check()?;
            let response = if provider_request && provider::matches(&url) {
                provider::send(self, &url, addresses, form, ctx).await?
            } else {
                let client = self.client(&url, addresses, ctx)?;
                let request = if let Some(form) = form {
                    client.post(url.clone()).form(form)
                } else {
                    client.get(url.clone())
                };
                request
                    .header(ACCEPT, "text/html, application/xhtml+xml")
                    .header(ACCEPT_ENCODING, "identity")
                    .header(CONNECTION, "close")
                    .send()
                    .await
                    .map_err(network_error)?
            };
            ctx.check()?;
            let status = response.status();
            if matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
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
                let target = parse_url(target.as_str(), &self.limits)?;
                filter.check(&target)?;
                if url.scheme() == "https" && target.scheme() != "https" {
                    return Err(Error::new(
                        ErrorCode::BlockedDestination,
                        "HTTPS redirects to HTTP are not allowed",
                    ));
                }
                if form.is_some() {
                    if matches!(status.as_u16(), 301..=303) {
                        form = None;
                    } else if target.origin() != url.origin() {
                        return Err(Error::new(
                            ErrorCode::BlockedDestination,
                            "Redirect cannot forward form data to a different origin",
                        ));
                    }
                }
                drop(response);
                url = target;
                continue;
            }
            check_status(&response)?;
            let encoding = response_encoding(&response)?;
            if response
                .content_length()
                .is_some_and(|size| size > self.limits.max_download_bytes as u64)
            {
                return Err(download_too_large());
            }
            let html = self.read_html(response, encoding, ctx).await?;
            ctx.check()?;
            return Ok(FetchedPage {
                final_url: url,
                html,
            });
        }
        Err(Error::new(ErrorCode::Internal, "Redirect state is invalid"))
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
        let deadline = ctx
            .deadline
            .min(Instant::now() + Duration::from_secs(self.limits.dns_timeout_secs));
        let lookup = tokio::time::timeout_at(deadline, self.resolver.lookup_ip(format!("{host}.")))
            .await
            .map_err(|_| Error::new(ErrorCode::Timeout, "DNS lookup deadline exceeded"))?
            .map_err(|_| {
                Error::new(
                    ErrorCode::DnsFailed,
                    "DNS lookup failed or returned no addresses",
                )
            })?;
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

    fn check_form(&self, form: &[(&str, &str)]) -> Result<(), Error> {
        let mut size = form.len().saturating_sub(1);
        if form.len() > self.limits.max_request_bytes {
            return Err(Error::new(
                ErrorCode::TooLarge,
                "Outbound form exceeds the request limit",
            ));
        }
        for (key, value) in form {
            for part in [key, value] {
                if part.len() > self.limits.max_request_bytes {
                    return Err(Error::new(
                        ErrorCode::TooLarge,
                        "Outbound form exceeds the request limit",
                    ));
                }
                size = size.saturating_add(
                    url::form_urlencoded::byte_serialize(part.as_bytes())
                        .map(str::len)
                        .sum::<usize>(),
                );
            }
            size = size.saturating_add(1);
            if size > self.limits.max_request_bytes {
                return Err(Error::new(
                    ErrorCode::TooLarge,
                    "Outbound form exceeds the request limit",
                ));
            }
        }
        Ok(())
    }

    async fn read_html(
        &self,
        mut response: Response,
        encoding: Option<&'static Encoding>,
        ctx: &RequestContext,
    ) -> Result<String, Error> {
        let expected_length = response.content_length();
        let mut downloaded = 0usize;
        let mut prefix = [0u8; MAX_CHARSET_SNIFF_BYTES];
        let mut prefix_len = 0;
        let mut decoder = None;
        let mut html = String::new();
        loop {
            let chunk = response.chunk().await.map_err(network_error)?;
            ctx.check()?;
            let last = chunk.is_none();
            if last && expected_length.is_some_and(|length| length != downloaded as u64) {
                return Err(Error::new(
                    ErrorCode::UpstreamFailed,
                    "Upstream closed an incomplete HTML response",
                ));
            }
            let mut input = chunk.as_deref().unwrap_or_default();
            downloaded = downloaded
                .checked_add(input.len())
                .ok_or_else(download_too_large)?;
            if downloaded > self.limits.max_download_bytes {
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
                    html_encoding(&prefix[..prefix_len], encoding, ctx)?.new_decoder();
                decode(
                    &mut selected,
                    &prefix[..prefix_len],
                    false,
                    &mut html,
                    &self.limits,
                    ctx,
                )?;
                decoder = Some(selected);
            }
            let decoder = decoder.as_mut().unwrap();
            for input in input.chunks(8192) {
                decode(decoder, input, false, &mut html, &self.limits, ctx)?;
            }
            if last {
                decode(decoder, &[], true, &mut html, &self.limits, ctx)?;
                break;
            }
        }
        if html.trim().is_empty() {
            return Err(Error::new(
                ErrorCode::UnsupportedContent,
                "Upstream returned an empty HTML page",
            ));
        }
        Ok(html)
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

fn response_encoding(response: &Response) -> Result<Option<&'static Encoding>, Error> {
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
                "Page must declare an HTML Content-Type",
            )
        })?;
    if types.next().is_some() {
        return Err(Error::new(
            ErrorCode::UnsupportedContent,
            "Page has multiple Content-Type headers",
        ));
    }
    let mut parts = content_type.split(';');
    if !parts.next().is_some_and(|kind| {
        matches!(
            kind.trim().to_ascii_lowercase().as_str(),
            "text/html" | "application/xhtml+xml"
        )
    }) {
        return Err(Error::new(
            ErrorCode::UnsupportedContent,
            "Only HTML and XHTML pages are supported",
        ));
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
    Ok(encoding)
}

fn html_encoding(
    prefix: &[u8],
    encoding: Option<&'static Encoding>,
    ctx: &RequestContext,
) -> Result<&'static Encoding, Error> {
    let prefix = &prefix[..prefix.len().min(MAX_CHARSET_SNIFF_BYTES)];
    if let Some((encoding, _)) = Encoding::for_bom(prefix) {
        return Ok(encoding);
    }
    if let Some(encoding) = encoding {
        return Ok(encoding);
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
                Error::new(ErrorCode::TooLarge, "Decoded HTML exceeds the byte limit")
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
                    "Cannot allocate memory for decoded HTML",
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
        "Downloaded HTML exceeds the byte limit",
    )
}

fn unsupported_encoding() -> Error {
    Error::new(
        ErrorCode::UnsupportedContent,
        "Server must return identity Content-Encoding",
    )
}
