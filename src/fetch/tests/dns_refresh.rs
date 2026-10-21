use std::net::Ipv4Addr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use hickory_resolver::config::ResolverConfig;

use super::provider_redirect_dns::DnsFixture;
use super::*;

const HOST: &str = "example.org";
const URL: &str = "https://example.org/page";
const FIRST: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 1);
const SECOND: Ipv4Addr = Ipv4Addr::new(8, 8, 8, 8);

async fn fetch_addresses(fetcher: &Fetcher, url: &str) -> Result<Vec<SocketAddr>, Error> {
    let mut sent = None;
    let result = timeout(
        BUDGET,
        fetcher.fetch_with(
            Url::parse(url).unwrap(),
            &DomainFilter::default(),
            None,
            &RequestContext::new(BUDGET),
            |_, addresses, _| {
                assert!(sent.is_none());
                sent = Some(addresses);
                std::future::ready(Ok(Response::from(
                    http::Response::builder()
                        .header(CONTENT_TYPE, "text/html")
                        .body("<p>local response</p>")
                        .unwrap(),
                )))
            },
        ),
    )
    .await
    .expect("Fetch exceeded its budget");
    match result {
        Ok(page) => {
            assert_eq!(page.html, "<p>local response</p>");
            Ok(sent.unwrap())
        }
        Err(error) => {
            assert!(sent.is_none(), "Rejected lookup reached HTTP send");
            Err(error)
        }
    }
}

#[tokio::test]
async fn long_lived_fetcher_and_clone_refresh_dns_for_the_same_hostname() {
    let mut first = DnsFixture::start(&[(HOST, FIRST)]).await;
    let mut second = DnsFixture::start(&[(HOST, SECOND), (HOST, SECOND)]).await;
    let config = Arc::new(Mutex::new(first.config.clone()));
    let reads = Arc::new(AtomicUsize::new(0));
    let mut fetcher = fetcher(Limits::default());
    fetcher.dns = dns::Dns::with_reader(2, {
        let config = config.clone();
        let reads = reads.clone();
        move || {
            reads.fetch_add(1, Ordering::SeqCst);
            Ok(config.lock().unwrap().clone())
        }
    });
    let clone = fetcher.clone();
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    assert_eq!(
        fetch_addresses(&fetcher, URL).await.unwrap(),
        [SocketAddr::new(FIRST.into(), 443)]
    );
    *config.lock().unwrap() = second.config.clone();
    for fetcher in [&clone, &fetcher] {
        assert_eq!(
            fetch_addresses(fetcher, URL).await.unwrap(),
            [SocketAddr::new(SECOND.into(), 443)]
        );
    }
    assert_eq!(reads.load(Ordering::SeqCst), 3);
    assert_eq!(first.finish().await, [(HOST.to_owned(), FIRST)]);
    assert_eq!(second.finish().await, vec![(HOST.to_owned(), SECOND); 2]);
}

#[tokio::test]
async fn invalid_dns_configuration_never_uses_stale_settings_and_recovers() {
    for failure in 0..3 {
        let mut first = DnsFixture::start(&[(HOST, FIRST)]).await;
        let mut second = DnsFixture::start(&[(HOST, SECOND)]).await;
        let config = Arc::new(Mutex::new(Ok(first.config.clone())));
        let mut fetcher = fetcher(Limits::default());
        fetcher.dns = dns::Dns::with_reader(1, {
            let config = config.clone();
            move || config.lock().unwrap().clone()
        });
        assert_eq!(
            fetch_addresses(&fetcher, URL).await.unwrap(),
            [SocketAddr::new(FIRST.into(), 443)]
        );
        let expected = if failure == 0 {
            Error::new(ErrorCode::DnsFailed, "Temporary configuration read failure")
        } else {
            Error::new(
                ErrorCode::DnsFailed,
                "System DNS configuration must contain between one and eight servers",
            )
        };
        *config.lock().unwrap() = match failure {
            0 => Err(expected.clone()),
            1 => Ok(ResolverConfig::from_name_servers(Vec::new())),
            _ => {
                let server = first.config.name_servers[0].clone();
                Ok(ResolverConfig::from_name_servers(vec![server; 9]))
            }
        };
        assert_eq!(fetch_addresses(&fetcher, URL).await.unwrap_err(), expected);
        *config.lock().unwrap() = Ok(second.config.clone());
        assert_eq!(
            fetch_addresses(&fetcher, URL).await.unwrap(),
            [SocketAddr::new(SECOND.into(), 443)]
        );
        assert_eq!(first.finish().await, [(HOST.to_owned(), FIRST)]);
        assert_eq!(second.finish().await, [(HOST.to_owned(), SECOND)]);
    }
}

