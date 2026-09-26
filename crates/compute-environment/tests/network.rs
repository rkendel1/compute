//! Network certification: domains route only within their environment,
//! DNS is reconciled and repaired at its provider, and certificates are
//! issued by ACME (Pebble), served by SNI, and renewed — with keys and
//! credentials kept out of control state.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use compute_core::{
    IsolationRequirement, NetworkPolicy, ResourceLimits, RuntimeKind, WORKLOAD_SPEC_VERSION,
    WorkloadBundle, WorkloadSpec,
};
use compute_environment::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn bundle(source: &str) -> Vec<u8> {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("main.py"), source).unwrap();
    let spec = WorkloadSpec {
        version: WORKLOAD_SPEC_VERSION.into(),
        runtime: RuntimeKind::Python,
        runtime_version: None,
        architecture: None,
        entrypoint: "main.py".into(),
        args: vec![],
        env: BTreeMap::new(),
        inputs: vec![],
        outputs: vec![],
        resources: ResourceLimits::default(),
        network: NetworkPolicy::Network,
        isolation: IsolationRequirement::default(),
        dependencies: None,
    };
    WorkloadBundle::create_from(spec, root.path())
        .unwrap()
        .to_bytes()
        .unwrap()
}

/// Answers `<environment> <revision>`.
fn site(revision: &str) -> RevisionDefinition {
    let source = format!(
        r#"import http.server, os
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = (os.environ["ENVIRONMENT_NAME"] + " {revision}").encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *args):
        pass
http.server.ThreadingHTTPServer(("127.0.0.1", int(os.environ["PORT"])), H).serve_forever()
"#
    );
    RevisionDefinition {
        revision: revision.into(),
        source: None,
        workloads: vec![WorkloadDefinition {
            name: "web".into(),
            kind: WorkloadKind::Service,
            bundle: bundle(&source),
            ports: vec![PortSpec {
                name: "http".into(),
                port: 8080,
            }],
            restart: RestartPolicy::Never,
            desired_state: DesiredState::Running,
            readiness: None,
        }],
    }
}

async fn python() -> bool {
    let available = compute_runtime::Compute::new()
        .runtime(RuntimeKind::Python, None)
        .await
        .is_ok_and(|runtime| runtime.available);
    if !available {
        eprintln!("skipping: python is unavailable on this host");
    }
    available
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn port_windows(config: &mut DaemonConfig) {
    static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    let window = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst) * 100;
    config.port_range = (22000 + window, 22099 + window);
    config.instance_port_range = (42000 + window, 42099 + window);
}

fn daemon_config(node: &Path, network: NetworkConfig) -> DaemonConfig {
    let store = Arc::new(compute_state_memory::MemoryState::new());
    let artifacts = Arc::new(compute_state::StateArtifacts::new(
        compute_state::ControlState::new(store.clone()),
    ));
    let mut config = DaemonConfig::new(node, store, artifacts);
    config.provider = std::sync::Arc::new(common::provider());
    config.reconcile_interval = Duration::from_millis(200);
    config.network = network;
    port_windows(&mut config);
    config
}

async fn environment(daemon: &Arc<Daemon>, name: &str) {
    daemon
        .create_environment(EnvironmentDefinition {
            name: name.into(),
            desired_state: DesiredState::Running,
            env: BTreeMap::from([("ENVIRONMENT_NAME".to_string(), name.to_string())]),
            policy: None,
            provider: None,
        })
        .await
        .unwrap();
}

