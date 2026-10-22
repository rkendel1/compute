//! Agent runtimes are discoverable through the API, and only where declared.
//!
//! Base Compute is agent-runtime agnostic and must never advertise one. A
//! configured distribution declares its agent runtime in its own profile, and
//! a control plane (OpenDots) discovers it from the API without knowing where
//! that profile lives or which package version it names.

mod common;

use std::sync::Arc;

use compute_environment::client::DaemonClient;
use compute_environment::*;

/// A controller serving `/info` on a loopback port.
struct Node {
    #[allow(dead_code)]
    daemon: Arc<Daemon>,
    endpoint: String,
    _dir: tempfile::TempDir,
}

async fn serving() -> Node {
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
    Node {
        daemon,
        endpoint,
        _dir: dir,
    }
}

/// A configured distribution profile declaring stack-agent as its default agent
/// runtime, exactly as `compute-configured` ships it.
fn configured_profile(root: &std::path::Path) {
    std::fs::write(
        root.join("stack.json"),
        r#"{
  "format": "compute.distribution-profile@1",
  "profile": "configured",
  "agent": {
    "default": "stack-agent",
    "runtimes": [
      {
        "name": "stack-agent",
        "package": "github:rkendel1/stack-agent",
        "version": "1cf91e63d2242f9aca4cbeb2d1d939b84a00be14",
        "executable": "stack-agent/bin/compute-stack-agent",
        "default": true,
        "capabilities": ["Agent/loop@1"],
        "lifecycle": "service",
        "environment": "compute.session"
      }
    ]
  }
}"#,
    )
    .unwrap();
}

/// Base and configured are two ways of installing the same binary, so they are
/// observed in one test, in order.
///
/// `COMPUTE_CONFIGURED_HOME` is process-global: Rust runs tests in one process
/// on parallel threads, so a test that set it would otherwise leak into its
/// neighbours and make "base Compute advertises nothing" a race rather than a
/// fact. Observing both sides here keeps the assertion honest.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn base_advertises_no_agent_runtime_and_configured_advertises_stack_agent() {
    // Remove any ambient value first: a base installation has no profile, and
    // the controller must answer exactly as it would on a developer machine.
    unsafe { std::env::remove_var(agents::PROFILE_HOME_ENV) };

    let base = serving().await;
    let client = DaemonClient::new(&base.endpoint).unwrap();
    let info: serde_json::Value = client.get("/info").await.unwrap();
    // Absent, not empty-and-wrong: a control plane asks this question, and
    // "no agent runtime" is the honest answer for base Compute.
    assert!(
        info.get("agents").is_none(),
        "base Compute must not advertise an agent runtime"
    );

    // Now the configured installation: same binary, one profile.
    let home = tempfile::tempdir().unwrap();
    configured_profile(home.path());
    unsafe { std::env::set_var(agents::PROFILE_HOME_ENV, home.path()) };
    let configured_node = serving().await;
    let configured_client = DaemonClient::new(&configured_node.endpoint).unwrap();
    let info: serde_json::Value = configured_client.get("/info").await.unwrap();
    unsafe { std::env::remove_var(agents::PROFILE_HOME_ENV) };

    let agents_view = &info["agents"];
    assert_eq!(agents_view["default"], "stack-agent");
    let agent = &agents_view["runtimes"][0];
    assert_eq!(agent["name"], "stack-agent");
    assert_eq!(agent["package"], "github:rkendel1/stack-agent");
    assert_eq!(agent["version"], "1cf91e63d2242f9aca4cbeb2d1d939b84a00be14");
    assert_eq!(agent["default"], true);
    // A long-running service whose capabilities run in Compute sessions.
    assert_eq!(agent["lifecycle"], "service");
    assert_eq!(agent["environment"], "compute.session");
    assert!(!info.to_string().contains("@appport/chip"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_control_plane_can_resolve_the_default_without_hardcoding_a_version() {
    let home = tempfile::tempdir().unwrap();
    configured_profile(home.path());
    let capabilities = agents::declared(home.path());

    // This is the whole OpenDots contract: read the default's identity from the
    // contract, rather than knowing which commit the default is pinned to.
    let default = capabilities.default_runtime().expect("a default runtime");
    assert_eq!(default.name, "stack-agent");
    assert!(default.default);
    assert_eq!(capabilities.runtimes.len(), 1);
}
