use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use rustls::ServerConfig;
use rustls::pki_types::PrivatePkcs8KeyDer;
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_rustls::LazyConfigAcceptor;
use url::Url;

use crate::config::Limits;
use crate::context::RequestContext;

fn is_certificate_rejection(error: &(dyn Error + 'static)) -> bool {
    matches!(
        error.downcast_ref::<rustls::Error>(),
        Some(rustls::Error::InvalidCertificate(_))
    ) || error
        .downcast_ref::<std::io::Error>()
        .and_then(std::io::Error::get_ref)
        .is_some_and(|inner| is_certificate_rejection(inner))
        || error.source().is_some_and(is_certificate_rejection)
}

#[tokio::test]
async fn provider_tls_rejects_untrusted_certificate_and_preserves_pinned_hostname() {
    timeout(Duration::from_secs(5), async {
        for (hostname, path) in [
            ("duckduckgo.com", "/?q=test"),
            ("links.duckduckgo.com", "/d.js?o=json"),
        ] {
            let certificate =
                rcgen::generate_simple_self_signed(vec![hostname.to_owned()]).unwrap();
            let config = ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::aws_lc_rs::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![certificate.cert.der().clone()],
                PrivatePkcs8KeyDer::from(certificate.signing_key.serialize_der()).into(),
            )
            .unwrap();
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let url = Url::parse(&format!("https://{hostname}{path}")).unwrap();
            let ctx = RequestContext::new(Duration::from_secs(3));
            let client = super::fetcher(Limits::default())
                .client(&url, vec![listener.local_addr().unwrap()], &ctx)
                .unwrap();
            let server = async {
                let (socket, _) = listener.accept().await.unwrap();
                let handshake =
                    LazyConfigAcceptor::new(rustls::server::Acceptor::default(), socket)
                        .await
                        .unwrap();
                assert_eq!(handshake.client_hello().server_name(), Some(hostname));
                assert!(
                    handshake.into_stream(Arc::new(config)).await.is_err(),
                    "Untrusted TLS handshake was accepted"
                );
            };
            let (response, ()) = tokio::join!(client.get(url.as_str()).send(), server);
            let error = response.expect_err("Provider accepted a self-signed certificate");
            assert!(error.is_connect(), "{error:?}");
            assert!(!error.is_timeout(), "{error:?}");
            assert!(
                is_certificate_rejection(&error),
                "Expected certificate rejection: {error:?}"
            );
        }
    })
    .await
    .expect("Loopback TLS test exceeded its budget");
}
