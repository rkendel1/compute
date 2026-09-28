//! G-ARCH-2: an application deployment is the canonical computer lifecycle,
//! and nothing else.
//!
//! ```text
//! application deploy
//!   → Environment `application-<name>` → ComputerRecord (placed, owned)
//!   → authenticated target session → durable target jobs (source import,
//!     checkout, process start) → Version (published) → Rollout (deployed)
//!   → the computer's endpoint → the target's receipt for the start job
//! ```
//!
//! These tests prove each canonical record exists and that the application
//! API names exactly those records, and they prove the old parallel path is
//! gone: no daemon-host supervisor, no revision/deployment/execution/receipt
//! records of the node model, no application-only state machine. Failures
//! of the target (bad credential, unreachable, lost, replaced) and of the
//! control plane (restart) are the computer's, with the computer's
//! semantics.

mod common;

use std::net::TcpListener as StdTcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use compute_core::{
    ApplicationArtifact, ApplicationDescription, ApplicationIdentity, ComputerStatus,
    ExecutionReceipt, JobStatus, ProcessState, WorkloadBundle, WorkloadSpec,
};
use compute_environment::*;
use compute_placement::{PoolConfig, ProviderConfig, ProviderKind};
use compute_provider::{RemoteProvider, ServerConfig, WorkspaceSessionProvider};
use compute_state::{
    Collection, ControlState, DeploymentRecord, EnvironmentProjectRecord, ExecutionRecord,
    ProjectRecord, ProjectRevisionRecord, Query, ReceiptRecord, RolloutKind, RolloutStatus,
    StateStore, VersionStatus,
};
use compute_state_memory::MemoryState;
use tokio::runtime::Runtime;

// ---- A target: `compute serve` hosting sessions, in a runtime of its own --

struct Target {
    runtime: Option<Runtime>,
    endpoint: String,
    address: std::net::SocketAddr,
    trust: PathBuf,
    token_file: PathBuf,
    stores: tempfile::TempDir,
    workspaces: tempfile::TempDir,
}

impl Target {
    /// A target that trusts one control plane: the token in `token_file`.
    fn start() -> Self {
        let stores = tempfile::tempdir().unwrap();
        let mut credentials = compute_provider::TargetCredentials::default();
        let (_, token) = credentials.issue("test-control-plane").unwrap();
        let trust = stores.path().join("credentials.json");
        credentials.save(&trust).unwrap();
        let token_file = stores.path().join("control-plane.token");
        compute_provider::credentials::write_token_file(&token_file, &token).unwrap();
        let socket = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        let mut target = Self {
            runtime: None,
            endpoint: format!("http://{address}"),
            address,
            trust,
            token_file,
            stores,
            workspaces: tempfile::tempdir().unwrap(),
        };
        target.serve(socket);
        target
    }

    fn serve(&mut self, socket: StdTcpListener) {
        socket.set_nonblocking(true).unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let endpoint = self.endpoint.clone();
        let mut config = ServerConfig::local(endpoint.clone());
        config.authorizer = Arc::new(compute_provider::TargetAuthorizer::from_file(
            self.trust.clone(),
        ));
        config.provider = Arc::new(
            compute_provider::LocalProvider::with_identity(
                compute_core::ProviderIdentity::Remote {
                    id: endpoint.clone(),
                    endpoint,
                },
            )
            .with_runtime_catalog(common::catalog()),
        );
        config.job_store = self.stores.path().join("jobs");
        config.session_store = self.stores.path().join("sessions");
        config.session_provider = Some(Arc::new(WorkspaceSessionProvider::new(
            self.workspaces.path(),
        )));
        config.execution.sessions = true;
        config.session_sweep = Duration::from_millis(100);
        runtime.spawn(async move {
            let listener = tokio::net::TcpListener::from_std(socket).unwrap();
            let _ = compute_provider::serve_listener(listener, config).await;
        });
        self.runtime = Some(runtime);
    }

