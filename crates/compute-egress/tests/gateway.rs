//! The gateway against real TLS upstreams on this machine: what it lets
//! through, what it refuses, and that a brokered credential reaches the
//! upstream while the session never holds it.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use compute_egress::{
    CredentialBinding, EgressPolicy, Gateway, GatewayConfig, HostRule, is_public,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// An HTTPS upstream for `name` that answers with the `authorization` and
/// `host` it received.
async fn upstream(name: &str) -> (SocketAddr, CertificateDer<'static>) {
    let key = rcgen::KeyPair::generate().unwrap();
    let certificate = rcgen::CertificateParams::new(vec![name.to_owned()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let der = certificate.der().clone();
    let config = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![der.clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(stream).await else {
                    return;
                };
                let mut head = vec![];
                let mut byte = [0_u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if tls.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(byte[0]);
                }
                let text = String::from_utf8_lossy(&head).to_lowercase();
                let header = |name: &str| {
                    text.lines()
                        .find_map(|line| line.strip_prefix(&format!("{name}: ")))
                        .unwrap_or("none")
                        .trim()
                        .to_owned()
                };
                let body = format!("{}|{}", header("authorization"), header("host"));
                let _ = tls
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
                let _ = tls.shutdown().await;
            });
        }
    });
    (address, der)
}

/// Open a tunnel through the gateway; the response line, and the stream
/// when the tunnel is open.
async fn connect(gateway: SocketAddr, authority: &str) -> (String, Option<TcpStream>) {
    let mut stream = TcpStream::connect(gateway).await.unwrap();
    stream
        .write_all(format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut head = vec![];
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).await.unwrap() == 0 {
            break;
        }
        head.push(byte[0]);
    }
    let line = String::from_utf8_lossy(&head)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned();
    let open = line.contains(" 200 ");
    (line, open.then_some(stream))
}

/// One HTTPS request through an open tunnel, trusting `root`.
async fn get(
    tunnel: TcpStream,
    name: &str,
    root: CertificateDer<'static>,
    host_header: &str,
    authorization: Option<&str>,
) -> std::io::Result<String> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(root).unwrap();
    let config = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let mut tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from(name.to_owned()).unwrap(), tunnel)
        .await?;
    let mut request = format!("GET / HTTP/1.1\r\nHost: {host_header}\r\n");
    if let Some(value) = authorization {
        request.push_str(&format!("Authorization: {value}\r\n"));
    }
    request.push_str("\r\n");
    tls.write_all(request.as_bytes()).await?;
    let mut response = String::new();
    let _ = tls.read_to_string(&mut response).await;
    Ok(response)
}

fn rule(host: &str, port: u16) -> HostRule {
    HostRule {
        host: host.into(),
        ports: vec![port, 443],
    }
}

