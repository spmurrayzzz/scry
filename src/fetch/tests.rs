use std::io;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use super::*;

const BUDGET: Duration = Duration::from_secs(3);

struct WireServer {
    address: SocketAddr,
    task: JoinHandle<io::Result<String>>,
}

impl WireServer {
    async fn start(status: &str, headers: &str, chunked: bool, chunks: &[&[u8]]) -> Self {
        let listener = timeout(BUDGET, TcpListener::bind(("127.0.0.1", 0)))
            .await
            .unwrap()
            .unwrap();
        let address = listener.local_addr().unwrap();
        let transfer = if chunked {
            "Transfer-Encoding: chunked\r\n"
        } else {
            ""
        };
        let mut wire = vec![
            format!("HTTP/1.1 {status}\r\nConnection: close\r\n{transfer}{headers}\r\n")
                .into_bytes(),
        ];
        for chunk in chunks {
            assert!(!chunk.is_empty());
            if chunked {
                wire.push(format!("{:x}\r\n", chunk.len()).into_bytes());
                wire.push(chunk.to_vec());
                wire.push(b"\r\n".to_vec());
            } else {
                wire.push(chunk.to_vec());
            }
        }
        if chunked {
            wire.push(b"0\r\n\r\n".to_vec());
        }
        let task = tokio::spawn(async move {
            timeout(BUDGET, async move {
                let (mut socket, _) = listener.accept().await?;
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                    let count = socket.read(&mut buffer).await?;
                    if count == 0 || request.len() + count > 8192 {
                        return Err(io::Error::other("Missing or oversized request headers"));
                    }
                    request.extend_from_slice(&buffer[..count]);
                }
                for part in wire {
                    socket.write_all(&part).await?;
                }
                socket.shutdown().await?;
                String::from_utf8(request).map_err(io::Error::other)
            })
            .await
            .map_err(io::Error::other)?
        });
        Self { address, task }
    }

    async fn finish(&mut self) -> String {
        timeout(BUDGET, &mut self.task)
            .await
            .expect("HTTP responder exceeded its budget")
            .expect("HTTP responder panicked")
            .expect("HTTP responder failed")
    }
}

impl Drop for WireServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn fetcher(limits: Limits) -> Fetcher {
    let mut options = ResolverOpts::default();
    options.use_hosts_file = ResolveHosts::Never;
    let resolver = Resolver::builder_with_config(
        ResolverConfig::from_name_servers(Vec::new()),
        TokioRuntimeProvider::default(),
    )
    .with_options(options)
    .build()
    .unwrap();
    Fetcher {
        semaphore: Arc::new(Semaphore::new(limits.max_fetches)),
        limits,
        resolver,
    }
}

async fn response(
    fetcher: &Fetcher,
    status: &str,
    headers: &str,
    chunked: bool,
    chunks: &[&[u8]],
) -> (Response, WireServer, RequestContext) {
    let server = WireServer::start(status, headers, chunked, chunks).await;
    let ctx = RequestContext::new(BUDGET);
    let url = Url::parse(&format!(
        "http://pinned.invalid:{}/page",
        server.address.port()
    ))
    .unwrap();
    let client = fetcher.client(&url, vec![server.address], &ctx).unwrap();
    let response = timeout(BUDGET, client.get(url).send())
        .await
        .expect("HTTP request exceeded its budget")
        .unwrap();
    (response, server, ctx)
}

async fn body_decoder(
    limits: Limits,
    headers: &str,
    chunked: bool,
    chunks: &[&[u8]],
) -> Result<String, Error> {
    let fetcher = fetcher(limits);
    let (response, mut server, ctx) = response(&fetcher, "200 OK", headers, chunked, chunks).await;
    assert_eq!(response.content_length(), None);
    let result = match response_encoding(&response) {
        Ok(encoding) => timeout(BUDGET, fetcher.read_html(response, encoding, &ctx))
            .await
            .expect("Body decoder exceeded its budget"),
        Err(error) => Err(error),
    };
    server.finish().await;
    result
}