    /// Stop answering, as a machine that went away does.
    fn stop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }

    /// Answer again on the same address, with the same stores.
    fn restart(&mut self) {
        self.stop();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let socket = loop {
            match StdTcpListener::bind(self.address) {
                Ok(socket) => break socket,
                Err(error) => {
                    assert!(std::time::Instant::now() < deadline, "rebind: {error}");
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        };
        self.serve(socket);
    }

    /// Lose a machine: its processes end and its workspace is gone, as a
    /// machine that vanished.
    fn lose_machine(&self, resource: &str) {
        let workspace = self.workspaces.path().join(resource);
        kill_processes(&workspace);
        std::fs::remove_dir_all(&workspace).unwrap();
    }

    fn client(&self) -> RemoteProvider {
        RemoteProvider::new(self.endpoint.clone()).with_bearer_token(
            compute_provider::credentials::read_token_file(&self.token_file).unwrap(),
        )
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        self.stop();
        // Processes the computers started outlive the target's runtime.
        if let Ok(entries) = std::fs::read_dir(self.workspaces.path()) {
            for entry in entries.flatten() {
                kill_processes(&entry.path());
            }
        }
    }
}

fn kill_processes(workspace: &Path) {
    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg(
            "for f in \"$1\"/.compute/processes/*.pid; do [ -e \"$f\" ] && kill -KILL -\"$(cat \"$f\")\" 2>/dev/null; done; true",
        )
        .arg("kill")
        .arg(workspace)
        .status();
}

fn member(target: &Target, token_file: &Path) -> ProviderConfig {
    ProviderConfig {
        kind: ProviderKind::Remote,
        endpoint: Some(target.endpoint.clone()),
        application_endpoint: None,
        priority: 0,
        token_env: None,
        token_file: Some(token_file.to_path_buf()),
    }
}

fn pool(target: &Target) -> PoolConfig {
    PoolConfig {
        pool: Default::default(),
        providers: [("target-a".to_owned(), member(target, &target.token_file))].into(),
    }
}

async fn start_daemon(
    store: Arc<dyn StateStore>,
    pool: Option<PoolConfig>,
) -> (Arc<Daemon>, tempfile::TempDir) {
    let node = tempfile::tempdir().unwrap();
    let artifacts = Arc::new(compute_state::StateArtifacts::new(ControlState::new(
        store.clone(),
    )));
    let mut config = DaemonConfig::new(node.path(), store, artifacts);
    config.provider = Arc::new(common::provider());
    config.pool = pool;
    config.reconcile_interval = Duration::from_millis(200);
    config.computer_probe = Duration::from_millis(400);
    config.computer_liveness = Duration::from_millis(300);
    config.computer_liveness_timeout = Duration::from_secs(3);
    // Endpoints this test's applications listen on: a window of its own.
    let base = {
        let probe = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        port.clamp(20_000, 60_000)
    };
    config.port_range = (base, base + 9);
    config.instance_port_range = (base + 10, base + 19);
    (Daemon::start(config).await.unwrap(), node)
}

// ---- An application ----------------------------------------------------

/// A Python HTTP application that answers `greeting`, packed as a portable
/// artifact exactly as `compute application pack` packs one.
fn artifact(name: &str, greeting: &str) -> (Vec<u8>, String) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("main.py"),
        format!(
            "import os\nfrom http.server import HTTPServer, BaseHTTPRequestHandler\n\
             class H(BaseHTTPRequestHandler):\n    def do_GET(self):\n        \
             self.send_response(200); self.end_headers(); self.wfile.write(b'{greeting}')\n\
             HTTPServer(('127.0.0.1', int(os.environ['PORT'])), H).serve_forever()\n"
        ),
    )
    .unwrap();
    let workload: WorkloadSpec = serde_json::from_value(serde_json::json!({
        "version": "1",
        "runtime": "python",
        "entrypoint": "main.py",
        "network": "network",
    }))
    .unwrap();
    let bundle = WorkloadBundle::create_from(workload, directory.path()).unwrap();
    let artifact = ApplicationArtifact::new(
        ApplicationIdentity::new(name, Some(3000)).unwrap(),
        ApplicationDescription {
            version: Some(greeting.into()),
            required_env: Default::default(),
            capabilities: ["http.hello".into()].into(),
            metadata: Default::default(),
        },
        &bundle,
    )
    .unwrap();
    (
        artifact.to_bytes().unwrap(),
        artifact.artifact_id().unwrap(),
    )
}

