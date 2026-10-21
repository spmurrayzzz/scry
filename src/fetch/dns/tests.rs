use std::net::Ipv4Addr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use hickory_resolver::config::{ConnectionConfig, NameServerConfig};
use tokio::net::UdpSocket;
use tokio::time::{advance, pause, resume, timeout};

use super::*;

const BUDGET: Duration = Duration::from_secs(3);

#[tokio::test]
async fn resolver_preserves_limits_and_disables_hosts_cache_and_retries() {
    let limits = Limits {
        dns_timeout_secs: 7,
        max_fetches: 3,
        ..Limits::default()
    };
    let server = NameServerConfig::udp_and_tcp(Ipv4Addr::LOCALHOST.into());
    for count in [0, 1, 8, 9] {
        let result = resolver(
            ResolverConfig::from_name_servers(vec![server.clone(); count]),
            &limits,
        );
        if count == 0 || count == 9 {
            let error = result.err().unwrap();
            assert_eq!(error.code, ErrorCode::DnsFailed);
            assert_eq!(
                error.message,
                "System DNS configuration must contain between one and eight servers"
            );
            continue;
        }
        let resolver = result.unwrap();
        let options = resolver.options();
        assert_eq!(options.timeout, Duration::from_secs(7));
        assert_eq!(options.attempts, 0);
        assert_eq!(options.ip_strategy, LookupIpStrategy::Ipv4AndIpv6);
        assert_eq!(options.cache_size, 0);
        assert_eq!(options.use_hosts_file, ResolveHosts::Never);
        assert_eq!(options.num_concurrent_reqs, 1);
        assert_eq!(options.max_active_requests, 6);
        assert_eq!(options.positive_max_ttl, Some(Duration::ZERO));
        assert_eq!(options.negative_max_ttl, Some(Duration::ZERO));
    }
}

#[test]
fn lookup_errors_classify_timeouts_without_exposing_resolver_details() {
    let error = lookup_error(NetError::Timeout);
    assert_eq!(error.code, ErrorCode::Timeout);
    assert_eq!(error.message, "DNS lookup deadline exceeded");
    for cause in [
        NetError::NoConnections,
        NetError::Message("arbitrary resolver details"),
    ] {
        let error = lookup_error(cause);
        assert_eq!(error.code, ErrorCode::DnsFailed);
        assert_eq!(error.message, "DNS lookup failed or returned no addresses");
        assert_eq!(error.retry_after_secs, None);
    }
}

#[tokio::test(start_paused = true)]
async fn expired_and_cancelled_lookups_do_not_read_configuration() {
    let dns = Dns::with_reader(1, || panic!("Unexpected configuration read"));
    let limits = Limits::default();
    for (expired, cancelled) in [(false, true), (true, false), (true, true)] {
        let ctx = RequestContext::new(BUDGET);
        if expired {
            advance(BUDGET).await;
        }
        if cancelled {
            ctx.cancellation.cancel();
        }
        assert_eq!(
            dns.lookup("example.org", &limits, &ctx)
                .await
                .unwrap_err()
                .code,
            if cancelled {
                ErrorCode::Cancelled
            } else {
                ErrorCode::Timeout
            }
        );
        assert_eq!(dns.reads.available_permits(), 1);
    }
}

