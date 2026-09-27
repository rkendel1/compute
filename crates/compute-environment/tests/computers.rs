//! Environments on a computer, against targets that are ordinary
//! `compute.remote@1` servers hosting sessions. The provider behind each
//! target is provider-neutral: the real workspace provider, wrapped so a
//! test can count, delay, fail, or withhold what it does.
//!
//! The acceptance path: a persistent environment gets a repository and an
//! application, the repository moves to another revision, and the running
//! computer changes in place — one provisioning, one session, durable
//! evidence of every job — across a controller restart, a stop, a resume,
//! and a destroy that keeps the record.

mod common;

use std::collections::BTreeMap;
use std::net::TcpListener as StdTcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use compute_core::{
    ComputerLifecycle, ComputerRequirements, ComputerStatus, EnvironmentContents, NetworkPolicy,
    ProcessDesired, ProcessKind, ProcessSpec, ProcessState, RepositorySpec, SessionCapabilities,
    SessionCommand,
};
use compute_environment::*;
use compute_placement::{PoolConfig, ProviderConfig, ProviderKind};
use compute_provider::{
    EnvironmentState, ProviderConnection, ProviderError, ProviderErrorKind, ProviderRequest,
    ProvisionRequest, ProvisionedSession, RemoteProvider, ServerConfig, SessionEnvironment,
    SessionProvider, WorkspaceSessionProvider,
};
use compute_state::StateStore;
use compute_state_memory::MemoryState;
use tokio::runtime::Runtime;
use tokio::sync::Semaphore;

/// The real workspace provider, observable and steerable.
struct Steered {
    inner: WorkspaceSessionProvider,
    capabilities: SessionCapabilities,
    provisions: AtomicUsize,
    fail_provision: AtomicBool,
    gate: Option<Arc<Semaphore>>,
}

impl Steered {
    fn new(
        root: &Path,
        capabilities: SessionCapabilities,
        gate: Option<Arc<Semaphore>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: WorkspaceSessionProvider::new(root),
            capabilities,
            provisions: AtomicUsize::new(0),
            fail_provision: AtomicBool::new(false),
            gate,
        })
    }
}

fn full() -> SessionCapabilities {
    WorkspaceSessionProvider::new("/unused").capabilities()
}

#[async_trait]
impl SessionProvider for Steered {
    fn kind(&self) -> String {
        "steered".into()
    }
    fn capabilities(&self) -> SessionCapabilities {
        self.capabilities
    }
    async fn provision(
        &self,
        request: &ProvisionRequest,
    ) -> Result<ProvisionedSession, ProviderError> {
        self.provisions.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.gate {
            gate.acquire().await.unwrap().forget();
        }
        if self.fail_provision.load(Ordering::SeqCst) {
            return Err(ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                "no capacity on this target",
            ));
        }
        let mut provisioned = self.inner.provision(request).await?;
        provisioned.capabilities = SessionCapabilities {
            network: provisioned.capabilities.network,
            ..self.capabilities
        };
        Ok(provisioned)
    }
    async fn inspect(&self, id: &str) -> Result<EnvironmentState, ProviderError> {
        self.inner.inspect(id).await
    }
    async fn exec(
        &self,
        environment: &SessionEnvironment,
        command: &SessionCommand,
    ) -> Result<ProviderRequest, ProviderError> {
        self.inner.exec(environment, command).await
    }
    async fn destroy(&self, id: &str) -> Result<(), ProviderError> {
        self.inner.destroy(id).await
    }
    async fn connect(
        &self,
        environment: &SessionEnvironment,
    ) -> Result<ProviderConnection, ProviderError> {
        self.inner.connect(environment).await
    }
    async fn stop(&self, id: &str) -> Result<(), ProviderError> {
        self.inner.stop(id).await
    }
    async fn resume(&self, id: &str) -> Result<(), ProviderError> {
        if !self.capabilities.resume {
            return Err(compute_provider::unsupported("steered", "resume"));
        }
        self.inner.resume(id).await
    }
    async fn claim(&self, id: &str) -> Result<(), ProviderError> {
        self.inner.claim(id).await
    }
}

/// A target: a server hosting sessions, in a runtime of its own.
struct Target {
    runtime: Option<Runtime>,
    endpoint: String,
    provider: Arc<Steered>,
    _stores: tempfile::TempDir,
}

