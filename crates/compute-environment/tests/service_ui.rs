//! A registered service's UI contribution, end to end through a real daemon and
//! a real HTTP server standing in for the service. The service is untrusted:
//! every way it can be absent, slow, refusing or wrong must end as a status,
//! never a crash, and never as anything but data.

mod common;

use std::sync::Arc;
use std::time::Duration;

use compute_environment::service_ui::UiStatus;
use compute_environment::*;
use compute_state::{ControlState, StateArtifacts, StateStore};
use compute_state_memory::MemoryState;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// What `@appport/services` answers at `/v1/ui`: the protocol's caller-filtered
/// view for a caller holding no capabilities (only the capability-free overview).
const SERVICES_DOCUMENT: &str = include_str!("fixtures/appport-ui/appport-services.json");
/// The full contribution, as a host with capability context would serve it.
const FULL_DOCUMENT: &str = include_str!("fixtures/appport-ui/appport-services-full.json");

fn config(node: &std::path::Path, store: Arc<dyn StateStore>) -> DaemonConfig {
    let artifacts = Arc::new(StateArtifacts::new(ControlState::new(store.clone())));
    let mut config = DaemonConfig::new(node, store, artifacts);
    config.provider = Arc::new(common::provider());
    config.port_range = (21400, 21499);
    config.instance_port_range = (41400, 41499);
    config
}

/// What the stand-in service answers to `GET /v1/ui`.
#[derive(Clone)]
enum Reply {
    Json(u16, String),
    Raw(Vec<u8>),
    Stall,
}

/// Serve every connection with `reply`; returns the base URL and the requests
/// received (so a test can prove what Compute sent).
async fn service(reply: Reply) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(std::sync::Mutex::new(vec![]));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let reply = reply.clone();
            let log = log.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 16384];
                let read = stream.read(&mut buffer).await.unwrap_or(0);
                log.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buffer[..read]).into_owned());
                match reply {
                    Reply::Json(status, body) => {
                        let response = format!(
                            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    }
                    Reply::Raw(bytes) => {
                        let _ = stream.write_all(&bytes).await;
                    }
                    Reply::Stall => tokio::time::sleep(Duration::from_secs(30)).await,
                }
            });
        }
    });
    (url, seen)
}

async fn daemon() -> (tempfile::TempDir, Arc<Daemon>) {
    let node = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(config(node.path(), Arc::new(MemoryState::new())))
        .await
        .unwrap();
    (node, daemon)
}

async fn register(daemon: &Arc<Daemon>, name: &str, capabilities: &[&str], endpoint: Option<&str>) {
    daemon
        .register_service(ServiceDefinition {
            name: name.into(),
            capabilities: capabilities.iter().map(|c| c.to_string()).collect(),
            provider: "local".into(),
            environment: None,
            project: None,
            workload: None,
            endpoint: endpoint.map(str::to_owned),
            description: None,
        })
        .await
        .unwrap();
}

