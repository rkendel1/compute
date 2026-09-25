//! Control-plane certification: durable desired state, reconciliation,
//! deployment and promotion, and failing closed.
//!
//! The critical assertion: Compute's memory can disappear. The desired
//! state cannot.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use compute_core::{
    IsolationRequirement, NetworkPolicy, ResourceLimits, RuntimeKind, WORKLOAD_SPEC_VERSION,
    WorkloadBundle, WorkloadSpec,
};
use compute_environment::*;
use compute_state::{
    BackendInfo, Batch, Collection, ControlState, EnvironmentProjectRecord, Query, Record,
    StateArtifacts, StateError, StateStore, Write,
};
use compute_state_memory::MemoryState;

fn bundle(source: &str) -> Vec<u8> {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("main.sh"), source).unwrap();
    let spec = WorkloadSpec {
        version: WORKLOAD_SPEC_VERSION.into(),
        runtime: RuntimeKind::Shell,
        runtime_version: None,
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

const SERVICE: &str = "echo \"pid=$$ revision=$REVISION\"; while :; do sleep 0.2; done";

fn service(name: &str) -> WorkloadDefinition {
    WorkloadDefinition {
        name: name.into(),
        kind: WorkloadKind::Service,
        bundle: bundle(SERVICE),
        ports: vec![],
        restart: RestartPolicy::OnFailure,
        desired_state: DesiredState::Running,
        readiness: None,
    }
}

fn revision(label: &str, workloads: Vec<WorkloadDefinition>) -> RevisionDefinition {
    RevisionDefinition {
        revision: label.into(),
        source: None,
        workloads,
    }
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

/// A control-state backend that can be taken away, as a network partition
/// from Managed FeltDB would.
struct Partitionable {
    inner: MemoryState,
    down: AtomicBool,
}

impl Partitionable {
    fn check(&self) -> Result<(), StateError> {
        if self.down.load(Ordering::SeqCst) {
            Err(StateError::Unavailable(
                "partitioned from control state".into(),
            ))
        } else {
            Ok(())
        }
    }
}

#[async_trait::async_trait]
impl StateStore for Partitionable {
    fn backend(&self) -> BackendInfo {
        BackendInfo {
            kind: "partitionable".into(),
            location: "memory".into(),
            durable: true,
        }
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

fn config(node: &std::path::Path, store: Arc<dyn StateStore>) -> DaemonConfig {
    let artifacts = Arc::new(StateArtifacts::new(ControlState::new(store.clone())));
    let mut config = DaemonConfig::new(node, store, artifacts);
    config.restart_delay = Duration::from_millis(100);
    config.reconcile_interval = Duration::from_millis(200);
    port_windows(&mut config);
    config
}

/// Each daemon in this test process gets its own port windows, so daemons
/// of concurrent tests never choose the same port.
fn port_windows(config: &mut DaemonConfig) {
    static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    let window = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst) * 100;
    config.port_range = (20000 + window, 20099 + window);
    config.instance_port_range = (40000 + window, 40099 + window);
}

async fn eventually<F, Fut>(what: &str, mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !condition().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn actual(daemon: &Daemon, environment: &str, project: &str, workload: &str) -> ActualState {
    daemon
        .workload(environment, project, workload)
        .await
        .map(|view| view.actual_state)
        .unwrap_or(ActualState::Pending)
}

async fn running(daemon: &Daemon, environment: &str, project: &str, workload: &str) {
    eventually(
        &format!("{environment}/{project}/{workload} running"),
        || async { actual(daemon, environment, project, workload).await == ActualState::Running },
    )
    .await;
}

async fn deploy(
    daemon: &Arc<Daemon>,
    environment: &str,
    project: &str,
    label: &str,
) -> DeploymentView {
    daemon
        .deploy(DeployRequest {
            project: project.into(),
            environment: environment.into(),
            revision: Some(label.into()),
            config: Some(BTreeMap::from([("REVISION".into(), label.into())])),
            desired_state: None,
        })
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compute_memory_can_disappear_desired_state_cannot() {
    let store: Arc<dyn StateStore> = Arc::new(MemoryState::new());
    let first_node = tempfile::tempdir().unwrap();
    let first = Daemon::start(config(first_node.path(), store.clone()))
        .await
        .unwrap();
    first
        .create_environment(environment("production"))
        .await
        .unwrap();
    first
        .register_revision("attn", revision("abc123", vec![service("api")]))
        .await
        .unwrap();
    let deployment = deploy(&first, "production", "attn", "abc123").await;
    running(&first, "production", "attn", "api").await;
    first.shutdown().await;
    drop(first);
    // The node is gone entirely: its cache, logs, and memory.
    drop(first_node);

    let second_node = tempfile::tempdir().unwrap();
    let second = Daemon::start(config(second_node.path(), store.clone()))
        .await
        .unwrap();
    running(&second, "production", "attn", "api").await;
    let project = second.project("production", "attn").await.unwrap();
    assert_eq!(project.revision, "abc123");
    assert_eq!(
        project.deployment.unwrap().deployment_id,
        deployment.deployment_id,
        "the same deployment is current"
    );
    // The bundle came back from durable artifacts, not the old node.
    let logs = second.logs("production", "attn", "api").await.unwrap().0;
    eventually("service output", || async {
        second
            .logs("production", "attn", "api")
            .await
            .unwrap()
            .0
            .contains("revision=abc123")
    })
    .await;
    drop(logs);
    second.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_killed_workload_is_restored_by_the_reconciler() {
    let node = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(config(node.path(), Arc::new(MemoryState::new())))
        .await
        .unwrap();
    daemon
        .create_environment(environment("production"))
        .await
        .unwrap();
    daemon
        .register_revision(
            "attn",
            revision("abc123", vec![service("api"), service("worker")]),
        )
        .await
        .unwrap();
    deploy(&daemon, "production", "attn", "abc123").await;
    running(&daemon, "production", "attn", "api").await;
    running(&daemon, "production", "attn", "worker").await;
    let worker_execution = daemon
        .workload("production", "attn", "worker")
        .await
        .unwrap()
        .started_at;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let pid = loop {
        let output = daemon.logs("production", "attn", "api").await.unwrap().0;
        if let Some(pid) = output.split_whitespace().find_map(|word| {
            word.strip_prefix("pid=")
                .and_then(|pid| pid.parse::<i32>().ok())
        }) {
            break pid;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the service never reported its pid"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let before = daemon.workload("production", "attn", "api").await.unwrap();

    // Kill the process out from under Compute.
    // SAFETY: signalling a child process this test started through Compute.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);

    eventually("the killed service is restored", || async {
        let view = daemon.workload("production", "attn", "api").await.unwrap();
        view.actual_state == ActualState::Running
            && view.restarts > before.restarts
            && view.started_at > before.started_at
    })
    .await;
    assert_eq!(
        daemon
            .workload("production", "attn", "worker")
            .await
            .unwrap()
            .started_at,
        worker_execution,
        "its sibling was not touched"
    );
    let kinds = daemon
        .events(EventFilter::default())
        .await
        .unwrap()
        .into_iter()
        .map(|event| event.kind)
        .collect::<Vec<_>>();
    assert!(kinds.contains(&"service.failed".to_string()), "{kinds:?}");
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_reconciler_follows_desired_state_written_elsewhere() {
    // Another control-plane client (or an operator editing FeltDB) changes
    // desired state; the daemon converges without being told.
    let store: Arc<dyn StateStore> = Arc::new(MemoryState::new());
    let node = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(config(node.path(), store.clone()))
        .await
        .unwrap();
    daemon
        .create_environment(environment("production"))
        .await
        .unwrap();
    daemon
        .register_revision("factory", revision("def456", vec![service("api")]))
        .await
        .unwrap();
    deploy(&daemon, "production", "factory", "def456").await;
    running(&daemon, "production", "factory", "api").await;

    let other = ControlState::new(store.clone());
    let set = |desired: DesiredState| {
        let other = other.clone();
        async move {
            let membership = other
                .list::<EnvironmentProjectRecord>()
                .await
                .unwrap()
                .into_iter()
                .next()
                .unwrap();
            other
                .transaction(
                    Batch::new()
                        .update(&membership, serde_json::json!({ "desired_state": desired })),
                )
                .await
                .unwrap();
        }
    };
    set(DesiredState::Stopped).await;
    eventually("desired stopped → stopped", || async {
        actual(&daemon, "production", "factory", "api").await == ActualState::Stopped
    })
    .await;
    set(DesiredState::Running).await;
    running(&daemon, "production", "factory", "api").await;
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deploy_verify_and_promote_the_exact_revision() {
    let node = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(config(node.path(), Arc::new(MemoryState::new())))
        .await
        .unwrap();
    for name in ["preprod", "production"] {
        daemon.create_environment(environment(name)).await.unwrap();
    }
    let registered = daemon
        .register_revision("feltdb", revision("abc123", vec![service("api")]))
        .await
        .unwrap();
    let again = daemon
        .register_revision("feltdb", revision("abc123", vec![service("api")]))
        .await
        .unwrap();
    assert_eq!(
        again.revision_id, registered.revision_id,
        "registration is idempotent"
    );
    let mut different = service("api");
    different.bundle = bundle("echo other; while :; do sleep 1; done");
    assert!(
        matches!(
            daemon
                .register_revision("feltdb", revision("abc123", vec![different]))
                .await,
            Err(EnvironmentError::Conflict(_))
        ),
        "a label always names the same content"
    );

    // Deploy to preprod and verify.
    let preprod = deploy(&daemon, "preprod", "feltdb", "abc123").await;
    assert!(
        !matches!(
            preprod.record.status,
            DeploymentStatus::Pending | DeploymentStatus::Failed
        ),
        "{:?}",
        preprod.record
    );
    assert!(
        preprod
            .record
            .workloads
            .iter()
            .all(|workload| workload.admitted
                && workload.admission_id.is_some()
                && workload.placement_id.is_some())
    );
    eventually("the preprod release completes", || async {
        daemon
            .deployment(&preprod.deployment_id)
            .await
            .unwrap()
            .record
            .status
            == DeploymentStatus::Complete
    })
    .await;
    let kinds = daemon
        .events(EventFilter {
            deployment_id: Some(preprod.deployment_id.clone()),
            ..EventFilter::default()
        })
        .await
        .unwrap()
        .into_iter()
        .map(|event| event.kind)
        .collect::<Vec<_>>();
    for expected in [
        "deployment.started",
        "deployment.admitted",
        "deployment.placed",
        "project.added",
        "deployment.ready",
        "deployment.switched",
        "deployment.activated",
        "deployment.draining",
        "service.started",
        "service.healthy",
        "deployment.completed",
    ] {
        assert!(
            kinds.contains(&expected.to_string()),
            "{expected} missing from {kinds:?}"
        );
    }

    // Promote: production runs exactly what preprod validated.
    let promoted = daemon
        .promote(PromoteRequest {
            project: "feltdb".into(),
            from: "preprod".into(),
            to: "production".into(),
            allow_unhealthy: false,
            config: None,
        })
        .await
        .unwrap();
    assert_eq!(
        promoted.record.revision_digest,
        preprod.record.revision_digest
    );
    assert_eq!(promoted.record.revision_id, preprod.record.revision_id);
    assert_eq!(
        promoted.record.promoted_from.as_deref(),
        Some(preprod.deployment_id.as_str())
    );
    running(&daemon, "production", "feltdb", "api").await;
    let production = daemon.project("production", "feltdb").await.unwrap();
    assert_eq!(production.revision, "abc123");
    assert_eq!(production.revision_digest, registered.revision_digest);

    // One project, two environments, independent desired state.
    let summary = daemon.project_detail("feltdb").await.unwrap().summary;
    assert_eq!(
        summary
            .environments
            .iter()
            .map(|placement| placement.environment.as_str())
            .collect::<Vec<_>>(),
        vec!["preprod", "production"]
    );
    let production_execution = daemon
        .workload("production", "feltdb", "api")
        .await
        .unwrap()
        .started_at;
    daemon
        .set_environment_state("preprod", DesiredState::Stopped, false)
        .await
        .unwrap();
    assert_eq!(
        actual(&daemon, "preprod", "feltdb", "api").await,
        ActualState::Stopped
    );
    assert_eq!(
        actual(&daemon, "production", "feltdb", "api").await,
        ActualState::Running
    );
    assert_eq!(
        daemon
            .workload("production", "feltdb", "api")
            .await
            .unwrap()
            .started_at,
        production_execution,
        "stopping preprod does not affect production"
    );
    assert_eq!(
        daemon
            .deployment(&preprod.deployment_id)
            .await
            .unwrap()
            .record
            .status,
        DeploymentStatus::Complete,
        "a stopped project keeps its release"
    );

    // A new revision is released next to the old one and replaces it.
    daemon
        .register_revision("feltdb", revision("def456", vec![service("api")]))
        .await
        .unwrap();
    let next = deploy(&daemon, "production", "feltdb", "def456").await;
    eventually("the new revision is running", || async {
        daemon
            .project("production", "feltdb")
            .await
            .unwrap()
            .revision
            == "def456"
            && daemon
                .workload("production", "feltdb", "api")
                .await
                .unwrap()
                .deployment_id
                == next.deployment_id
            && actual(&daemon, "production", "feltdb", "api").await == ActualState::Running
    })
    .await;
    let released = daemon.deployment(&next.deployment_id).await.unwrap().record;
    assert_eq!(
        released.previous.as_deref(),
        Some(promoted.deployment_id.as_str())
    );
    assert_eq!(released.old_revision.as_deref(), Some("abc123"));
    daemon
        .create_environment(environment("staging"))
        .await
        .unwrap();
    assert!(
        daemon
            .promote(PromoteRequest {
                project: "feltdb".into(),
                from: "staging".into(),
                to: "production".into(),
                allow_unhealthy: false,
                config: None,
            })
            .await
            .is_err(),
        "only a revision released in the source environment is promoted"
    );
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compute_fails_closed_without_its_control_state() {
    let store = Arc::new(Partitionable {
        inner: MemoryState::new(),
        down: AtomicBool::new(true),
    });
    let node = tempfile::tempdir().unwrap();
    // Required to have its control state, it refuses to start rather than
    // invent local state.
    let mut strict = config(node.path(), store.clone());
    strict.require_state_at_start = true;
    assert!(matches!(
        Daemon::start(strict).await,
        Err(EnvironmentError::Unavailable(_))
    ));
    // By default it starts with a degraded control plane: it invents
    // nothing, changes nothing, and says so.
    let degraded = Daemon::start(config(node.path(), store.clone()))
        .await
        .unwrap();
    assert!(!degraded.status().await.state_available);
    assert!(matches!(
        degraded.create_environment(environment("production")).await,
        Err(EnvironmentError::Unavailable(_))
    ));
    degraded.shutdown().await;
    drop(degraded);
    store.down.store(false, Ordering::SeqCst);
    let daemon = Daemon::start(config(node.path(), store.clone()))
        .await
        .unwrap();
    daemon
        .create_environment(environment("production"))
        .await
        .unwrap();
    daemon
        .register_revision("attn", revision("abc123", vec![service("api")]))
        .await
        .unwrap();
    deploy(&daemon, "production", "attn", "abc123").await;
    running(&daemon, "production", "attn", "api").await;
    let execution = daemon
        .workload("production", "attn", "api")
        .await
        .unwrap()
        .started_at;

    store.down.store(true, Ordering::SeqCst);
    assert!(matches!(
        daemon.create_environment(environment("preprod")).await,
        Err(EnvironmentError::Unavailable(_))
    ));
    assert!(matches!(
        daemon
            .set_project_state("production", "attn", DesiredState::Stopped, false)
            .await,
        Err(EnvironmentError::Unavailable(_))
    ));
    daemon.reconcile().await;
    let status = daemon.status().await;
    assert!(!status.state_available);
    assert!(status.state_error.is_some());

    // What runs keeps running: an unreachable store is not a stop order.
    store.down.store(false, Ordering::SeqCst);
    daemon.reconcile().await;
    assert!(daemon.status().await.state_available);
    let view = daemon.workload("production", "attn", "api").await.unwrap();
    assert_eq!(view.actual_state, ActualState::Running);
    assert_eq!(view.started_at, execution, "the same run");
    assert!(
        daemon.environment("preprod").await.is_err(),
        "nothing was created locally"
    );
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_services_and_providers_are_recorded() {
    let node = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(config(node.path(), Arc::new(MemoryState::new())))
        .await
        .unwrap();
    let providers = daemon.providers().await.unwrap();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0].provider_id, "local");
    daemon
        .register_service(ServiceDefinition {
            name: "laya".into(),
            capabilities: vec!["llm.generate@1".into()],
            provider: "local".into(),
            environment: None,
            project: None,
            workload: None,
            endpoint: Some("http://127.0.0.1:20001".into()),
            description: Some("LLM gateway".into()),
        })
        .await
        .unwrap();
    let services = daemon.services().await.unwrap();
    assert_eq!(services[0].capabilities, vec!["llm.generate@1".to_string()]);
    assert!(
        daemon
            .register_service(ServiceDefinition {
                name: "elsewhere".into(),
                capabilities: vec![],
                provider: "missing".into(),
                environment: None,
                project: None,
                workload: None,
                endpoint: None,
                description: None,
            })
            .await
            .is_err()
    );
    daemon.remove_service("laya").await.unwrap();
    assert!(daemon.services().await.unwrap().is_empty());
    daemon.shutdown().await;
}