#[tokio::test]
async fn a_bound_credential_reaches_the_upstream_and_never_the_session() {
    let (api, api_cert) = upstream("api.test").await;
    let (open, open_cert) = upstream("open.test").await;
    let gateway = Gateway::start(
        "127.0.0.1:0".parse().unwrap(),
        GatewayConfig {
            policy: EgressPolicy {
                allow: vec![rule("api.test", 443), rule("open.test", open.port())],
                allow_private: true,
            },
            bindings: vec![CredentialBinding {
                host: "api.test".into(),
                header: "authorization".into(),
                value: "Bearer s3cret".into(),
            }],
            resolve: BTreeMap::from([
                ("api.test".to_owned(), vec![api]),
                ("open.test".to_owned(), vec![open]),
            ]),
            upstream_roots: Some(vec![api_cert, open_cert.clone()]),
        },
    )
    .await
    .unwrap();
    assert!(gateway.ca_pem().contains("BEGIN CERTIFICATE"));
    assert!(!format!("{:?}", GatewayConfig::default()).contains("s3cret"));

    // The session trusts only the gateway's CA for the bound host, sends a
    // forged value, and the upstream sees the bound one.
    let (line, tunnel) = connect(gateway.address(), "api.test:443").await;
    let response = get(
        tunnel.unwrap_or_else(|| panic!("{line}")),
        "api.test",
        gateway.ca_der(),
        "api.test",
        Some("Bearer forged"),
    )
    .await
    .unwrap();
    assert!(response.ends_with("bearer s3cret|api.test"), "{response}");

    // An unbound allowed host is tunnelled untouched: end-to-end TLS with
    // the upstream's own certificate, and nothing added.
    let (line, tunnel) = connect(gateway.address(), &format!("open.test:{}", open.port())).await;
    let response = get(
        tunnel.unwrap_or_else(|| panic!("{line}")),
        "open.test",
        open_cert,
        "open.test",
        None,
    )
    .await
    .unwrap();
    assert!(response.ends_with("none|open.test"), "{response}");

    // A request in the bound tunnel that names another host is refused.
    let (_, tunnel) = connect(gateway.address(), "api.test:443").await;
    let response = get(
        tunnel.unwrap(),
        "api.test",
        gateway.ca_der(),
        "evil.test",
        None,
    )
    .await
    .unwrap();
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");

    // The gateway's CA cannot vouch for anything but the bound hosts.
    let (_, tunnel) = connect(gateway.address(), &format!("open.test:{}", open.port())).await;
    assert!(
        get(
            tunnel.unwrap(),
            "open.test",
            gateway.ca_der(),
            "open.test",
            None
        )
        .await
        .is_err(),
        "an unbound host is never terminated by the gateway"
    );

    let decisions = gateway.decisions();
    assert!(decisions.iter().any(|d| d.host == "api.test" && d.brokered));
    assert!(
        decisions
            .iter()
            .any(|d| d.host == "open.test" && d.allowed && !d.brokered)
    );
    assert!(
        decisions
            .iter()
            .any(|d| !d.allowed && d.reason.contains("another host"))
    );
}

#[tokio::test]
async fn destinations_outside_the_policy_or_on_private_addresses_are_refused() {
    let (api, _) = upstream("api.test").await;
    let gateway = Gateway::start(
        "127.0.0.1:0".parse().unwrap(),
        GatewayConfig {
            policy: EgressPolicy {
                allow: vec![rule("api.test", api.port()), rule("*.example.com", 443)],
                allow_private: false,
            },
            resolve: BTreeMap::from([("api.test".to_owned(), vec![api])]),
            ..GatewayConfig::default()
        },
    )
    .await
    .unwrap();
    let (line, tunnel) = connect(gateway.address(), "elsewhere.test:443").await;
    assert!(line.contains("403") && tunnel.is_none(), "{line}");
    // Allowed by name, but it resolves to this machine.
    let (line, _) = connect(gateway.address(), &format!("api.test:{}", api.port())).await;
    assert!(line.contains("403"), "{line}");
    // An address literal is a host like any other: not in the policy.
    let (line, _) = connect(gateway.address(), "127.0.0.1:22").await;
    assert!(line.contains("403"), "{line}");
    // The apex is not a subdomain.
    let (line, _) = connect(gateway.address(), "example.com:443").await;
    assert!(line.contains("403"), "{line}");
    let decisions = gateway.decisions();
    assert_eq!(decisions.len(), 4);
    assert!(decisions.iter().all(|decision| !decision.allowed));
    assert!(
        decisions[1].reason.contains("not a public address"),
        "{:?}",
        decisions[1]
    );
}

#[tokio::test]
async fn a_credential_cannot_be_bound_to_a_host_the_policy_does_not_allow() {
    let refused = Gateway::start(
        "127.0.0.1:0".parse().unwrap(),
        GatewayConfig {
            bindings: vec![CredentialBinding {
                host: "api.test".into(),
                header: "authorization".into(),
                value: "x".into(),
            }],
            ..GatewayConfig::default()
        },
    )
    .await;
    assert!(refused.is_err());
}

#[test]
fn only_public_addresses_are_public() {
    for private in [
        "127.0.0.1",
        "10.1.2.3",
        "172.16.0.1",
        "192.168.1.1",
        "169.254.169.254",
        "100.64.0.1",
        "0.0.0.0",
        "::1",
        "fd00::1",
        "fe80::1",
        "::ffff:127.0.0.1",
    ] {
        assert!(!is_public(private.parse().unwrap()), "{private}");
    }
    for public in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
        assert!(is_public(public.parse().unwrap()), "{public}");
    }
}
