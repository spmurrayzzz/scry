use std::sync::Arc;
use std::time::Duration;

use hickory_resolver::config::{LookupIpStrategy, ResolveHosts, ResolverConfig, ResolverOpts};
use hickory_resolver::lookup_ip::LookupIp;
use hickory_resolver::net::NetError;
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::{Resolver, TokioResolver};
use tokio::sync::Semaphore;
use tokio::time::{Instant, sleep_until};

use crate::api::{Error, ErrorCode};
use crate::config::Limits;
use crate::context::RequestContext;

#[cfg(test)]
mod tests;

#[derive(Clone)]
pub(super) struct Dns {
    read_config: Arc<dyn Fn() -> Result<ResolverConfig, Error> + Send + Sync>,
    reads: Arc<Semaphore>,
}

impl Dns {
    pub(super) fn new(limits: &Limits) -> Result<Self, Error> {
        resolver(system_config()?, limits)?;
        Ok(Self::with_reader(limits.max_fetches, system_config))
    }

    pub(super) fn with_reader(
        max_reads: usize,
        read_config: impl Fn() -> Result<ResolverConfig, Error> + Send + Sync + 'static,
    ) -> Self {
        Self {
            read_config: Arc::new(read_config),
            reads: Arc::new(Semaphore::new(max_reads)),
        }
    }

    pub(super) async fn lookup(
        &self,
        host: &str,
        limits: &Limits,
        parent: &RequestContext,
    ) -> Result<LookupIp, Error> {
        let mut ctx = parent.child();
        ctx.deadline = ctx
            .deadline
            .min(Instant::now() + Duration::from_secs(limits.dns_timeout_secs));
        ctx.check()?;
        let lookup = async {
            let permit =
                self.reads.clone().acquire_owned().await.map_err(|_| {
                    Error::new(ErrorCode::Internal, "DNS configuration limiter closed")
                })?;
            let read_config = self.read_config.clone();
            let read_ctx = ctx.clone();
            let task = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                read_ctx.check()?;
                read_config()
            });
            let config = task
                .await
                .map_err(|_| Error::new(ErrorCode::Internal, "DNS configuration task failed"))??;
            ctx.check()?;
            resolver(config, limits)?
                .lookup_ip(format!("{host}."))
                .await
                .map_err(lookup_error)
        };
        tokio::select! {
            biased;
            _ = ctx.cancellation.cancelled() => Err(Error::cancelled()),
            _ = sleep_until(ctx.deadline) => Err(dns_timeout()),
            result = lookup => {
                ctx.check()?;
                result
            }
        }
    }
}

fn system_config() -> Result<ResolverConfig, Error> {
    hickory_resolver::system_conf::read_system_conf()
        .map(|(config, _)| config)
        .map_err(|_| {
            Error::new(
                ErrorCode::DnsFailed,
                "Cannot read system DNS server configuration",
            )
        })
}

fn resolver(config: ResolverConfig, limits: &Limits) -> Result<TokioResolver, Error> {
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
    Resolver::builder_with_config(
        ResolverConfig::from_name_servers(config.name_servers),
        TokioRuntimeProvider::default(),
    )
    .with_options(options)
    .build()
    .map_err(|_| Error::new(ErrorCode::DnsFailed, "Cannot initialize the DNS resolver"))
}

fn dns_timeout() -> Error {
    Error::new(ErrorCode::Timeout, "DNS lookup deadline exceeded")
}

fn lookup_error(error: NetError) -> Error {
    match error {
        NetError::Timeout => dns_timeout(),
        _ => Error::new(
            ErrorCode::DnsFailed,
            "DNS lookup failed or returned no addresses",
        ),
    }
}
