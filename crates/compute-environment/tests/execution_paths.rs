//! The daemon-host execution paths that remain, pinned to what the code
//! does (docs/architecture.md, "Every way Compute executes software").
//!
//! Node environments are the one durable deployment model that still runs
//! outside a Computer (G-ARCH-5, blocked on named Computer capabilities).
//! These tests hold its boundary so it cannot grow or blur:
//!
//! - a node service always executes on the daemon's own host, through the
//!   supervisor, and its evidence is the daemon's own ExecutionRecord and
//!   ReceiptRecord — never a Computer, session, or target job;
//! - placement that selects anything else is refused, not followed;
//! - a node task pinned to a target runs there as a one-shot request,
//!   outside any Computer or session;
//! - the node model never enters an environment that has a Computer.
//!
//! When node environments converge, these tests are replaced by the
//! convergence regression `tests/applications.rs` uses.

mod common;
#[path = "common/target.rs"]
mod target;

use std::sync::Arc;
use std::time::Duration;

use compute_core::{ComputerLifecycle, WorkloadBundle, WorkloadSpec};
use compute_environment::*;
use compute_state::{Collection, ComputerRecord, ControlState, Query, ReceiptRecord, StateStore};
use compute_state_memory::MemoryState;
use target::*;

fn bundle(source: &str) -> Vec<u8> {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("main.py"), source).unwrap();
    let workload: WorkloadSpec = serde_json::from_value(serde_json::json!({
        "version": "1", "runtime": "python", "entrypoint": "main.py", "network": "network",
    }))
    .unwrap();
    WorkloadBundle::create_from(workload, directory.path())
        .unwrap()
        .to_bytes()
        .unwrap()
}

fn service() -> WorkloadDefinition {
    WorkloadDefinition {
        name: "web".into(),
        kind: WorkloadKind::Service,
        bundle: bundle(
            "import os, http.server\n\
             http.server.HTTPServer(('127.0.0.1', int(os.environ['PORT'])), \
             http.server.SimpleHTTPRequestHandler).serve_forever()\n",
        ),
        ports: vec![PortSpec {
            name: "http".into(),
            port: 8080,
        }],
        restart: RestartPolicy::Never,
        desired_state: DesiredState::Running,
        readiness: None,
    }
}

fn task() -> WorkloadDefinition {
    WorkloadDefinition {
        name: "job".into(),
        kind: WorkloadKind::Task,
        bundle: bundle("print('ran')\n"),
        ports: vec![],
        restart: RestartPolicy::Never,
        desired_state: DesiredState::Running,
        readiness: None,
    }
}

async fn node_environment(daemon: &Arc<Daemon>, provider: Option<&str>) {
    daemon
        .create_environment(EnvironmentDefinition {
            name: "legacy".into(),
            desired_state: DesiredState::Running,
            env: Default::default(),
            policy: None,
            provider: provider.map(str::to_owned),
        })
        .await
        .unwrap();
}

