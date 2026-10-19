use std::collections::HashSet;
use std::future::Future;
use std::io::{self, Write};
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::{FuturesUnordered, StreamExt};
use serde::Serialize;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use tokio::time::{Instant, sleep_until};
use url::Url;

use crate::api::{
    ContentOrigin, ContentsRequest, Error, ErrorCode, ItemStatus, Response, SearchRequest,
    SearchResult,
};
use crate::config::{Config, Limits};
use crate::context::RequestContext;
use crate::duckduckgo::{self, DiscoveredResult};
use crate::extract;
use crate::fetch::Fetcher;
use crate::policy::{self, DomainFilter};

#[cfg(test)]
mod tests;

#[derive(Clone)]
pub struct Scry {
    inner: Arc<Shared>,
}

#[derive(Clone, Copy)]
struct ContentOptions<'a> {
    query: Option<&'a str>,
    include_text: bool,
    include_highlights: bool,
}

struct Shared {
    config: Config,
    fetcher: Fetcher,
    requests: Arc<Semaphore>,
    pipeline: Arc<Semaphore>,
    parsers: Arc<Semaphore>,
    provider_ready: Mutex<Option<Instant>>,
}

impl Scry {
    pub fn new(config: Config) -> Result<Self, Error> {
        config.validate()?;
        let limits = &config.limits;
        Ok(Self {
            inner: Arc::new(Shared {
                fetcher: Fetcher::new(limits.clone())?,
                requests: Arc::new(Semaphore::new(limits.max_requests)),
                pipeline: Arc::new(Semaphore::new(limits.max_fetches)),
                parsers: Arc::new(Semaphore::new(limits.max_parsers)),
                provider_ready: Mutex::new(Some(Instant::now())),
                config,
            }),
        })
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    pub async fn search(&self, request: SearchRequest) -> Result<Response, Error> {
        self.search_with_context(request, self.context()).await
    }

    pub async fn contents(&self, request: ContentsRequest) -> Result<Response, Error> {
        self.contents_with_context(request, self.context()).await
    }

    pub async fn search_with_context(
        &self,
        request: SearchRequest,
        ctx: RequestContext,
    ) -> Result<Response, Error> {
        ctx.check()?;
        let ctx = self.limit_context(&ctx);
        let _cancel = ctx.cancellation.clone().drop_guard();
        let _admission = self.admit()?;
        tokio::select! {
            biased;
            _ = ctx.cancellation.cancelled() => Err(Error::cancelled()),
            result = self.search_inner(request, &ctx) => {
                if ctx.cancellation.is_cancelled() {
                    Err(Error::cancelled())
                } else {
                    result
                }
            }
        }
    }

    pub async fn contents_with_context(
        &self,
        request: ContentsRequest,
        ctx: RequestContext,
    ) -> Result<Response, Error> {
        ctx.check()?;
        let ctx = self.limit_context(&ctx);
        let _cancel = ctx.cancellation.clone().drop_guard();
        let _admission = self.admit()?;
        tokio::select! {
            biased;
            _ = ctx.cancellation.cancelled() => Err(Error::cancelled()),
            result = self.contents_inner(request, &ctx) => {
                if ctx.cancellation.is_cancelled() {
                    Err(Error::cancelled())
                } else {
                    result
                }
            }
        }
    }

    fn context(&self) -> RequestContext {
        RequestContext::new(Duration::from_secs(
            self.config().limits.request_timeout_secs,
        ))
    }

    fn limit_context(&self, parent: &RequestContext) -> RequestContext {
        let mut ctx = parent.child();
        ctx.deadline = ctx.deadline.min(self.context().deadline);
        ctx
    }

    fn admit(&self) -> Result<tokio::sync::OwnedSemaphorePermit, Error> {
        self.inner
            .requests
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::new(ErrorCode::Overloaded, "Request capacity exhausted"))
    }