fn request(bytes: Vec<u8>) -> ApplicationDeployRequest {
    ApplicationDeployRequest {
        artifact: Some(ApplicationArtifactSource::Inline { data: bytes }),
        bundle: vec![],
        port: None,
        env: None,
        source: None,
        placement: None,
    }
}

async fn eventually<T>(what: &str, mut check: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(value) = check().await {
            return value;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A released version, once its rollout ended.
async fn released(daemon: &Arc<Daemon>, name: &str, id: &str) -> ApplicationDeploymentView {
    eventually("the release to end", async || {
        daemon
            .application_deployment(name, id)
            .await
            .ok()
            .filter(|view| view.state != ApplicationDeploymentState::Deploying)
    })
    .await
}

async fn deploy(
    daemon: &Arc<Daemon>,
    name: &str,
    operator: &str,
    greeting: &str,
) -> ApplicationDeploymentView {
    let started = daemon
        .deploy_application(name, operator, request(artifact(name, greeting).0))
        .await
        .unwrap();
    let view = released(daemon, name, &started.deployment_id).await;
    assert_eq!(view.state, ApplicationDeploymentState::Active, "{view:#?}");
    view
}

async fn fetch(url: &str) -> Option<String> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let response = client.get(url).send().await.ok()?;
    response.status().is_success().then_some(())?;
    response.text().await.ok()
}

async fn answers(url: &str, body: &str) {
    eventually(&format!("{url} to answer {body}"), async || {
        (fetch(url).await.as_deref() == Some(body)).then_some(())
    })
    .await
}

/// Every record of the node-environment model: what an application
/// deployment used to create, and must not any more.
async fn legacy_records(store: &Arc<dyn StateStore>) -> Vec<String> {
    let control = ControlState::new(store.clone());
    let mut found = vec![];
    macro_rules! scan {
        ($record:ty, $collection:expr) => {
            for stored in control
                .query::<$record>(Query::all($collection))
                .await
                .unwrap()
            {
                found.push(format!("{:?} {}", $collection, stored.id));
            }
        };
    }
    scan!(ProjectRecord, Collection::Project);
    scan!(ProjectRevisionRecord, Collection::ProjectRevision);
    scan!(EnvironmentProjectRecord, Collection::EnvironmentProject);
    scan!(DeploymentRecord, Collection::Deployment);
    scan!(ExecutionRecord, Collection::Execution);
    scan!(ReceiptRecord, Collection::Receipt);
    found
}

// ---- The architecture has converged ---------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_application_deployment_is_the_canonical_computer_lifecycle() {
    let target = Target::start();
    let store: Arc<dyn StateStore> = Arc::new(MemoryState::new());
    let (daemon, _node) = start_daemon(store.clone(), Some(pool(&target))).await;
    let name = "hello";
    let (_, artifact_id) = artifact(name, "v1");

    let v1 = deploy(&daemon, name, "alice", "v1").await;
    let records = v1.canonical.clone().expect("canonical records");

    // ✓ ComputerRecord: the application's own environment, placed on the
    //   pool's target and owned by the caller.
    assert_eq!(records.environment, application_environment(name));
    let computer = daemon.computer(&records.environment).await.unwrap();
    assert_eq!(computer.status, ComputerStatus::Running);
    assert_eq!(computer.owner, "alice");
    assert_eq!(computer.target.as_deref(), Some("target-a"));
    assert_eq!(
        records.computer_id,
        compute_state::ids::computer(&computer.environment_id)
    );
    // ✓ Canonical project and version: published from the imported source,
    //   bound to the artifact it came from.
    assert!(computer.desired.projects.iter().any(|p| p.name == name));
    let version = daemon.version(name, &records.version).await.unwrap();
    assert_eq!(version.version_id, records.version_id);
    assert_eq!(version.status, VersionStatus::Published);
    assert_eq!(version.commit, records.commit);
    assert!(version.package_digest.is_some());
    assert_eq!(
        version.artifact.as_ref().unwrap().artifact_id,
        artifact_id,
        "the version names its artifact"
    );
    assert_eq!(v1.artifact.as_ref().unwrap().artifact_id, artifact_id);
    assert_eq!(v1.runtime.as_deref(), Some("python"));
    // The computer holds that commit.
    assert_eq!(
        computer.observed.repositories[name].commit, version.commit,
        "the computer runs the version's commit"
    );
    // ✓ Canonical deployment: a rollout of that version.
    let rollout = daemon.rollout(&v1.deployment_id).await.unwrap();
    assert_eq!(rollout.rollout_id, records.rollout_id);
    assert_eq!(rollout.kind, RolloutKind::Deploy);
    assert_eq!(rollout.status, RolloutStatus::Active);
    assert_eq!(rollout.version_id, version.version_id);
    assert_eq!(rollout.environment_id, computer.environment_id);
    // ✓ Authenticated target session: the target has it, for this control
    //   plane's credential only.
    let client = target.client();
    let session_id = records.session_id.clone().unwrap();
    assert_eq!(computer.session_id.as_deref(), Some(session_id.as_str()));
    client.session(&session_id).await.unwrap();
    let stranger = RemoteProvider::new(target.endpoint.clone()).with_bearer_token("not-trusted");
    assert!(stranger.session(&session_id).await.is_err());
    // ✓ Canonical job/execution: the durable target job that started it, in
    //   that session.
    let job_id = records.job_id.clone().expect("the start job");
    let job = client.job_status(&job_id).await.unwrap();
    assert_eq!(job.status, JobStatus::Succeeded);
    assert_eq!(
        job.session_id.as_ref().map(|session| session.0.as_str()),
        Some(session_id.as_str())
    );
    assert_eq!(job.execution_id, records.execution_id);
    assert_eq!(
        computer.observed.processes[name].evidence.job_id, job_id,
        "the job that runs the process now"
    );
    assert_eq!(
        computer.observed.processes[name].state,
        ProcessState::Running
    );
    // ✓ Canonical endpoint: the computer's endpoint for the process.
    let endpoint = computer
        .endpoints
        .iter()
        .find(|endpoint| endpoint.process == name)
        .and_then(|endpoint| endpoint.url.clone())
        .unwrap();
    assert_eq!(v1.endpoint.as_deref(), Some(endpoint.as_str()));
    let view = daemon.application(name).await.unwrap();
    assert_eq!(view.endpoint.as_deref(), Some(endpoint.as_str()));
    assert_eq!(view.status, "running");
    answers(&endpoint, "v1").await;
    // ✓ Canonical receipt: the target's receipt for that job, served
    //   byte for byte, verifiable offline.
    let receipt_id = v1.receipt.clone().expect("a receipt");
    assert_eq!(v1.execution_receipts, vec![receipt_id.clone()]);
    let bytes = daemon.application_receipt(name, "v1").await.unwrap();
    let canonical = client.job_receipt(&job_id).await.unwrap().receipt;
    assert_eq!(bytes, canonical.encoded_bytes().unwrap(), "exact bytes");
    let served: ExecutionReceipt = serde_json::from_slice(&bytes).unwrap();
    served.verify().unwrap();
    assert_eq!(served.receipt_hash.0, receipt_id);
    assert_eq!(served.execution_id.0, records.execution_id.clone().unwrap());
    // ✓ Logs: the process's log, read by a job in the target session.
    fetch(&endpoint).await;
    let logs = eventually("the request in the log", async || {
        let logs = daemon.application_logs(name, "alice").await.ok()?;
        logs["stdout"]
            .as_str()?
            .contains("GET / HTTP")
            .then_some(logs)
    })
    .await;
    let log_job = logs["job_id"].as_str().unwrap();
    assert!(client.job_status(log_job).await.is_ok(), "a target job");

    // ✗ No daemon-host supervisor execution, no independent execution,
    //   receipt, deployment, revision, or project record: the node model
    //   holds nothing for the application.
    assert_eq!(legacy_records(&store).await, Vec::<String>::new());
    assert!(matches!(
        daemon.receipt(&receipt_id).await,
        Err(EnvironmentError::NotFound(_))
    ));
    // ✗ No deployment bypassing the computer's authorization: another
    //   operator can neither deploy, stop, roll back, nor read its logs.
    for refused in [
        daemon
            .deploy_application(name, "mallory", request(artifact(name, "x").0))
            .await
            .map(|_| ()),
        daemon.stop_application(name, "mallory").await.map(|_| ()),
        daemon
            .rollback_application(
                name,
                "mallory",
                ApplicationRollbackRequest {
                    target: "v1".into(),
                    placement: None,
                },
            )
            .await
            .map(|_| ()),
        daemon.application_logs(name, "mallory").await.map(|_| ()),
    ] {
        assert!(
            matches!(refused, Err(EnvironmentError::Forbidden(_))),
            "{refused:?}"
        );
    }

    // v2 replaces v1 in place, at the same endpoint.
    let v2 = deploy(&daemon, name, "alice", "v2").await;
    assert_eq!(v2.version, 2);
    assert_eq!(v2.endpoint.as_deref(), Some(endpoint.as_str()));
    answers(&endpoint, "v2").await;
    let history = daemon.application_deployments(name).await.unwrap();
    assert_eq!(history[0].state, ApplicationDeploymentState::Active);
    assert_eq!(history[1].state, ApplicationDeploymentState::Superseded);
    assert_ne!(history[0].receipt, history[1].receipt, "a new execution");

    // Rollback through the application API is the canonical rollback …
    let started = daemon
        .rollback_application(
            name,
            "alice",
            ApplicationRollbackRequest {
                target: "v1".into(),
                placement: None,
            },
        )
        .await
        .unwrap();
    let v3 = released(&daemon, name, &started.deployment_id).await;
    assert_eq!(v3.version, 3);
    assert_eq!(v3.rollback_of, Some(1));
    answers(&endpoint, "v1").await;
    // … and a rollback through the canonical version API is an application
    // version too. The two converge on the same canonical state.
    let v2_label = v2.canonical.clone().unwrap().version;
    let canonical_rollout = daemon
        .rollback_version(
            name,
            "alice",
            RollbackRequest {
                environment: records.environment.clone(),
                version: Some(v2_label.clone()),
            },
        )
        .await
        .unwrap();
    let v4 = released(&daemon, name, &canonical_rollout.rollout_id).await;
    assert_eq!(v4.version, 4);
    assert_eq!(v4.rollback_of, Some(2));
    answers(&endpoint, "v2").await;
    let by_application = daemon.rollout(&v3.deployment_id).await.unwrap();
    let by_version_api = daemon.rollout(&v4.deployment_id).await.unwrap();
    for (rollout, view, label) in [
        (&by_application, &v3, records.version.clone()),
        (&by_version_api, &v4, v2_label),
    ] {
        assert_eq!(rollout.kind, RolloutKind::Rollback);
        assert_eq!(rollout.environment_id, computer.environment_id);
        assert_eq!(rollout.version, label);
        assert_eq!(
            rollout.steps.iter().map(|s| &s.name).collect::<Vec<_>>(),
            [
                "Desired state",
                "Checkout",
                "Build",
                "Restart applications",
                "Health check"
            ]
        );
        assert!(view.receipt.is_some() && view.canonical.as_ref().unwrap().job_id.is_some());
    }
    let computer = daemon.computer(&records.environment).await.unwrap();
    assert_eq!(
        computer.observed.repositories[name].commit,
        daemon
            .version(name, &v4.canonical.clone().unwrap().version)
            .await
            .unwrap()
            .commit
    );
    // The software view (versions and rollouts of the project) and the
    // application's history are the same records.
    let software = daemon.software_view(name).await.unwrap();
    let history = daemon.application_deployments(name).await.unwrap();
    assert_eq!(
        software
            .rollouts
            .iter()
            .map(|rollout| rollout.rollout_id.clone())
            .collect::<Vec<_>>(),
        history
            .iter()
            .map(|deployment| deployment.deployment_id.clone())
            .collect::<Vec<_>>()
    );

    // Stop is the process's desired state: the computer stays, the
    // endpoint stops answering, history remains.
    let stopped = daemon.stop_application(name, "alice").await.unwrap();
    assert!(["stopping", "stopped"].contains(&stopped.status.as_str()));
    eventually("the application to stop", async || {
        (daemon.application(name).await.ok()?.status == "stopped").then_some(())
    })
    .await;
    eventually("the endpoint to stop answering", async || {
        fetch(&endpoint).await.is_none().then_some(())
    })
    .await;
    let history = daemon.application_deployments(name).await.unwrap();
    assert_eq!(history[0].state, ApplicationDeploymentState::Stopped);

    // A control-plane restart: a new controller on the same state finds the
    // same application, computer, session, versions, and evidence.
    daemon.shutdown().await;
    let (restarted, _node) = start_daemon(store.clone(), Some(pool(&target))).await;
    let after = restarted.application(name).await.unwrap();
    assert_eq!(after.status, "stopped");
    assert_eq!(after.endpoint.as_deref(), Some(endpoint.as_str()));
    assert_eq!(
        after
            .deployments
            .iter()
            .map(|deployment| (
                deployment.version,
                deployment.deployment_id.clone(),
                deployment.receipt.clone()
            ))
            .collect::<Vec<_>>(),
        history
            .iter()
            .map(|deployment| (
                deployment.version,
                deployment.deployment_id.clone(),
                deployment.receipt.clone()
            ))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        after.computer.as_ref().unwrap().session_id.as_deref(),
        Some(session_id.as_str()),
        "the same machine"
    );
    // Deploying after a stop serves again, as the next version.
    let v5 = deploy(&restarted, name, "alice", "v5").await;
    assert_eq!(v5.version, 5);
    answers(&endpoint, "v5").await;
    assert_eq!(legacy_records(&store).await, Vec::<String>::new());
    restarted.shutdown().await;
}