async fn release(
    daemon: &Arc<Daemon>,
    label: &str,
    workloads: Vec<WorkloadDefinition>,
) -> DeploymentView {
    daemon
        .register_revision(
            "site",
            RevisionDefinition {
                revision: label.into(),
                source: None,
                workloads,
            },
        )
        .await
        .unwrap();
    let started = daemon
        .deploy(DeployRequest {
            project: "site".into(),
            environment: "legacy".into(),
            revision: Some(label.into()),
            ..DeployRequest::default()
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let view = daemon.deployment(&started.deployment_id).await.unwrap();
        if view.record.status.is_terminal() {
            return view;
        }
        assert!(tokio::time::Instant::now() < deadline, "{view:#?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn count<T: compute_state::Document>(
    store: &Arc<dyn StateStore>,
    collection: Collection,
) -> usize {
    ControlState::new(store.clone())
        .query::<T>(Query::all(collection))
        .await
        .unwrap()
        .len()
}

/// A node service is a durable deployment that runs on the daemon's own
/// host, supervised there, with the daemon's own evidence: the boundary of
/// G-ARCH-5, stated as what the code does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_service_is_a_daemon_host_deployment_outside_any_computer() {
    let target = Target::start();
    let store: Arc<dyn StateStore> = Arc::new(MemoryState::new());
    // The pool has a Computer target; the node model does not use it.
    let (daemon, _node) = start_daemon(store.clone(), Some(pool(&target))).await;
    node_environment(&daemon, None).await;
    let released = release(&daemon, "v1", vec![service(), task()]).await;
    assert_eq!(
        released.record.status,
        DeploymentStatus::Complete,
        "{released:#?}"
    );
    // Durable deployment semantics: desired state, a revision, a stable
    // endpoint, recorded in control state.
    let web = daemon.workload("legacy", "site", "web").await.unwrap();
    let port = web.ports[0].host;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_err()
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the node endpoint never answered"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Executed on the daemon's own node: the only provider a node service
    // may use.
    for workload in &released.record.workloads {
        assert_eq!(workload.provider.as_deref(), Some("local"), "{workload:#?}");
    }
    // Its evidence is the daemon's: an execution record and a receipt in
    // control state, for the task it ran.
    let ran = daemon.run_task("legacy", "site", "job").await.unwrap();
    assert_eq!(ran.record.provider.as_deref(), Some("local"));
    let receipt = ran.record.receipt_id.clone().expect("a receipt");
    assert!(
        daemon.receipt(&receipt).await.is_ok(),
        "a control-state receipt"
    );
    assert!(count::<ReceiptRecord>(&store, Collection::Receipt).await >= 1);
    // And nothing of the Computer model: no computer, no ComputerRecord,
    // no session on the target.
    assert!(daemon.computer("legacy").await.is_err());
    assert_eq!(
        count::<ComputerRecord>(&store, Collection::Computer).await,
        0
    );
    assert!(target.client().sessions().await.unwrap().is_empty());
    daemon.shutdown().await;
}

/// Placement cannot move a node service: when it selects anything but the
/// daemon's own node, the release fails rather than follow it. A node
/// service's host is the daemon host by construction, not by placement.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn placement_that_selects_a_target_for_a_node_service_is_refused() {
    let target = Target::start();
    let store: Arc<dyn StateStore> = Arc::new(MemoryState::new());
    let (daemon, _node) = start_daemon(store.clone(), Some(pool(&target))).await;
    node_environment(&daemon, Some("target-a")).await;
    let released = release(&daemon, "v1", vec![service()]).await;
    assert_eq!(released.record.status, DeploymentStatus::Failed);
    assert!(
        released
            .record
            .failure
            .as_deref()
            .is_some_and(|failure| failure.contains("services run on the daemon's own node")),
        "{:?}",
        released.record.failure
    );
    assert!(target.client().sessions().await.unwrap().is_empty());
    assert_eq!(
        count::<ComputerRecord>(&store, Collection::Computer).await,
        0
    );
    daemon.shutdown().await;
}

/// A node task pinned to a target runs there as a one-shot request: on the
/// target, authenticated with the pool's credential, but outside any
/// Computer or session, with the daemon's own record and receipt as its
/// evidence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_task_pinned_to_a_target_runs_outside_any_computer_or_session() {
    let target = Target::start();
    let store: Arc<dyn StateStore> = Arc::new(MemoryState::new());
    let (daemon, _node) = start_daemon(store.clone(), Some(pool(&target))).await;
    node_environment(&daemon, Some("target-a")).await;
    let released = release(&daemon, "v1", vec![task()]).await;
    assert_eq!(
        released.record.status,
        DeploymentStatus::Complete,
        "{released:#?}"
    );
    let ran = daemon.run_task("legacy", "site", "job").await.unwrap();
    assert_eq!(ran.record.status, "completed", "{ran:#?}");
    assert_eq!(ran.record.provider.as_deref(), Some("target-a"));
    let receipt = ran.record.receipt_id.clone().expect("a receipt");
    let stored: compute_core::ExecutionReceipt =
        serde_json::from_slice(&daemon.receipt(&receipt).await.unwrap()).unwrap();
    assert!(
        matches!(
            stored.provider,
            Some(compute_core::ProviderIdentity::Remote { ref endpoint, .. }) if *endpoint == target.endpoint
        ),
        "{:?}",
        stored.provider
    );
    assert!(
        target.client().sessions().await.unwrap().is_empty(),
        "no session"
    );
    assert_eq!(
        count::<ComputerRecord>(&store, Collection::Computer).await,
        0
    );
    daemon.shutdown().await;
}

/// The node model never enters an environment with a Computer: a bundle
/// release there is refused before anything is recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_node_model_never_enters_a_computer_environment() {
    let target = Target::start();
    let store: Arc<dyn StateStore> = Arc::new(MemoryState::new());
    let (daemon, _node) = start_daemon(store.clone(), Some(pool(&target))).await;
    daemon
        .create_computer_environment(
            ComputerEnvironmentDefinition {
                name: "machine".into(),
                desired_state: DesiredState::Running,
                env: Default::default(),
                policy: None,
                computer: ComputerRequest {
                    lifecycle: ComputerLifecycle::Persistent,
                    requirements: Default::default(),
                    target: None,
                    ttl_seconds: None,
                },
                contents: Default::default(),
            },
            "alice",
        )
        .await
        .unwrap();
    daemon
        .register_revision(
            "site",
            RevisionDefinition {
                revision: "v1".into(),
                source: None,
                workloads: vec![task()],
            },
        )
        .await
        .unwrap();
    let refused = daemon
        .deploy(DeployRequest {
            project: "site".into(),
            environment: "machine".into(),
            revision: Some("v1".into()),
            ..DeployRequest::default()
        })
        .await;
    assert!(
        matches!(&refused, Err(EnvironmentError::Invalid(message)) if message.contains("runs on its own computer")),
        "{refused:?}"
    );
    assert!(
        daemon
            .deployments(Some("machine".into()), None, None)
            .await
            .unwrap()
            .is_empty()
    );
    daemon.shutdown().await;
}