    async fn search_inner(
        &self,
        request: SearchRequest,
        ctx: &RequestContext,
    ) -> Result<Response, Error> {
        let limits = &self.config().limits;
        if request.start_published_date.is_some() || request.end_published_date.is_some() {
            return Err(Error::new(
                ErrorCode::Unsupported,
                "Strict publication-date filters are not supported",
            ));
        }
        let query_limit = limits.max_query_chars.min(499);
        let query_length = request.query.chars().take(query_limit + 1).count();
        if query_length == 0 || query_length > query_limit || request.query.trim().is_empty() {
            return Err(Error::new(
                ErrorCode::InvalidInput,
                format!("query must contain 1 to {query_limit} characters"),
            ));
        }
        if !(1..=20).contains(&request.num_results) {
            return Err(Error::new(
                ErrorCode::InvalidInput,
                "numResults must be between 1 and 20",
            ));
        }
        if !matches!(request.search_type.as_str(), "auto" | "keyword") {
            return Err(Error::new(
                ErrorCode::Unsupported,
                "Only auto and keyword search types are supported; both use DuckDuckGo keyword search",
            ));
        }
        if request
            .include_domains
            .len()
            .saturating_add(request.exclude_domains.len())
            > limits.max_domain_filters
        {
            return Err(Error::new(
                ErrorCode::InvalidInput,
                "Too many domain filters",
            ));
        }
        let filter = DomainFilter::new(&request.include_domains, &request.exclude_domains, limits)?;
        check_request_size(&request, limits, ctx)?;
        let mut response = Response {
            results: Vec::new(),
            warnings: Vec::new(),
        };
        let query = provider_query(&request, query_limit, &mut response.warnings);
        let discovered = self.discover(query, ctx).await?;
        let mut seen = HashSet::new();
        let mut jobs = Vec::new();
        let mut filtered = false;
        let options = ContentOptions {
            query: Some(&request.query),
            include_text: request.include_text,
            include_highlights: request.include_highlights,
        };
        for discovered in discovered.into_iter().take(20) {
            let url = match policy::parse_url(discovered.url.as_str(), limits) {
                Ok(url) => url,
                Err(_) => {
                    filtered = true;
                    continue;
                }
            };
            if filter.check(&url).is_err() {
                filtered = true;
                continue;
            }
            if !seen.insert(url.as_str().to_owned()) {
                continue;
            }
            let mut result = metadata(url.as_str().to_owned());
            result.title = Some(discovered.title);
            result.truncated = discovered.truncated;
            if !discovered.snippet.is_empty() {
                if request.include_highlights {
                    result.highlights.push(discovered.snippet.clone());
                }
                result.snippet = Some(discovered.snippet);
                result.content_origin = Some(ContentOrigin::ProviderSnippet);
            }
            if request.include_text {
                jobs.push((response.results.len(), url));
            }
            response.results.push(result);
            if seen.len() == request.num_results {
                break;
            }
        }
        if filtered && seen.len() < request.num_results {
            response.warnings.push(
                "Fewer results remain after local URL and domain filtering; no additional provider pages were requested"
                    .to_owned(),
            );
        }
        self.collect(response, jobs, &filter, options, ctx).await
    }

    async fn contents_inner(
        &self,
        request: ContentsRequest,
        ctx: &RequestContext,
    ) -> Result<Response, Error> {
        let limits = &self.config().limits;
        let count = request.urls.len().saturating_add(request.ids.len());
        if count == 0 || count > limits.max_urls {
            return Err(Error::new(
                ErrorCode::InvalidInput,
                format!(
                    "urls and ids must contain 1 to {} entries in total",
                    limits.max_urls
                ),
            ));
        }
        check_request_size(&request, limits, ctx)?;
        let mut seen = HashSet::new();
        let mut prepared = Vec::new();
        for input in request.urls.into_iter().chain(request.ids) {
            ctx.check()?;
            match policy::parse_url(&input, limits) {
                Ok(url) => {
                    if seen.insert(url.as_str().to_owned()) {
                        prepared.push((metadata(url.as_str().to_owned()), Some(url)));
                    }
                }
                Err(error) => {
                    let mut result = metadata(input);
                    result.status = ItemStatus::Error;
                    result.error = Some(error);
                    prepared.push((result, None));
                }
            }
        }
        ctx.check()?;
        let filter = DomainFilter::default();
        let options = ContentOptions {
            query: None,
            include_text: request.include_text,
            include_highlights: request.include_highlights,
        };
        let mut jobs = Vec::new();
        let mut response = Response {
            results: Vec::with_capacity(prepared.len()),
            warnings: Vec::new(),
        };
        for (result, url) in prepared {
            if (request.include_text || request.include_highlights)
                && let Some(url) = url
            {
                jobs.push((response.results.len(), url));
            }
            response.results.push(result);
        }
        if !request.include_text && !request.include_highlights {
            response.warnings.push(
                "Content retrieval was not requested; URLs were validated but not fetched"
                    .to_owned(),
            );
        }
        self.collect(response, jobs, &filter, options, ctx).await
    }