#[tokio::test]
async fn late_old_configuration_read_cannot_replace_new_settings() {
    let mut first = DnsFixture::start(&[(HOST, FIRST)]).await;
    let mut second = DnsFixture::start(&[(HOST, SECOND), (HOST, SECOND)]).await;
    let (started, reading) = tokio::sync::oneshot::channel();
    let (release, gate) = std::sync::mpsc::channel();
    let reads = Arc::new(AtomicUsize::new(0));
    let mut fetcher = fetcher(Limits::default());
    fetcher.dns = dns::Dns::with_reader(2, {
        let first = first.config.clone();
        let second = second.config.clone();
        let started = Mutex::new(Some(started));
        let gate = Mutex::new(gate);
        let reads = reads.clone();
        move || {
            if reads.fetch_add(1, Ordering::SeqCst) == 0 {
                started.lock().unwrap().take().unwrap().send(()).unwrap();
                gate.lock().unwrap().recv_timeout(BUDGET).unwrap();
                Ok(first.clone())
            } else {
                Ok(second.clone())
            }
        }
    });
    let old = tokio::spawn({
        let fetcher = fetcher.clone();
        async move { fetch_addresses(&fetcher, URL).await }
    });
    timeout(BUDGET, reading).await.unwrap().unwrap();
    assert_eq!(
        fetch_addresses(&fetcher, URL).await.unwrap(),
        [SocketAddr::new(SECOND.into(), 443)]
    );
    release.send(()).unwrap();
    assert_eq!(
        timeout(BUDGET, old).await.unwrap().unwrap().unwrap(),
        [SocketAddr::new(FIRST.into(), 443)]
    );
    assert_eq!(
        fetch_addresses(&fetcher, URL).await.unwrap(),
        [SocketAddr::new(SECOND.into(), 443)]
    );
    assert_eq!(reads.load(Ordering::SeqCst), 3);
    assert_eq!(first.finish().await, [(HOST.to_owned(), FIRST)]);
    assert_eq!(second.finish().await, vec![(HOST.to_owned(), SECOND); 2]);
}

#[tokio::test]
async fn redirect_uses_changed_configuration_and_rejects_private_answers_before_send() {
    for provider in [None, Some(false)] {
        let host = "duckduckgo.com";
        let private = Ipv4Addr::new(10, 0, 0, 1);
        let mut first = DnsFixture::start(&[(host, FIRST)]).await;
        let mut second = DnsFixture::start(&[(host, private)]).await;
        let config = Arc::new(Mutex::new(first.config.clone()));
        let mut fetcher = fetcher(Limits::default());
        fetcher.dns = dns::Dns::with_reader(1, {
            let config = config.clone();
            move || Ok(config.lock().unwrap().clone())
        });
        let mut sends = 0;
        let initial = Url::parse("https://duckduckgo.com/?q=first").unwrap();
        let error = timeout(
            BUDGET,
            fetcher.fetch_with(
                initial.clone(),
                &DomainFilter::default(),
                provider,
                &RequestContext::new(BUDGET),
                |url, addresses, _| {
                    sends += 1;
                    assert_eq!(sends, 1, "Private redirect target reached HTTP send");
                    assert_eq!(url, initial);
                    assert_eq!(addresses, [SocketAddr::new(FIRST.into(), 443)]);
                    *config.lock().unwrap() = second.config.clone();
                    std::future::ready(Ok(Response::from(
                        http::Response::builder()
                            .status(302)
                            .header(LOCATION, "/?q=next")
                            .body("")
                            .unwrap(),
                    )))
                },
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::BlockedDestination);
        assert_eq!(sends, 1);
        assert_eq!(first.finish().await, [(host.to_owned(), FIRST)]);
        assert_eq!(second.finish().await, [(host.to_owned(), private)]);
    }
}

#[tokio::test]
async fn ip_literals_bypass_configuration_reads_but_keep_destination_checks() {
    let reads = Arc::new(AtomicUsize::new(0));
    let mut fetcher = fetcher(Limits::default());
    fetcher.dns = dns::Dns::with_reader(1, {
        let reads = reads.clone();
        move || {
            reads.fetch_add(1, Ordering::SeqCst);
            Err(Error::new(
                ErrorCode::DnsFailed,
                "Unexpected configuration read",
            ))
        }
    });
    for (url, expected) in [
        ("https://1.1.1.1/", "1.1.1.1:443"),
        (
            "http://[2606:4700:4700::1111]/",
            "[2606:4700:4700::1111]:80",
        ),
    ] {
        assert_eq!(
            fetch_addresses(&fetcher, url).await.unwrap(),
            [expected.parse::<SocketAddr>().unwrap()]
        );
    }
    for url in [
        "https://127.0.0.1/",
        "https://10.0.0.1/",
        "https://[::1]/",
        "https://[fc00::1]/",
    ] {
        assert_eq!(
            fetch_addresses(&fetcher, url).await.unwrap_err().code,
            ErrorCode::BlockedDestination
        );
        assert_eq!(
            fetcher
                .resolve(&Url::parse(url).unwrap(), &RequestContext::new(BUDGET))
                .await
                .unwrap_err()
                .code,
            ErrorCode::BlockedDestination
        );
    }
    assert_eq!(reads.load(Ordering::SeqCst), 0);
}