// ---- The computer's failures are the application's ---------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_application_follows_its_computer_through_target_failures() {
    let mut target = Target::start();
    let store: Arc<dyn StateStore> = Arc::new(MemoryState::new());
    let (daemon, _node) = start_daemon(store.clone(), Some(pool(&target))).await;
    let name = "resilient";
    let v1 = deploy(&daemon, name, "alice", "one").await;
    let environment = application_environment(name);
    let endpoint = v1.endpoint.clone().unwrap();
    answers(&endpoint, "one").await;
    let session = daemon.computer(&environment).await.unwrap().session_id;

    // Unreachable: the target stops answering. The application says so,
    // and nothing is deployed to it.
    target.stop();
    eventually("the application to be unreachable", async || {
        (daemon.application(name).await.ok()?.status == "unreachable").then_some(())
    })
    .await;
    let refused = daemon
        .deploy_application(name, "alice", request(artifact(name, "two").0))
        .await;
    assert!(
        matches!(refused, Err(EnvironmentError::RuntimeUnavailable(_))),
        "{refused:?}"
    );
    assert!(daemon.application_logs(name, "alice").await.is_err());

    // Recovery: the same machine answers again; the application runs on,
    // from the same process, without being redeployed.
    target.restart();
    eventually("the application to run again", async || {
        (daemon.application(name).await.ok()?.status == "running").then_some(())
    })
    .await;
    let view = daemon.computer(&environment).await.unwrap();
    assert_eq!(view.session_id, session, "the same machine");
    answers(&endpoint, "one").await;
    let rollouts = daemon.application_deployments(name).await.unwrap();
    assert_eq!(rollouts.len(), 1, "recovery is not a deployment");

    // Lost: the target no longer has the machine. A deploy cannot revive
    // it, and no stale answer makes a version active on it.
    let resource = daemon
        .computer(&environment)
        .await
        .unwrap()
        .machine
        .unwrap()
        .resource
        .unwrap();
    target.lose_machine(&resource);
    eventually("the application to be lost", async || {
        (daemon.application(name).await.ok()?.status == "lost").then_some(())
    })
    .await;
    let refused = daemon
        .deploy_application(name, "alice", request(artifact(name, "two").0))
        .await;
    assert!(
        matches!(&refused, Err(EnvironmentError::Conflict(message)) if message.contains("lost")),
        "{refused:?}"
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(daemon.application(name).await.unwrap().status, "lost");
    let history = daemon.application_deployments(name).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].deployment_id, v1.deployment_id);

    // Replaced: the computer's explicit replacement provisions a new
    // machine; deploying the application brings its source there.
    daemon
        .replace_computer(
            &environment,
            "alice",
            daemon.computer(&environment).await.unwrap().requirements,
        )
        .await
        .unwrap();
    let replaced = eventually("the replacement machine", async || {
        let view = daemon.computer(&environment).await.ok()?;
        (view.status == ComputerStatus::Running && view.session_id != session).then_some(view)
    })
    .await;
    let v2 = deploy(&daemon, name, "alice", "two").await;
    assert_eq!(v2.version, 2);
    assert_eq!(
        v2.canonical.as_ref().unwrap().session_id,
        replaced.session_id,
        "the new version runs on the replacement"
    );
    answers(&v2.endpoint.clone().unwrap(), "two").await;
    assert_eq!(legacy_records(&store).await, Vec::<String>::new());
    daemon.shutdown().await;
}

