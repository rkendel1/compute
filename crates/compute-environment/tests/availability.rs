//! Control-plane availability: durable control state (FeltDB) can become
//! unreachable. Workloads keep running and endpoints keep serving; reads
//! are served from the last snapshot and say so; changes are refused with
//! `state_unavailable`; a controller can start without it; and when it
//! returns, the controller reconciles and records what happened, once.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use compute_core::{
    IsolationRequirement, NetworkPolicy, ResourceLimits, RuntimeKind, WORKLOAD_SPEC_VERSION,
    WorkloadBundle, WorkloadSpec,
};
use compute_environment::*;
use compute_state::{BackendInfo, Collection, Query, Record, StateError, StateStore, Write};

/// A store that can be switched off, like a FeltDB that stops answering.
struct Flaky {
    inner: compute_state_memory::MemoryState,
    down: AtomicBool,
}

impl Flaky {
    fn check(&self) -> Result<(), StateError> {
        if self.down.load(Ordering::SeqCst) {
            Err(StateError::Unavailable(
                "FeltDB is unreachable (test)".into(),
            ))
        } else {
            Ok(())
        }
    }
}

#[async_trait::async_trait]
impl StateStore for Flaky {
    fn backend(&self) -> BackendInfo {
        self.inner.backend()
    }
    async fn get(&self, collection: Collection, id: &str) -> Result<Option<Record>, StateError> {
        self.check()?;
        self.inner.get(collection, id).await
    }
    async fn query(&self, query: &Query) -> Result<Vec<Record>, StateError> {
        self.check()?;
        self.inner.query(query).await
    }
    async fn commit(&self, writes: Vec<Write>) -> Result<(), StateError> {
        self.check()?;
        self.inner.commit(writes).await
    }
}

