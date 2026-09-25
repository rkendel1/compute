//! Ingress: routing by host and by SNI, ACME HTTP-01 answers, redirects,
//! and TLS termination.

use std::collections::BTreeMap;
use std::sync::Arc;

use compute_network::acme::ChallengeResponder;
use compute_network::{Ingress, IngressRoute};
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A plain HTTP backend answering `name`.
async fn backend(name: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buffer = [0u8; 2048];
                let _ = stream.read(&mut buffer).await;
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{name}",
                    name.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
            });
        }
    });
    port
}

async fn http(port: u16, host: &str, path: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

fn self_signed(name: &str) -> (String, String) {
    let certified = rcgen::generate_simple_self_signed(vec![name.to_string()]).unwrap();
    (certified.cert.pem(), certified.signing_key.serialize_pem())
}

async fn https(port: u16, name: &str, root_pem: &str) -> std::io::Result<String> {
    let mut roots = rustls::RootCertStore::empty();
    for certificate in compute_network::tls::certificates(root_pem.as_bytes()).unwrap() {
        roots.add(certificate).unwrap();
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let stream = TcpStream::connect(("127.0.0.1", port)).await?;
    let mut tls = connector
        .connect(ServerName::try_from(name.to_string()).unwrap(), stream)
        .await?;
    tls.write_all(
        format!("GET / HTTP/1.1\r\nHost: {name}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await?;
    let mut response = String::new();
    let _ = tls.read_to_string(&mut response).await;
    Ok(response)
}

#[tokio::test]
async fn hosts_route_only_where_their_domain_says() {
    let alpha = backend("alpha").await;
    let beta = backend("beta").await;
    let ingress = Ingress::new();
    let http_port = ingress
        .listen_http("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap()
        .port();
    let https_port = ingress
        .listen_https("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap()
        .port();
    ingress.set_routes(BTreeMap::from([
        (
            "alpha.example.com".to_string(),
            IngressRoute {
                endpoint_port: alpha,
            },
        ),
        (
            "Beta.Example.com".to_string(),
            IngressRoute {
                endpoint_port: beta,
            },
        ),
    ]));
    assert!(
        http(http_port, "alpha.example.com", "/")
            .await
            .ends_with("alpha")
    );
    assert!(
        http(http_port, "BETA.example.com:80", "/")
            .await
            .ends_with("beta")
    );
    let unknown = http(http_port, "gamma.example.com", "/").await;
    assert!(unknown.starts_with("HTTP/1.1 404"), "{unknown}");

    // ACME HTTP-01: answered for any host while outstanding.
    ingress.present("token-1", "token-1.thumbprint");
    let answer = http(
        http_port,
        "gamma.example.com",
        "/.well-known/acme-challenge/token-1",
    )
    .await;
    assert!(
        answer.starts_with("HTTP/1.1 200") && answer.ends_with("token-1.thumbprint"),
        "{answer}"
    );
    ingress.clear("token-1");
    let gone = http(
        http_port,
        "gamma.example.com",
        "/.well-known/acme-challenge/token-1",
    )
    .await;
    assert!(gone.starts_with("HTTP/1.1 404"), "{gone}");

    // With a certificate, HTTP redirects and HTTPS terminates TLS by SNI.
    let (chain, key) = self_signed("alpha.example.com");
    ingress
        .set_certificate("alpha.example.com", chain.as_bytes(), key.as_bytes())
        .unwrap();
    let redirect = http(http_port, "alpha.example.com", "/login?next=1").await;
    assert!(redirect.starts_with("HTTP/1.1 308"), "{redirect}");
    assert!(
        redirect.contains("location: https://alpha.example.com/login?next=1"),
        "{redirect}"
    );
    assert!(
        http(http_port, "beta.example.com", "/")
            .await
            .ends_with("beta")
    );
    let secure = https(https_port, "alpha.example.com", &chain)
        .await
        .unwrap();
    assert!(secure.ends_with("alpha"), "{secure}");
    // No certificate for the name: no handshake.
    assert!(https(https_port, "beta.example.com", &chain).await.is_err());
    assert_eq!(
        ingress.certificate("alpha.example.com").unwrap(),
        compute_network::tls::fingerprint(
            CertificateDer::from(
                compute_network::tls::certificates(chain.as_bytes()).unwrap()[0].to_vec()
            )
            .as_ref()
        )
    );
    let info = compute_network::tls::info(chain.as_bytes()).unwrap();
    assert_eq!(info.names, vec!["alpha.example.com".to_string()]);
    assert!(info.not_after > info.not_before);
}