// ---- No computer, no deployment -----------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_application_is_never_deployed_without_a_computer() {
    // No target in the pool: the daemon's own node never runs it.
    let store: Arc<dyn StateStore> = Arc::new(MemoryState::new());
    let (daemon, _node) = start_daemon(store.clone(), None).await;
    let refused = daemon
        .deploy_application("orphan", "alice", request(artifact("orphan", "x").0))
        .await;
    let Err(EnvironmentError::Invalid(message)) = refused else {
        panic!("{refused:?}");
    };
    assert!(message.contains("has no computer to run on"), "{message}");
    assert!(
        message.contains("no target can host this computer"),
        "{message}"
    );
    assert!(matches!(
        daemon.environment(&application_environment("orphan")).await,
        Err(EnvironmentError::NotFound(_))
    ));
    assert!(daemon.applications().await.unwrap().is_empty());
    assert_eq!(legacy_records(&store).await, Vec::<String>::new());
    daemon.shutdown().await;

    // A target that does not trust this control plane's credential hosts
    // nothing for it.
    let target = Target::start();
    let wrong = tempfile::tempdir().unwrap();
    let token_file = wrong.path().join("wrong.token");
    compute_provider::credentials::write_token_file(&token_file, "not-a-credential").unwrap();
    let store: Arc<dyn StateStore> = Arc::new(MemoryState::new());
    let (daemon, _node) = start_daemon(
        store.clone(),
        Some(PoolConfig {
            pool: Default::default(),
            providers: [("target-a".to_owned(), member(&target, &token_file))].into(),
        }),
    )
    .await;
    let refused = daemon
        .deploy_application("untrusted", "alice", request(artifact("untrusted", "x").0))
        .await;
    assert!(refused.is_err(), "{refused:?}");
    assert!(
        daemon
            .application("untrusted")
            .await
            .is_err_and(|error| matches!(error, EnvironmentError::NotFound(_))),
        "nothing ran"
    );
    assert!(
        target.client().sessions().await.unwrap().is_empty(),
        "the target created nothing"
    );
    assert_eq!(legacy_records(&store).await, Vec::<String>::new());
    daemon.shutdown().await;
}