    async fn collect(
        &self,
        mut response: Response,
        jobs: Vec<(usize, Url)>,
        filter: &DomainFilter,
        options: ContentOptions<'_>,
        ctx: &RequestContext,
    ) -> Result<Response, Error> {
        let work_ctx = ctx.child();
        let _cancel = work_ctx.cancellation.clone().drop_guard();
        let mut unfinished = vec![false; response.results.len()];
        let mut pending = FuturesUnordered::new();
        for (index, url) in jobs {
            unfinished[index] = true;
            let result = response.results[index].clone();
            let work_ctx = &work_ctx;
            pending.push(async move {
                (
                    index,
                    self.enrich(result, url, filter, options, work_ctx).await,
                )
            });
        }
        while !pending.is_empty() {
            tokio::select! {
                biased;
                _ = ctx.cancellation.cancelled() => return Err(Error::cancelled()),
                _ = sleep_until(ctx.deadline) => break,
                Some((index, result)) = pending.next() => {
                    response.results[index] = result;
                    unfinished[index] = false;
                }
            }
        }
        work_ctx.cancellation.cancel();
        drop(pending);
        for (index, unfinished) in unfinished.into_iter().enumerate() {
            if unfinished {
                fail_result(&mut response.results[index], Error::timeout());
            }
        }
        let final_ctx =
            RequestContext::with_token(Duration::from_secs(1), ctx.cancellation.clone());
        bound_response(response, &self.config().limits, &final_ctx).map_err(|error| {
            if error.code == ErrorCode::Timeout {
                Error::new(
                    ErrorCode::TooLarge,
                    "Response finalization exceeded its time budget",
                )
            } else {
                error
            }
        })
    }

    async fn discover(
        &self,
        query: String,
        ctx: &RequestContext,
    ) -> Result<Vec<DiscoveredResult>, Error> {
        let mut ready = within(ctx, self.inner.provider_ready.lock()).await?;
        let Some(next) = *ready else {
            return Err(provider_rate_limited(u64::MAX));
        };
        if next >= ctx.deadline {
            let remaining = next.saturating_duration_since(Instant::now());
            return Err(provider_rate_limited(
                remaining
                    .as_secs()
                    .saturating_add(u64::from(remaining.subsec_nanos() != 0)),
            ));
        }
        within(ctx, sleep_until(next)).await?;
        let pipeline_permit = within(ctx, self.inner.pipeline.clone().acquire_owned())
            .await?
            .map_err(|_| Error::new(ErrorCode::Internal, "Pipeline limiter closed"))?;
        *ready = Instant::now().checked_add(Duration::from_secs(1));
        let url = Url::parse("https://html.duckduckgo.com/html/")
            .map_err(|_| Error::new(ErrorCode::Internal, "Invalid provider URL"))?;
        let page = within(
            ctx,
            self.inner.fetcher.post_form(
                url,
                &[("q", query.as_str()), ("kl", "us-en"), ("b", "")],
                ctx,
            ),
        )
        .await?;
        if let Err(error) = &page
            && let Some(seconds) = error.retry_after_secs
        {
            *ready = match (
                *ready,
                Instant::now().checked_add(Duration::from_secs(seconds)),
            ) {
                (Some(previous), Some(next)) => Some(previous.max(next)),
                _ => None,
            };
        }
        drop(ready);
        let page = page?;
        self.parse(ctx, pipeline_permit, move |ctx, limits| {
            duckduckgo::search_results(&page.html, &page.final_url, ctx, limits)
        })
        .await
    }

