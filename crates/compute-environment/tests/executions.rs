//! Execution correctness: every accepted execution of a task terminalizes
//! exactly once with its own record and receipt, however many clients run
//! the same task at the same time.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use compute_core::{
    IsolationRequirement, NetworkPolicy, ResourceLimits, RuntimeKind, WORKLOAD_SPEC_VERSION,
    WorkloadBundle, WorkloadSpec,
};
use compute_environment::*;
use compute_provider::LocalProvider;

fn bundle(script: &str) -> Vec<u8> {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("main.sh"), script).unwrap();
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

async fn daemon_with_task(script: &str) -> (Arc<Daemon>, Arc<LocalProvider>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(compute_state_memory::MemoryState::new());
    let artifacts = Arc::new(compute_state::StateArtifacts::new(
        compute_state::ControlState::new(store.clone()),
    ));
    let provider = Arc::new(common::provider());
    let mut config = DaemonConfig::new(dir.path(), store, artifacts);
    config.provider = provider.clone();
    config.port_range = (26000, 26099);
    config.instance_port_range = (46000, 46099);
    let daemon = Daemon::start(config).await.unwrap();
    daemon
        .create_environment(EnvironmentDefinition {
            name: "prod".into(),
            desired_state: DesiredState::Running,
            env: BTreeMap::new(),
            policy: None,
            provider: None,
        })
        .await
        .unwrap();
    daemon
        .add_project(
            "prod",
            ProjectDefinition {
                name: "jobs".into(),
                revision: "rev-1".into(),
                source: None,
                desired_state: DesiredState::Running,
                env: BTreeMap::new(),
                workloads: vec![WorkloadDefinition {
                    name: "work".into(),
                    kind: WorkloadKind::Task,
                    bundle: bundle(script),
                    ports: vec![],
                    restart: RestartPolicy::Never,
                    desired_state: DesiredState::Running,
                    readiness: None,
                }],
            },
        )
        .await
        .unwrap();
    (daemon, provider, dir)
}

/// `clients` concurrent clients each run the same task `runs` times.
async fn concurrent_runs(
    daemon: &Arc<Daemon>,
    clients: usize,
    runs: usize,
) -> Vec<Result<ExecutionView, EnvironmentError>> {
    let mut handles = vec![];
    for _ in 0..clients {
        let daemon = daemon.clone();
        handles.push(tokio::spawn(async move {
            let mut results = vec![];
            for _ in 0..runs {
                results.push(daemon.run_task("prod", "jobs", "work").await);
            }
            results
        }));
    }
    let mut results = vec![];
    for handle in handles {
        results.extend(handle.await.unwrap());
    }
    results
}

async fn assert_every_execution_has_evidence(
    daemon: &Daemon,
    provider: &LocalProvider,
    results: &[Result<ExecutionView, EnvironmentError>],
) {
    let total = results.len();
    let failures = results
        .iter()
        .filter_map(|result| result.as_ref().err())
        .map(|error| format!("{}: {}", error.kind(), error.message()))
        .collect::<Vec<_>>();
    assert!(
        failures.is_empty(),
        "{} of {total} failed: {:?}",
        failures.len(),
        &failures[..failures.len().min(3)]
    );
    let views = results
        .iter()
        .map(|result| result.as_ref().unwrap())
        .collect::<Vec<_>>();
    // Every execution is its own: unique IDs, each completed, each with a
    // receipt.
    let ids = views
        .iter()
        .map(|view| view.record.execution_id.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(ids.len(), total, "execution IDs are unique");
    assert!(views.iter().all(|view| view.record.status == "completed"));
    assert!(views.iter().all(|view| view.stdout.contains("ran")));
    let receipts = views
        .iter()
        .filter_map(|view| view.record.receipt_id.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(receipts.len(), total, "every execution has its own receipt");
    assert_eq!(provider.executions_started(), total as u64, "each ran once");
    // Durable: every record and receipt reference is in control state.
    let durable = daemon.executions("prod", "jobs", usize::MAX).await.unwrap();
    assert_eq!(durable.len(), total, "durable execution records");
    assert_eq!(
        durable
            .iter()
            .map(|record| record.execution_id.clone())
            .collect::<BTreeSet<_>>(),
        ids
    );
    let references = daemon.receipts("prod", "jobs", usize::MAX).await.unwrap();
    assert_eq!(references.len(), total, "durable receipt references");
    // Each receipt document is present and verifies.
    for view in views.iter().step_by((total / 20).max(1)) {
        let receipt: compute_core::ExecutionReceipt = serde_json::from_slice(
            &daemon
                .receipt(view.record.receipt_id.as_ref().unwrap())
                .await
                .unwrap(),
        )
        .unwrap();
        receipt.verify().unwrap();
        assert_eq!(receipt.execution_id.0, view.record.execution_id);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eighty_concurrent_runs_of_one_task_each_produce_one_receipt() {
    let (daemon, provider, _dir) = daemon_with_task("echo ran; sleep 0.05").await;
    let results = concurrent_runs(&daemon, 8, 10).await;
    assert_eq!(results.len(), 80);
    assert_every_execution_has_evidence(&daemon, &provider, &results).await;
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_thousand_concurrent_runs_lose_no_evidence() {
    let (daemon, provider, _dir) = daemon_with_task("echo ran").await;
    let results = concurrent_runs(&daemon, 100, 10).await;
    assert_eq!(results.len(), 1000);
    assert_every_execution_has_evidence(&daemon, &provider, &results).await;
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_run_is_a_workload_failure_not_a_denial() {
    let (daemon, _provider, _dir) = daemon_with_task("echo ran; exit 7").await;
    let results = concurrent_runs(&daemon, 4, 2).await;
    for result in results {
        let view = result.expect("the task executed; its failure is its own");
        assert_eq!(view.record.exit_code, Some(7));
        // The workload failed, not Compute: the kinds stay distinct.
        assert_eq!(view.record.failure.as_deref(), Some("workload_failed"));
        assert!(view.record.receipt_id.is_some());
    }
    tokio::time::sleep(Duration::from_millis(10)).await;
    daemon.shutdown().await;
}