#[tokio::test]
async fn body_decoder_bounds_chunked_and_close_delimited_downloads() {
    for chunked in [true, false] {
        let limits = Limits {
            max_download_bytes: 5,
            ..Limits::default()
        };
        assert_eq!(
            body_decoder(
                limits.clone(),
                "Content-Type: text/html\r\n",
                chunked,
                &[b"abc", b"de"],
            )
            .await
            .unwrap(),
            "abcde"
        );
        let error = body_decoder(
            limits,
            "Content-Type: text/html\r\n",
            chunked,
            &[b"abc", b"def"],
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::TooLarge);
        assert_eq!(error.message, "Downloaded HTML exceeds the byte limit");
    }
}

#[tokio::test]
async fn body_decoder_bounds_decoded_utf8_bytes_at_exact_boundary() {
    for (limit, expected) in [(6, Ok("€€")), (5, Err(ErrorCode::TooLarge))] {
        let result = body_decoder(
            Limits {
                max_download_bytes: 2,
                max_decoded_bytes: limit,
                ..Limits::default()
            },
            "Content-Type: text/html; charset=windows-1252\r\n",
            true,
            &[b"\x80", b"\x80"],
        )
        .await;
        match expected {
            Ok(text) => assert_eq!(result.unwrap(), text),
            Err(code) => {
                let error = result.unwrap_err();
                assert_eq!(error.code, code);
                assert_eq!(error.message, "Decoded HTML exceeds the byte limit");
            }
        }
    }
}

#[tokio::test]
async fn body_decoder_charset_precedence_is_bom_then_header_then_meta() {
    for (headers, chunks, expected) in [
        (
            "Content-Type: text/html; charset=windows-1252\r\n",
            vec![
                b"\xef".as_slice(),
                b"\xbb\xbf<meta charset=windows-1252>\xe2\x82\xac".as_slice(),
            ],
            "<meta charset=windows-1252>€",
        ),
        (
            "Content-Type: text/html; charset=windows-1252\r\n",
            vec![b"<meta charset=utf-8>\x80".as_slice()],
            "<meta charset=utf-8>€",
        ),
        (
            "Content-Type: text/html\r\n",
            vec![
                b"<meta char".as_slice(),
                b"set=windows-1252>\x80".as_slice(),
            ],
            "<meta charset=windows-1252>€",
        ),
        (
            "Content-Type: application/xhtml+xml\r\n",
            vec![
                b"<meta http-equiv=content-type content='text/html; charset=windows-1252'>\x80"
                    .as_slice(),
            ],
            "<meta http-equiv=content-type content='text/html; charset=windows-1252'>€",
        ),
    ] {
        assert_eq!(
            body_decoder(Limits::default(), headers, true, &chunks)
                .await
                .unwrap(),
            expected
        );
    }
}

#[tokio::test]
async fn body_decoder_keeps_charset_state_across_chunks_after_sniff_prefix() {
    let mut prefix = vec![b'a'; 1023];
    prefix.push(0xe2);
    assert_eq!(
        body_decoder(
            Limits::default(),
            "Content-Type: text/html; charset=utf-8\r\n",
            true,
            &[&prefix, b"\x82", b"\xac!"],
        )
        .await
        .unwrap(),
        format!("{}€!", "a".repeat(1023))
    );
    let prefix = vec![b'a'; 1024];
    assert_eq!(
        body_decoder(
            Limits::default(),
            "Content-Type: text/html; charset=windows-1252\r\n",
            true,
            &[&prefix, b"\x80", b"\x80!"],
        )
        .await
        .unwrap(),
        format!("{}€€!", "a".repeat(1024))
    );
}

#[tokio::test]
async fn body_decoder_rejects_unsupported_mime_charset_and_content_encoding() {
    for headers in [
        "",
        "Content-Type: text/plain\r\n",
        "Content-Type: application/pdf\r\n",
        "Content-Type: text/html; charset=not-a-charset\r\n",
        "Content-Type: text/html; charset=replacement\r\n",
        "Content-Type: text/html; charset=utf-8; charset=windows-1252\r\n",
        "Content-Type: text/html\r\nContent-Type: application/xhtml+xml\r\n",
        "Content-Type: text/html\r\nContent-Encoding: gzip\r\n",
        "Content-Type: text/html\r\nContent-Encoding: identity, br\r\n",
        "Content-Type: text/html\r\nContent-Encoding: identity\r\nContent-Encoding: deflate\r\n",
    ] {
        let error = body_decoder(Limits::default(), headers, false, &[b"<p>x</p>"])
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::UnsupportedContent, "{headers:?}");
    }
    assert_eq!(
        body_decoder(
            Limits::default(),
            "Content-Type: application/xhtml+xml; charset=\"utf-8\"\r\nContent-Encoding: Identity\r\n",
            false,
            &[b"<p>x</p>"],
        )
        .await
        .unwrap(),
        "<p>x</p>"
    );
}