async fn released(daemon: &Arc<Daemon>, environment: &str, revision: &str) {
    daemon
        .register_revision("site", site(revision))
        .await
        .unwrap();
    let started = daemon
        .deploy(DeployRequest {
            project: "site".into(),
            environment: environment.into(),
            revision: Some(revision.into()),
            config: None,
            desired_state: None,
            placement: None,
            ..DeployRequest::default()
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let view = daemon.deployment(&started.deployment_id).await.unwrap();
        if view.record.status == DeploymentStatus::Complete {
            return;
        }
        assert!(
            !view.record.status.is_terminal() && tokio::time::Instant::now() < deadline,
            "{:?}",
            view.record
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn eventually<F, Fut>(what: &str, seconds: u64, mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    while !condition().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn http(port: u16, host: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    stream
        .write_all(
            format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

fn domain(name: &str, environment: &str) -> DomainDefinition {
    DomainDefinition {
        name: name.into(),
        environment: environment.into(),
        project: "site".into(),
        workload: None,
        port: None,
        dns_provider: None,
        tls: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn domains_route_within_their_environment_and_dns_is_reconciled() {
    if !python().await {
        return;
    }
    let node = tempfile::tempdir().unwrap();
    let zone = node.path().join("zone.json");
    let http_port = free_port();
    let network = NetworkConfig {
        ingress_http: Some(format!("127.0.0.1:{http_port}").parse().unwrap()),
        public_ipv4: Some("192.0.2.10".into()),
        dns: BTreeMap::from([
            (
                "local".to_string(),
                DnsProviderConfig::File {
                    zone: "example.com".into(),
                    path: zone.clone(),
                },
            ),
            (
                "hetzner".to_string(),
                DnsProviderConfig::Hetzner {
                    zone: "example.org".into(),
                    token_env: "COMPUTE_TEST_UNSET_HETZNER_TOKEN".into(),
                    api_url: None,
                },
            ),
        ]),
        dns_interval: Duration::from_secs(1),
        ..NetworkConfig::default()
    };
    let daemon = Daemon::start(daemon_config(&node.path().join("node"), network))
        .await
        .unwrap();
    for name in ["preprod", "production"] {
        environment(&daemon, name).await;
        released(&daemon, name, "v1").await;
    }

    daemon
        .add_domain(domain("app.example.com", "production"))
        .await
        .unwrap();
    daemon
        .add_domain(domain("preview.example.com", "preprod"))
        .await
        .unwrap();
    eventually("both domains are healthy", 20, || async {
        daemon
            .domains()
            .await
            .unwrap()
            .iter()
            .all(|domain| domain.record.status == "healthy")
    })
    .await;
    let app = daemon.domain("app.example.com").await.unwrap();
    assert_eq!(app.record.dns.status, "healthy");
    assert_eq!(app.record.tls.status, "disabled");
    assert_eq!(app.record.routing.status, "healthy");
    assert_eq!(app.endpoint, "production/site/web/http");
    assert_eq!(app.dns_records.len(), 1);
    assert_eq!(app.dns_records[0].record.value, "192.0.2.10");
    let records: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&zone).unwrap()).unwrap();
    assert_eq!(records["app A"]["value"], "192.0.2.10");
    assert_eq!(records["preview A"]["value"], "192.0.2.10");

    // Each domain routes only to its own environment.
    assert!(
        http(http_port, "app.example.com")
            .await
            .ends_with("production v1")
    );
    assert!(
        http(http_port, "preview.example.com")
            .await
            .ends_with("preprod v1")
    );
    assert!(
        http(http_port, "other.example.com")
            .await
            .starts_with("HTTP/1.1 404")
    );

    // Isolation and uniqueness are enforced when a domain is added.
    let mut elsewhere = domain("x.example.com", "production");
    elsewhere.project = "missing".into();
    assert!(matches!(
        daemon.add_domain(elsewhere).await,
        Err(EnvironmentError::NotFound(_))
    ));
    assert!(matches!(
        daemon
            .add_domain(domain("app.example.com", "preprod"))
            .await,
        Err(EnvironmentError::Conflict(_))
    ));
    let mut outside = domain("app.example.net", "production");
    outside.dns_provider = Some("local".into());
    assert!(matches!(
        daemon.add_domain(outside).await,
        Err(EnvironmentError::Invalid(_))
    ));
    assert!(
        daemon
            .add_domain(domain("Not A Domain", "production"))
            .await
            .is_err()
    );

    // Drift at the provider is detected and repaired.
    let mut drifted = records.clone();
    drifted["app A"]["value"] = serde_json::json!("203.0.113.66");
    std::fs::write(&zone, serde_json::to_vec(&drifted).unwrap()).unwrap();
    daemon.reconcile_dns().await.unwrap();
    let records: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&zone).unwrap()).unwrap();
    assert_eq!(records["app A"]["value"], "192.0.2.10", "drift repaired");
    let events = daemon.events(EventFilter::default()).await.unwrap();
    let drift = events
        .iter()
        .find(|event| event.kind == "network.dns.drifted")
        .expect("drift is an event");
    assert_eq!(drift.data["found"], "203.0.113.66");

    // A release moves the domain with its endpoint.
    released(&daemon, "production", "v2").await;
    assert!(
        http(http_port, "app.example.com")
            .await
            .ends_with("production v2")
    );
    assert!(
        http(http_port, "preview.example.com")
            .await
            .ends_with("preprod v1")
    );

    // Credentials come from the environment; missing ones are reported by
    // name and nothing secret is stored.
    let mut missing = domain("api.example.org", "production");
    missing.dns_provider = Some("hetzner".into());
    daemon.add_domain(missing).await.unwrap();
    eventually("the missing token is reported", 20, || async {
        daemon
            .domain("api.example.org")
            .await
            .unwrap()
            .record
            .dns
            .status
            == "failed"
    })
    .await;
    let failed = daemon.domain("api.example.org").await.unwrap();
    assert!(
        failed
            .record
            .dns
            .last_error
            .as_deref()
            .unwrap()
            .contains("COMPUTE_TEST_UNSET_HETZNER_TOKEN"),
        "{:?}",
        failed.record.dns
    );
    let network = daemon.network_status().await;
    assert!(
        network
            .dns_providers
            .iter()
            .any(|provider| provider.name == "hetzner" && provider.error.is_some())
    );

    // A domain must go before what it routes to.
    assert!(matches!(
        daemon.remove_project("production", "site").await,
        Err(EnvironmentError::Conflict(_))
    ));
    assert!(matches!(
        daemon.destroy_environment("preprod").await,
        Err(EnvironmentError::Conflict(_))
    ));
    daemon.remove_domain("app.example.com").await.unwrap();
    let records: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&zone).unwrap()).unwrap();
    assert!(records.get("app A").is_none(), "its DNS record is removed");
    assert!(
        http(http_port, "app.example.com")
            .await
            .starts_with("HTTP/1.1 404")
    );
    daemon.shutdown().await;
}

/// Pebble and its DNS test server, stopped on drop.
struct Pebble {
    children: Vec<std::process::Child>,
    directory: String,
    management: u16,
    ca_file: PathBuf,
}

impl Drop for Pebble {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn find(binary: &str, variable: &str) -> Option<PathBuf> {
    std::env::var_os(variable).map(PathBuf::from).or_else(|| {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|path| path.join(binary))
                .find(|path| path.is_file())
        })
    })
}

/// Pebble's source tree, for its test certificates.
fn pebble_source() -> Option<PathBuf> {
    std::env::var_os("PEBBLE_SOURCE")
        .map(PathBuf::from)
        .or_else(|| {
            let home = std::env::var_os("HOME")?;
            let modules = Path::new(&home).join("go/pkg/mod/github.com/letsencrypt/pebble");
            std::fs::read_dir(modules)
                .ok()?
                .flatten()
                .map(|entry| entry.path())
                .find(|path| path.join("test/certs/pebble.minica.pem").is_file())
        })
}

async fn start_pebble(work: &Path, http_port: u16) -> Option<Pebble> {
    let (Some(pebble), Some(challtestsrv), Some(source)) = (
        find("pebble", "PEBBLE_BIN"),
        find("pebble-challtestsrv", "PEBBLE_CHALLTESTSRV_BIN"),
        pebble_source(),
    ) else {
        eprintln!(
            "skipping: Pebble is not installed (PEBBLE_BIN, PEBBLE_CHALLTESTSRV_BIN, PEBBLE_SOURCE)"
        );
        return None;
    };
    let (dns, dns_management, acme, management) =
        (free_port(), free_port(), free_port(), free_port());
    let config = serde_json::json!({
        "pebble": {
            "listenAddress": format!("127.0.0.1:{acme}"),
            "managementListenAddress": format!("127.0.0.1:{management}"),
            "certificate": source.join("test/certs/localhost/cert.pem"),
            "privateKey": source.join("test/certs/localhost/key.pem"),
            "httpPort": http_port,
            "tlsPort": free_port(),
            "ocspResponderURL": "",
            "externalAccountBindingRequired": false
        }
    });
    let config_path = work.join("pebble.json");
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let quiet = || std::process::Stdio::null();
    let dns_server = std::process::Command::new(challtestsrv)
        .args([
            "-defaultIPv4",
            "127.0.0.1",
            "-defaultIPv6",
            "",
            "-dns01",
            &format!("127.0.0.1:{dns}"),
            "-management",
            &format!("127.0.0.1:{dns_management}"),
            "-http01",
            "",
            "-https01",
            "",
            "-tlsalpn01",
            "",
            "-doh",
            "",
        ])
        .stdout(quiet())
        .stderr(quiet())
        .spawn()
        .unwrap();
    let acme_server = std::process::Command::new(pebble)
        .args([
            "-config",
            &config_path.display().to_string(),
            "-dnsserver",
            &format!("127.0.0.1:{dns}"),
        ])
        .env("PEBBLE_VA_NOSLEEP", "1")
        .env("PEBBLE_WFE_NONCEREJECT", "0")
        .stdout(quiet())
        .stderr(quiet())
        .spawn()
        .unwrap();
    let pebble = Pebble {
        children: vec![dns_server, acme_server],
        directory: format!("https://localhost:{acme}/dir"),
        management,
        ca_file: source.join("test/certs/pebble.minica.pem"),
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::net::TcpStream::connect(("127.0.0.1", acme))
        .await
        .is_err()
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "Pebble did not start"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Some(pebble)
}

/// Pebble's issuing root, from its management interface.
async fn issuing_root(pebble: &Pebble) -> String {
    let ca = reqwest::Certificate::from_pem(&std::fs::read(&pebble.ca_file).unwrap()).unwrap();
    reqwest::Client::builder()
        .add_root_certificate(ca)
        .build()
        .unwrap()
        .get(format!("https://localhost:{}/roots/0", pebble.management))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

async fn https(port: u16, name: &str, root_pem: &str) -> std::io::Result<(String, String)> {
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
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    let mut tls = connector
        .connect(
            rustls::pki_types::ServerName::try_from(name.to_string()).unwrap(),
            stream,
        )
        .await?;
    let leaf = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|chain| chain.first())
        .map(|leaf| compute_network::tls::fingerprint(leaf.as_ref()))
        .unwrap_or_default();
    tls.write_all(
        format!("GET / HTTP/1.1\r\nHost: {name}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await?;
    let mut response = String::new();
    let _ = tls.read_to_string(&mut response).await;
    Ok((response, leaf))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn certificates_are_issued_served_and_renewed_by_acme() {
    if !python().await {
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let (http_port, https_port) = (free_port(), free_port());
    let Some(pebble) = start_pebble(work.path(), http_port).await else {
        return;
    };
    let store = Arc::new(compute_state_memory::MemoryState::new());
    let artifacts = Arc::new(compute_state::StateArtifacts::new(
        compute_state::ControlState::new(store.clone()),
    ));
    let mut config = DaemonConfig::new(work.path().join("node"), store.clone(), artifacts);
    config.provider = std::sync::Arc::new(common::provider());
    config.reconcile_interval = Duration::from_millis(200);
    port_windows(&mut config);
    config.network = NetworkConfig {
        ingress_http: Some(format!("127.0.0.1:{http_port}").parse().unwrap()),
        ingress_https: Some(format!("127.0.0.1:{https_port}").parse().unwrap()),
        acme: Some(AcmeConfig {
            directory: pebble.directory.clone(),
            contact: Some("mailto:ops@compute.test".into()),
            ca_file: Some(pebble.ca_file.clone()),
            renew_before_days: 30,
        }),
        ..NetworkConfig::default()
    };
    let daemon = Daemon::start(config).await.unwrap();
    environment(&daemon, "production").await;
    released(&daemon, "production", "v1").await;

    let mut definition = domain("app.compute.test", "production");
    definition.dns_provider = Some("none".into());
    daemon.add_domain(definition).await.unwrap();
    eventually("the certificate is issued", 60, || async {
        daemon
            .domain("app.compute.test")
            .await
            .unwrap()
            .record
            .status
            == "healthy"
    })
    .await;
    let view = daemon.domain("app.compute.test").await.unwrap();
    let certificate = view.certificate.unwrap();
    assert_eq!(certificate.record.status, "valid");
    assert!(certificate.held_here);
    assert_eq!(view.record.dns.status, "unmanaged");
    let first = certificate.record.fingerprint.clone().unwrap();
    let reference = certificate.record.secret_reference.clone().unwrap();
    assert!(reference.starts_with("node:"), "{reference}");
    assert!(certificate.record.expires_at.unwrap() > chrono::Utc::now());

    // Served by SNI, trusted through Pebble's root, forwarded to the site.
    let root = issuing_root(&pebble).await;
    let (response, leaf) = https(https_port, "app.compute.test", &root).await.unwrap();
    assert!(response.ends_with("production v1"), "{response}");
    assert_eq!(leaf, first);
    let redirect = http(http_port, "app.compute.test").await;
    assert!(redirect.starts_with("HTTP/1.1 308"), "{redirect}");

    // Renewal replaces the certificate without interrupting service.
    daemon.renew_certificate("app.compute.test").await.unwrap();
    eventually("the certificate is renewed", 60, || async {
        daemon.certificates().await.unwrap()[0]
            .record
            .fingerprint
            .as_deref()
            != Some(first.as_str())
            && daemon.certificates().await.unwrap()[0].record.status == "valid"
    })
    .await;
    let renewed = daemon.certificates().await.unwrap()[0].record.clone();
    eventually("the renewed certificate is served", 20, || async {
        https(https_port, "app.compute.test", &root)
            .await
            .is_ok_and(|(_, leaf)| Some(leaf) == renewed.fingerprint)
    })
    .await;
    let events = daemon.events(EventFilter::default()).await.unwrap();
    for kind in ["network.certificate.issued", "network.certificate.renewed"] {
        assert!(events.iter().any(|event| event.kind == kind), "{kind}");
    }

    // No key material or credentials in control state: only references.
    for collection in compute_state::Collection::ALL {
        if matches!(
            collection,
            compute_state::Collection::Artifact | compute_state::Collection::ArtifactChunk
        ) {
            continue;
        }
        let records = compute_state::StateStore::query(
            store.as_ref(),
            &compute_state::Query::all(collection),
        )
        .await
        .unwrap();
        let text = serde_json::to_string(
            &records
                .into_iter()
                .map(|record| record.value)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(
            !text.contains("PRIVATE KEY"),
            "{} holds a key",
            collection.name()
        );
        assert!(
            !text.contains("BEGIN CERTIFICATE"),
            "{} holds a certificate",
            collection.name()
        );
    }
    daemon.shutdown().await;
}