    async fn enrich(
        &self,
        mut result: SearchResult,
        url: Url,
        filter: &DomainFilter,
        options: ContentOptions<'_>,
        ctx: &RequestContext,
    ) -> SearchResult {
        let extracted = async {
            filter.check(&url)?;
            let pipeline_permit = within(ctx, self.inner.pipeline.clone().acquire_owned())
                .await?
                .map_err(|_| Error::new(ErrorCode::Internal, "Pipeline limiter closed"))?;
            let page = within(ctx, self.inner.fetcher.get(url, filter, ctx)).await??;
            filter.check(&page.final_url)?;
            result.final_url = Some(page.final_url.as_str().to_owned());
            let query = options.query.map(str::to_owned);
            self.parse(ctx, pipeline_permit, move |ctx, limits| {
                extract::extract(&page.html, &page.final_url, query.as_deref(), ctx, limits)
            })
            .await
        }
        .await;
        match extracted {
            Ok(page) => {
                if result.title.as_ref().is_none_or(|title| title.is_empty()) {
                    result.title = page.title;
                }
                result.text = options.include_text.then_some(page.text);
                result.highlights = if options.include_highlights {
                    page.highlights
                } else {
                    Vec::new()
                };
                result.content_origin = Some(ContentOrigin::Live);
                result.truncated |= page.truncated;
            }
            Err(error) => fail_result(&mut result, error),
        }
        result
    }

    async fn parse<T, F>(
        &self,
        ctx: &RequestContext,
        pipeline_permit: OwnedSemaphorePermit,
        parse: F,
    ) -> Result<T, Error>
    where
        T: Send + 'static,
        F: FnOnce(&RequestContext, &Limits) -> Result<T, Error> + Send + 'static,
    {
        let permit = within(ctx, self.inner.parsers.clone().acquire_owned())
            .await?
            .map_err(|_| Error::new(ErrorCode::Internal, "Parser limiter closed"))?;
        let parser_ctx = ctx.clone();
        let limits = self.config().limits.clone();
        let task = tokio::task::spawn_blocking(move || {
            let _pipeline_permit = pipeline_permit;
            let _permit = permit;
            parser_ctx.check()?;
            parse(&parser_ctx, &limits)
        });
        within(ctx, task)
            .await?
            .map_err(|_| Error::new(ErrorCode::Internal, "Parser task failed"))?
    }
}

async fn within<T>(ctx: &RequestContext, future: impl Future<Output = T>) -> Result<T, Error> {
    ctx.check()?;
    tokio::select! {
        biased;
        _ = ctx.cancellation.cancelled() => Err(Error::cancelled()),
        _ = sleep_until(ctx.deadline) => Err(Error::timeout()),
        value = future => {
            ctx.check()?;
            Ok(value)
        }
    }
}

fn provider_rate_limited(seconds: u64) -> Error {
    let mut error = Error::new(ErrorCode::RateLimited, "DuckDuckGo is in a retry cooldown");
    error.retry_after_secs = Some(seconds);
    error
}

fn fail_result(result: &mut SearchResult, error: Error) {
    result.status = if result.snippet.is_some() || result.title.is_some() {
        ItemStatus::Partial
    } else {
        ItemStatus::Error
    };
    result.error = Some(error);
}

fn metadata(url: String) -> SearchResult {
    SearchResult {
        id: url.clone(),
        url,
        final_url: None,
        title: None,
        text: None,
        highlights: Vec::new(),
        snippet: None,
        status: ItemStatus::Ok,
        error: None,
        content_origin: None,
        truncated: false,
    }
}

fn provider_query(request: &SearchRequest, limit: usize, warnings: &mut Vec<String>) -> String {
    let normalize = |domain: &str| -> Option<String> {
        let domain = domain.trim_end_matches('.');
        let host = url::Host::parse(domain)
            .ok()?
            .to_string()
            .to_ascii_lowercase();
        if host.is_empty()
            || !host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
        {
            return None;
        }
        Some(host)
    };
    let mut operators = Vec::new();
    let includes: Option<Vec<_>> = request
        .include_domains
        .iter()
        .map(|domain| normalize(domain).map(|host| format!("site:{host}")))
        .collect();
    if let Some(includes) = includes {
        if !includes.is_empty() {
            operators.push(format!("({})", includes.join(" OR ")));
        }
    } else {
        warnings.push("Include-domain filters are applied locally only".to_owned());
    }
    for domain in &request.exclude_domains {
        if let Some(host) = normalize(domain) {
            operators.push(format!("-site:{host}"));
        } else if !warnings
            .iter()
            .any(|warning| warning.starts_with("Exclude-domain"))
        {
            warnings.push("Exclude-domain filters are applied locally only".to_owned());
        }
    }
    if operators.is_empty() {
        return request.query.clone();
    }
    let expanded = format!("({}) {}", request.query, operators.join(" "));
    if expanded.chars().count() > limit {
        warnings.push(
            "Domain operators exceed the provider query limit; all domain filters are applied locally"
                .to_owned(),
        );
        request.query.clone()
    } else {
        expanded
    }
}