// ---- The compatibility API stays a thin adapter ---------------------------

/// The application module resolves, invokes a canonical operation, and
/// adapts the result. It must never again own a controller, a store, a
/// supervisor, a release path, or evidence of its own: this fails the
/// moment the old path is reintroduced.
#[test]
fn the_application_api_is_a_thin_adapter_over_the_canonical_model() {
    let source = include_str!("../src/daemon/applications.rs");
    let code = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    for forbidden in [
        // The node-environment release path and its records.
        "register_revision",
        "RevisionDefinition",
        "DeployRequest {",
        "self.deploy(",
        "self.release(",
        "self.rollback(",
        "set_project_state",
        "self.logs(",
        "DeploymentRecord",
        "ProjectRevisionRecord",
        "ExecutionRecord",
        "ReceiptRecord",
        "WorkloadDefinition",
        "Readiness",
        // The daemon-host data plane.
        "supervisor",
        "Supervisor",
        "dataplane",
        "instance_port",
        // A store or controller of its own.
        ".create(&",
        ".replace(&",
        "Change::new",
        "self.apply(",
        "spawn_operation",
        "tokio::spawn",
        "APPLICATIONS_ENVIRONMENT",
    ] {
        assert!(
            !code.contains(forbidden),
            "daemon/applications.rs uses `{forbidden}`: applications must go through the canonical computer, version, and rollout operations"
        );
    }
    // What it does use: the canonical operations, and nothing else that
    // changes state.
    for canonical in [
        "create_computer_environment(",
        "owned_environment(",
        "import_source(",
        "change_environment(",
        "publish_version_from(",
        "deploy_version(",
        "rollback_version(",
        "set_process(",
        "computer_logs(",
        "job_receipt(",
    ] {
        assert!(code.contains(canonical), "expected `{canonical}`");
    }
}
