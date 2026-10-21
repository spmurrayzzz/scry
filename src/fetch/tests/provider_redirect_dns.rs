use std::net::Ipv4Addr;

use hickory_resolver::config::{ConnectionConfig, NameServerConfig};
use tokio::net::UdpSocket;

use super::*;

struct DnsFixture {
    resolver: TokioResolver,
    task: JoinHandle<Vec<(String, Ipv4Addr)>>,
}

impl DnsFixture {
    async fn start(answers: [(&str, Ipv4Addr); 2]) -> Self {
        let socket = timeout(BUDGET, UdpSocket::bind("127.0.0.1:0"))
            .await
            .unwrap()
            .unwrap();
        let address = socket.local_addr().unwrap();
        let mut connection = ConnectionConfig::udp();
        connection.port = address.port();
        let mut options = ResolverOpts::default();
        options.ip_strategy = LookupIpStrategy::Ipv4Only;
        options.use_hosts_file = ResolveHosts::Never;
        options.cache_size = 0;
        options.positive_max_ttl = Some(Duration::ZERO);
        options.negative_max_ttl = Some(Duration::ZERO);
        options.attempts = 0;
        options.timeout = BUDGET;
        let resolver = Resolver::builder_with_config(
            ResolverConfig::from_name_servers(vec![NameServerConfig::new(
                address.ip(),
                true,
                vec![connection],
            )]),
            TokioRuntimeProvider::default(),
        )
        .with_options(options)
        .build()
        .unwrap();
        let answers = answers.map(|(host, ip)| (host.to_owned(), ip));
        let task = tokio::spawn(async move {
            timeout(BUDGET, async move {
                let mut queries = Vec::new();
                for (host, ip) in answers {
                    let mut data = [0u8; 4096];
                    let (length, peer) = socket.recv_from(&mut data).await.unwrap();
                    assert!(length >= 12);
                    assert_eq!(&data[4..6], &[0, 1]);
                    let mut question = Vec::new();
                    for label in host.split('.') {
                        question.push(u8::try_from(label.len()).unwrap());
                        question.extend_from_slice(label.as_bytes());
                    }
                    question.extend_from_slice(&[0, 0, 1, 0, 1]);
                    let question_end = 12 + question.len();
                    assert!(length >= question_end);
                    assert_eq!(&data[12..question_end], question);
                    data[2] = 0x81;
                    data[3] = 0x80;
                    data[6..12].copy_from_slice(&[0, 1, 0, 0, 0, 0]);
                    let mut reply = data[..question_end].to_vec();
                    reply.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
                    reply.extend_from_slice(&ip.octets());
                    socket.send_to(&reply, peer).await.unwrap();
                    queries.push((host, ip));
                }
                queries
            })
            .await
            .expect("DNS responder exceeded its budget")
        });
        Self { resolver, task }
    }

    async fn finish(&mut self) -> Vec<(String, Ipv4Addr)> {
        timeout(BUDGET, &mut self.task)
            .await
            .expect("DNS responder did not finish")
            .expect("DNS responder panicked")
    }
}

impl Drop for DnsFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn assert_redirect_dns_rejected(initial: &str, location: &str, script: bool) {
    let initial = Url::parse(initial).unwrap();
    let target = initial.join(location).unwrap();
    let public = Ipv4Addr::new(1, 1, 1, 1);
    for status in [301, 302] {
        for blocked in [Ipv4Addr::LOCALHOST, Ipv4Addr::new(10, 0, 0, 1)] {
            let answers = [
                (initial.host_str().unwrap(), public),
                (target.host_str().unwrap(), blocked),
            ];
            let mut dns = DnsFixture::start(answers).await;
            let mut fetcher = fetcher(Limits {
                max_fetches: 1,
                ..Limits::default()
            });
            fetcher.resolver = dns.resolver.clone();
            let mut sends = Vec::new();
            let error = timeout(
                BUDGET,
                fetcher.fetch_with(
                    initial.clone(),
                    &DomainFilter::default(),
                    Some(script),
                    &RequestContext::new(BUDGET),
                    |url: Url, addresses: Vec<SocketAddr>, ctx: RequestContext| {
                        ctx.check().unwrap();
                        sends.push(url.clone());
                        assert_eq!(sends.len(), 1, "Redirect target reached network send");
                        assert_eq!(url, initial);
                        assert_eq!(addresses, [SocketAddr::new(public.into(), 443)]);
                        assert_eq!(fetcher.semaphore.available_permits(), 0);
                        std::future::ready(Ok(Response::from(
                            http::Response::builder()
                                .status(status)
                                .header(LOCATION, location)
                                .body("")
                                .unwrap(),
                        )))
                    },
                ),
            )
            .await
            .expect("Redirect DNS validation exceeded its budget")
            .unwrap_err();
            assert_eq!(error.code, ErrorCode::BlockedDestination);
            assert_eq!(
                error.message,
                "Destination is not an allowed public address"
            );
            assert_eq!(sends.as_slice(), std::slice::from_ref(&initial));
            assert_eq!(fetcher.semaphore.available_permits(), 1);
            let permit = fetcher.semaphore.try_acquire().unwrap();
            drop(permit);
            assert_eq!(
                dns.finish().await,
                answers.map(|(host, ip)| (host.to_owned(), ip))
            );
        }
    }
}

#[tokio::test]
async fn provider_redirect_revalidates_dns_across_trusted_hostnames_before_network_send() {
    for (initial, target) in [
        (
            "https://duckduckgo.com/d.js?o=json",
            "https://links.duckduckgo.com/d.js?o=json&v=next",
        ),
        (
            "https://links.duckduckgo.com/d.js?o=json",
            "https://duckduckgo.com/d.js?o=json&v=next",
        ),
    ] {
        assert_redirect_dns_rejected(initial, target, true).await;
    }
}

#[tokio::test]
async fn provider_redirect_revalidates_dns_on_same_hostname_rebinding_before_network_send() {
    for (initial, target, script) in [
        ("https://duckduckgo.com/?q=first", "/?q=next", false),
        (
            "https://links.duckduckgo.com/d.js?o=json",
            "/d.js?o=json&v=next",
            true,
        ),
    ] {
        assert_redirect_dns_rejected(initial, target, script).await;
    }
}