#[tokio::test]
async fn configuration_reads_remain_bounded_after_timeout_and_cancellation() {
    for (parent_secs, cancelled) in [(30, false), (1, false), (30, true)] {
        let (started, entered) = tokio::sync::oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let dns = Dns::with_reader(1, {
            let started = Mutex::new(Some(started));
            let gate = Mutex::new(gate);
            let calls = calls.clone();
            move || {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    started.lock().unwrap().take().unwrap().send(()).unwrap();
                    gate.lock().unwrap().recv_timeout(BUDGET).unwrap();
                }
                Err(Error::new(
                    ErrorCode::DnsFailed,
                    "Configuration unavailable",
                ))
            }
        });
        let limits = Limits::default();
        let ctx = RequestContext::new(Duration::from_secs(parent_secs));
        let mut lookup = Box::pin(dns.lookup("example.org", &limits, &ctx));
        tokio::select! {
            result = entered => result.unwrap(),
            _ = lookup.as_mut() => panic!("Lookup finished before configuration read"),
            _ = tokio::time::sleep(BUDGET) => panic!("Configuration read did not start"),
        }
        pause();
        let deadline = ctx
            .deadline
            .min(Instant::now() + Duration::from_secs(limits.dns_timeout_secs));
        if cancelled {
            ctx.cancellation.cancel();
        }
        advance(deadline - Instant::now() + Duration::from_millis(1)).await;
        assert_eq!(
            lookup.await.unwrap_err().code,
            if cancelled {
                ErrorCode::Cancelled
            } else {
                ErrorCode::Timeout
            }
        );
        assert_eq!(dns.reads.available_permits(), 0);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let clone = dns.clone();
        let ctx = RequestContext::new(Duration::from_secs(30));
        let mut waiting = Box::pin(clone.lookup("example.org", &limits, &ctx));
        assert!(futures_util::poll!(waiting.as_mut()).is_pending());
        advance(Duration::from_millis(3001)).await;
        assert_eq!(waiting.await.unwrap_err().code, ErrorCode::Timeout);
        assert_eq!(dns.reads.available_permits(), 0);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        resume();
        release.send(()).unwrap();
        let permit = timeout(BUDGET, dns.reads.acquire()).await.unwrap().unwrap();
        drop(permit);
        let error = timeout(
            BUDGET,
            dns.lookup("example.org", &limits, &RequestContext::new(BUDGET)),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(error.message, "Configuration unavailable");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(dns.reads.available_permits(), 1);
    }
}

#[tokio::test]
async fn configuration_read_and_network_lookup_share_one_cancellable_deadline() {
    for cancelled in [false, true] {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = socket.local_addr().unwrap();
        let mut connection = ConnectionConfig::udp();
        connection.port = address.port();
        let config = ResolverConfig::from_name_servers(vec![NameServerConfig::new(
            address.ip(),
            true,
            vec![connection],
        )]);
        let (started, entered) = tokio::sync::oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let dns = Dns::with_reader(1, {
            let started = Mutex::new(Some(started));
            let gate = Mutex::new(gate);
            move || {
                started.lock().unwrap().take().unwrap().send(()).unwrap();
                gate.lock().unwrap().recv_timeout(BUDGET).unwrap();
                Ok(config.clone())
            }
        });
        let limits = Limits::default();
        pause();
        let ctx = RequestContext::new(Duration::from_secs(30));
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut lookup = Box::pin(dns.lookup("example.org", &limits, &ctx));
        assert!(futures_util::poll!(lookup.as_mut()).is_pending());
        resume();
        tokio::select! {
            result = entered => result.unwrap(),
            _ = lookup.as_mut() => panic!("Lookup finished before configuration read"),
            _ = tokio::time::sleep(BUDGET) => panic!("Configuration read did not start"),
        }
        pause();
        advance(Duration::from_secs(2)).await;
        resume();
        release.send(()).unwrap();
        let mut data = [0u8; 4096];
        tokio::select! {
            result = socket.recv_from(&mut data) => {
                let (length, _) = result.unwrap();
                assert!(length > 12);
            }
            _ = lookup.as_mut() => panic!("Lookup finished before DNS query"),
            _ = tokio::time::sleep(BUDGET) => panic!("DNS query did not arrive"),
        }
        pause();
        assert_eq!(dns.reads.available_permits(), 1);
        assert!(deadline - Instant::now() <= Duration::from_secs(1));
        advance(deadline - Instant::now() - Duration::from_millis(1)).await;
        assert!(futures_util::poll!(lookup.as_mut()).is_pending());
        if cancelled {
            ctx.cancellation.cancel();
        }
        advance(Duration::from_millis(2)).await;
        let error = lookup.await.unwrap_err();
        assert!(Instant::now() <= deadline + Duration::from_millis(2));
        assert_eq!(
            error.code,
            if cancelled {
                ErrorCode::Cancelled
            } else {
                ErrorCode::Timeout
            }
        );
        if !cancelled {
            assert_eq!(error.message, "DNS lookup deadline exceeded");
        }
        resume();
    }
}
