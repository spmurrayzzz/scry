use std::net::Ipv4Addr;

use hickory_resolver::config::{ConnectionConfig, NameServerConfig, ResolverConfig};
use tokio::net::UdpSocket;

use super::*;

pub(super) struct DnsFixture {
    pub(super) config: ResolverConfig,
    task: JoinHandle<Vec<(String, Ipv4Addr)>>,
}

impl DnsFixture {
    pub(super) async fn start(answers: &[(&str, Ipv4Addr)]) -> Self {
        let socket = timeout(BUDGET, UdpSocket::bind("127.0.0.1:0"))
            .await
            .unwrap()
            .unwrap();
        let address = socket.local_addr().unwrap();
        let mut connection = ConnectionConfig::udp();
        connection.port = address.port();
        let config = ResolverConfig::from_name_servers(vec![NameServerConfig::new(
            address.ip(),
            true,
            vec![connection],
        )]);
        let answers: Vec<_> = answers
            .iter()
            .map(|(host, ip)| ((*host).to_owned(), *ip))
            .collect();
        let task = tokio::spawn(async move {
            timeout(BUDGET, async move {
                let mut queries = Vec::new();
                for (host, ip) in answers {
                    let mut name = Vec::new();
                    for label in host.split('.') {
                        name.push(u8::try_from(label.len()).unwrap());
                        name.extend_from_slice(label.as_bytes());
                    }
                    name.push(0);
                    let mut seen = [false; 2];
                    for _ in 0..2 {
                        let mut data = [0u8; 4096];
                        let (length, peer) = socket.recv_from(&mut data).await.unwrap();
                        let type_offset = 12 + name.len();
                        let question_end = type_offset + 4;
                        assert!(length >= question_end);
                        assert_eq!(&data[4..6], &[0, 1]);
                        assert_eq!(&data[12..type_offset], name);
                        assert_eq!(&data[type_offset + 2..question_end], &[0, 1]);
                        let kind = u16::from_be_bytes([data[type_offset], data[type_offset + 1]]);
                        let index = match kind {
                            1 => 0,
                            28 => 1,
                            _ => panic!("Unexpected DNS query type: {kind}"),
                        };
                        assert!(!seen[index], "Duplicate DNS query type: {kind}");
                        seen[index] = true;
                        data[2] = 0x81;
                        data[3] = 0x80;
                        data[6..12].fill(0);
                        data[7] = u8::from(kind == 1);
                        let mut reply = data[..question_end].to_vec();
                        if kind == 1 {
                            reply.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
                            reply.extend_from_slice(&ip.octets());
                        }
                        socket.send_to(&reply, peer).await.unwrap();
                    }
                    assert_eq!(seen, [true, true]);
                    queries.push((host, ip));
                }
                queries
            })
            .await
            .expect("DNS responder exceeded its budget")
        });
        Self { config, task }
    }

    pub(super) async fn finish(&mut self) -> Vec<(String, Ipv4Addr)> {
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
            let mut dns = DnsFixture::start(&answers).await;
            let mut fetcher = fetcher(Limits {
                max_fetches: 1,
                ..Limits::default()
            });
            let config = dns.config.clone();
            fetcher.dns = dns::Dns::with_reader(1, move || Ok(config.clone()));
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