fn service_bundle() -> Vec<u8> {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("main.sh"),
        "echo \"serving\"; while :; do sleep 0.2; done",
    )
    .unwrap();
    let spec = WorkloadSpec {
        version: WORKLOAD_SPEC_VERSION.into(),
        runtime: RuntimeKind::Shell,
        runtime_version: None,
        architecture: None,
        entrypoint: "main.sh".into(),
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

fn environment(name: &str) -> EnvironmentDefinition {
    EnvironmentDefinition {
        name: name.into(),
        desired_state: DesiredState::Running,
        env: BTreeMap::new(),
        policy: None,
        provider: None,
    }
}

fn project() -> ProjectDefinition {
    ProjectDefinition {
        name: "app".into(),
        revision: "rev-1".into(),
        source: None,
        desired_state: DesiredState::Running,
        env: BTreeMap::new(),
        workloads: vec![WorkloadDefinition {
            name: "api".into(),
            kind: WorkloadKind::Service,
            bundle: service_bundle(),
            ports: vec![],
            restart: RestartPolicy::OnFailure,
            desired_state: DesiredState::Running,
            readiness: None,
        }],
    }
}

fn config(dir: &std::path::Path, store: Arc<Flaky>, window: u16) -> DaemonConfig {
    let artifacts = Arc::new(compute_state::StateArtifacts::new(
        compute_state::ControlState::new(store.clone()),
    ));
    let mut config = DaemonConfig::new(dir, store, artifacts);
    config.port_range = (window, window + 99);
    config.instance_port_range = (window + 20000, window + 20099);
    config.reconcile_interval = Duration::from_millis(200);
    config.read_cache = Duration::from_millis(0);
    config
}

async fn serve(daemon: &Arc<Daemon>) -> client::DaemonClient {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(api::serve(listener, daemon.clone(), None));
    client::DaemonClient::new(&endpoint).unwrap()
}

async fn running(daemon: &Daemon) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(view) = daemon.workload("production", "app", "api").await
            && view.actual_state == ActualState::Running
        {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "api never ran");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Raw GET, for the freshness headers.
async fn raw_get(endpoint: &str, path: &str) -> (u16, String, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let authority = endpoint.trim_start_matches("http://");
    let mut stream = tokio::net::TcpStream::connect(authority).await.unwrap();
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, head.to_string(), body.to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workloads_keep_running_and_changes_are_refused_while_state_is_unreachable() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Flaky {
        inner: compute_state_memory::MemoryState::new(),
        down: AtomicBool::new(false),
    });
    let daemon = Daemon::start(config(dir.path(), store.clone(), 29000))
        .await
        .unwrap();
    daemon
        .create_environment(environment("production"))
        .await
        .unwrap();
    daemon.add_project("production", project()).await.unwrap();
    running(&daemon).await;
    let client = serve(&daemon).await;
    let pid_before = daemon.workload("production", "app", "api").await.unwrap();

    store.down.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(600)).await;
    // Degraded, and saying so.
    let health: serde_json::Value = client.get("/health").await.unwrap();
    assert_eq!(health["status"], "degraded_control_plane");
    let status: DaemonStatus = client.get("/status").await.unwrap();
    assert!(!status.state_available);
    // The workload is untouched.
    let view = daemon.workload("production", "app", "api").await.unwrap();
    assert_eq!(view.actual_state, ActualState::Running);
    assert_eq!(view.started_at, pid_before.started_at, "not restarted");
    // Changes are refused before anything is attempted.
    let refused = client
        .post::<_, EnvironmentView>("/environments", Some(&environment("staging")))
        .await
        .unwrap_err();
    assert_eq!(refused.kind(), "state_unavailable");
    let refused = client
        .post::<(), ProjectView>("/environments/production/projects/app/stop", None)
        .await
        .unwrap_err();
    assert_eq!(refused.kind(), "state_unavailable");
    assert_eq!(
        daemon
            .workload("production", "app", "api")
            .await
            .unwrap()
            .actual_state,
        ActualState::Running,
        "a refused stop stopped nothing"
    );
    // Reads are served from the last snapshot, marked stale.
    let listed: Vec<EnvironmentSummary> = client.get("/environments").await.unwrap();
    assert_eq!(listed.len(), 1);

    // It returns: the controller reconciles and records the outage once.
    store.down.store(false, Ordering::SeqCst);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let health: serde_json::Value = client.get("/health").await.unwrap();
        if health["status"] == "ok" {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "never recovered");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let events = daemon
        .events(EventFilter {
            after: None,
            limit: Some(500),
            environment: None,
            project: None,
            deployment_id: None,
        })
        .await
        .unwrap();
    let recovered = events
        .iter()
        .filter(|event| event.kind == compute_state::events::FELTDB_RECOVERED)
        .count();
    assert_eq!(recovered, 1, "the outage is recorded once");
    // Sequences stay unique.
    let mut sequences = events
        .iter()
        .map(|event| event.sequence)
        .collect::<Vec<_>>();
    let total = sequences.len();
    sequences.dedup();
    assert_eq!(sequences.len(), total);
    // Changes work again.
    let _: EnvironmentView = client
        .post("/environments", Some(&environment("staging")))
        .await
        .unwrap();
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_controller_starts_degraded_and_converges_when_state_returns() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Flaky {
        inner: compute_state_memory::MemoryState::new(),
        down: AtomicBool::new(false),
    });
    // Desired state exists from an earlier controller.
    {
        let first = Daemon::start(config(dir.path(), store.clone(), 29200))
            .await
            .unwrap();
        first
            .create_environment(environment("production"))
            .await
            .unwrap();
        first.add_project("production", project()).await.unwrap();
        running(&first).await;
        first.shutdown().await;
    }
    store.down.store(true, Ordering::SeqCst);
    // Starts anyway, degraded; reads have nothing to serve yet.
    let daemon = Daemon::start(config(dir.path(), store.clone(), 29200))
        .await
        .unwrap();
    let client = serve(&daemon).await;
    let health: serde_json::Value = client.get("/health").await.unwrap();
    assert_eq!(health["status"], "degraded_control_plane");
    let info: ControllerInfo = client.get("/info").await.unwrap();
    assert_eq!(info.control_plane.mode, "degraded_control_plane");
    assert_eq!(
        client
            .get::<Vec<EnvironmentSummary>>("/environments")
            .await
            .unwrap_err()
            .kind(),
        "state_unavailable"
    );
    // A controller that requires state refuses instead.
    let strict_dir = tempfile::tempdir().unwrap();
    let mut strict = config(strict_dir.path(), store.clone(), 29400);
    strict.require_state_at_start = true;
    assert!(Daemon::start(strict).await.is_err());

    store.down.store(false, Ordering::SeqCst);
    running(&daemon).await;
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reads_say_how_fresh_they_are() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Flaky {
        inner: compute_state_memory::MemoryState::new(),
        down: AtomicBool::new(false),
    });
    let mut config = config(dir.path(), store.clone(), 29600);
    config.read_cache = Duration::from_secs(30);
    let daemon = Daemon::start(config).await.unwrap();
    daemon
        .create_environment(environment("production"))
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(api::serve(listener, daemon.clone(), None));
    let (status, head, _) = raw_get(&endpoint, "/environments").await;
    assert_eq!(status, 200);
    assert!(
        head.contains("X-Compute-State: live") || head.contains("X-Compute-State: cached"),
        "{head}"
    );
    let (_, head, _) = raw_get(&endpoint, "/environments").await;
    assert!(head.contains("X-Compute-State: cached"), "{head}");
    // A write here drops the cache: read-your-writes.
    daemon
        .create_environment(environment("staging"))
        .await
        .unwrap();
    let (_, _, body) = raw_get(&endpoint, "/environments").await;
    assert!(body.contains("staging"), "{body}");
    // Unreachable: the last read, marked stale.
    store.down.store(true, Ordering::SeqCst);
    daemon
        .create_environment(environment("never"))
        .await
        .unwrap_err();
    let (status, head, body) = raw_get(&endpoint, "/environments").await;
    assert_eq!(status, 200);
    assert!(head.contains("X-Compute-State: stale"), "{head}");
    assert!(head.contains("X-Compute-State-As-Of:"), "{head}");
    assert!(body.contains("staging"));
    store.down.store(false, Ordering::SeqCst);
    daemon.shutdown().await;
}