impl Target {
    fn start(provider: Arc<Steered>, features: &[&str]) -> Self {
        let stores = tempfile::tempdir().unwrap();
        let socket = StdTcpListener::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", socket.local_addr().unwrap());
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let mut config = ServerConfig::local(endpoint.clone());
        config.provider = Arc::new(
            compute_provider::LocalProvider::with_identity(
                compute_core::ProviderIdentity::Remote {
                    id: endpoint.clone(),
                    endpoint: endpoint.clone(),
                },
            )
            .with_runtime_catalog(common::catalog()),
        );
        config.job_store = stores.path().join("jobs");
        config.session_store = stores.path().join("sessions");
        config.session_provider = Some(provider.clone());
        config.execution.sessions = true;
        config.session_sweep = Duration::from_millis(100);
        config.target_features = features.iter().map(|feature| feature.to_string()).collect();
        runtime.spawn(async move {
            let listener = tokio::net::TcpListener::from_std(socket).unwrap();
            let _ = compute_provider::serve_listener(listener, config).await;
        });
        Self {
            runtime: Some(runtime),
            endpoint,
            provider,
            _stores: stores,
        }
    }

    fn client(&self) -> RemoteProvider {
        RemoteProvider::new(self.endpoint.clone())
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

fn pool(targets: &[(&str, &Target)]) -> PoolConfig {
    PoolConfig {
        pool: Default::default(),
        providers: targets
            .iter()
            .map(|(id, target)| {
                (
                    id.to_string(),
                    ProviderConfig {
                        kind: ProviderKind::Remote,
                        endpoint: Some(target.endpoint.clone()),
                        application_endpoint: None,
                        priority: 0,
                        token_env: None,
                    },
                )
            })
            .collect(),
    }
}

async fn start_daemon(
    store: Arc<dyn StateStore>,
    pool: PoolConfig,
) -> (Arc<Daemon>, tempfile::TempDir) {
    let node = tempfile::tempdir().unwrap();
    let artifacts = Arc::new(compute_state::StateArtifacts::new(
        compute_state::ControlState::new(store.clone()),
    ));
    let mut config = DaemonConfig::new(node.path(), store, artifacts);
    config.provider = Arc::new(common::provider());
    config.pool = Some(pool);
    config.reconcile_interval = Duration::from_millis(200);
    config.computer_probe = Duration::from_millis(400);
    let seed = (std::process::id() % 400) as u16 * 20;
    config.port_range = (41000 + seed, 41000 + seed + 9);
    config.instance_port_range = (49000 + seed, 49000 + seed + 9);
    (Daemon::start(config).await.unwrap(), node)
}

/// A git repository with `v1` and `v2`, whose application writes the
/// version it runs from into the computer's workspace.
fn repository() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("app");
    std::fs::create_dir_all(&source).unwrap();
    let git = |arguments: &[&str]| {
        let status = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(arguments)
            .current_dir(&source)
            .env_remove("GIT_DIR")
            .output()
            .unwrap();
        assert!(status.status.success(), "{status:?}");
    };
    git(&["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(
        source.join("serve.sh"),
        "cat VERSION > \"$COMPUTE_SESSION_WORKSPACE/running-version\"\nexec sleep 600\n",
    )
    .unwrap();
    for version in ["v1", "v2"] {
        std::fs::write(source.join("VERSION"), version).unwrap();
        git(&["add", "."]);
        git(&["commit", "--quiet", "-m", version]);
        git(&["tag", version]);
    }
    (root, source)
}

fn contents(url: &Path, revision: &str) -> EnvironmentContents {
    EnvironmentContents {
        repositories: vec![RepositorySpec {
            name: "app".into(),
            url: url.display().to_string(),
            revision: revision.into(),
        }],
        packages: vec![],
        processes: vec![ProcessSpec {
            name: "api".into(),
            kind: ProcessKind::Application,
            command: vec!["sh".into(), "serve.sh".into()],
            repository: Some("app".into()),
            env: BTreeMap::new(),
            desired: ProcessDesired::Running,
        }],
        generation: 0,
    }
}

fn definition(
    name: &str,
    lifecycle: ComputerLifecycle,
    requirements: ComputerRequirements,
    contents: EnvironmentContents,
) -> ComputerEnvironmentDefinition {
    ComputerEnvironmentDefinition {
        name: name.into(),
        desired_state: DesiredState::Running,
        env: BTreeMap::from([("APP_ENV".into(), name.into())]),
        policy: None,
        computer: ComputerRequest {
            lifecycle,
            requirements,
            target: None,
            ttl_seconds: None,
        },
        contents,
    }
}

fn requirements() -> ComputerRequirements {
    ComputerRequirements {
        cpu_count: Some(1),
        memory_bytes: Some(64 << 20),
        network: NetworkPolicy::Network,
        ..Default::default()
    }
}

async fn eventually<T>(what: &str, mut check: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
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

async fn computer_where(
    daemon: &Arc<Daemon>,
    name: &str,
    what: &str,
    wanted: impl Fn(&ComputerView) -> bool,
) -> ComputerView {
    let last = std::sync::Mutex::new(None);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(view) = daemon.computer(name).await {
            if wanted(&view) {
                return view;
            }
            *last.lock().unwrap() = Some(view);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}; last saw {:#?}",
            last.lock().unwrap()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Run a command in the computer and return its output.
async fn run(
    daemon: &Arc<Daemon>,
    name: &str,
    operator: &str,
    command: &[&str],
) -> (String, ComputerJob) {
    let exec = daemon
        .computer_exec(
            name,
            operator,
            SessionCommand::new(command.iter().map(|part| part.to_string()).collect()),
        )
        .await
        .unwrap();
    let job = eventually("the command", async || {
        daemon
            .computer_job(name, operator, &exec.job_id)
            .await
            .ok()
            .filter(|job| job.result.is_some())
    })
    .await;
    let output = job.result.as_ref().unwrap().result.stdout.text.clone();
    (output, job)
}

async fn events(daemon: &Arc<Daemon>, name: &str) -> Vec<(String, serde_json::Value)> {
    daemon
        .events(EventFilter {
            environment: Some(name.into()),
            limit: Some(1000),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_iter()
        .map(|event| (event.kind, event.data))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_persistent_environment_is_changed_in_place_and_outlives_its_controller() {
    let (_repositories, source) = repository();
    let workspaces = tempfile::tempdir().unwrap();
    let target = Target::start(Steered::new(workspaces.path(), full(), None), &[]);
    let store: Arc<dyn StateStore> = Arc::new(MemoryState::new());
    let (first, _node) = start_daemon(store.clone(), pool(&[("target-a", &target)])).await;

    // No provider is named: placement finds the target.
    let created = first
        .create_computer_environment(
            definition(
                "myapp",
                ComputerLifecycle::Persistent,
                requirements(),
                contents(&source, "v1"),
            ),
            "alice",
        )
        .await
        .unwrap();
    let computer = created.computer.unwrap();
    assert_eq!(computer.owner, "alice");
    assert_eq!(computer.lifecycle, ComputerLifecycle::Persistent);
    assert!(computer.expires_at.is_none());

    let running = computer_where(&first, "myapp", "the computer to converge", |view| {
        view.converged
    })
    .await;
    assert_eq!(running.target.as_deref(), Some("target-a"));
    assert_eq!(running.provider_kind.as_deref(), Some("steered"));
    let session_id = running.session_id.clone().unwrap();
    let v1 = running.observed.repositories["app"].commit.clone().unwrap();
    let api = &running.observed.processes["api"];
    assert_eq!(api.state, ProcessState::Running);
    let first_pid = api.pid.unwrap();
    let (version, _) = run(&first, "myapp", "alice", &["cat", "running-version"]).await;
    assert_eq!(version, "v1");

    // Change the repository's revision: the running computer changes in
    // place. No new session, no new provisioning.
    first
        .upsert_repository(
            "myapp",
            "alice",
            RepositorySpec {
                name: "app".into(),
                url: source.display().to_string(),
                revision: "v2".into(),
            },
        )
        .await
        .unwrap();
    let changed = computer_where(&first, "myapp", "the new revision", |view| {
        view.converged && view.desired.generation == 2
    })
    .await;
    assert_eq!(
        changed.session_id.as_deref(),
        Some(session_id.as_str()),
        "the same computer"
    );
    assert_ne!(
        changed.observed.repositories["app"].commit.as_deref(),
        Some(v1.as_str())
    );
    assert_ne!(
        changed.observed.processes["api"].pid,
        Some(first_pid),
        "the application restarted on the new commit"
    );
    let (version, job) = run(&first, "myapp", "alice", &["cat", "running-version"]).await;
    assert_eq!(version, "v2");
    assert_eq!(
        job.job.session_id.as_ref().map(|id| id.0.as_str()),
        Some(session_id.as_str())
    );
    assert_eq!(
        target.provider.provisions.load(Ordering::SeqCst),
        1,
        "no redeployment"
    );

    // Evidence: every change is an event naming the durable job that made
    // it, and that job's receipt verifies at the target.
    let recorded = events(&first, "myapp").await;
    let applied = recorded
        .iter()
        .filter(|(kind, _)| kind == "environment.contents_applied")
        .collect::<Vec<_>>();
    assert!(applied.len() >= 4, "{recorded:#?}");
    let job_id = applied.last().unwrap().1["job_id"]
        .as_str()
        .unwrap()
        .to_owned();
    target
        .client()
        .job_receipt(&job_id)
        .await
        .unwrap()
        .receipt
        .verify()
        .unwrap();
    for kind in [
        "computer.requested",
        "computer.placed",
        "computer.provisioned",
        "computer.running",
        "environment.contents_changed",
        "environment.contents_converged",
        "environment.exec",
    ] {
        assert!(
            recorded.iter().any(|(recorded, _)| recorded == kind),
            "missing {kind}"
        );
    }

    // The controller goes away and a new one takes over from durable state.
    first.shutdown().await;
    drop(first);
    let (second, _node2) = start_daemon(store.clone(), pool(&[("target-a", &target)])).await;
    let after = computer_where(&second, "myapp", "the computer after a restart", |view| {
        view.converged
    })
    .await;
    assert_eq!(after.session_id.as_deref(), Some(session_id.as_str()));
    assert_eq!(
        after.observed.repositories["app"],
        changed.observed.repositories["app"]
    );
    let (version, _) = run(&second, "myapp", "alice", &["cat", "running-version"]).await;
    assert_eq!(version, "v2");

    // Stop and resume: the same computer, its processes stopped and started.
    second
        .set_environment_state("myapp", DesiredState::Stopped, false)
        .await
        .unwrap();
    let stopped = computer_where(&second, "myapp", "the computer to stop", |view| {
        view.status == ComputerStatus::Stopped
    })
    .await;
    assert_eq!(
        stopped.observed.processes["api"].state,
        ProcessState::Stopped
    );
    assert!(
        second
            .computer_exec("myapp", "alice", SessionCommand::new(vec!["true".into()]))
            .await
            .is_err()
    );
    second
        .set_environment_state("myapp", DesiredState::Running, false)
        .await
        .unwrap();
    let resumed = computer_where(&second, "myapp", "the computer to resume", |view| {
        view.status == ComputerStatus::Running
            && view
                .observed
                .processes
                .get("api")
                .is_some_and(|api| api.state == ProcessState::Running)
    })
    .await;
    assert_eq!(resumed.session_id.as_deref(), Some(session_id.as_str()));
    assert_eq!(target.provider.provisions.load(Ordering::SeqCst), 1);

    // Destroy: the machine goes, the record stays.
    let destroying = second.destroy_computer("myapp", "alice").await.unwrap();
    assert!(!destroying.status.is_terminal() || destroying.status == ComputerStatus::Destroyed);
    let destroyed = computer_where(&second, "myapp", "the computer to be destroyed", |view| {
        view.status == ComputerStatus::Destroyed
    })
    .await;
    assert!(destroyed.ended_at.is_some());
    assert_eq!(
        destroyed.observed.repositories["app"], changed.observed.repositories["app"],
        "evidence remains"
    );
    let session = target.client().session(&session_id).await.unwrap();
    assert_eq!(session.status, compute_core::SessionStatus::Destroyed);
    let refused = second
        .upsert_process(
            "myapp",
            "alice",
            contents(&source, "v2").processes[0].clone(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(refused, EnvironmentError::Conflict(_)),
        "{refused:?}"
    );
    assert!(
        second.environment("myapp").await.is_ok(),
        "the environment remains"
    );
    second.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_the_owner_changes_or_uses_a_computer() {
    let workspaces = tempfile::tempdir().unwrap();
    let target = Target::start(Steered::new(workspaces.path(), full(), None), &[]);
    let (daemon, _node) =
        start_daemon(Arc::new(MemoryState::new()), pool(&[("target-a", &target)])).await;
    daemon
        .create_computer_environment(
            definition(
                "owned",
                ComputerLifecycle::Persistent,
                requirements(),
                EnvironmentContents::default(),
            ),
            "alice",
        )
        .await
        .unwrap();
    computer_where(&daemon, "owned", "the computer", |view| {
        view.status == ComputerStatus::Running
    })
    .await;
    let forbidden = |result: Result<(), EnvironmentError>| {
        assert!(
            matches!(result, Err(EnvironmentError::Forbidden(_))),
            "{result:?}"
        );
    };
    forbidden(
        daemon
            .computer_exec("owned", "mallory", SessionCommand::new(vec!["true".into()]))
            .await
            .map(|_| ()),
    );
    forbidden(
        daemon
            .computer_connect("owned", "mallory")
            .await
            .map(|_| ()),
    );
    forbidden(
        daemon
            .computer_logs("owned", "mallory", None, 10)
            .await
            .map(|_| ()),
    );
    forbidden(
        daemon
            .computer_job("owned", "mallory", "job_x")
            .await
            .map(|_| ()),
    );
    forbidden(
        daemon
            .upsert_process(
                "owned",
                "mallory",
                ProcessSpec {
                    name: "x".into(),
                    kind: ProcessKind::Agent,
                    command: vec!["true".into()],
                    repository: None,
                    env: BTreeMap::new(),
                    desired: ProcessDesired::Running,
                },
            )
            .await
            .map(|_| ()),
    );
    forbidden(
        daemon
            .reconcile_computer("owned", "mallory")
            .await
            .map(|_| ()),
    );
    forbidden(
        daemon
            .replace_computer("owned", "mallory", requirements())
            .await
            .map(|_| ()),
    );
    forbidden(
        daemon
            .destroy_computer("owned", "mallory")
            .await
            .map(|_| ()),
    );
    forbidden(daemon.authorize_environment("owned", "mallory").await);
    daemon
        .authorize_environment("owned", "alice")
        .await
        .unwrap();
    // Reading what an environment is stays open to readers.
    assert_eq!(daemon.computer("owned").await.unwrap().owner, "alice");
    // The API binds each route to a scope; changing contents is operation,
    // running a command is execution.
    use compute_environment::auth::{Scope, required_scope};
    assert_eq!(
        required_scope("POST", &["environments", "e", "exec"]),
        Scope::Execute
    );
    assert_eq!(
        required_scope("POST", &["environments", "e", "connect"]),
        Scope::Execute
    );
    assert_eq!(
        required_scope("POST", &["environments", "e", "repositories"]),
        Scope::Operate
    );
    assert_eq!(
        required_scope("POST", &["environments", "e", "processes", "p", "stop"]),
        Scope::Operate
    );
    assert_eq!(
        required_scope("DELETE", &["environments", "e", "packages", "p"]),
        Scope::Operate
    );
    assert_eq!(
        required_scope("POST", &["environments", "e", "replace"]),
        Scope::Operate
    );
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn placement_chooses_a_target_by_what_the_computer_needs() {
    let plain_root = tempfile::tempdir().unwrap();
    let kvm_root = tempfile::tempdir().unwrap();
    let plain = Target::start(
        Steered::new(plain_root.path(), full(), None),
        &["containers"],
    );
    let kvm = Target::start(
        Steered::new(kvm_root.path(), full(), None),
        &["firecracker", "kvm", "virtualization"],
    );
    let (daemon, _node) = start_daemon(
        Arc::new(MemoryState::new()),
        pool(&[("plain", &plain), ("kvm-host", &kvm)]),
    )
    .await;
    let targets = daemon.targets().await;
    assert!(
        targets
            .iter()
            .all(|target| target.target_id == "local" || target.hosts_computers)
    );
    assert_eq!(
        targets
            .iter()
            .find(|target| target.target_id == "kvm-host")
            .unwrap()
            .features,
        ["firecracker", "kvm", "virtualization"]
    );

    let microvm = ComputerRequirements {
        features: vec!["firecracker".into(), "kvm".into()],
        ..requirements()
    };
    daemon
        .create_computer_environment(
            definition(
                "microvm",
                ComputerLifecycle::Ephemeral,
                microvm.clone(),
                EnvironmentContents::default(),
            ),
            "alice",
        )
        .await
        .unwrap();
    let placed = computer_where(&daemon, "microvm", "placement", |view| {
        view.status == ComputerStatus::Running
    })
    .await;
    assert_eq!(placed.target.as_deref(), Some("kvm-host"));
    assert_eq!(plain.provider.provisions.load(Ordering::SeqCst), 0);

    // Naming an incompatible target is refused, with its reasons; nothing
    // is created.
    let mut pinned = definition(
        "pinned",
        ComputerLifecycle::Ephemeral,
        microvm,
        EnvironmentContents::default(),
    );
    pinned.computer.target = Some("plain".into());
    let error = daemon
        .create_computer_environment(pinned, "alice")
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("target_feature_unsupported"),
        "{error}"
    );
    assert!(daemon.environment("pinned").await.is_err());
    // A requirement no target meets is refused.
    let gpu = ComputerRequirements {
        features: vec!["gpu".into()],
        ..requirements()
    };
    assert!(
        daemon
            .create_computer_environment(
                definition(
                    "gpu",
                    ComputerLifecycle::Ephemeral,
                    gpu,
                    EnvironmentContents::default()
                ),
                "alice"
            )
            .await
            .is_err()
    );
    // And an explicit, compatible target is honoured.
    let mut there = definition(
        "there",
        ComputerLifecycle::Ephemeral,
        requirements(),
        EnvironmentContents::default(),
    );
    there.computer.target = Some("plain".into());
    daemon
        .create_computer_environment(there, "alice")
        .await
        .unwrap();
    let there = computer_where(&daemon, "there", "the pinned computer", |view| {
        view.status == ComputerStatus::Running
    })
    .await;
    assert_eq!(there.target.as_deref(), Some("plain"));
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ephemeral_environment_expires_and_keeps_its_evidence() {
    let workspaces = tempfile::tempdir().unwrap();
    let target = Target::start(Steered::new(workspaces.path(), full(), None), &[]);
    let (daemon, _node) =
        start_daemon(Arc::new(MemoryState::new()), pool(&[("target-a", &target)])).await;
    let mut ephemeral = definition(
        "try-it",
        ComputerLifecycle::Ephemeral,
        requirements(),
        EnvironmentContents::default(),
    );
    ephemeral.computer.ttl_seconds = Some(2);
    daemon
        .create_computer_environment(ephemeral, "alice")
        .await
        .unwrap();
    let running = computer_where(&daemon, "try-it", "the computer", |view| {
        view.status == ComputerStatus::Running
    })
    .await;
    let (output, _) = run(&daemon, "try-it", "alice", &["echo", "tried"]).await;
    assert_eq!(output, "tried\n");
    let expired = computer_where(&daemon, "try-it", "expiry", |view| {
        view.status == ComputerStatus::Expired
    })
    .await;
    assert!(expired.ended_at.is_some());
    let session = target
        .client()
        .session(running.session_id.as_deref().unwrap())
        .await
        .unwrap();
    assert_eq!(session.status, compute_core::SessionStatus::Destroyed);
    assert!(
        events(&daemon, "try-it")
            .await
            .iter()
            .any(|(kind, _)| kind == "computer.expired")
    );
    // A persistent computer has no TTL.
    let mut contradictory = definition(
        "never",
        ComputerLifecycle::Persistent,
        requirements(),
        EnvironmentContents::default(),
    );
    contradictory.computer.ttl_seconds = Some(60);
    assert!(
        daemon
            .create_computer_environment(contradictory, "alice")
            .await
            .is_err()
    );
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provisioning_survives_a_controller_restart_and_orphans_are_torn_down() {
    let workspaces = tempfile::tempdir().unwrap();
    let gate = Arc::new(Semaphore::new(0));
    let target = Target::start(
        Steered::new(workspaces.path(), full(), Some(gate.clone())),
        &[],
    );
    // A session a controller once created and no computer owns any more.
    let orphan = target
        .client()
        .create_session(
            &compute_provider::SessionCreateRequest::new(
                &compute_provider::SessionEnvironmentSpec {
                    resources: Default::default(),
                    network: NetworkPolicy::Network,
                    isolation: Default::default(),
                    architecture: None,
                },
                compute_core::SessionSpec {
                    reference: Some("cmp_000000000000000000000000:1".into()),
                    ..Default::default()
                },
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let store: Arc<dyn StateStore> = Arc::new(MemoryState::new());
    let (first, _node) = start_daemon(store.clone(), pool(&[("target-a", &target)])).await;
    first
        .create_computer_environment(
            definition(
                "slow",
                ComputerLifecycle::Persistent,
                requirements(),
                EnvironmentContents::default(),
            ),
            "alice",
        )
        .await
        .unwrap();
    let provisioning = computer_where(&first, "slow", "a session to be requested", |view| {
        view.session_id.is_some()
    })
    .await;
    assert_eq!(provisioning.status, ComputerStatus::Provisioning);
    first.shutdown().await;
    drop(first);

    let (second, _node2) = start_daemon(store.clone(), pool(&[("target-a", &target)])).await;
    gate.add_permits(64);
    let running = computer_where(&second, "slow", "provisioning to finish", |view| {
        view.status == ComputerStatus::Running
    })
    .await;
    assert_eq!(
        running.session_id, provisioning.session_id,
        "one computer, not two"
    );
    // The orphan was torn down by the sweep; the live computer was not.
    eventually("the orphan to go", async || {
        let session = target.client().session(&orphan.session_id.0).await.ok()?;
        session.status.is_terminal().then_some(())
    })
    .await;
    let live = target
        .client()
        .sessions()
        .await
        .unwrap()
        .into_iter()
        .filter(|session| !session.status.is_terminal())
        .count();
    assert_eq!(live, 1);
    second.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_failures_and_replacements_are_explicit() {
    let (_repositories, source) = repository();
    let failing_root = tempfile::tempdir().unwrap();
    let failing = Steered::new(failing_root.path(), full(), None);
    failing.fail_provision.store(true, Ordering::SeqCst);
    let failing = Target::start(failing, &[]);
    let (daemon, _node) =
        start_daemon(Arc::new(MemoryState::new()), pool(&[("failing", &failing)])).await;
    daemon
        .create_computer_environment(
            definition(
                "broken",
                ComputerLifecycle::Persistent,
                requirements(),
                EnvironmentContents::default(),
            ),
            "alice",
        )
        .await
        .unwrap();
    let failed = computer_where(&daemon, "broken", "the failure", |view| {
        view.status == ComputerStatus::Failed
    })
    .await;
    let failure = failed.failure.unwrap();
    assert_eq!(failure.phase, "provisioning");
    assert!(failure.message.contains("no capacity"), "{failure:?}");
    assert_eq!(failure.target.as_deref(), Some("failing"));
    daemon.shutdown().await;

    // Replacement: new requirements provision a new computer, move the
    // contents onto it, and retire the old one — explicitly.
    let workspaces = tempfile::tempdir().unwrap();
    let target = Target::start(Steered::new(workspaces.path(), full(), None), &[]);
    let (daemon, _node) =
        start_daemon(Arc::new(MemoryState::new()), pool(&[("target-a", &target)])).await;
    daemon
        .create_computer_environment(
            definition(
                "resized",
                ComputerLifecycle::Persistent,
                requirements(),
                contents(&source, "v1"),
            ),
            "alice",
        )
        .await
        .unwrap();
    let before = computer_where(&daemon, "resized", "the first computer", |view| {
        view.converged
    })
    .await;
    let bigger = ComputerRequirements {
        cpu_count: Some(2),
        ..requirements()
    };
    let requested = daemon
        .replace_computer("resized", "alice", bigger.clone())
        .await
        .unwrap();
    assert_eq!(requested.spec_generation, 2);
    let after = computer_where(&daemon, "resized", "the replacement", |view| {
        view.converged && view.running_generation == 2
    })
    .await;
    assert_ne!(after.session_id, before.session_id);
    assert_eq!(after.requirements, bigger);
    assert_eq!(target.provider.provisions.load(Ordering::SeqCst), 2);
    let (version, _) = run(&daemon, "resized", "alice", &["cat", "running-version"]).await;
    assert_eq!(version, "v1", "the contents moved to the new computer");
    let old = target
        .client()
        .session(before.session_id.as_deref().unwrap())
        .await
        .unwrap();
    eventually("the old computer to go", async || {
        let session = target.client().session(&old.session_id.0).await.ok()?;
        session.status.is_terminal().then_some(())
    })
    .await;
    assert!(
        events(&daemon, "resized")
            .await
            .iter()
            .any(|(kind, _)| kind == "computer.replacing")
    );
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_target_without_optional_capabilities_is_still_a_target() {
    let workspaces = tempfile::tempdir().unwrap();
    let limited = SessionCapabilities {
        resume: false,
        claim: false,
        ..full()
    };
    let target = Target::start(Steered::new(workspaces.path(), limited, None), &[]);
    let (daemon, _node) =
        start_daemon(Arc::new(MemoryState::new()), pool(&[("limited", &target)])).await;
    // It cannot keep a computer: persistent computers are placed elsewhere.
    let error = daemon
        .create_computer_environment(
            definition(
                "kept",
                ComputerLifecycle::Persistent,
                requirements(),
                EnvironmentContents::default(),
            ),
            "alice",
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("session_capability_unsupported"),
        "{error}"
    );
    daemon
        .create_computer_environment(
            definition(
                "brief",
                ComputerLifecycle::Ephemeral,
                requirements(),
                EnvironmentContents::default(),
            ),
            "alice",
        )
        .await
        .unwrap();
    computer_where(&daemon, "brief", "the computer", |view| {
        view.status == ComputerStatus::Running
    })
    .await;
    daemon
        .set_environment_state("brief", DesiredState::Stopped, false)
        .await
        .unwrap();
    computer_where(&daemon, "brief", "stop", |view| {
        view.status == ComputerStatus::Stopped
    })
    .await;
    // Resuming is not something this target does: the computer stays
    // stopped, with the reason, and nothing replaces it.
    daemon
        .set_environment_state("brief", DesiredState::Running, false)
        .await
        .unwrap();
    let refused = computer_where(&daemon, "brief", "the refusal", |view| {
        view.failure
            .as_ref()
            .is_some_and(|failure| failure.code == "resume_unsupported")
    })
    .await;
    assert_eq!(refused.status, ComputerStatus::Stopped);
    assert_eq!(target.provider.provisions.load(Ordering::SeqCst), 1);
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drift_is_reconciled_and_processes_follow_their_desired_state() {
    let (_repositories, source) = repository();
    let workspaces = tempfile::tempdir().unwrap();
    let target = Target::start(Steered::new(workspaces.path(), full(), None), &[]);
    let (daemon, _node) =
        start_daemon(Arc::new(MemoryState::new()), pool(&[("target-a", &target)])).await;
    daemon
        .create_computer_environment(
            definition(
                "drift",
                ComputerLifecycle::Persistent,
                requirements(),
                contents(&source, "v1"),
            ),
            "alice",
        )
        .await
        .unwrap();
    let running = computer_where(&daemon, "drift", "the application", |view| view.converged).await;
    let pid = running.observed.processes["api"].pid.unwrap();
    // Someone kills it behind Compute's back: the probe notices, and the
    // reconciler starts it again.
    run(&daemon, "drift", "alice", &["kill", &pid.to_string()]).await;
    let restarted = computer_where(&daemon, "drift", "the restart", |view| {
        view.observed.processes["api"].state == ProcessState::Running
            && view.observed.processes["api"].pid != Some(pid)
    })
    .await;
    assert!(restarted.converged);
    // Stopping a process is a change to desired state, applied in place.
    daemon
        .set_process("drift", "alice", "api", ProcessDesired::Stopped)
        .await
        .unwrap();
    computer_where(&daemon, "drift", "the process to stop", |view| {
        view.converged && view.observed.processes["api"].state == ProcessState::Stopped
    })
    .await;
    // A service and a package, added while it runs.
    daemon
        .upsert_package(
            "drift",
            "alice",
            compute_core::PackageSpec {
                name: "deps".into(),
                install: vec![
                    "sh".into(),
                    "-c".into(),
                    "echo installed > ../../deps-installed".into(),
                ],
                repository: Some("app".into()),
            },
        )
        .await
        .unwrap();
    daemon
        .upsert_process(
            "drift",
            "alice",
            ProcessSpec {
                name: "cache".into(),
                kind: ProcessKind::Service,
                command: vec!["sleep".into(), "600".into()],
                repository: None,
                env: BTreeMap::new(),
                desired: ProcessDesired::Running,
            },
        )
        .await
        .unwrap();
    let grown = computer_where(&daemon, "drift", "the additions", |view| {
        view.converged && view.desired.generation >= 4
    })
    .await;
    assert_eq!(
        grown.observed.processes["cache"].state,
        ProcessState::Running
    );
    assert_eq!(
        grown.observed.packages["deps"].evidence.outcome,
        "succeeded"
    );
    let (installed, _) = run(&daemon, "drift", "alice", &["cat", "deps-installed"]).await;
    assert_eq!(installed, "installed\n");
    // A failing item is recorded, not retried in a loop, and retried on
    // request.
    daemon
        .upsert_package(
            "drift",
            "alice",
            compute_core::PackageSpec {
                name: "broken".into(),
                install: vec!["sh".into(), "-c".into(), "echo nope >&2; exit 3".into()],
                repository: None,
            },
        )
        .await
        .unwrap();
    let failed = computer_where(&daemon, "drift", "the failure", |view| {
        view.observed.packages.contains_key("broken")
    })
    .await;
    assert_eq!(
        failed.observed.packages["broken"].evidence.outcome,
        "failed"
    );
    assert!(failed.failure.as_ref().unwrap().message.contains("nope"));
    let logs = daemon
        .computer_logs("drift", "alice", Some("cache"), 5)
        .await
        .unwrap();
    assert!(
        logs["evidence"]["job_id"]
            .as_str()
            .unwrap()
            .starts_with("job_")
    );
    daemon
        .remove_content("drift", "alice", "packages", "broken")
        .await
        .unwrap();
    computer_where(&daemon, "drift", "the removal", |view| {
        view.converged && !view.observed.packages.contains_key("broken")
    })
    .await;
    assert_eq!(
        target.provider.provisions.load(Ordering::SeqCst),
        1,
        "all of it in place"
    );
    daemon.shutdown().await;
}
