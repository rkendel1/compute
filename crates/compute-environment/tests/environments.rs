//! Environment certification: isolation, independence, multi-project,
//! multi-environment, persistence, services, admission, placement,
//! receipts, restart, and failure — against a real daemon running real
//! services.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use compute_core::{
    IsolationRequirement, NetworkPolicy, ResourceLimits, RuntimeKind, WORKLOAD_SPEC_VERSION,
    WorkloadBundle, WorkloadSpec,
};
use compute_environment::*;
use compute_provider::LocalProvider;

fn bundle(runtime: RuntimeKind, entrypoint: &str, source: &str) -> Vec<u8> {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(entrypoint), source).unwrap();
    let spec = WorkloadSpec {
        version: WORKLOAD_SPEC_VERSION.into(),
        runtime,
        runtime_version: None,
        architecture: None,
        entrypoint: entrypoint.into(),
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

const SERVICE: &str = "echo \"started project=$PROJECT_NAME env=$ENVIRONMENT_NAME port=$PORT\"; while :; do sleep 0.2; done";

fn service(name: &str) -> WorkloadDefinition {
    WorkloadDefinition {
        name: name.into(),
        kind: WorkloadKind::Service,
        bundle: bundle(RuntimeKind::Shell, "main.sh", SERVICE),
        ports: vec![],
        restart: RestartPolicy::Never,
        desired_state: DesiredState::Running,
        readiness: None,
    }
}

fn task(name: &str, script: &str) -> WorkloadDefinition {
    WorkloadDefinition {
        name: name.into(),
        kind: WorkloadKind::Task,
        bundle: bundle(RuntimeKind::Shell, "main.sh", script),
        ports: vec![],
        restart: RestartPolicy::Never,
        desired_state: DesiredState::Running,
        readiness: None,
    }
}

fn project(name: &str, workloads: Vec<WorkloadDefinition>) -> ProjectDefinition {
    ProjectDefinition {
        name: name.into(),
        revision: "rev-1".into(),
        source: None,
        desired_state: DesiredState::Running,
        env: BTreeMap::from([("PROJECT_NAME".into(), name.into())]),
        workloads,
    }
}

fn environment(name: &str) -> EnvironmentDefinition {
    EnvironmentDefinition {
        name: name.into(),
        desired_state: DesiredState::Running,
        env: BTreeMap::from([("ENVIRONMENT_NAME".into(), name.into())]),
        policy: None,
        provider: None,
    }
}

fn process_readiness() -> Readiness {
    Readiness {
        check: ReadinessCheck::Process,
        port: None,
        path: None,
        task: None,
        timeout_ms: 30_000,
        interval_ms: 100,
    }
}

struct Harness {
    daemon: Arc<Daemon>,
    provider: Arc<LocalProvider>,
    _state: tempfile::TempDir,
}

/// A daemon on in-memory control state.
fn memory_config(dir: &std::path::Path) -> DaemonConfig {
    let store = Arc::new(compute_state_memory::MemoryState::new());
    let artifacts = Arc::new(compute_state::StateArtifacts::new(
        compute_state::ControlState::new(store.clone()),
    ));
    let mut config = DaemonConfig::new(dir, store, artifacts);
    config.provider = std::sync::Arc::new(common::provider());
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

async fn harness() -> Harness {
    let state = tempfile::tempdir().unwrap();
    let provider = Arc::new(common::provider());
    let mut config = memory_config(state.path());
    config.provider = provider.clone();
    config.restart_delay = Duration::from_millis(100);
    Harness {
        daemon: Daemon::start(config).await.unwrap(),
        provider,
        _state: state,
    }
}

async fn state(daemon: &Daemon, environment: &str, project: &str, workload: &str) -> ActualState {
    daemon
        .workload(environment, project, workload)
        .await
        .unwrap()
        .actual_state
}

/// The identity of a workload's current run: when it started. A running
/// service has no execution ID until it ends.
async fn execution(
    daemon: &Daemon,
    environment: &str,
    project: &str,
    workload: &str,
) -> Option<chrono::DateTime<chrono::Utc>> {
    daemon
        .workload(environment, project, workload)
        .await
        .unwrap()
        .started_at
}

/// Wait until every listed service is running.
async fn running(daemon: &Daemon, services: &[(&str, &str, &str)]) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let mut all = true;
        for (environment, project, workload) in services {
            all &= state(daemon, environment, project, workload).await == ActualState::Running;
        }
        if all {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "services did not start"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for(daemon: &Daemon, key: (&str, &str, &str), wanted: ActualState) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while state(daemon, key.0, key.1, key.2).await != wanted {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{key:?} never became {wanted:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn two_environments(harness: &Harness) {
    for name in ["preprod", "prod"] {
        harness
            .daemon
            .create_environment(environment(name))
            .await
            .unwrap();
        for project_name in ["authboundry", "factory"] {
            harness
                .daemon
                .add_project(name, project(project_name, vec![service("api")]))
                .await
                .unwrap();
        }
    }
    running(
        &harness.daemon,
        &[
            ("preprod", "authboundry", "api"),
            ("preprod", "factory", "api"),
            ("prod", "authboundry", "api"),
            ("prod", "factory", "api"),
        ],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_a_child_never_stops_its_parent_or_siblings() {
    let harness = harness().await;
    two_environments(&harness).await;
    let daemon = &harness.daemon;
    let instance = daemon.status().await.instance_id;
    let before = [
        execution(daemon, "preprod", "factory", "api").await,
        execution(daemon, "prod", "authboundry", "api").await,
        execution(daemon, "prod", "factory", "api").await,
    ];

    daemon
        .set_project_state("preprod", "authboundry", DesiredState::Stopped, false)
        .await
        .unwrap();

    assert_eq!(
        daemon.status().await.instance_id,
        instance,
        "Compute keeps running"
    );
    assert_eq!(
        state(daemon, "preprod", "authboundry", "api").await,
        ActualState::Stopped
    );
    assert_eq!(
        daemon.environment("preprod").await.unwrap().actual_state,
        ActualState::Running
    );
    assert_eq!(
        state(daemon, "preprod", "factory", "api").await,
        ActualState::Running
    );
    assert_eq!(
        daemon.environment("prod").await.unwrap().actual_state,
        ActualState::Running
    );
    assert_eq!(
        state(daemon, "prod", "authboundry", "api").await,
        ActualState::Running
    );
    assert_eq!(
        state(daemon, "prod", "factory", "api").await,
        ActualState::Running
    );
    let after = [
        execution(daemon, "preprod", "factory", "api").await,
        execution(daemon, "prod", "authboundry", "api").await,
        execution(daemon, "prod", "factory", "api").await,
    ];
    assert_eq!(before, after, "no sibling was restarted");
    let project = daemon.project("preprod", "authboundry").await.unwrap();
    assert_eq!(project.desired_state, DesiredState::Stopped);
    assert_eq!(project.actual_state, ActualState::Stopped);

    // Stopping a whole environment leaves the other untouched.
    daemon
        .set_environment_state("preprod", DesiredState::Stopped, false)
        .await
        .unwrap();
    assert_eq!(
        state(daemon, "preprod", "factory", "api").await,
        ActualState::Stopped
    );
    assert_eq!(
        daemon.environment("preprod").await.unwrap().actual_state,
        ActualState::Stopped
    );
    assert_eq!(
        state(daemon, "prod", "factory", "api").await,
        ActualState::Running
    );
    assert_eq!(execution(daemon, "prod", "factory", "api").await, after[2]);

    // Starting the environment again restores exactly what should run:
    // the project stopped on its own stays stopped.
    daemon
        .set_environment_state("preprod", DesiredState::Running, false)
        .await
        .unwrap();
    running(daemon, &[("preprod", "factory", "api")]).await;
    assert_eq!(
        state(daemon, "preprod", "authboundry", "api").await,
        ActualState::Stopped
    );
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restarts_are_scoped_and_projects_are_independent() {
    let harness = harness().await;
    two_environments(&harness).await;
    let daemon = &harness.daemon;
    let untouched = [
        ("preprod", "factory"),
        ("prod", "authboundry"),
        ("prod", "factory"),
    ];
    let mut before = vec![];
    for (environment, project) in untouched {
        before.push(execution(daemon, environment, project, "api").await);
    }
    let restarted_before = execution(daemon, "preprod", "authboundry", "api").await;

    daemon
        .set_project_state("preprod", "authboundry", DesiredState::Running, true)
        .await
        .unwrap();
    running(daemon, &[("preprod", "authboundry", "api")]).await;
    assert_ne!(
        execution(daemon, "preprod", "authboundry", "api").await,
        restarted_before
    );
    for ((environment, project), previous) in untouched.iter().zip(&before) {
        assert_eq!(
            &execution(daemon, environment, project, "api").await,
            previous
        );
    }

    // Replacing a project revision touches only that project.
    let mut upgraded = project("authboundry", vec![service("api"), service("worker")]);
    upgraded.revision = "rev-2".into();
    let view = daemon.add_project("prod", upgraded).await.unwrap();
    assert_eq!(view.revision, "rev-2");
    running(
        daemon,
        &[
            ("prod", "authboundry", "api"),
            ("prod", "authboundry", "worker"),
        ],
    )
    .await;
    assert_eq!(execution(daemon, "prod", "factory", "api").await, before[2]);
    assert_eq!(
        execution(daemon, "preprod", "factory", "api").await,
        before[0]
    );

    // Removing a project leaves the rest running.
    daemon.remove_project("prod", "authboundry").await.unwrap();
    assert!(daemon.project("prod", "authboundry").await.is_err());
    assert_eq!(
        state(daemon, "prod", "factory", "api").await,
        ActualState::Running
    );
    assert_eq!(daemon.environment("prod").await.unwrap().project_count, 1);
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn environments_are_isolated_configuration_ports_and_state() {
    let harness = harness().await;
    let daemon = &harness.daemon;
    for name in ["preprod", "prod"] {
        daemon.create_environment(environment(name)).await.unwrap();
        let mut api = service("api");
        api.ports = vec![PortSpec {
            name: "http".into(),
            port: 8000,
        }];
        api.readiness = Some(process_readiness());
        daemon
            .add_project(name, project("authboundry", vec![api]))
            .await
            .unwrap();
    }
    running(
        daemon,
        &[
            ("preprod", "authboundry", "api"),
            ("prod", "authboundry", "api"),
        ],
    )
    .await;
    let preprod = daemon
        .workload("preprod", "authboundry", "api")
        .await
        .unwrap();
    let prod = daemon.workload("prod", "authboundry", "api").await.unwrap();
    assert_eq!(preprod.ports[0].logical, 8000);
    assert_eq!(prod.ports[0].logical, 8000);
    assert_ne!(
        preprod.ports[0].host, prod.ports[0].host,
        "no shared host port"
    );
    assert_ne!(preprod.workload_id, prod.workload_id);
    assert_ne!(preprod.log_directory, prod.log_directory);
    // Each instance listens on its own port behind its stable endpoint.
    let instance_port = |deployment: String| async move {
        daemon.deployment(&deployment).await.unwrap().instances[0]
            .record
            .ports[0]
            .host
    };
    let preprod_instance = instance_port(preprod.deployment_id.clone()).await;
    let prod_instance = instance_port(prod.deployment_id.clone()).await;
    assert_ne!(preprod_instance, prod_instance);

    // Each environment's configuration reaches only its own workloads.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let (pre, _) = daemon.logs("preprod", "authboundry", "api").await.unwrap();
        let (pro, _) = daemon.logs("prod", "authboundry", "api").await.unwrap();
        if pre.contains("started") && pro.contains("started") {
            assert!(pre.contains("env=preprod") && !pre.contains("env=prod "));
            assert!(pro.contains("env=prod") && !pro.contains("env=preprod"));
            assert!(pre.contains(&format!("port={preprod_instance}")));
            assert!(pro.contains(&format!("port={prod_instance}")));
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "no service output");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Reserved configuration is refused.
    let mut reserved = environment("staging");
    reserved
        .env
        .insert("COMPUTE_WORK_DIR".into(), "/etc".into());
    assert!(daemon.create_environment(reserved).await.is_err());

    // Ordinary names, not a hard-coded set.
    for name in ["development", "staging", "review-123", "customer-acme"] {
        daemon.create_environment(environment(name)).await.unwrap();
    }
    assert!(
        daemon
            .create_environment(environment("Prod!"))
            .await
            .is_err()
    );
    assert!(
        daemon
            .create_environment(environment("prod"))
            .await
            .is_err(),
        "duplicate"
    );
    assert_eq!(daemon.environments().await.unwrap().len(), 6);
    daemon.destroy_environment("review-123").await.unwrap();
    assert_eq!(daemon.environments().await.unwrap().len(), 5);
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn services_survive_tasks_and_failures_stay_contained() {
    let harness = harness().await;
    let daemon = &harness.daemon;
    daemon
        .create_environment(environment("preprod"))
        .await
        .unwrap();
    // Both pass readiness, then fail.
    let mut flaky = service("flaky");
    flaky.bundle = bundle(
        RuntimeKind::Shell,
        "main.sh",
        "echo failing; sleep 1; exit 3",
    );
    flaky.restart = RestartPolicy::OnFailure;
    let mut crash = service("crash");
    crash.bundle = bundle(RuntimeKind::Shell, "main.sh", "sleep 1; exit 4");
    crash.restart = RestartPolicy::Never;
    daemon
        .add_project(
            "preprod",
            project(
                "factory",
                vec![
                    service("api"),
                    flaky,
                    crash,
                    task("migrate", "echo migrated"),
                    task("broken", "echo broken >&2; exit 9"),
                ],
            ),
        )
        .await
        .unwrap();
    running(daemon, &[("preprod", "factory", "api")]).await;
    let api = execution(daemon, "preprod", "factory", "api").await;

    let migrated = daemon
        .run_task("preprod", "factory", "migrate")
        .await
        .unwrap();
    assert_eq!(migrated.record.exit_code, Some(0));
    assert_eq!(migrated.stdout, "migrated\n");
    assert_eq!(
        state(daemon, "preprod", "factory", "migrate").await,
        ActualState::Completed
    );
    let broken = daemon
        .run_task("preprod", "factory", "broken")
        .await
        .unwrap();
    assert_eq!(broken.record.exit_code, Some(9));
    assert_eq!(
        state(daemon, "preprod", "factory", "broken").await,
        ActualState::Failed
    );

    // A crashed service is failed and held; its siblings keep running.
    wait_for(daemon, ("preprod", "factory", "crash"), ActualState::Failed).await;
    assert_eq!(
        state(daemon, "preprod", "factory", "api").await,
        ActualState::Running
    );
    assert_eq!(
        execution(daemon, "preprod", "factory", "api").await,
        api,
        "tasks and failures did not restart the service"
    );
    let view = daemon.project("preprod", "factory").await.unwrap();
    assert_eq!(view.actual_state, ActualState::Degraded);
    assert_eq!(
        daemon.environment("preprod").await.unwrap().actual_state,
        ActualState::Degraded
    );

    // on_failure restarts only the failing service.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while daemon
        .workload("preprod", "factory", "flaky")
        .await
        .unwrap()
        .restarts
        < 2
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "on_failure did not restart"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(execution(daemon, "preprod", "factory", "api").await, api);

    // An explicit start releases the held failure.
    let crash_before = execution(daemon, "preprod", "factory", "crash").await;
    daemon
        .set_workload_state("preprod", "factory", "crash", DesiredState::Running, false)
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while execution(daemon, "preprod", "factory", "crash").await == crash_before {
        assert!(
            tokio::time::Instant::now() < deadline,
            "explicit start did not rerun"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn environment_policy_is_admitted_before_anything_deploys() {
    let harness = harness().await;
    let daemon = &harness.daemon;
    let mut prod = environment("prod");
    prod.policy = Some(
        compute_policy::Policy::from_json(
            br#"{"version": 1, "name": "prod-policy", "allowed_runtimes": ["wasm"], "limits": {"max_timeout_ms": 1000}}"#,
        )
        .unwrap(),
    );
    daemon.create_environment(prod).await.unwrap();
    daemon
        .create_environment(environment("preprod"))
        .await
        .unwrap();
    daemon
        .add_project(
            "preprod",
            project("attn", vec![service("api"), task("lint", "echo ok")]),
        )
        .await
        .unwrap();
    running(daemon, &[("preprod", "attn", "api")]).await;
    // The daemon reports a service running just before the runtime counts
    // its execution; wait for the count to settle.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while harness.provider.executions_started() < 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "preprod api never executed"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let started = harness.provider.executions_started();

    // The same revision is refused by prod's policy at admission: the
    // deployment fails, nothing runs, and prod is unchanged.
    let refused = daemon
        .add_project(
            "prod",
            project("attn", vec![service("api"), task("lint", "echo ok")]),
        )
        .await;
    assert!(
        matches!(&refused, Err(EnvironmentError::Denied(message)) if message.contains("not allowed by policy")),
        "{refused:?}"
    );
    assert!(
        daemon.project("prod", "attn").await.is_err(),
        "no membership"
    );
    let deployments = daemon
        .deployments(Some("prod".into()), Some("attn".into()), None)
        .await
        .unwrap();
    assert_eq!(deployments.len(), 1);
    let failed = &deployments[0].record;
    assert_eq!(failed.status, DeploymentStatus::Failed);
    assert!(failed.workloads.iter().all(|workload| !workload.admitted));
    assert!(
        failed
            .workloads
            .iter()
            .all(|workload| workload.admission_id.is_some())
    );
    assert_eq!(
        harness.provider.executions_started(),
        started,
        "denials never reached the runtime"
    );
    let preprod = daemon.environment("preprod").await.unwrap();
    let prod = daemon.environment("prod").await.unwrap();
    assert_ne!(preprod.policy_id, prod.policy_id);
    assert_eq!(preprod.actual_state, ActualState::Running);
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn receipts_carry_environment_project_workload_and_execution() {
    let harness = harness().await;
    let daemon = &harness.daemon;
    let view = daemon
        .create_environment(environment("prod"))
        .await
        .unwrap();
    daemon
        .add_project(
            "prod",
            project(
                "factory",
                vec![service("api"), task("migrate", "echo done")],
            ),
        )
        .await
        .unwrap();
    let execution = daemon.run_task("prod", "factory", "migrate").await.unwrap();
    let record = &execution.record;
    let receipt_id = record.receipt_id.clone().unwrap();
    // The receipt is evidence: a durable artifact, referenced by ID.
    let stored = daemon.receipt(&receipt_id).await.unwrap();
    let receipt: compute_core::ExecutionReceipt = serde_json::from_slice(&stored).unwrap();
    receipt.verify().unwrap();
    // Served exactly as stored: its canonical encoding verifies offline.
    assert_eq!(receipt.encoded_bytes().unwrap(), stored);
    let scope = receipt.scope.as_ref().unwrap();
    // application → deployment → execution, in the receipt itself.
    assert_eq!(scope.deployment_id, record.deployment_id);
    assert!(scope.deployment_id.is_some());
    assert_eq!(
        receipt
            .application
            .as_ref()
            .map(|application| application.name.as_str()),
        Some("factory")
    );
    assert_eq!(scope.environment, "prod");
    assert_eq!(scope.environment_id, view.environment_id);
    assert_eq!(scope.project, "factory");
    assert_eq!(scope.revision, "rev-1");
    assert_eq!(scope.workload, "migrate");
    assert_eq!(scope.workload_kind, "task");
    assert_eq!(receipt.execution_id.0, record.execution_id);
    assert_eq!(receipt.admission_id, record.admission_id);
    assert_eq!(receipt.policy_id, record.policy_id);
    assert!(receipt.placement.is_some());
    assert_eq!(receipt.receipt_hash.0, receipt_id);
    assert_eq!(
        daemon.execution(&record.execution_id).await.unwrap(),
        execution
    );
    let references = daemon.receipts("prod", "factory", 10).await.unwrap();
    assert_eq!(references.len(), 1);
    assert_eq!(references[0].execution_id, record.execution_id);
    assert_eq!(references[0].admission_id, record.admission_id);
    assert!(record.deployment_id.is_some());

    // Services record evidence too once they finish.
    running(daemon, &[("prod", "factory", "api")]).await;
    daemon
        .set_workload_state("prod", "factory", "api", DesiredState::Stopped, false)
        .await
        .unwrap();
    let api = daemon.workload("prod", "factory", "api").await.unwrap();
    assert_eq!(api.actual_state, ActualState::Stopped);
    assert_eq!(api.evidence.receipt_ids.len(), 1);
    assert_eq!(api.placement.provider.as_deref(), Some("local"));
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn desired_state_persists_across_daemon_restarts() {
    let state = tempfile::tempdir().unwrap();
    let windows = memory_config(std::path::Path::new("/nonexistent"));
    let config = || {
        let store = Arc::new(
            compute_state_file::FileState::open(state.path().join("control-state.json")).unwrap(),
        );
        let artifacts = Arc::new(compute_state_file::DirectoryArtifacts::new(
            state.path().join("artifacts"),
        ));
        let mut config = DaemonConfig::new(state.path().join("node"), store, artifacts);
        config.provider = std::sync::Arc::new(common::provider());
        config.restart_delay = Duration::from_millis(100);
        config.port_range = windows.port_range;
        config.instance_port_range = windows.instance_port_range;
        config
    };
    let first = Daemon::start(config()).await.unwrap();
    let created = first.create_environment(environment("prod")).await.unwrap();
    first
        .add_project(
            "prod",
            project("attn", vec![service("api"), service("worker")]),
        )
        .await
        .unwrap();
    first
        .set_workload_state("prod", "attn", "worker", DesiredState::Stopped, false)
        .await
        .unwrap();
    running(&first, &[("prod", "attn", "api")]).await;
    first.shutdown().await;
    drop(first);

    let second = Daemon::start(config()).await.unwrap();
    running(&second, &[("prod", "attn", "api")]).await;
    let view = second.environment("prod").await.unwrap();
    assert_eq!(view.environment_id, created.environment_id);
    assert_eq!(state_of(&second, "worker").await, ActualState::Stopped);
    second.shutdown().await;

    async fn state_of(daemon: &Daemon, workload: &str) -> ActualState {
        daemon
            .workload("prod", "attn", workload)
            .await
            .unwrap()
            .actual_state
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn environment_placement_participates_in_provider_selection() {
    // A remote provider joins the daemon's pool; an environment pinned to
    // it runs its tasks there, and admission still applies.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let jobs = tempfile::tempdir().unwrap();
    let mut server = compute_provider::ServerConfig::local(endpoint.clone());
    server.provider = Arc::new(
        LocalProvider::with_identity(compute_core::ProviderIdentity::Remote {
            id: endpoint.clone(),
            endpoint: endpoint.clone(),
        })
        .with_runtime_catalog(common::catalog()),
    );
    server.job_store = jobs.path().to_path_buf();
    let handle = tokio::spawn(async move {
        let _ = compute_provider::serve_listener(listener, server).await;
    });
    let state = tempfile::tempdir().unwrap();
    let mut config = memory_config(state.path());
    config.pool = Some(
        compute_placement::PoolConfig::parse(&format!(
            "[providers.local]\nkind = \"local\"\npriority = 10\n\n[providers.edge]\nkind = \"remote\"\nendpoint = \"{endpoint}\"\npriority = 1\n"
        ))
        .unwrap(),
    );
    let daemon = Daemon::start(config).await.unwrap();
    let mut pinned = environment("edge-env");
    pinned.provider = Some("edge".into());
    daemon.create_environment(pinned).await.unwrap();
    daemon
        .create_environment(environment("local-env"))
        .await
        .unwrap();
    for name in ["edge-env", "local-env"] {
        daemon
            .add_project(name, project("factory", vec![task("build", "echo built")]))
            .await
            .unwrap();
    }
    let remote = daemon
        .run_task("edge-env", "factory", "build")
        .await
        .unwrap();
    assert_eq!(remote.record.provider.as_deref(), Some("edge"));
    let local = daemon
        .run_task("local-env", "factory", "build")
        .await
        .unwrap();
    assert_eq!(
        local.record.provider.as_deref(),
        Some("local"),
        "priority selects local otherwise"
    );
    let mut unknown = environment("nowhere");
    unknown.provider = Some("missing".into());
    assert!(daemon.create_environment(unknown).await.is_err());
    daemon.shutdown().await;
    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_api_is_the_only_path_and_it_is_authorized() {
    // A development daemon with a shared token: every request, reads
    // included, must carry it.
    let state = tempfile::tempdir().unwrap();
    let provider = Arc::new(common::provider());
    let mut config = memory_config(state.path());
    config.provider = provider.clone();
    config.security.legacy_token = Some("operator".into());
    let harness = Harness {
        daemon: Daemon::start(config).await.unwrap(),
        provider,
        _state: state,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(api::serve(listener, harness.daemon.clone(), None));
    let anonymous = client::DaemonClient::new(&endpoint).unwrap();
    let operator = client::DaemonClient::new(&endpoint)
        .unwrap()
        .with_bearer_token("operator");

    assert!(matches!(
        anonymous.get::<DaemonStatus>("/status").await,
        Err(EnvironmentError::Unauthorized(_))
    ));
    let status: DaemonStatus = operator.get("/status").await.unwrap();
    assert_eq!(status.instance_id, harness.daemon.instance_id());
    assert!(matches!(
        anonymous
            .post::<_, EnvironmentView>("/environments", Some(&environment("prod")))
            .await,
        Err(EnvironmentError::Unauthorized(_))
    ));
    let created: EnvironmentView = operator
        .post("/environments", Some(&environment("prod")))
        .await
        .unwrap();
    let added: ProjectView = operator
        .post(
            "/environments/prod/projects",
            Some(&project("authboundry", vec![service("api")])),
        )
        .await
        .unwrap();
    assert_eq!(added.name, "authboundry");
    running(&harness.daemon, &[("prod", "authboundry", "api")]).await;
    let by_id: EnvironmentView = operator
        .get(&format!("/environments/{}", created.environment_id))
        .await
        .unwrap();
    assert_eq!(by_id.name, "prod");
    let listed: Vec<EnvironmentSummary> = operator.get("/environments").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].project_count, 1);
    let stopped: ProjectView = operator
        .post::<(), _>("/environments/prod/projects/authboundry/stop", None)
        .await
        .unwrap();
    assert_eq!(stopped.actual_state, ActualState::Stopped);
    let first: EnvironmentView = operator.get("/environments/prod/status").await.unwrap();
    let second: EnvironmentView = operator.get("/environments/prod/status").await.unwrap();
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&second).unwrap(),
        "deterministic JSON"
    );
    assert!(matches!(
        operator
            .get::<EnvironmentView>("/environments/missing")
            .await,
        Err(EnvironmentError::NotFound(_))
    ));
    let _: serde_json::Value = operator.delete("/environments/prod").await.unwrap();
    let listed: Vec<EnvironmentSummary> = operator.get("/environments").await.unwrap();
    assert!(listed.is_empty());
    let _: serde_json::Value = operator.post::<(), _>("/shutdown", None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn listening_services_are_healthy_through_their_environment_port() {
    let python = compute_runtime_available(RuntimeKind::Python).await;
    if !python {
        eprintln!("skipping: python is unavailable on this host");
        return;
    }
    let harness = harness().await;
    let daemon = &harness.daemon;
    daemon
        .create_environment(environment("prod"))
        .await
        .unwrap();
    let web = WorkloadDefinition {
        name: "web".into(),
        kind: WorkloadKind::Service,
        bundle: bundle(
            RuntimeKind::Python,
            "main.py",
            "import http.server, os\nclass H(http.server.BaseHTTPRequestHandler):\n    def do_GET(self):\n        body = os.environ['ENVIRONMENT_NAME'].encode()\n        self.send_response(200); self.send_header('Content-Length', str(len(body))); self.end_headers(); self.wfile.write(body)\nhttp.server.HTTPServer(('127.0.0.1', int(os.environ['PORT'])), H).serve_forever()\n",
        ),
        ports: vec![PortSpec {
            name: "http".into(),
            port: 8000,
        }],
        restart: RestartPolicy::Never,
        desired_state: DesiredState::Running,
        readiness: None,
    };
    // A service that never listens is released on process readiness, and
    // then reports unhealthy.
    let mut silent = service("silent");
    silent.ports = vec![PortSpec {
        name: "http".into(),
        port: 8000,
    }];
    silent.readiness = Some(process_readiness());
    daemon
        .add_project("prod", project("appport-services", vec![web, silent]))
        .await
        .unwrap();
    running(
        daemon,
        &[
            ("prod", "appport-services", "web"),
            ("prod", "appport-services", "silent"),
        ],
    )
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let web = loop {
        let view = daemon
            .workload("prod", "appport-services", "web")
            .await
            .unwrap();
        if view.health == Health::Healthy {
            break view;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "web never became healthy"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", web.ports[0].host))
        .await
        .unwrap();
    stream.write_all(b"GET / HTTP/1.0\r\n\r\n").await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    assert!(response.ends_with("prod"), "{response}");
    let silent = daemon
        .workload("prod", "appport-services", "silent")
        .await
        .unwrap();
    assert_eq!(
        silent.health,
        Health::Unhealthy,
        "a declared port nobody listens on"
    );
    assert_eq!(
        daemon
            .project("prod", "appport-services")
            .await
            .unwrap()
            .health,
        Health::Unhealthy
    );
    daemon.shutdown().await;
}

async fn compute_runtime_available(kind: RuntimeKind) -> bool {
    compute_runtime::Compute::new()
        .runtime(kind, None)
        .await
        .is_ok_and(|runtime| runtime.available)
}
