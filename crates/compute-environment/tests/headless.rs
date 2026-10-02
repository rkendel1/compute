//! Headless Compute: the platform is fully usable with no UI.
//!
//! This is the architectural proof that Compute-configured is an API-first,
//! headless execution platform rather than a UI with an API attached. Every
//! assertion here goes through the same HTTP API a control plane uses, so a
//! UI dependency would fail these tests rather than being tolerated.

mod common;

use std::sync::Arc;

use compute_environment::client::DaemonClient;
use compute_environment::*;

struct Headless {
    /// Held so the controller outlives each request; never read directly.
    #[allow(dead_code)]
    daemon: Arc<Daemon>,
    endpoint: String,
    _dir: tempfile::TempDir,
}

/// A controller started with `ui: false`, exactly as `--headless` does.
async fn headless() -> Headless {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(compute_state_memory::MemoryState::new());
    let artifacts = Arc::new(compute_state::StateArtifacts::new(
        compute_state::ControlState::new(store.clone()),
    ));
    let mut config = DaemonConfig::new(dir.path().join("node"), store, artifacts);
    config.provider = Arc::new(common::provider());
    config.ui = false;
    let daemon = Daemon::start(config).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(api::serve(listener, daemon.clone(), None));
    Headless {
        daemon,
        endpoint,
        _dir: dir,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn headless_starts_and_serves_the_api_without_the_ui() {
    let node = headless().await;
    let client = DaemonClient::new(&node.endpoint).unwrap();

    // Liveness and readiness both work, and readiness is independent of the UI.
    let health: serde_json::Value = client.get("/health").await.unwrap();
    assert_eq!(health["status"], "ok");

    let ready: serde_json::Value = client.get("/ready").await.unwrap();
    assert_eq!(ready["status"], "ready");
    assert_eq!(ready["accepting_work"], true);
    assert_eq!(
        ready["ui"], false,
        "readiness reports the UI as absent, and is reported anyway"
    );
    assert_eq!(ready["api"], "compute.api@1");

    // The UI is simply not served: no redirect at the root, no assets.
    assert!(client.get::<serde_json::Value>("/ui").await.is_err());
    assert!(client.get::<serde_json::Value>("/ui/app.js").await.is_err());
    assert!(
        client
            .get::<serde_json::Value>("/ui/app.css")
            .await
            .is_err()
    );
    assert!(client.get::<serde_json::Value>("/").await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn headless_registers_and_discovers_a_service_through_the_api() {
    // A control plane registering a service and reading its discovered UI is
    // the same path the operator UI uses, and it does not depend on the UI.
    let node = headless().await;
    let client = DaemonClient::new(&node.endpoint).unwrap();
    client
        .post::<_, serde_json::Value>(
            "/services",
            Some(&serde_json::json!({
                "name": "appport-services",
                "capabilities": ["AppPort/ui/1"],
                "provider": "local",
                "endpoint": "http://127.0.0.1:4100",
            })),
        )
        .await
        .unwrap();
    let services: Vec<serde_json::Value> = client.get("/services").await.unwrap();
    assert_eq!(services.len(), 1);
    assert_eq!(services[0]["name"], "appport-services");
    // Discovery against a dead endpoint reports unreachable, not an error:
    // the API is what serves this, and no UI is involved.
    let ui: serde_json::Value = client.get("/services/appport-services/ui").await.unwrap();
    assert!(ui["status"].is_string(), "discovery answered: {ui}");
}

/// A raw HTTP GET, for responses that are not JSON (the UI's own assets).
/// Returns `(status_line, body)`.
async fn raw_get(endpoint: &str, path: &str) -> (String, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let authority = endpoint.trim_start_matches("http://").to_owned();
    let mut stream = tokio::net::TcpStream::connect(&authority).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut body = String::new();
    stream.read_to_string(&mut body).await.unwrap();
    let (status, rest) = body.split_once("\r\n\r\n").unwrap_or((body.as_str(), ""));
    (status.to_owned(), rest.to_owned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn normal_startup_still_serves_the_ui() {
    // The UI is optional, not removed: an ordinary controller serves it.
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(compute_state_memory::MemoryState::new());
    let artifacts = Arc::new(compute_state::StateArtifacts::new(
        compute_state::ControlState::new(store.clone()),
    ));
    let mut config = DaemonConfig::new(dir.path().join("node"), store, artifacts);
    config.provider = Arc::new(common::provider());
    config.ui = true;
    let daemon = Daemon::start(config).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(api::serve(listener, daemon.clone(), None));
    let client = DaemonClient::new(&endpoint).unwrap();

    let ready: serde_json::Value = client.get("/ready").await.unwrap();
    assert_eq!(ready["ui"], true);

    let (status, body) = raw_get(&endpoint, "/ui").await;
    assert!(status.contains("200"), "the UI index is served: {status}");
    assert!(
        body.contains("Compute Control Plane"),
        "the UI index is the operator page"
    );
    let (status, _) = raw_get(&endpoint, "/").await;
    assert!(
        status.contains("303") || status.contains("302"),
        "the root redirects to the UI: {status}"
    );
    let (status, _) = raw_get(&endpoint, "/ui/app.js").await;
    assert!(status.contains("200"), "the UI script is served: {status}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn headless_mode_is_the_only_ui_dependency() {
    // The UI and the platform server share one router. Headless removes three
    // asset routes and the root redirect, and nothing else: this asserts the
    // platform's own routes are all still present when the UI is off.
    let node = headless().await;
    let client = DaemonClient::new(&node.endpoint).unwrap();
    for path in [
        "/health",
        "/ready",
        "/compute/capabilities",
        "/compute/capacity",
        "/info",
        "/status",
        "/events",
        "/environments",
        "/services",
        "/providers",
        "/targets",
        "/recipes",
        "/software",
        "/projects",
        "/sessions",
        "/deployments",
        "/domains",
        "/dns",
        "/certificates",
        "/applications",
        "/audit",
    ] {
        // A route that 404s would mean headless removed it.
        assert!(
            !matches!(
                client.get::<serde_json::Value>(path).await,
                Err(EnvironmentError::NotFound(_))
            ),
            "{path} is not served in headless mode"
        );
    }
}

/// Invariant 1 (headless execution) and 6 (optional UI): the UI is confined to
/// its own routes. Nothing in the platform half of `dispatch` may serve an
/// asset, so removing the UI cannot remove a capability.
#[test]
fn ui_assets_are_confined_to_their_own_routes() {
    let source = include_str!("../src/api.rs");
    let assets = ["UI_HTML", "UI_SCRIPT", "UI_STYLE"];

    // Each asset is used exactly once, and only ever as the body of a
    // `Response::Static`. A platform route returns `Response::Json`, so
    // this is what makes "the platform never depends on a UI asset" true
    // rather than merely intended.
    for asset in assets {
        let uses = source.matches(asset).count();
        assert_eq!(uses, 2, "{asset} is declared once and served once");
    }
    for (index, _) in source.match_indices("UI_") {
        if source[..index].ends_with("const ") {
            continue; // the declaration itself
        }
        // The response constructor that encloses this use.
        let enclosing = source[..index].rfind("Response::").map(|at| {
            source[at + "Response::".len()..]
                .chars()
                .take_while(|c| c.is_alphanumeric())
                .collect::<String>()
        });
        assert_eq!(
            enclosing.as_deref(),
            Some("Static"),
            "a UI asset is used outside a static response: {:?}",
            source[..index].lines().last().unwrap_or("").trim()
        );
    }
}

/// Invariant 2 (API authority): the route table is the whole API, and the UI
/// is a strict subset of it. `tests/parity.rs` proves the subset direction
/// against the real server; this pins the other direction statically so a
/// platform capability can never be added as UI-only.
#[test]
fn the_api_route_table_covers_every_domain_a_control_plane_needs() {
    let routes = api::ROUTES
        .iter()
        .map(|(method, path)| format!("{method} {path}"))
        .collect::<Vec<_>>();
    let has = |needle: &str| routes.iter().any(|route| route.contains(needle));
    for domain in [
        "/health",
        "/ready",
        "/compute/capabilities",
        "/compute/execute",
        "/environments",
        "/projects",
        "/sessions",
        "/executions",
        "/deployments",
        "/software/{project}/versions",
        "/software/{project}/rollback",
        "/deployments/{deployment}/rollback",
        "/applications",
        "/domains",
        "/dns",
        "/certificates",
        "/services",
        "/receipts",
        "/events/stream",
        "/compute/jobs",
    ] {
        assert!(has(domain), "the API does not expose {domain}");
    }
    assert!(
        !has("/api/v1"),
        "Compute did not add a parallel /api/v1 tree: compute.api@1 is the API"
    );
}