struct JsonSize<'a> {
    bytes: usize,
    limit: usize,
    ctx: &'a RequestContext,
}

impl Write for JsonSize<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.ctx.check().map_err(io::Error::other)?;
        if bytes.len() > self.limit.saturating_sub(self.bytes) {
            return Err(io::Error::other("Serialized size limit exceeded"));
        }
        self.bytes += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn json_size(value: &impl Serialize, limit: usize, ctx: &RequestContext) -> Result<usize, Error> {
    let mut writer = JsonSize {
        bytes: 0,
        limit,
        ctx,
    };
    serde_json::to_writer(&mut writer, value).map_err(|_| {
        ctx.check().err().unwrap_or_else(|| {
            Error::new(
                ErrorCode::TooLarge,
                "Serialized data exceeds the size limit",
            )
        })
    })?;
    Ok(writer.bytes)
}

fn check_request_size(
    request: &impl Serialize,
    limits: &Limits,
    ctx: &RequestContext,
) -> Result<(), Error> {
    json_size(request, limits.max_request_bytes, ctx).map(|_| ())
}

fn bound_response(
    mut response: Response,
    limits: &Limits,
    ctx: &RequestContext,
) -> Result<Response, Error> {
    let mut size = json_size(&response, usize::MAX, ctx)?;
    if size > limits.max_response_bytes {
        response
            .warnings
            .push("Content was omitted to fit the response size limit".to_owned());
        size = json_size(&response, usize::MAX, ctx)?;
    }
    for result in response.results.iter_mut().rev() {
        if size <= limits.max_response_bytes {
            break;
        }
        if result.text.is_some() {
            let before = json_size(result, usize::MAX, ctx)?;
            result.text = None;
            result.truncated = true;
            size = size - before + json_size(result, usize::MAX, ctx)?;
        }
    }
    for result in response.results.iter_mut().rev() {
        if size <= limits.max_response_bytes {
            break;
        }
        while size > limits.max_response_bytes && !result.highlights.is_empty() {
            let before = json_size(result, usize::MAX, ctx)?;
            if let Some(highlight) = result.highlights.last_mut() {
                trim_json_string(highlight, size - limits.max_response_bytes, ctx)?;
                if highlight.is_empty() {
                    result.highlights.pop();
                }
            }
            result.truncated = true;
            size = size - before + json_size(result, usize::MAX, ctx)?;
        }
        if size > limits.max_response_bytes && result.snippet.is_some() {
            let before = json_size(result, usize::MAX, ctx)?;
            if let Some(snippet) = result.snippet.as_mut() {
                trim_json_string(snippet, size - limits.max_response_bytes, ctx)?;
                if snippet.is_empty() {
                    result.snippet = None;
                }
            }
            result.truncated = true;
            size = size - before + json_size(result, usize::MAX, ctx)?;
        }
    }
    json_size(&response, limits.max_response_bytes, ctx).map_err(|error| {
        if error.code == ErrorCode::TooLarge {
            Error::new(
                ErrorCode::TooLarge,
                "Response metadata exceeds the response size limit",
            )
        } else {
            error
        }
    })?;
    Ok(response)
}

fn trim_json_string(
    text: &mut String,
    bytes_to_remove: usize,
    ctx: &RequestContext,
) -> Result<(), Error> {
    let mut removed = 0;
    let mut end = text.len();
    for (index, ch) in text.char_indices().rev() {
        ctx.check()?;
        if removed >= bytes_to_remove {
            break;
        }
        removed += match ch {
            '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
            '\u{0}'..='\u{1f}' => 6,
            _ => ch.len_utf8(),
        };
        end = index;
    }
    text.truncate(end);
    Ok(())
}