const UI: &str = "AppPort/ui/1";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_service_that_declares_a_ui_is_discovered_and_exposed_as_links_to_its_own_pages() {
    let (_node, daemon) = daemon().await;
    let (endpoint, seen) = service(Reply::Json(200, SERVICES_DOCUMENT.into())).await;
    register(
        &daemon,
        "services",
        &[UI, "appport.services@1"],
        Some(&endpoint),
    )
    .await;

    let view = daemon.service_ui("services").await.unwrap();
    assert_eq!(view.status, UiStatus::Available, "{view:?}");
    assert_eq!(view.product.as_ref().unwrap().id, "appport-services");
    // Compute knows no page by name: the link is the service's own route. An
    // anonymous caller gets the protocol's filtered view, the overview page.
    assert_eq!(view.links.len(), 1, "{view:?}");
    assert_eq!(view.links[0].url, format!("{endpoint}/services"));
    assert_eq!(view.links[0].group, "AppPort Services");

    // The request carried nothing about who asked: no credential, no cookie.
    let request = seen.lock().unwrap()[0].to_lowercase();
    assert!(request.starts_with("get /v1/ui "), "{request}");
    for header in [
        "authorization",
        "cookie",
        "x-compute",
        "proxy-authorization",
    ] {
        assert!(!request.contains(header), "{header} was sent: {request}");
    }
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_service_that_advertises_several_surfaces_gets_a_link_for_each_navigation_entry() {
    let (_node, daemon) = daemon().await;
    let (endpoint, _) = service(Reply::Json(200, FULL_DOCUMENT.into())).await;
    register(&daemon, "full", &[UI], Some(&endpoint)).await;
    let view = daemon.service_ui("full").await.unwrap();
    assert_eq!(view.status, UiStatus::Available, "{view:?}");
    assert_eq!(view.links.len(), 9);
    for route in [
        "/services",
        "/api-keys",
        "/webhooks",
        "/jobs",
        "/schedules",
        "/notifications",
        "/files",
        "/configuration",
        "/secrets",
    ] {
        assert!(
            view.links
                .iter()
                .any(|l| l.url == format!("{endpoint}{route}")),
            "{route}"
        );
    }
    // Ordered by the service's own group and order: the overview first.
    assert_eq!(view.links[0].url, format!("{endpoint}/services"));
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_service_that_declares_no_ui_is_a_normal_service_and_is_never_contacted() {
    let (_node, daemon) = daemon().await;
    let (endpoint, seen) = service(Reply::Json(200, SERVICES_DOCUMENT.into())).await;
    register(&daemon, "laya", &["llm.generate@1"], Some(&endpoint)).await;
    register(&daemon, "plain", &[], None).await;
    for name in ["laya", "plain"] {
        let view = daemon.service_ui(name).await.unwrap();
        assert_eq!(view.status, UiStatus::None);
        assert!(view.links.is_empty() && view.message.is_none());
    }
    // Even a service that happens to serve /v1/ui is not asked unless it declares the capability.
    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(daemon.services().await.unwrap().len(), 2);
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_way_a_service_can_fail_ends_as_a_status() {
    let (_node, daemon) = daemon().await;
    let oversize = format!("{{\"x\":\"{}\"}}", "a".repeat(300 * 1024));
    let cases: Vec<(&str, Reply, UiStatus)> = vec![
        ("not-json", Reply::Json(200, "<html>nope</html>".into()), UiStatus::Invalid),
        ("empty-body", Reply::Json(200, String::new()), UiStatus::Invalid),
        ("json-array", Reply::Json(200, "[]".into()), UiStatus::Invalid),
        ("wrong-version", Reply::Json(200, FULL_DOCUMENT.replace("AppPort/ui/1", "AppPort/ui/2")), UiStatus::Invalid),
        ("bad-route", Reply::Json(200, FULL_DOCUMENT.replace("\"/api-keys\"", "\"https://evil.example/\"")), UiStatus::Invalid),
        ("oversize", Reply::Json(200, oversize), UiStatus::Invalid),
        ("garbage", Reply::Raw(b"\x00\x01 not http".to_vec()), UiStatus::Unreachable),
        ("closed", Reply::Raw(vec![]), UiStatus::Unreachable),
        ("not-advertised", Reply::Json(404, "{}".into()), UiStatus::Empty),
        ("unauthorized", Reply::Json(401, "{}".into()), UiStatus::Unauthorized),
        ("forbidden", Reply::Json(403, "{}".into()), UiStatus::Unauthorized),
        ("server-error", Reply::Json(500, "boom".into()), UiStatus::Unreachable),
        (
            "redirect",
            Reply::Raw(b"HTTP/1.1 302 Found\r\nlocation: http://169.254.169.254/\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec()),
            UiStatus::Unreachable,
        ),
        (
            "needs-identity",
            Reply::Json(200, SERVICES_DOCUMENT.replace("\"requires\": []", "\"requires\": [\"identity\"]")),
            UiStatus::Unsupported,
        ),
    ];
    for (name, reply, expected) in cases {
        let (endpoint, _) = service(reply).await;
        register(&daemon, name, &[UI], Some(&endpoint)).await;
        let view = daemon.service_ui(name).await.unwrap();
        assert_eq!(view.status, expected, "{name}: {view:?}");
        assert!(view.links.is_empty(), "{name}");
        // Failure messages are plain text and never echo the service's body.
        if let Some(message) = &view.message {
            assert!(
                !message.contains("evil.example") || name == "bad-route",
                "{name}: {message}"
            );
            assert!(!message.contains("<html>"), "{name}: {message}");
        }
    }
    // Declared, but the endpoint is not one Compute will contact.
    for (name, endpoint) in [
        ("no-endpoint", None),
        ("file", Some("file:///etc/passwd")),
        ("script", Some("javascript:alert(1)")),
        ("credentials", Some("http://user:pw@127.0.0.1:1/")),
    ] {
        register(&daemon, name, &[UI], endpoint).await;
        assert_eq!(
            daemon.service_ui(name).await.unwrap().status,
            UiStatus::Invalid,
            "{name}"
        );
    }
    // Nothing listening.
    register(&daemon, "down", &[UI], Some("http://127.0.0.1:1")).await;
    assert_eq!(
        daemon.service_ui("down").await.unwrap().status,
        UiStatus::Unreachable
    );
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_service_that_never_answers_times_out_instead_of_hanging() {
    let (_node, daemon) = daemon().await;
    let (endpoint, _) = service(Reply::Stall).await;
    register(&daemon, "slow", &[UI], Some(&endpoint)).await;
    let started = std::time::Instant::now();
    let view = daemon.service_ui("slow").await.unwrap();
    assert_eq!(view.status, UiStatus::Unreachable);
    assert!(view.message.unwrap().contains("in time"));
    assert!(started.elapsed() < Duration::from_secs(15));
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn discovery_follows_the_current_registration() {
    let (_node, daemon) = daemon().await;
    let (first, _) = service(Reply::Json(200, SERVICES_DOCUMENT.into())).await;
    let (second, _) = service(Reply::Json(200, SERVICES_DOCUMENT.into())).await;
    register(&daemon, "services", &[UI], Some(&first)).await;
    let before = daemon.service_ui("services").await.unwrap();
    assert!(before.links[0].url.starts_with(&first));

    // The endpoint changes: the next discovery uses it; nothing was cached.
    register(&daemon, "services", &[UI], Some(&second)).await;
    let after = daemon.service_ui("services").await.unwrap();
    assert!(after.links[0].url.starts_with(&second));

    // The capability is dropped: it stops being an AppPort UI service.
    register(&daemon, "services", &[], Some(&second)).await;
    assert_eq!(
        daemon.service_ui("services").await.unwrap().status,
        UiStatus::None
    );

    // The service is removed: there is nothing to discover.
    daemon.remove_service("services").await.unwrap();
    assert!(daemon.service_ui("services").await.is_err());
    assert!(daemon.service_ui("never-registered").await.is_err());
    daemon.shutdown().await;
}

#[test]
fn the_route_is_listed() {
    assert!(api::ROUTES.contains(&("GET", "/services/{service}/ui")));
}

#[test]
fn the_fixture_is_what_appport_services_publishes() {
    // Provenance: generated from `createUiDiscoveryDocument` in
    // rkendel1/appport-services (see the documentation page).
    let document: serde_json::Value = serde_json::from_str(SERVICES_DOCUMENT).unwrap();
    assert_eq!(document["protocol"], "AppPort/ui/1");
    assert_eq!(document["product"]["id"], "appport-services");
    let full: serde_json::Value = serde_json::from_str(FULL_DOCUMENT).unwrap();
    assert_eq!(full["protocol"], "AppPort/ui/1");
}

/// The UI renders what a service sends only as text and as links the daemon
/// built, so the script must never use an API that parses markup or runs code.
#[test]
fn the_ui_never_parses_markup_or_runs_code_from_a_service() {
    let script = include_str!("../ui/app.js");
    for forbidden in [
        "innerHTML",
        "outerHTML",
        "insertAdjacentHTML",
        "document.write",
        "eval(",
        "new Function",
        "srcdoc",
        "createContextualFragment",
    ] {
        assert!(!script.contains(forbidden), "app.js uses {forbidden}");
    }
    // A link is only ever made from a URL that passed `safeHttpUrl`.
    let cell = script
        .split("function serviceUiCell")
        .nth(1)
        .and_then(|rest| rest.split("async function servicesView").next())
        .expect("the service UI cell");
    assert_eq!(cell.matches("href:").count(), 1);
    assert!(cell.contains("href: url") && cell.contains("safeHttpUrl(link.url)"));
    assert!(cell.contains("rel: 'noopener noreferrer'"));
}