#[tokio::test]
async fn http_status_helper_maps_errors_and_preserves_retry_after() {
    let fetcher = fetcher(Limits::default());
    for (status, code, retry_after, expected_delay) in [
        ("202 Accepted", ErrorCode::UpstreamBlocked, "17", None),
        ("401 Unauthorized", ErrorCode::UpstreamBlocked, "17", None),
        ("403 Forbidden", ErrorCode::UpstreamBlocked, "17", None),
        (
            "429 Too Many Requests",
            ErrorCode::RateLimited,
            "17",
            Some(17),
        ),
        (
            "503 Service Unavailable",
            ErrorCode::UpstreamFailed,
            "23",
            Some(23),
        ),
        (
            "503 Service Unavailable",
            ErrorCode::UpstreamFailed,
            "Sun, 06 Nov 1994 08:49:37 GMT",
            Some(0),
        ),
        (
            "429 Too Many Requests",
            ErrorCode::RateLimited,
            "invalid",
            None,
        ),
        (
            "500 Internal Server Error",
            ErrorCode::UpstreamFailed,
            "17",
            None,
        ),
    ] {
        let (response, mut server, _) = response(
            &fetcher,
            status,
            &format!("Retry-After: {retry_after}\r\n"),
            false,
            &[],
        )
        .await;
        let error = check_status(&response).unwrap_err();
        assert_eq!(error.code, code, "{status}");
        assert_eq!(error.retry_after_secs, expected_delay, "{status}");
        server.finish().await;
    }
}

#[tokio::test]
async fn connector_frozen_dns_pins_socket_and_preserves_host_without_credentials() {
    let fetcher = fetcher(Limits::default());
    let (response, mut server, _) = response(&fetcher, "200 OK", "", false, &[]).await;
    assert_eq!(response.remote_addr(), Some(server.address));
    check_status(&response).unwrap();
    let request = server.finish().await;
    let mut lines = request.split("\r\n");
    assert_eq!(lines.next(), Some("GET /page HTTP/1.1"));
    let headers: Vec<_> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim()))
        .collect();
    let hosts: Vec<_> = headers
        .iter()
        .filter(|(name, _)| name == "host")
        .map(|(_, value)| *value)
        .collect();
    assert_eq!(hosts, [format!("pinned.invalid:{}", server.address.port())]);
    for forbidden in ["cookie", "authorization", "proxy-authorization", "referer"] {
        assert!(headers.iter().all(|(name, _)| name != forbidden));
    }
}

#[tokio::test]
async fn connector_frozen_dns_rejects_unvalidated_hostname_without_fallback() {
    let listener = timeout(BUDGET, TcpListener::bind(("127.0.0.1", 0)))
        .await
        .unwrap()
        .unwrap();
    let address = listener.local_addr().unwrap();
    let fetcher = fetcher(Limits::default());
    let ctx = RequestContext::new(BUDGET);
    let validated = Url::parse(&format!("http://pinned.invalid:{}/", address.port())).unwrap();
    let client = fetcher.client(&validated, vec![address], &ctx).unwrap();
    let unexpected = format!("http://unexpected.invalid:{}/", address.port());
    let error = timeout(BUDGET, client.get(unexpected).send())
        .await
        .expect("Connector rejection exceeded its budget")
        .unwrap_err();
    assert!(error.is_connect());
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    let mut rejected_name = false;
    while let Some(error) = source {
        rejected_name |= error
            .to_string()
            .contains("Unvalidated DNS name is not allowed");
        source = error.source();
    }
    assert!(rejected_name, "{error:?}");
    assert!(
        timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
}
