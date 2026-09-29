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
    /// Refuse any command whose text contains this (a fault to inject).
    fail_exec_containing: std::sync::Mutex<Option<String>>,
    /// Called (once, then cleared) before any command whose text contains
    /// the marker, with the workspace directory the command runs in.
    tamper: std::sync::Mutex<Option<(String, Box<dyn Fn(&Path) + Send + Sync>)>>,
    root: PathBuf,
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
            fail_exec_containing: std::sync::Mutex::new(None),
            tamper: std::sync::Mutex::new(None),
            root: root.to_path_buf(),
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
        if let Some(marker) = self.fail_exec_containing.lock().unwrap().as_deref()
            && command.command.iter().any(|part| part.contains(marker))
        {
            return Err(ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                "an injected fault",
            ));
        }
        let due = {
            let mut tamper = self.tamper.lock().unwrap();
            let matched = tamper.as_ref().is_some_and(|(marker, _)| {
                command.command.iter().any(|part| part.contains(marker))
            });
            if matched { tamper.take() } else { None }
        };
        if let Some((_, tamper)) = due {
            tamper(&self.root.join(&environment.provider_session_id));
        }
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

/// A target: a server hosting sessions, in a runtime of its own. It can be
/// stopped (unreachable), started again on the same address with the same
/// stores (a target restart), or lose its session store.
struct Target {
    runtime: Option<Runtime>,
    endpoint: String,
    address: std::net::SocketAddr,
    provider: Arc<Steered>,
    features: Vec<String>,
    /// The target's trust file: `compute serve --credentials`.
    trust: PathBuf,
    /// The control plane's token for this target, in the file its pool
    /// member names.
    token_file: PathBuf,
    stores: tempfile::TempDir,
}

impl Target {
    fn start(provider: Arc<Steered>, features: &[&str]) -> Self {
        let stores = tempfile::tempdir().unwrap();
        // The target trusts one control plane, as `compute serve
        // --credentials` does.
        let mut credentials = compute_provider::TargetCredentials::default();
        let (_, token) = credentials.issue("test-control-plane").unwrap();
        let trust = stores.path().join("credentials.json");
        credentials.save(&trust).unwrap();
        let token_file = stores.path().join("control-plane.token");
        compute_provider::credentials::write_token_file(&token_file, &token).unwrap();
        Self::start_trusting(provider, features, stores, trust, token_file)
    }

    /// Another target that trusts the same control plane as `other`: a
    /// different machine answering with the same credential.
    fn trusting_like(provider: Arc<Steered>, other: &Target) -> Self {
        let stores = tempfile::tempdir().unwrap();
        let trust = stores.path().join("credentials.json");
        std::fs::copy(&other.trust, &trust).unwrap();
        Self::start_trusting(provider, &[], stores, trust, other.token_file.clone())
    }

    fn start_trusting(
        provider: Arc<Steered>,
        features: &[&str],
        stores: tempfile::TempDir,
        trust: PathBuf,
        token_file: PathBuf,
    ) -> Self {
        let socket = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        let mut target = Self {
            runtime: None,
            endpoint: format!("http://{address}"),
            address,
            provider,
            features: features.iter().map(|feature| feature.to_string()).collect(),
            trust,
            token_file,
            stores,
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
        config.session_provider = Some(self.provider.clone());
        config.execution.sessions = true;
        config.session_sweep = Duration::from_millis(100);
        config.target_features = self.features.clone();
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
                    assert!(
                        std::time::Instant::now() < deadline,
                        "cannot listen on {} again: {error}",
                        self.address
                    );
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        };
        self.serve(socket);
    }

    /// Lose every session record, as a target whose store was wiped does.
    fn wipe_sessions(&self) {
        std::fs::remove_dir_all(self.stores.path().join("sessions")).unwrap();
    }

    fn client(&self) -> RemoteProvider {
        RemoteProvider::new(self.endpoint.clone()).with_bearer_token(
            compute_provider::credentials::read_token_file(&self.token_file).unwrap(),
        )
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
                        token_file: Some(target.token_file.clone()),
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
    let artifacts = Arc::new(compute_state::StateArtifacts::new(
        compute_state::ControlState::new(store.clone()),
    ));
    start_daemon_with(store, artifacts, pool).await
}

async fn start_daemon_with(
    store: Arc<dyn StateStore>,
    artifacts: Arc<dyn compute_state::ArtifactStore>,
    pool: PoolConfig,
) -> (Arc<Daemon>, tempfile::TempDir) {
    start_daemon_tuned(store, artifacts, pool, |_| {}).await
}

async fn start_daemon_tuned(
    store: Arc<dyn StateStore>,
    artifacts: Arc<dyn compute_state::ArtifactStore>,
    pool: PoolConfig,
    tune: impl FnOnce(&mut DaemonConfig),
) -> (Arc<Daemon>, tempfile::TempDir) {
    let node = tempfile::tempdir().unwrap();
    let mut config = DaemonConfig::new(node.path(), store, artifacts);
    config.provider = Arc::new(common::provider());
    config.pool = Some(pool);
    config.reconcile_interval = Duration::from_millis(200);
    config.computer_probe = Duration::from_millis(400);
    config.computer_liveness = Duration::from_millis(300);
    config.computer_liveness_timeout = Duration::from_secs(3);
    config.replacement_deadline = Duration::from_secs(20);
    tune(&mut config);
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
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
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
            sync: 0,
        }],
        packages: vec![],
        processes: vec![ProcessSpec {
            name: "api".into(),
            kind: ProcessKind::Application,
            runtime: None,
            command: vec!["sh".into(), "serve.sh".into()],
            repository: Some("app".into()),
            env: BTreeMap::new(),
            desired: ProcessDesired::Running,
            port: None,
            restart: 0,
            readiness: None,
            restart_policy: Default::default(),
            max_restarts: compute_core::DEFAULT_MAX_RESTARTS,
        }],
        projects: vec![],
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
                sync: 0,
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
                    runtime: None,
                    command: vec!["true".into()],
                    repository: None,
                    env: BTreeMap::new(),
                    desired: ProcessDesired::Running,
                    port: None,
                    restart: 0,
                    readiness: None,
                    restart_policy: Default::default(),
                    max_restarts: compute_core::DEFAULT_MAX_RESTARTS,
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
    // Long enough to be seen running on a busy machine, short enough to
    // expire within the test.
    ephemeral.computer.ttl_seconds = Some(8);
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
                runtime: None,
                command: vec!["sleep".into(), "600".into()],
                repository: None,
                env: BTreeMap::new(),
                desired: ProcessDesired::Running,
                port: None,
                restart: 0,
                readiness: None,
                restart_policy: Default::default(),
                max_restarts: compute_core::DEFAULT_MAX_RESTARTS,
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

// ---- One environment, one computer: deployment and work are the same thing --

/// Wait for a job in the environment's computer and return it.
async fn job_of(
    daemon: &Arc<Daemon>,
    name: &str,
    operator: &str,
    exec: &ComputerExec,
) -> ComputerJob {
    eventually("the job", async || {
        daemon
            .computer_job(name, operator, &exec.job_id)
            .await
            .ok()
            .filter(|job| job.result.is_some())
    })
    .await
}

fn project_contents(url: &Path, revision: &str) -> EnvironmentContents {
    let shell = |command: &str| vec!["sh".to_owned(), "-c".into(), command.into()];
    EnvironmentContents {
        repositories: vec![RepositorySpec {
            name: "app".into(),
            url: url.display().to_string(),
            revision: revision.into(),
            sync: 0,
        }],
        packages: vec![],
        projects: vec![compute_core::ProjectSpec {
            name: "app".into(),
            repository: "app".into(),
            build: shell("cat VERSION > BUILT"),
            test: shell("grep -q '^v' BUILT"),
            commands: BTreeMap::from([("where".into(), shell("pwd"))]),
            checks: vec![],
        }],
        processes: vec![ProcessSpec {
            name: "api".into(),
            kind: ProcessKind::Application,
            runtime: None,
            command: shell(
                "cat BUILT > \"$COMPUTE_SESSION_WORKSPACE/running-build\"; \
                 echo \"$PORT\" > \"$COMPUTE_SESSION_WORKSPACE/port\"; exec sleep 600",
            ),
            repository: Some("app".into()),
            env: BTreeMap::new(),
            desired: ProcessDesired::Running,
            port: Some(18_555),
            restart: 0,
            readiness: None,
            restart_policy: Default::default(),
            max_restarts: compute_core::DEFAULT_MAX_RESTARTS,
        }],
        generation: 0,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deployment_is_reconciliation_of_the_same_computer() {
    let (_repositories, source) = repository();
    let workspaces = tempfile::tempdir().unwrap();
    let target = Target::start(Steered::new(workspaces.path(), full(), None), &[]);
    let store: Arc<dyn StateStore> = Arc::new(MemoryState::new());
    let (daemon, node) = start_daemon(store.clone(), pool(&[("target-a", &target)])).await;

    daemon
        .create_computer_environment(
            definition(
                "myapp",
                ComputerLifecycle::Persistent,
                requirements(),
                project_contents(&source, "v1"),
            ),
            "alice",
        )
        .await
        .unwrap();
    let first = computer_where(&daemon, "myapp", "the first build and start", |view| {
        view.converged
    })
    .await;
    assert_eq!(first.observed.builds["app"].evidence.outcome, "succeeded");
    let machine = first.machine.clone().expect("a machine");
    let resource = machine.resource.clone().expect("the provider's resource");
    assert_eq!(first.endpoints[0].port, 18_555);
    assert!(first.endpoints[0].url.as_deref() == Some("http://127.0.0.1:18555"));
    assert_eq!(
        run(&daemon, "myapp", "alice", &["cat", "running-build"])
            .await
            .0,
        "v1"
    );
    assert_eq!(
        run(&daemon, "myapp", "alice", &["cat", "port"]).await.0,
        "18555\n"
    );

    // Build, test, and project commands run inside the environment's
    // computer — the target's workspace — never on the daemon's node.
    for command in ["build", "test", "where"] {
        let exec = daemon
            .project_command(
                "myapp",
                "alice",
                ProjectCommandRequest {
                    project: "app".into(),
                    command: command.into(),
                    env: BTreeMap::new(),
                    timeout: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(exec.session_id, machine.session_id);
        let job = job_of(&daemon, "myapp", "alice", &exec).await;
        let result = job.result.unwrap();
        assert_eq!(result.result.exit_code, Some(0), "{command}: {result:?}");
        if command == "where" {
            let directory = result.result.stdout.text.trim().to_owned();
            assert!(
                directory.starts_with(&workspaces.path().display().to_string())
                    && directory.ends_with("repos/app"),
                "{directory} is not the computer's checkout"
            );
            assert!(!directory.starts_with(&node.path().display().to_string()));
        }
    }
    let unknown = daemon
        .project_command(
            "myapp",
            "alice",
            ProjectCommandRequest {
                project: "app".into(),
                command: "deploy-to-prod".into(),
                env: BTreeMap::new(),
                timeout: None,
            },
        )
        .await;
    assert!(matches!(unknown, Err(EnvironmentError::NotFound(_))));

    // A bundle is never deployed to the daemon's node for this environment.
    let refused = daemon
        .deploy(DeployRequest {
            project: "app".into(),
            environment: "myapp".into(),
            ..DeployRequest::default()
        })
        .await;
    assert!(
        matches!(&refused, Err(EnvironmentError::Invalid(message)) if message.contains("own computer")),
        "{refused:?}"
    );

    // A release is a change of desired state: the same machine checks out
    // v2, builds it, and restarts the application. No redeployment.
    let released = daemon
        .release_project(
            "myapp",
            "alice",
            ReleaseRequest {
                project: "app".into(),
                revision: "v2".into(),
                expected_generation: Some(first.desired.generation),
            },
        )
        .await
        .unwrap();
    let generation = released.desired.generation;
    let second = computer_where(&daemon, "myapp", "the release", |view| {
        view.converged && view.observed.converged_generation == generation
    })
    .await;
    assert_eq!(
        run(&daemon, "myapp", "alice", &["cat", "running-build"])
            .await
            .0,
        "v2"
    );
    let after = second.machine.clone().unwrap();
    assert_eq!(
        after.resource.as_deref(),
        Some(resource.as_str()),
        "the same provider resource"
    );
    assert_eq!(after.session_id, machine.session_id);
    assert_eq!(target.provider.provisions.load(Ordering::SeqCst), 1);
    let kinds = events(&daemon, "myapp").await;
    assert!(
        kinds
            .iter()
            .any(|(kind, data)| kind == "environment.release"
                && data["from"] == "v1"
                && data["to"] == "v2")
    );
    assert!(
        kinds
            .iter()
            .any(|(kind, data)| kind == "environment.contents_applied"
                && data["kind"] == "build"
                && data["session_id"] == machine.session_id.as_str())
    );
    assert!(
        kinds
            .iter()
            .any(|(kind, data)| kind == "environment.command"
                && data["command"] == "test"
                && data["machine"] == resource.as_str())
    );
    // A stale release is refused, never applied over someone else's change.
    let stale = daemon
        .release_project(
            "myapp",
            "alice",
            ReleaseRequest {
                project: "app".into(),
                revision: "v1".into(),
                expected_generation: Some(first.desired.generation),
            },
        )
        .await;
    assert!(matches!(stale, Err(EnvironmentError::Conflict(_))));

    // Configuration is in place too: what depends on it restarts.
    let pid = second.observed.processes["api"].pid;
    let configured = daemon
        .set_config(
            "myapp",
            "alice",
            BTreeMap::from([("GREETING".into(), "hi".into())]),
        )
        .await
        .unwrap();
    let generation = configured.desired.generation;
    let third = computer_where(&daemon, "myapp", "the new configuration", |view| {
        view.converged && view.observed.converged_generation == generation
    })
    .await;
    assert_ne!(third.observed.processes["api"].pid, pid, "restarted");
    assert_eq!(
        third.machine.as_ref().unwrap().session_id,
        machine.session_id
    );
    assert_eq!(
        run(&daemon, "myapp", "alice", &["printenv", "GREETING"])
            .await
            .0,
        "hi\n"
    );

    // A broken build leaves what runs from the repository running.
    let pid = third.observed.processes["api"].pid;
    let mut broken = third.desired.projects[0].clone();
    broken.build = vec!["false".into()];
    daemon
        .upsert_project("myapp", "alice", broken)
        .await
        .unwrap();
    let failed = computer_where(&daemon, "myapp", "the failed build", |view| {
        view.observed
            .builds
            .get("app")
            .is_some_and(|build| build.evidence.outcome == "failed")
    })
    .await;
    assert_eq!(failed.observed.processes["api"].pid, pid);
    assert_eq!(
        failed.observed.processes["api"].state,
        ProcessState::Running
    );
    let fixed = third.desired.projects[0].clone();
    daemon
        .upsert_project("myapp", "alice", fixed)
        .await
        .unwrap();
    computer_where(&daemon, "myapp", "the fixed build", |view| view.converged).await;

    // The controller restarts: same environment, same computer, same
    // desired state, and reconciliation resumes.
    daemon.shutdown().await;
    let (daemon, _node) = start_daemon(store.clone(), pool(&[("target-a", &target)])).await;
    let restarted = computer_where(&daemon, "myapp", "the restarted controller", |view| {
        view.converged
    })
    .await;
    assert_eq!(
        restarted.machine.as_ref().unwrap().resource.as_deref(),
        Some(resource.as_str())
    );
    assert_eq!(restarted.desired.repositories[0].revision, "v2");
    // Configuration survives the restart: the process environment holds it,
    // and the view reports it without the value (not a known-public name).
    assert!(!restarted.config.contains_key("GREETING"));
    let greeting = restarted
        .configuration
        .variables
        .iter()
        .find(|variable| variable.name == "GREETING")
        .expect("the variable is configured");
    assert!(greeting.configured && greeting.sensitive && greeting.value.is_none());
    assert_eq!(
        run(&daemon, "myapp", "alice", &["printenv", "GREETING"])
            .await
            .0
            .trim(),
        "hi"
    );
    let released = daemon
        .release_project(
            "myapp",
            "alice",
            ReleaseRequest {
                project: "app".into(),
                revision: "v1".into(),
                expected_generation: None,
            },
        )
        .await
        .unwrap();
    let generation = released.desired.generation;
    computer_where(&daemon, "myapp", "a release after the restart", |view| {
        view.converged && view.observed.converged_generation == generation
    })
    .await;
    assert_eq!(
        run(&daemon, "myapp", "alice", &["cat", "running-build"])
            .await
            .0,
        "v1"
    );
    assert_eq!(target.provider.provisions.load(Ordering::SeqCst), 1);

    // Only an explicit replacement changes the backing machine.
    daemon
        .replace_computer(
            "myapp",
            "alice",
            ComputerRequirements {
                cpu_count: Some(2),
                ..requirements()
            },
        )
        .await
        .unwrap();
    let replaced = computer_where(&daemon, "myapp", "the replacement", |view| {
        view.converged
            && view.running_generation == 2
            && view
                .machine
                .as_ref()
                .is_some_and(|machine| machine.session_id != after.session_id)
    })
    .await;
    assert_ne!(
        replaced.machine.as_ref().unwrap().resource.as_deref(),
        Some(resource.as_str())
    );
    assert_eq!(target.provider.provisions.load(Ordering::SeqCst), 2);
    assert_eq!(
        run(&daemon, "myapp", "alice", &["cat", "running-build"])
            .await
            .0,
        "v1"
    );
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_sessions_enter_environments_and_temporary_ones_end_with_them() {
    let workspaces = tempfile::tempdir().unwrap();
    let target = Target::start(Steered::new(workspaces.path(), full(), None), &[]);
    let (daemon, _node) =
        start_daemon(Arc::new(MemoryState::new()), pool(&[("target-a", &target)])).await;

    // A persistent environment: sessions come and go, it stays.
    daemon
        .create_computer_environment(
            definition(
                "keep",
                ComputerLifecycle::Persistent,
                requirements(),
                EnvironmentContents::default(),
            ),
            "alice",
        )
        .await
        .unwrap();
    let running = computer_where(&daemon, "keep", "the computer", |view| {
        view.status == ComputerStatus::Running
    })
    .await;
    let session = daemon
        .open_session(
            "alice",
            OpenSessionRequest {
                environment: Some("keep".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(session.kind, compute_state::WorkSessionKind::Attached);
    assert_eq!(session.status, compute_state::WorkSessionStatus::Open);
    assert!(session.connection.is_some(), "a way in");
    // Someone else can neither enter nor end it.
    let intruder = daemon
        .open_session(
            "mallory",
            OpenSessionRequest {
                environment: Some("keep".into()),
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(intruder, Err(EnvironmentError::Forbidden(_))));
    assert!(matches!(
        daemon.close_session("mallory", &session.session_id).await,
        Err(EnvironmentError::Forbidden(_))
    ));
    assert!(matches!(
        daemon.work_session("mallory", &session.session_id).await,
        Err(EnvironmentError::Forbidden(_))
    ));
    let closed = daemon
        .close_session("alice", &session.session_id)
        .await
        .unwrap();
    assert_eq!(closed.status, compute_state::WorkSessionStatus::Closed);
    tokio::time::sleep(Duration::from_millis(600)).await;
    let still = daemon.computer("keep").await.unwrap();
    assert_eq!(
        still.status,
        ComputerStatus::Running,
        "the environment outlives its session"
    );
    assert_eq!(still.session_id, running.session_id);
    // Enter it again: the same computer.
    let again = daemon
        .open_session(
            "alice",
            OpenSessionRequest {
                environment: Some("keep".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_ne!(again.session_id, session.session_id);
    assert_eq!(again.environment_id, session.environment_id);
    let listed = daemon.work_sessions("alice", Some("keep")).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].session_id, again.session_id, "newest first");
    assert!(
        daemon
            .work_sessions("mallory", None)
            .await
            .unwrap()
            .is_empty()
    );

    // A temporary environment of the session's own ends with it.
    let temporary = daemon
        .open_session(
            "alice",
            OpenSessionRequest {
                computer: Some(ComputerRequest {
                    lifecycle: ComputerLifecycle::Ephemeral,
                    requirements: requirements(),
                    target: None,
                    ttl_seconds: Some(3600),
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(temporary.kind, compute_state::WorkSessionKind::Ephemeral);
    assert!(temporary.expires_at.is_some());
    let name = temporary.environment.clone();
    computer_where(&daemon, &name, "the temporary computer", |view| {
        view.status == ComputerStatus::Running && view.lifecycle == ComputerLifecycle::Ephemeral
    })
    .await;
    run(&daemon, &name, "alice", &["true"]).await;
    daemon
        .close_session("alice", &temporary.session_id)
        .await
        .unwrap();
    let ended = computer_where(&daemon, &name, "the temporary computer's end", |view| {
        view.status == ComputerStatus::Destroyed
    })
    .await;
    assert!(ended.ended_at.is_some(), "the record remains as evidence");

    // One that expires ends its session too.
    let short = daemon
        .open_session(
            "alice",
            OpenSessionRequest {
                computer: Some(ComputerRequest {
                    lifecycle: ComputerLifecycle::Ephemeral,
                    requirements: requirements(),
                    target: None,
                    ttl_seconds: Some(3),
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    computer_where(&daemon, &short.environment, "expiry", |view| {
        view.status == ComputerStatus::Expired
    })
    .await;
    let expired = eventually("the session to end", async || {
        daemon
            .work_session("alice", &short.session_id)
            .await
            .ok()
            .filter(|view| view.status == compute_state::WorkSessionStatus::Closed)
    })
    .await;
    assert!(expired.close_reason.unwrap().contains("expired"));
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn go_changes_lifetime_and_configuration_in_place_and_refuses_stale_views() {
    let (_repositories, source) = repository();
    let workspaces = tempfile::tempdir().unwrap();
    let target = Target::start(Steered::new(workspaces.path(), full(), None), &[]);
    let (daemon, _node) =
        start_daemon(Arc::new(MemoryState::new()), pool(&[("target-a", &target)])).await;
    let mut definition = definition(
        "trial",
        ComputerLifecycle::Ephemeral,
        requirements(),
        contents(&source, "v1"),
    );
    definition.computer.ttl_seconds = Some(600);
    daemon
        .create_computer_environment(definition, "alice")
        .await
        .unwrap();
    let loaded = computer_where(&daemon, "trial", "the temporary computer", |view| {
        view.converged
    })
    .await;
    let session_id = loaded.machine.as_ref().unwrap().session_id.clone();
    let owned = target.client().session(&session_id).await.unwrap();
    assert_eq!(owned.ownership, compute_core::SessionOwnership::Ephemeral);

    // GO: contents, configuration, and lifetime in one fenced change.
    let mut contents = loaded.desired.clone();
    contents.repositories[0].revision = "v2".into();
    let went = daemon
        .set_contents(
            "trial",
            "alice",
            ContentsUpdate {
                contents: contents.clone(),
                expected_generation: Some(loaded.desired.generation),
                config: Some(BTreeMap::from([("MODE".into(), "kept".into())])),
                lifecycle: Some(LifecycleChange {
                    lifecycle: ComputerLifecycle::Persistent,
                    ttl_seconds: None,
                }),
            },
        )
        .await
        .unwrap();
    assert_eq!(went.requested_lifecycle, ComputerLifecycle::Persistent);
    let kept = computer_where(&daemon, "trial", "the kept computer", |view| {
        view.converged && view.lifecycle == ComputerLifecycle::Persistent
    })
    .await;
    assert!(kept.expires_at.is_none());
    assert_eq!(
        kept.machine.as_ref().unwrap().session_id,
        session_id,
        "in place"
    );
    let claimed = target.client().session(&session_id).await.unwrap();
    assert_eq!(claimed.ownership, compute_core::SessionOwnership::Claimed);
    assert!(claimed.expires_at.is_none());
    assert_eq!(
        run(&daemon, "trial", "alice", &["cat", "running-version"])
            .await
            .0,
        "v2"
    );
    assert_eq!(
        run(&daemon, "trial", "alice", &["printenv", "MODE"])
            .await
            .0,
        "kept\n"
    );

    // Someone else's change in between: the stale GO is refused, and
    // nothing of it is applied.
    let stale = daemon
        .set_contents(
            "trial",
            "alice",
            ContentsUpdate {
                contents: loaded.desired.clone(),
                expected_generation: Some(loaded.desired.generation),
                config: None,
                lifecycle: None,
            },
        )
        .await;
    assert!(
        matches!(&stale, Err(EnvironmentError::Conflict(message)) if message.contains("changed since you loaded it"))
    );
    assert_eq!(
        daemon.computer("trial").await.unwrap().desired.repositories[0].revision,
        "v2"
    );

    // And temporary again: Compute, not the target, ends it.
    let temporary = daemon
        .set_lifecycle(
            "trial",
            "alice",
            LifecycleChange {
                lifecycle: ComputerLifecycle::Ephemeral,
                ttl_seconds: Some(2),
            },
        )
        .await
        .unwrap();
    assert!(temporary.expires_at.is_some());
    let expired = computer_where(&daemon, "trial", "expiry", |view| {
        view.status == ComputerStatus::Expired
    })
    .await;
    assert_eq!(
        expired.observed.repositories["app"].revision, "v2",
        "evidence kept"
    );
    daemon.shutdown().await;
}

// ---- Versions: publish, deploy, promote, roll back --------------------------

async fn version_where(
    daemon: &Arc<Daemon>,
    project: &str,
    label: &str,
    wanted: impl Fn(&compute_state::VersionRecord) -> bool,
) -> compute_state::VersionRecord {
    eventually(&format!("{project} {label}"), async || {
        daemon
            .version(project, label)
            .await
            .ok()
            .filter(|version| wanted(version))
    })
    .await
}

async fn rollout_where(
    daemon: &Arc<Daemon>,
    rollout_id: &str,
    wanted: impl Fn(&compute_state::RolloutRecord) -> bool,
) -> compute_state::RolloutRecord {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        let rollout = daemon.rollout(rollout_id).await.unwrap();
        if wanted(&rollout) {
            return rollout;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for rollout {rollout_id}: {rollout:#?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn versions_are_published_deployed_promoted_and_rolled_back_in_place() {
    let (_repositories, source) = repository();
    let workspaces = tempfile::tempdir().unwrap();
    let target = Target::start(Steered::new(workspaces.path(), full(), None), &[]);
    let store: Arc<dyn StateStore> = Arc::new(MemoryState::new());
    let (daemon, _node) = start_daemon(store.clone(), pool(&[("target-a", &target)])).await;
    let mut dev = project_contents(&source, "v1");
    dev.projects[0]
        .commands
        .insert("lint".into(), vec!["true".into()]);
    dev.projects[0].checks = vec!["lint".into()];
    dev.processes[0].port = None;
    for (name, contents) in [
        ("dev", dev),
        ("test", EnvironmentContents::default()),
        ("production", EnvironmentContents::default()),
    ] {
        daemon
            .create_computer_environment(
                definition(
                    name,
                    ComputerLifecycle::Persistent,
                    requirements(),
                    contents,
                ),
                "alice",
            )
            .await
            .unwrap();
    }
    computer_where(&daemon, "dev", "dev", |view| view.converged).await;
    computer_where(&daemon, "production", "production", |view| view.converged).await;
    let production = daemon
        .computer("production")
        .await
        .unwrap()
        .machine
        .unwrap();

    // Publish: build, tests, checks, and the package, in dev's computer.
    let publishing = daemon
        .publish_version(
            "app",
            "alice",
            PublishRequest {
                environment: "dev".into(),
                version: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(publishing.version, "0.1.0");
    let first = version_where(&daemon, "app", "0.1.0", |version| {
        version.status != compute_state::VersionStatus::Publishing
    })
    .await;
    assert_eq!(
        first.status,
        compute_state::VersionStatus::Published,
        "{first:#?}"
    );
    assert!(
        first
            .package_digest
            .as_deref()
            .is_some_and(|digest| digest.starts_with("sha256:"))
    );
    let steps = first
        .steps
        .iter()
        .map(|step| (step.name.as_str(), step.status))
        .collect::<Vec<_>>();
    assert!(
        steps.iter().all(|(_, status)| status.is_done()),
        "{steps:?}"
    );
    assert!(
        first
            .steps
            .iter()
            .find(|step| step.name == "Tests")
            .unwrap()
            .job_id
            .is_some()
    );
    assert!(first.assembly.repository.as_ref().unwrap().revision == first.commit.clone().unwrap());
    // A version is immutable.
    assert!(matches!(
        daemon
            .publish_version(
                "app",
                "alice",
                PublishRequest {
                    environment: "dev".into(),
                    version: Some("0.1.0".into())
                }
            )
            .await,
        Err(EnvironmentError::Conflict(_))
    ));

    // Deploy to test: an empty computer gets the repository, the project,
    // and its application.
    let rollout = daemon
        .deploy_version(
            "app",
            "alice",
            DeployVersionRequest {
                environment: "test".into(),
                version: "0.1.0".into(),
                expected_generation: None,
            },
        )
        .await
        .unwrap();
    let active = rollout_where(&daemon, &rollout.rollout_id, |rollout| {
        rollout.status != compute_state::RolloutStatus::Applying
    })
    .await;
    assert_eq!(
        active.status,
        compute_state::RolloutStatus::Active,
        "{active:#?}"
    );
    assert_eq!(
        run(&daemon, "test", "alice", &["cat", "running-build"])
            .await
            .0,
        "v1"
    );

    // Promote test → production, after reviewing what it would do.
    let plan = daemon
        .promotion_plan("app", "alice", "test", "production")
        .await
        .unwrap();
    assert_eq!(plan.version, "0.1.0");
    assert!(plan.to_current.is_none());
    assert!(plan.from_healthy);
    assert!(
        plan.changes
            .iter()
            .any(|change| change.contains("add repository"))
    );
    let promoted = daemon
        .promote_version(
            "app",
            "alice",
            PromoteVersionRequest {
                from: "test".into(),
                to: "production".into(),
                expected_generation: Some(plan.expected_generation),
            },
        )
        .await
        .unwrap();
    assert_eq!(promoted.kind, compute_state::RolloutKind::Promote);
    rollout_where(&daemon, &promoted.rollout_id, |rollout| {
        rollout.status == compute_state::RolloutStatus::Active
    })
    .await;
    assert_eq!(
        run(&daemon, "production", "alice", &["cat", "running-build"])
            .await
            .0,
        "v1"
    );

    // A new version from dev: v2 of the source.
    let released = daemon
        .release_project(
            "dev",
            "alice",
            ReleaseRequest {
                project: "app".into(),
                revision: "v2".into(),
                expected_generation: None,
            },
        )
        .await
        .unwrap();
    let generation = released.desired.generation;
    computer_where(&daemon, "dev", "dev at v2", |view| {
        view.converged && view.observed.converged_generation == generation
    })
    .await;
    daemon
        .publish_version(
            "app",
            "alice",
            PublishRequest {
                environment: "dev".into(),
                version: None,
            },
        )
        .await
        .unwrap();
    let second = version_where(&daemon, "app", "0.1.1", |version| {
        version.status == compute_state::VersionStatus::Published
    })
    .await;
    assert_ne!(second.commit, first.commit);
    let to_test = daemon
        .deploy_version(
            "app",
            "alice",
            DeployVersionRequest {
                environment: "test".into(),
                version: "0.1.1".into(),
                expected_generation: None,
            },
        )
        .await
        .unwrap();
    rollout_where(&daemon, &to_test.rollout_id, |rollout| {
        rollout.status == compute_state::RolloutStatus::Active
    })
    .await;

    // A controller restart in the middle of a promotion: it finishes.
    let promoting = daemon
        .promote_version(
            "app",
            "alice",
            PromoteVersionRequest {
                from: "test".into(),
                to: "production".into(),
                expected_generation: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(promoting.previous_version.as_deref(), Some("0.1.0"));
    daemon.shutdown().await;
    let (daemon, _node) = start_daemon(store.clone(), pool(&[("target-a", &target)])).await;
    let finished = rollout_where(&daemon, &promoting.rollout_id, |rollout| {
        rollout.status != compute_state::RolloutStatus::Applying
    })
    .await;
    assert_eq!(
        finished.status,
        compute_state::RolloutStatus::Active,
        "{finished:#?}"
    );
    assert_eq!(
        run(&daemon, "production", "alice", &["cat", "running-build"])
            .await
            .0,
        "v2"
    );
    let history = daemon
        .rollouts(Some("production"), Some("app"))
        .await
        .unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[1].status, compute_state::RolloutStatus::Superseded);

    // Roll back: the previous version, on the same machine.
    let rolled = daemon
        .rollback_version(
            "app",
            "alice",
            RollbackRequest {
                environment: "production".into(),
                version: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(rolled.version, "0.1.0");
    rollout_where(&daemon, &rolled.rollout_id, |rollout| {
        rollout.status == compute_state::RolloutStatus::Active
    })
    .await;
    assert_eq!(
        run(&daemon, "production", "alice", &["cat", "running-build"])
            .await
            .0,
        "v1"
    );
    let after = daemon
        .computer("production")
        .await
        .unwrap()
        .machine
        .unwrap();
    assert_eq!(
        after.resource, production.resource,
        "no machine was replaced"
    );
    assert_eq!(
        target.provider.provisions.load(Ordering::SeqCst),
        3,
        "one per environment"
    );

    // The overview: the project, where it runs, at which version.
    let software = daemon.software().await.unwrap();
    let app = software
        .iter()
        .find(|summary| summary.project == "app")
        .unwrap();
    assert_eq!(app.latest_version.as_deref(), Some("0.1.1"));
    let running = app
        .environments
        .iter()
        .find(|placement| placement.environment == "production")
        .unwrap();
    assert_eq!(running.version.as_deref(), Some("0.1.0"));

    // Someone else cannot publish, deploy, or promote it.
    assert!(matches!(
        daemon
            .deploy_version(
                "app",
                "mallory",
                DeployVersionRequest {
                    environment: "production".into(),
                    version: "0.1.1".into(),
                    expected_generation: None
                }
            )
            .await,
        Err(EnvironmentError::Forbidden(_))
    ));
    // A failing check refuses the version, with the evidence.
    let mut failing = daemon.computer("dev").await.unwrap().desired.projects[0].clone();
    failing.commands.insert("lint".into(), vec!["false".into()]);
    daemon
        .upsert_project("dev", "alice", failing)
        .await
        .unwrap();
    computer_where(&daemon, "dev", "the new check", |view| view.converged).await;
    daemon
        .publish_version(
            "app",
            "alice",
            PublishRequest {
                environment: "dev".into(),
                version: None,
            },
        )
        .await
        .unwrap();
    let refused = version_where(&daemon, "app", "0.1.2", |version| {
        version.status != compute_state::VersionStatus::Publishing
    })
    .await;
    assert_eq!(refused.status, compute_state::VersionStatus::Failed);
    let checks = refused
        .steps
        .iter()
        .find(|step| step.name == "Checks")
        .unwrap();
    assert_eq!(checks.status, compute_core::StepStatus::Failed);
    assert!(checks.job_id.is_some());
    daemon.shutdown().await;
}

// ---- Target reality ----------------------------------------------------

/// A TCP proxy in front of a target, steerable by a test: it forwards to
/// an upstream, refuses when there is none, and can hold the next answer
/// back until released, so the answer arrives after the world moved on.
struct Proxy {
    endpoint: String,
    upstream: Arc<std::sync::Mutex<Option<String>>>,
    hold_next: Arc<AtomicBool>,
    holding: Arc<AtomicBool>,
    release: Arc<tokio::sync::Notify>,
    runtime: Option<Runtime>,
}

impl Proxy {
    fn start(upstream: &Target) -> Self {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let socket = StdTcpListener::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", socket.local_addr().unwrap());
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let mut proxy = Self {
            endpoint,
            upstream: Arc::new(std::sync::Mutex::new(Some(upstream.address.to_string()))),
            hold_next: Arc::new(AtomicBool::new(false)),
            holding: Arc::new(AtomicBool::new(false)),
            release: Arc::new(tokio::sync::Notify::new()),
            runtime: None,
        };
        let (upstream, hold_next, holding, release) = (
            proxy.upstream.clone(),
            proxy.hold_next.clone(),
            proxy.holding.clone(),
            proxy.release.clone(),
        );
        runtime.spawn(async move {
            let listener = tokio::net::TcpListener::from_std(socket).unwrap();
            loop {
                let Ok((mut client, _)) = listener.accept().await else {
                    continue;
                };
                let upstream = upstream.lock().unwrap().clone();
                let (hold_next, holding, release) =
                    (hold_next.clone(), holding.clone(), release.clone());
                tokio::spawn(async move {
                    // No upstream: the connection closes unanswered.
                    let Some(upstream) = upstream else {
                        return;
                    };
                    let Ok(server) = tokio::net::TcpStream::connect(&upstream).await else {
                        return;
                    };
                    let (mut client_read, mut client_write) = client.split();
                    let (mut server_read, mut server_write) = server.into_split();
                    let forward = async {
                        let mut buffer = [0_u8; 8192];
                        loop {
                            match client_read.read(&mut buffer).await {
                                Ok(0) | Err(_) => break,
                                Ok(read) => {
                                    if server_write.write_all(&buffer[..read]).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                        std::future::pending::<()>().await
                    };
                    let answer = async {
                        let mut response = vec![];
                        let _ = server_read.read_to_end(&mut response).await;
                        response
                    };
                    let response = tokio::select! {
                        response = answer => response,
                        _ = forward => unreachable!(),
                    };
                    if hold_next.swap(false, Ordering::SeqCst) {
                        holding.store(true, Ordering::SeqCst);
                        release.notified().await;
                        holding.store(false, Ordering::SeqCst);
                    }
                    let _ = client_write.write_all(&response).await;
                    let _ = client_write.shutdown().await;
                });
            }
        });
        proxy.runtime = Some(runtime);
        proxy
    }

    fn forward_to(&self, target: Option<&Target>) {
        *self.upstream.lock().unwrap() = target.map(|target| target.address.to_string());
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.release.notify_one();
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

fn proxied_pool(id: &str, proxy: &Proxy, target: &Target) -> PoolConfig {
    let mut config = pool(&[(id, target)]);
    config.providers.get_mut(id).unwrap().endpoint = Some(proxy.endpoint.clone());
    config
}

async fn converged(daemon: &Arc<Daemon>, name: &str) -> ComputerView {
    computer_where(daemon, name, "a running, converged computer", |view| {
        view.status == ComputerStatus::Running
            && view.converged
            && view.reality.observed == "running"
    })
    .await
}

fn kinds(events: &[(String, serde_json::Value)]) -> Vec<&str> {
    events.iter().map(|(kind, _)| kind.as_str()).collect()
}

/// A target that stops answering makes its computer unreachable, never
/// running and never gone: the environment keeps wanting it, and the same
/// machine is running again when the target answers. A target that
/// refuses this control plane's credential is unreachable too, for that
/// reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreachable_target_keeps_desired_state_and_recovers_the_same_machine() {
    let (_repositories, source) = repository();
    let workspaces = tempfile::tempdir().unwrap();
    let mut target = Target::start(Steered::new(workspaces.path(), full(), None), &[]);
    let (daemon, _node) =
        start_daemon(Arc::new(MemoryState::new()), pool(&[("target-a", &target)])).await;
    daemon
        .create_computer_environment(
            definition(
                "reality",
                ComputerLifecycle::Persistent,
                requirements(),
                contents(&source, "v1"),
            ),
            "alice",
        )
        .await
        .unwrap();
    let running = converged(&daemon, "reality").await;
    assert_eq!(running.reality.desired, "running");
    assert!(running.reality.confirmed_at.is_some());
    let session = running.session_id.clone().unwrap();

    // The target goes away.
    target.stop();
    let unreachable = computer_where(&daemon, "reality", "unreachable", |view| {
        view.status == ComputerStatus::Unreachable
    })
    .await;
    assert_eq!(unreachable.reality.observed, "unreachable");
    assert_eq!(unreachable.reality.desired, "running");
    assert!(unreachable.reality.since.is_some());
    assert!(unreachable.reality.confirmed_at.is_none());
    assert!(
        unreachable
            .endpoints
            .iter()
            .all(|endpoint| !endpoint.serving)
    );
    let failure = unreachable.failure.clone().unwrap();
    assert_eq!(failure.code, "target_unreachable");
    assert!(failure.retryable);
    // Desired state is untouched: the environment still asks for the same
    // computer and contents.
    let environment = daemon.environment("reality").await.unwrap();
    assert_eq!(environment.desired_state, DesiredState::Running);
    assert_eq!(unreachable.desired.repositories[0].revision, "v1");
    assert_eq!(unreachable.session_id.as_deref(), Some(session.as_str()));
    // Work in it says why it cannot run, in the structured failure kind.
    let refused = daemon
        .computer_exec("reality", "alice", SessionCommand::new(vec!["true".into()]))
        .await
        .unwrap_err();
    assert_eq!(refused.kind(), "runtime_unavailable", "{refused}");
    assert!(refused.to_string().contains("unreachable"), "{refused}");
    // It stays unreachable while the target is away: never lost, never
    // running.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        daemon.computer("reality").await.unwrap().status,
        ComputerStatus::Unreachable
    );

    // The target comes back with its stores: the same machine, running.
    target.restart();
    let recovered = converged(&daemon, "reality").await;
    assert_eq!(recovered.session_id.as_deref(), Some(session.as_str()));
    assert!(recovered.failure.is_none());
    assert_eq!(target.provider.provisions.load(Ordering::SeqCst), 1);
    let history = events(&daemon, "reality").await;
    let history = kinds(&history);
    let unreachable_at = history
        .iter()
        .position(|kind| *kind == "computer.unreachable")
        .expect("an unreachable event");
    let recovered_at = history
        .iter()
        .position(|kind| *kind == "computer.recovered")
        .expect("a recovered event");
    assert!(unreachable_at < recovered_at, "{history:?}");
    assert!(!history.contains(&"computer.lost"), "{history:?}");
    let (output, _) = run(&daemon, "reality", "alice", &["cat", "running-version"]).await;
    assert_eq!(output.trim(), "v1");

    // The target stops trusting this control plane: unreachable, because
    // nothing can be known about the machine, not lost.
    let original = std::fs::read(&target.trust).unwrap();
    let mut trust = compute_provider::TargetCredentials::load(&target.trust).unwrap();
    for credential in trust.credentials.clone() {
        trust.revoke(&credential.credential_id).unwrap();
    }
    trust.save(&target.trust).unwrap();
    bump(&target.trust);
    let refused = computer_where(&daemon, "reality", "credential rejected", |view| {
        view.status == ComputerStatus::Unreachable
    })
    .await;
    assert_eq!(refused.failure.unwrap().code, "credential_rejected");
    let restored = target.trust.with_extension("restored");
    std::fs::write(&restored, original).unwrap();
    std::fs::rename(&restored, &target.trust).unwrap();
    bump(&target.trust);
    let trusted_again = converged(&daemon, "reality").await;
    assert_eq!(trusted_again.session_id.as_deref(), Some(session.as_str()));
    daemon.shutdown().await;
}

/// Make a rewritten trust file visibly newer, whatever the filesystem's
/// timestamp resolution.
fn bump(path: &Path) {
    let file = std::fs::File::options().append(true).open(path).unwrap();
    let modified = file.metadata().unwrap().modified().unwrap();
    file.set_modified(modified + Duration::from_secs(2))
        .unwrap();
}

/// A machine that disappears, or a target that loses its session store,
/// makes the computer lost: the target answered, and the machine is not
/// there. A lost computer is never reported healthy and never recreated
/// on its own; the environment keeps wanting it, and an explicit
/// replacement provisions a new machine with the same contents.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_machine_or_session_that_disappears_is_lost_until_replaced() {
    let (_repositories, source) = repository();
    let workspaces = tempfile::tempdir().unwrap();
    let mut target = Target::start(Steered::new(workspaces.path(), full(), None), &[]);
    let (daemon, _node) =
        start_daemon(Arc::new(MemoryState::new()), pool(&[("target-a", &target)])).await;
    daemon
        .create_computer_environment(
            definition(
                "vanishing",
                ComputerLifecycle::Persistent,
                requirements(),
                contents(&source, "v1"),
            ),
            "alice",
        )
        .await
        .unwrap();
    let running = converged(&daemon, "vanishing").await;
    let first_session = running.session_id.clone().unwrap();

    // The machine itself disappears (its workspace is gone).
    let resource = running.machine.as_ref().unwrap().resource.clone().unwrap();
    std::fs::remove_dir_all(workspaces.path().join(&resource)).unwrap();
    let lost = computer_where(&daemon, "vanishing", "lost", |view| {
        view.status == ComputerStatus::Lost
    })
    .await;
    assert_eq!(lost.reality.observed, "lost");
    assert_eq!(lost.reality.desired, "running");
    assert!(
        lost.reality.explanation.contains("replace"),
        "{:?}",
        lost.reality
    );
    let failure = lost.failure.clone().unwrap();
    assert_eq!(failure.code, "machine_missing");
    assert!(!failure.retryable);
    assert!(lost.endpoints.iter().all(|endpoint| !endpoint.serving));
    // It stays lost: nothing re-provisions it on its own, and an explicit
    // reconcile finds the machine still gone.
    tokio::time::sleep(Duration::from_secs(2)).await;
    daemon
        .reconcile_computer("vanishing", "alice")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let still = daemon.computer("vanishing").await.unwrap();
    assert_eq!(still.status, ComputerStatus::Lost);
    assert_eq!(target.provider.provisions.load(Ordering::SeqCst), 1);
    assert_eq!(
        daemon.environment("vanishing").await.unwrap().desired_state,
        DesiredState::Running
    );
    let refused = daemon
        .computer_exec(
            "vanishing",
            "alice",
            SessionCommand::new(vec!["true".into()]),
        )
        .await
        .unwrap_err();
    assert_eq!(refused.kind(), "conflict", "{refused}");
    assert!(refused.to_string().contains("lost"), "{refused}");
    let history = events(&daemon, "vanishing").await;
    assert!(kinds(&history).contains(&"computer.lost"), "{history:?}");

    // Replacement: a new machine for the same environment and contents.
    daemon
        .replace_computer("vanishing", "alice", requirements())
        .await
        .unwrap();
    let replaced = computer_where(&daemon, "vanishing", "a replacement", |view| {
        view.status == ComputerStatus::Running
            && view.converged
            && view.session_id.as_deref() != Some(first_session.as_str())
    })
    .await;
    assert_eq!(target.provider.provisions.load(Ordering::SeqCst), 2);
    let (output, _) = run(&daemon, "vanishing", "alice", &["cat", "running-version"]).await;
    assert_eq!(output.trim(), "v1");
    let second_session = replaced.session_id.clone().unwrap();

    // The target restarts without its session store: the session is gone.
    target.stop();
    target.wipe_sessions();
    target.restart();
    let lost_again = computer_where(&daemon, "vanishing", "lost after a wiped store", |view| {
        view.status == ComputerStatus::Lost
    })
    .await;
    assert_eq!(lost_again.failure.unwrap().code, "session_missing");
    assert_eq!(
        lost_again.session_id.as_deref(),
        Some(second_session.as_str())
    );

    // A destroy of a lost computer ends it; the record stays.
    daemon.destroy_computer("vanishing", "alice").await.unwrap();
    computer_where(&daemon, "vanishing", "destroyed", |view| {
        view.status == ComputerStatus::Destroyed
    })
    .await;
    daemon.shutdown().await;
}

/// An answer the target gave before the controller learned the machine is
/// gone cannot bring the computer back:
///
/// 1. the controller observes target A;
/// 2. target A becomes unavailable;
/// 3. the controller records unreachable, then lost;
/// 4. a delayed answer from target A (the machine is there) arrives;
/// 5. the computer stays lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_answer_from_a_target_cannot_revive_a_lost_computer() {
    let (_repositories, source) = repository();
    let workspaces = tempfile::tempdir().unwrap();
    let target_a = Target::start(Steered::new(workspaces.path(), full(), None), &[]);
    let proxy = Proxy::start(&target_a);
    let (daemon, _node) = start_daemon(
        Arc::new(MemoryState::new()),
        proxied_pool("target-a", &proxy, &target_a),
    )
    .await;
    daemon
        .create_computer_environment(
            definition(
                "fenced",
                ComputerLifecycle::Persistent,
                requirements(),
                contents(&source, "v1"),
            ),
            "alice",
        )
        .await
        .unwrap();
    // 1. Observed on target A.
    converged(&daemon, "fenced").await;

    // 2–3. Target A stops answering: unreachable.
    proxy.forward_to(None);
    computer_where(&daemon, "fenced", "unreachable", |view| {
        view.status == ComputerStatus::Unreachable
    })
    .await;

    // The controller asks target A again, and A answers that the machine
    // is there; that answer is held back.
    proxy.hold_next.store(true, Ordering::SeqCst);
    proxy.forward_to(Some(&target_a));
    eventually("an answer from target A to be held", async || {
        proxy.holding.load(Ordering::SeqCst).then_some(())
    })
    .await;

    // Meanwhile what answers at A's address is another machine, which does
    // not have the session: an explicit reconcile records the computer
    // lost.
    let other_root = tempfile::tempdir().unwrap();
    let target_b = Target::trusting_like(Steered::new(other_root.path(), full(), None), &target_a);
    proxy.forward_to(Some(&target_b));
    let lost = daemon.reconcile_computer("fenced", "alice").await.unwrap();
    assert_eq!(lost.status, ComputerStatus::Lost, "{:?}", lost.failure);
    let lost_generation = lost.generation;

    // 4. Target A's delayed answer arrives.
    proxy.release.notify_one();
    eventually("the held answer to be delivered", async || {
        (!proxy.holding.load(Ordering::SeqCst)).then_some(())
    })
    .await;

    // 5. The computer stays lost; nothing recovered it.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let after = daemon.computer("fenced").await.unwrap();
    assert_eq!(after.status, ComputerStatus::Lost);
    assert_eq!(after.reality.observed, "lost");
    assert!(after.generation >= lost_generation);
    let history = events(&daemon, "fenced").await;
    let history = kinds(&history);
    let lost_at = history
        .iter()
        .rposition(|kind| *kind == "computer.lost")
        .expect("a lost event");
    assert!(
        !history[lost_at..].contains(&"computer.recovered"),
        "a stale answer revived the computer: {history:?}"
    );
    daemon.shutdown().await;
}

fn empty_contents() -> EnvironmentContents {
    EnvironmentContents::default()
}

async fn running_computer(daemon: &Arc<Daemon>, name: &str) -> ComputerView {
    computer_where(daemon, name, "the computer to run", |view| {
        view.status == compute_core::ComputerStatus::Running
    })
    .await
}

async fn sh(daemon: &Arc<Daemon>, name: &str, script: &str) -> String {
    run(daemon, name, "alice", &["sh", "-c", script]).await.0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workspace_state_is_exported_seeded_and_verified_without_fork() {
    let (_repositories, source) = repository();
    let workspaces = tempfile::tempdir().unwrap();
    let target = Target::start(Steered::new(workspaces.path(), full(), None), &[]);
    let store: Arc<dyn StateStore> = Arc::new(MemoryState::new());
    let (daemon, _node) = start_daemon(store, pool(&[("target-a", &target)])).await;
    let create = |name: &'static str, contents: EnvironmentContents| {
        let daemon = daemon.clone();
        async move {
            daemon
                .create_computer_environment(
                    definition(
                        name,
                        ComputerLifecycle::Persistent,
                        requirements(),
                        contents,
                    ),
                    "alice",
                )
                .await
                .unwrap();
            running_computer(&daemon, name).await
        }
    };

    // A workspace with a repository (declared state), nested files, an empty
    // directory, an executable, and files no declared content owns.
    create("origin", contents(&source, "v1")).await;
    computer_where(&daemon, "origin", "converge", |view| view.converged).await;
    sh(
        &daemon,
        "origin",
        "mkdir -p data/nested empty && printf hello > data/notes.txt && printf deep > data/nested/x.bin \\
         && printf '#!/bin/sh\\necho hi\\n' > run.sh && chmod +x run.sh && printf mod >> repos/app/untracked.txt",
    )
    .await;

    let export = daemon.export_workspace("origin", "alice").await.unwrap();
    assert_eq!(export.identity, "compute.workspace@1");
    assert!(export.digest.starts_with("sha256:"));
    assert_eq!(
        export.directories, 1,
        "only the empty directory is recorded"
    );
    assert!(export.files >= 4, "{export:#?}");
    assert!(!export.job_id.is_empty());

    // Seed a second, empty workspace from it: no clone involved.
    create("dest", empty_contents()).await;
    let empty = daemon
        .verify_workspace("dest", "alice", WorkspaceVerifyRequest { digest: None })
        .await
        .unwrap();
    assert!(!empty.verified && empty.expected.is_none());
    assert_ne!(empty.digest, export.digest);
    let seed = daemon
        .seed_workspace(
            "dest",
            "alice",
            WorkspaceSeedRequest {
                archive: export.archive.clone(),
                digest: Some(export.digest.clone()),
            },
        )
        .await
        .unwrap();
    assert!(seed.verified);
    assert_eq!(seed.digest, export.digest);
    assert_eq!(seed.files, export.files);

    // Verified, and identical workspaces have identical digests.
    let verify = |name: &'static str, digest: String| {
        let daemon = daemon.clone();
        async move {
            daemon
                .verify_workspace(
                    name,
                    "alice",
                    WorkspaceVerifyRequest {
                        digest: Some(digest),
                    },
                )
                .await
                .unwrap()
        }
    };
    assert!(verify("dest", export.digest.clone()).await.verified);
    assert!(verify("origin", export.digest.clone()).await.verified);

    // The contents arrived; the repository (declared state) did not.
    assert_eq!(
        sh(&daemon, "dest", "cat data/notes.txt data/nested/x.bin").await,
        "hellodeep"
    );
    assert_eq!(
        sh(&daemon, "dest", "test -x run.sh && echo x || echo -")
            .await
            .trim(),
        "x"
    );
    assert_eq!(
        sh(&daemon, "dest", "test -d empty && echo yes")
            .await
            .trim(),
        "yes"
    );
    assert_eq!(
        sh(&daemon, "dest", "test -e repos && echo held || echo none")
            .await
            .trim(),
        "none",
        "declared repositories are not part of a workspace"
    );

    // Every part of the identity matters; nothing else does.
    for (change, undo) in [
        ("printf x > extra", "rm extra"),
        (
            "printf changed > data/notes.txt",
            "printf hello > data/notes.txt",
        ),
        ("chmod -x run.sh", "chmod +x run.sh"),
        ("rmdir empty", "mkdir empty"),
        ("mkdir another", "rmdir another"),
        (
            "mv data/notes.txt data/moved.txt",
            "mv data/moved.txt data/notes.txt",
        ),
    ] {
        sh(&daemon, "dest", change).await;
        let differs = verify("dest", export.digest.clone()).await;
        assert!(!differs.verified, "{change}");
        assert_ne!(differs.digest, export.digest, "{change}");
        sh(&daemon, "dest", undo).await;
        assert!(
            verify("dest", export.digest.clone()).await.verified,
            "{undo}"
        );
    }
    // Permissions, timestamps and non-empty directories are not identity.
    sh(
        &daemon,
        "dest",
        "chmod 640 data/notes.txt && touch -t 200001010000 data/notes.txt",
    )
    .await;
    assert!(verify("dest", export.digest.clone()).await.verified);
    sh(&daemon, "dest", "chmod 644 data/notes.txt").await;

    // A seed needs an empty workspace, and leaves what is there alone.
    let again = daemon
        .seed_workspace(
            "dest",
            "alice",
            WorkspaceSeedRequest {
                archive: export.archive.clone(),
                digest: None,
            },
        )
        .await;
    assert!(again.is_err(), "{again:?}");
    assert!(
        verify("dest", export.digest.clone()).await.verified,
        "nothing was disturbed"
    );

    // A digest that is not the archive's is refused before anything is sent.
    create("dest2", empty_contents()).await;
    let untouched = daemon
        .verify_workspace("dest2", "alice", WorkspaceVerifyRequest { digest: None })
        .await
        .unwrap()
        .digest;
    let wrong = format!("sha256:{}", "0".repeat(64));
    let mismatch = daemon
        .seed_workspace(
            "dest2",
            "alice",
            WorkspaceSeedRequest {
                archive: export.archive.clone(),
                digest: Some(wrong),
            },
        )
        .await;
    assert!(
        matches!(mismatch, Err(EnvironmentError::Conflict(_))),
        "{mismatch:?}"
    );
    // A corrupted archive is another workspace or unreadable; never seeded.
    let mut corrupted = export.archive.clone();
    let middle = corrupted.len() / 2;
    corrupted[middle] ^= 0xff;
    let refused = daemon
        .seed_workspace(
            "dest2",
            "alice",
            WorkspaceSeedRequest {
                archive: corrupted,
                digest: Some(export.digest.clone()),
            },
        )
        .await;
    assert!(refused.is_err(), "{refused:?}");
    let truncated = daemon
        .seed_workspace(
            "dest2",
            "alice",
            WorkspaceSeedRequest {
                archive: export.archive[..export.archive.len() / 2].to_vec(),
                digest: None,
            },
        )
        .await;
    assert!(truncated.is_err(), "{truncated:?}");
    let oversized = daemon
        .seed_workspace(
            "dest2",
            "alice",
            WorkspaceSeedRequest {
                archive: vec![0; compute_environment::daemon::WORKSPACE_ARCHIVE_LIMIT + 1],
                digest: None,
            },
        )
        .await;
    assert!(
        matches!(oversized, Err(EnvironmentError::Invalid(_))),
        "{oversized:?}"
    );
    assert_eq!(
        daemon
            .verify_workspace("dest2", "alice", WorkspaceVerifyRequest { digest: None })
            .await
            .unwrap()
            .digest,
        untouched,
        "every refused seed left the workspace empty"
    );

    // Portable state: the same export seeds a third and a fourth computer,
    // one of them ephemeral with different requirements. The primitive does
    // not know or care what the destination is for.
    let seeded = daemon
        .seed_workspace(
            "dest2",
            "alice",
            WorkspaceSeedRequest {
                archive: export.archive.clone(),
                digest: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(seeded.digest, export.digest);
    daemon
        .create_computer_environment(
            definition(
                "dest3",
                ComputerLifecycle::Ephemeral,
                ComputerRequirements {
                    memory_bytes: Some(128 << 20),
                    ..requirements()
                },
                empty_contents(),
            ),
            "alice",
        )
        .await
        .unwrap();
    running_computer(&daemon, "dest3").await;
    let third = daemon
        .seed_workspace(
            "dest3",
            "alice",
            WorkspaceSeedRequest {
                archive: export.archive.clone(),
                digest: Some(export.digest.clone()),
            },
        )
        .await
        .unwrap();
    assert!(third.verified);
    for name in ["origin", "dest", "dest2", "dest3"] {
        assert!(verify(name, export.digest.clone()).await.verified, "{name}");
    }

    // Only the owner exports, seeds, or verifies.
    assert!(daemon.export_workspace("origin", "mallory").await.is_err());
    assert!(
        daemon
            .seed_workspace(
                "dest2",
                "mallory",
                WorkspaceSeedRequest {
                    archive: export.archive.clone(),
                    digest: None
                }
            )
            .await
            .is_err()
    );
    assert!(
        daemon
            .verify_workspace("origin", "mallory", WorkspaceVerifyRequest { digest: None })
            .await
            .is_err()
    );

    // Unsupported entries are refused, by export and by verify.
    sh(&daemon, "origin", "ln -s /etc/passwd link").await;
    let linked = daemon.export_workspace("origin", "alice").await;
    assert!(
        matches!(&linked, Err(EnvironmentError::RuntimeUnavailable(reason)) if reason.contains("unsupported entry")),
        "{linked:?}"
    );
    assert!(
        daemon
            .verify_workspace("origin", "alice", WorkspaceVerifyRequest { digest: None })
            .await
            .is_err()
    );
    sh(&daemon, "origin", "rm link").await;
    assert!(daemon.export_workspace("origin", "alice").await.is_ok());

    // Each operation is evidenced.
    let recorded = events(&daemon, "dest").await;
    for command in ["workspace.seed", "workspace.verify"] {
        assert!(
            recorded.iter().any(|(_, data)| data["command"] == command),
            "{command}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_workspace_that_changes_while_it_is_captured_is_refused() {
    let workspaces = tempfile::tempdir().unwrap();
    let target = Target::start(Steered::new(workspaces.path(), full(), None), &[]);
    let store: Arc<dyn StateStore> = Arc::new(MemoryState::new());
    let (daemon, _node) = start_daemon(store, pool(&[("target-a", &target)])).await;
    let mut busy = EnvironmentContents::default();
    busy.processes.push(ProcessSpec {
        name: "writer".into(),
        kind: ProcessKind::Process,
        runtime: None,
        command: vec![
            "sh".into(),
            "-c".into(),
            "i=0; while [ $i -lt 3000 ]; do i=$((i+1)); : > \"tick$i\"; sleep 0.01; done".into(),
        ],
        repository: None,
        env: BTreeMap::new(),
        desired: ProcessDesired::Running,
        port: None,
        restart: 0,
        readiness: None,
        restart_policy: Default::default(),
        max_restarts: compute_core::DEFAULT_MAX_RESTARTS,
    });
    daemon
        .create_computer_environment(
            definition("busy", ComputerLifecycle::Persistent, requirements(), busy),
            "alice",
        )
        .await
        .unwrap();
    computer_where(&daemon, "busy", "the writer to run", |view| view.converged).await;
    eventually("the writer to be writing", async || {
        let count = sh(&daemon, "busy", "ls | grep -c '^tick' || true").await;
        (count.trim().parse::<u32>().unwrap_or(0) > 5).then_some(())
    })
    .await;
    let refused = daemon.export_workspace("busy", "alice").await;
    assert!(
        matches!(&refused, Err(EnvironmentError::RuntimeUnavailable(reason)) if changed_while_captured(reason)),
        "{refused:?}"
    );
    // A fork captures through the same path, so it is refused too, and
    // nothing is created under either name.
    let refused = daemon
        .fork_environment(
            "busy",
            "alice",
            ForkRequest {
                name: "busy-copy".into(),
                target: None,
                copy_config: false,
            },
        )
        .await;
    assert!(
        matches!(&refused, Err(EnvironmentError::RuntimeUnavailable(reason)) if changed_while_captured(reason)),
        "{refused:?}"
    );
    assert!(daemon.computer("busy-copy").await.is_err());
    assert!(daemon.computer("busy-copy--candidate").await.is_err());
    // A workspace process outlives its test unless it is stopped.
    daemon
        .set_process("busy", "alice", "writer", ProcessDesired::Stopped)
        .await
        .unwrap();
}

/// The identity of a computer's machine, and of what it holds.
struct Facts {
    environment_id: String,
    session: String,
    resource: String,
}

fn facts(view: &ComputerView) -> Facts {
    Facts {
        environment_id: view.environment_id.clone(),
        session: view.session_id.clone().unwrap(),
        resource: view.machine.as_ref().unwrap().resource.clone().unwrap(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replacing_a_computer_keeps_the_environment_and_its_workspace() {
    let (_repositories, source) = repository();
    let workspaces = tempfile::tempdir().unwrap();
    let target = Target::start(Steered::new(workspaces.path(), full(), None), &[]);
    let store: Arc<dyn StateStore> = Arc::new(MemoryState::new());
    let (daemon, _node) = start_daemon(store, pool(&[("target-a", &target)])).await;
    daemon
        .create_computer_environment(
            definition(
                "app",
                ComputerLifecycle::Persistent,
                requirements(),
                contents(&source, "v1"),
            ),
            "alice",
        )
        .await
        .unwrap();
    let first = computer_where(&daemon, "app", "converge", |view| view.converged).await;
    sh(
        &daemon,
        "app",
        "mkdir -p data/nested empty && printf hello > data/notes.txt && printf deep > data/nested/x.bin \\
         && printf '#!/bin/sh\\necho hi\\n' > run.sh && chmod +x run.sh",
    )
    .await;
    let digest = daemon
        .verify_workspace("app", "alice", WorkspaceVerifyRequest { digest: None })
        .await
        .unwrap()
        .digest;
    let before = facts(&first);
    let old_pid = first.observed.processes["api"].pid;

    // Names ending the candidate suffix are reserved.
    let reserved = daemon
        .create_computer_environment(
            definition(
                "x--candidate",
                ComputerLifecycle::Persistent,
                requirements(),
                empty_contents(),
            ),
            "alice",
        )
        .await;
    assert!(
        matches!(reserved, Err(EnvironmentError::Invalid(_))),
        "{reserved:?}"
    );

    let bigger = ComputerRequirements {
        cpu_count: Some(2),
        ..requirements()
    };
    let replaced = daemon
        .replace_computer("app", "alice", bigger.clone())
        .await
        .unwrap();

    // The Environment survives; the Computer does not.
    let after = facts(&replaced);
    assert_eq!(after.environment_id, before.environment_id);
    assert_ne!(after.session, before.session);
    assert_ne!(after.resource, before.resource);
    assert_eq!(replaced.requirements, bigger);
    assert_eq!(
        (replaced.spec_generation, replaced.running_generation),
        (2, 2)
    );
    assert_eq!(replaced.owner, first.owner);
    assert_eq!(
        replaced.desired, first.desired,
        "declared contents are untouched"
    );
    assert_eq!(replaced.config, first.config);

    // The workspace is the one that was captured, verified on the new machine.
    let verified = daemon
        .verify_workspace(
            "app",
            "alice",
            WorkspaceVerifyRequest {
                digest: Some(digest.clone()),
            },
        )
        .await
        .unwrap();
    assert!(verified.verified, "{verified:?}");
    assert_eq!(
        sh(&daemon, "app", "cat data/notes.txt data/nested/x.bin").await,
        "hellodeep"
    );
    assert_eq!(
        sh(&daemon, "app", "test -x run.sh && echo x").await.trim(),
        "x"
    );
    assert_eq!(
        sh(&daemon, "app", "test -d empty && echo yes").await.trim(),
        "yes"
    );

    // Reality: converged and running, the process on the new machine.
    let now = computer_where(&daemon, "app", "running and converged", |view| {
        view.converged && view.reality.observed == "running"
    })
    .await;
    assert_eq!(now.observed.processes["api"].state, ProcessState::Running);
    assert_ne!(now.observed.processes["api"].pid, old_pid);
    assert_eq!(sh(&daemon, "app", "cat running-version").await, "v1");

    // The old machine is retired: its session does not survive.
    eventually("the old session to end", async || {
        target
            .client()
            .session(&before.session)
            .await
            .ok()
            .filter(|s| s.status.is_terminal())
    })
    .await;
    // The candidate is gone; nothing of it outlives a successful replacement.
    assert!(daemon.computer("app--candidate").await.is_err());
    assert_eq!(target.provider.provisions.load(Ordering::SeqCst), 2);

    // The evidence tells the truth about the handoff.
    let recorded = events(&daemon, "app").await;
    let handoff = recorded
        .iter()
        .find(|(kind, _)| kind == "computer.replaced")
        .expect("the handoff is recorded");
    assert_eq!(handoff.1["from_session"], before.session.as_str());
    assert_eq!(handoff.1["to_session"], after.session.as_str());
    assert_eq!(handoff.1["workspace"], digest.as_str());
    assert_eq!(handoff.1["workspace_verified"], true);
    assert!(handoff.1["jobs"].as_array().unwrap().len() >= 4);

    // And it can be done again: the candidate name is free.
    let again = daemon
        .replace_computer("app", "alice", bigger)
        .await
        .unwrap();
    assert_ne!(again.session_id.as_deref(), Some(after.session.as_str()));
    assert_eq!((again.spec_generation, again.running_generation), (3, 3));
    assert!(
        daemon
            .verify_workspace(
                "app",
                "alice",
                WorkspaceVerifyRequest {
                    digest: Some(digest)
                }
            )
            .await
            .unwrap()
            .verified
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replacement_that_fails_before_the_handoff_leaves_the_old_computer_current() {
    let (_repositories, source) = repository();
    let workspaces = tempfile::tempdir().unwrap();
    let steered = Steered::new(workspaces.path(), full(), None);
    let target = Target::start(steered.clone(), &[]);
    let store: Arc<dyn StateStore> = Arc::new(MemoryState::new());
    let (daemon, _node) = start_daemon(store, pool(&[("target-a", &target)])).await;
    daemon
        .create_computer_environment(
            definition(
                "app",
                ComputerLifecycle::Persistent,
                requirements(),
                contents(&source, "v1"),
            ),
            "alice",
        )
        .await
        .unwrap();
    let first = computer_where(&daemon, "app", "converge", |view| view.converged).await;
    sh(
        &daemon,
        "app",
        "mkdir data && printf hello > data/notes.txt",
    )
    .await;
    let digest = daemon
        .verify_workspace("app", "alice", WorkspaceVerifyRequest { digest: None })
        .await
        .unwrap()
        .digest;
    let before = facts(&first);
    let pid = first.observed.processes["api"].pid;

    // Every failure below must leave: A current, converged, running the same
    // process, with the same workspace; and the candidate stopped and not ready.
    let assert_old_is_current =
        |phase: &'static str, failed: Result<ComputerView, EnvironmentError>| {
            let daemon = daemon.clone();
            let digest = digest.clone();
            let session = before.session.clone();
            async move {
                let Err(EnvironmentError::Conflict(reason)) = &failed else {
                    panic!("{phase}: {failed:?}")
                };
                assert!(reason.contains(&format!("while {phase}")), "{reason}");
                assert!(
                    reason.contains("remains current") && reason.contains("unverified"),
                    "{reason}"
                );
                let current = daemon.computer("app").await.unwrap();
                assert_eq!(current.session_id.as_deref(), Some(session.as_str()));
                assert!(current.converged);
                assert_eq!(current.observed.processes["api"].pid, pid);
                assert!(
                    daemon
                        .verify_workspace(
                            "app",
                            "alice",
                            WorkspaceVerifyRequest {
                                digest: Some(digest)
                            }
                        )
                        .await
                        .unwrap()
                        .verified,
                    "{phase}: the old workspace is untouched"
                );
                // The candidate is stopped, never presented as running or ready.
                let candidate =
                    computer_where(&daemon, "app--candidate", "the candidate to stop", |view| {
                        view.reality.observed == "stopped"
                    })
                    .await;
                assert_eq!(candidate.reality.desired, "stopped");
                let recorded = events(&daemon, "app").await;
                let failure = recorded
                    .iter()
                    .rev()
                    .find(|(_, data)| data["command"] == "replace" && data["outcome"] == "failed")
                    .expect("the failure is recorded on the environment");
                assert_eq!(failure.1["phase"], phase);
                assert_eq!(failure.1["workspace_verified"], false);
                assert!(
                    !recorded.iter().any(|(kind, _)| kind == "computer.replaced"),
                    "no handoff"
                );
            }
        };

    // Seed failure.
    *steered.fail_exec_containing.lock().unwrap() = Some("tar -xf".into());
    let failed = daemon
        .replace_computer("app", "alice", requirements())
        .await;
    assert_old_is_current("seeding", failed).await;
    *steered.fail_exec_containing.lock().unwrap() = None;

    // Verification failure: the seeded workspace is not the archive's.
    *steered.tamper.lock().unwrap() = Some((
        "workspace_check\ndigest".into(),
        Box::new(|workspace: &Path| std::fs::write(workspace.join("stray"), "x").unwrap()),
    ));
    let failed = daemon
        .replace_computer("app", "alice", requirements())
        .await;
    assert_old_is_current("seeding", failed).await;

    // The source changed after it was captured: what would move is not what
    // was captured, so nothing moves.
    let source_workspace = workspaces.path().join(&before.resource);
    *steered.tamper.lock().unwrap() = Some((
        "tar -xf".into(),
        Box::new(move |_| std::fs::write(source_workspace.join("late"), "x").unwrap()),
    ));
    let failed = daemon
        .replace_computer("app", "alice", requirements())
        .await;
    assert!(
        matches!(&failed, Err(EnvironmentError::Conflict(reason)) if reason.contains("no longer the")),
        "{failed:?}"
    );
    sh(&daemon, "app", "rm late").await;
    assert_old_is_current("verifying the source", failed).await;

    // Reconcile failure: the declared contents cannot be brought up on the
    // new machine, so it is not presented as ready and nothing moves.
    *steered.fail_exec_containing.lock().unwrap() = Some("git fetch".into());
    let failed = daemon
        .replace_computer("app", "alice", requirements())
        .await;
    assert_old_is_current("reconciling", failed).await;
    *steered.fail_exec_containing.lock().unwrap() = None;

    // A later replacement clears the leftover candidate and succeeds.
    let replaced = daemon
        .replace_computer("app", "alice", requirements())
        .await
        .unwrap();
    assert_ne!(
        replaced.session_id.as_deref(),
        Some(before.session.as_str())
    );
    computer_where(&daemon, "app", "the replacement to converge", |view| {
        view.converged
    })
    .await;
    assert!(daemon.computer("app--candidate").await.is_err());
    assert!(
        daemon
            .verify_workspace(
                "app",
                "alice",
                WorkspaceVerifyRequest {
                    digest: Some(digest)
                }
            )
            .await
            .unwrap()
            .verified
    );
}

/// A workspace written to during capture is refused, and either of two
/// checks may notice first: the archiver's own, or the before/after digest.
fn changed_while_captured(reason: &str) -> bool {
    reason.contains("changed while it was captured")
        || reason.contains("file changed as we read it")
}

fn fork_of(name: &str) -> ForkRequest {
    ForkRequest {
        name: name.into(),
        target: None,
        copy_config: false,
    }
}

async fn digest_of(daemon: &Arc<Daemon>, name: &str) -> String {
    daemon
        .verify_workspace(name, "alice", WorkspaceVerifyRequest { digest: None })
        .await
        .unwrap()
        .digest
}

fn a_policy() -> compute_policy::Policy {
    let mut policy: compute_policy::Policy =
        serde_json::from_value(serde_json::json!({ "version": 1, "name": "forked-policy" }))
            .unwrap();
    policy.defaults.network = Some(NetworkPolicy::Network);
    policy
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fork_is_an_independent_environment_made_from_portable_state() {
    let (_repositories, source) = repository();
    let workspaces = tempfile::tempdir().unwrap();
    let target = Target::start(Steered::new(workspaces.path(), full(), None), &[]);
    let store: Arc<dyn StateStore> = Arc::new(MemoryState::new());
    let (daemon, _node) = start_daemon(store, pool(&[("target-a", &target)])).await;
    let mut origin_definition = definition(
        "origin",
        ComputerLifecycle::Persistent,
        requirements(),
        contents(&source, "v1"),
    );
    origin_definition.policy = Some(a_policy());
    daemon
        .create_computer_environment(origin_definition, "alice")
        .await
        .unwrap();
    let origin = computer_where(&daemon, "origin", "the origin to converge", |view| {
        view.converged
    })
    .await;
    sh(
        &daemon,
        "origin",
        "mkdir -p data/nested empty && printf hello > data/notes.txt && printf deep > data/nested/x.bin",
    )
    .await;
    let origin_digest = digest_of(&daemon, "origin").await;
    let origin_pid = origin.observed.processes["api"].pid;
    let origin_environment = daemon.environment("origin").await.unwrap();

    let report = daemon
        .fork_environment("origin", "alice", fork_of("branch"))
        .await
        .unwrap();

    // A new environment and a new machine.
    let branch = report.computer.clone();
    assert_ne!(branch.environment_id, origin.environment_id);
    assert_ne!(branch.session_id, origin.session_id);
    assert_ne!(
        branch
            .machine
            .as_ref()
            .map(|machine| machine.resource.clone()),
        origin
            .machine
            .as_ref()
            .map(|machine| machine.resource.clone())
    );
    assert_eq!(
        branch.owner, "alice",
        "its own ownership, from the forking operator"
    );
    assert!(report.workspace_verified);
    assert!(report.jobs.len() >= 4, "export, upload, extract, measure");

    // Portable state moved: the same workspace, proved by the same digest.
    assert_eq!(report.workspace, origin_digest);
    assert_eq!(digest_of(&daemon, "branch").await, origin_digest);
    assert_eq!(
        sh(&daemon, "branch", "cat data/notes.txt data/nested/x.bin").await,
        "hellodeep"
    );
    assert_eq!(
        sh(&daemon, "branch", "test -d empty && echo yes")
            .await
            .trim(),
        "yes"
    );

    // Declared state was inherited, and reconciled there: the process runs in
    // B under its own pid because B started it, and A's was not touched.
    assert_eq!(branch.desired.repositories, origin.desired.repositories);
    assert_eq!(branch.desired.processes, origin.desired.processes);
    assert_eq!(
        branch.observed.processes["api"].state,
        ProcessState::Running
    );
    assert_ne!(branch.observed.processes["api"].pid, origin_pid);
    let (from, to) = &report.repositories["app"];
    assert!(from.is_some() && from == to, "{:?}", report.repositories);
    let still = daemon.computer("origin").await.unwrap();
    assert_eq!(still.session_id, origin.session_id);
    assert_eq!(still.observed.processes["api"].pid, origin_pid);
    assert!(still.converged);
    assert_eq!(digest_of(&daemon, "origin").await, origin_digest);

    // Policy is environment state, so it is inherited; configuration values
    // are where credentials live, so they are not.
    let branch_environment = daemon.environment("branch").await.unwrap();
    assert_eq!(branch_environment.policy_id, origin_environment.policy_id);
    assert_eq!(report.omitted_config, vec!["APP_ENV".to_string()]);
    assert!(branch.config.is_empty());
    assert_eq!(
        daemon.computer("origin").await.unwrap().config["APP_ENV"],
        "origin"
    );

    // The source's evidence is its own: the fork is recorded on B only.
    let branch_events = events(&daemon, "branch").await;
    let forked = branch_events
        .iter()
        .find(|(_, data)| data["command"] == "fork")
        .expect("a fork event");
    assert_eq!(forked.1["source"], "origin");
    assert_eq!(forked.1["workspace"], origin_digest.as_str());
    assert!(
        !events(&daemon, "origin")
            .await
            .iter()
            .any(|(_, data)| data["command"] == "fork")
    );
    assert!(
        daemon.computer("branch--candidate").await.is_err(),
        "nothing of the candidate remains"
    );

    // A change to either side never reaches the other: a fork, not a second
    // handle onto shared state.
    sh(
        &daemon,
        "origin",
        "printf a-only > data/a.txt && printf changed > data/notes.txt",
    )
    .await;
    assert_eq!(
        digest_of(&daemon, "branch").await,
        origin_digest,
        "B did not change"
    );
    assert_eq!(
        sh(
            &daemon,
            "branch",
            "cat data/notes.txt; test -e data/a.txt || echo none"
        )
        .await,
        "hellonone\n"
    );
    let origin_changed = digest_of(&daemon, "origin").await;
    assert_ne!(origin_changed, origin_digest);
    sh(
        &daemon,
        "branch",
        "printf b-only > data/b.txt && rm data/nested/x.bin",
    )
    .await;
    assert_eq!(
        digest_of(&daemon, "origin").await,
        origin_changed,
        "A did not change"
    );
    assert_eq!(
        sh(
            &daemon,
            "origin",
            "test -e data/b.txt || echo none; cat data/nested/x.bin"
        )
        .await
        .trim(),
        "none\ndeep"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_fork_creates_nothing_and_poisons_no_name() {
    let (_repositories, source) = repository();
    let workspaces = tempfile::tempdir().unwrap();
    let steered = Steered::new(workspaces.path(), full(), None);
    let target = Target::start(steered.clone(), &[]);
    let store: Arc<dyn StateStore> = Arc::new(MemoryState::new());
    let (daemon, _node) = start_daemon(store, pool(&[("target-a", &target)])).await;
    daemon
        .create_computer_environment(
            definition(
                "origin",
                ComputerLifecycle::Persistent,
                requirements(),
                contents(&source, "v1"),
            ),
            "alice",
        )
        .await
        .unwrap();
    let origin = computer_where(&daemon, "origin", "converge", |view| view.converged).await;
    sh(
        &daemon,
        "origin",
        "mkdir data && printf hello > data/notes.txt",
    )
    .await;
    let digest = digest_of(&daemon, "origin").await;
    let pid = origin.observed.processes["api"].pid;

    // After every failure: the source is exactly as it was, the requested
    // name does not exist, and its candidate is stopped and never ready.
    let assert_failed = async |phase: &str, failed: Result<ForkReport, EnvironmentError>| {
        let Err(EnvironmentError::Conflict(reason)) = &failed else {
            panic!("{phase}: {failed:?}")
        };
        assert!(reason.contains(&format!("while {phase}")), "{reason}");
        assert!(
            reason.contains("unverified") && reason.contains("no environment was created"),
            "{reason}"
        );
        let still = daemon.computer("origin").await.unwrap();
        assert_eq!(still.session_id, origin.session_id);
        assert!(still.converged);
        assert_eq!(still.observed.processes["api"].pid, pid);
        assert_eq!(
            digest_of(&daemon, "origin").await,
            digest,
            "{phase}: the source is untouched"
        );
        assert!(
            daemon.computer("branch").await.is_err(),
            "{phase}: the name is free"
        );
        let candidate = computer_where(
            &daemon,
            "branch--candidate",
            "the candidate to stop",
            |view| view.reality.observed == "stopped",
        )
        .await;
        assert_eq!(candidate.reality.desired, "stopped");
        let recorded = events(&daemon, "origin").await;
        let failure = recorded
            .iter()
            .rev()
            .find(|(_, data)| data["command"] == "fork" && data["outcome"] == "failed")
            .expect("the failure is recorded on the source");
        assert_eq!(failure.1["phase"], phase);
        assert_eq!(failure.1["workspace_verified"], false);
        assert!(
            !recorded
                .iter()
                .any(|(_, data)| data["command"] == "fork" && data["outcome"] != "failed")
        );
    };

    // Seed failure.
    *steered.fail_exec_containing.lock().unwrap() = Some("tar -xf".into());
    assert_failed(
        "seeding",
        daemon
            .fork_environment("origin", "alice", fork_of("branch"))
            .await,
    )
    .await;
    *steered.fail_exec_containing.lock().unwrap() = None;

    // Destination verification failure: what landed is not the archive.
    *steered.tamper.lock().unwrap() = Some((
        "workspace_check\ndigest".into(),
        Box::new(|workspace: &Path| std::fs::write(workspace.join("stray"), "x").unwrap()),
    ));
    assert_failed(
        "seeding",
        daemon
            .fork_environment("origin", "alice", fork_of("branch"))
            .await,
    )
    .await;

    // Reconcile failure: the declared contents cannot come up on the new machine.
    *steered.fail_exec_containing.lock().unwrap() = Some("git fetch".into());
    assert_failed(
        "reconciling",
        daemon
            .fork_environment("origin", "alice", fork_of("branch"))
            .await,
    )
    .await;
    *steered.fail_exec_containing.lock().unwrap() = None;

    // Refused before anything is created: a name in use, another operator,
    // a reserved name, and a workspace that cannot travel.
    assert!(matches!(
        daemon
            .fork_environment("origin", "alice", fork_of("origin"))
            .await,
        Err(EnvironmentError::Conflict(_))
    ));
    assert!(daemon.computer("origin--candidate").await.is_err());
    assert!(
        daemon
            .fork_environment("origin", "mallory", fork_of("theirs"))
            .await
            .is_err()
    );
    assert!(daemon.computer("theirs").await.is_err());
    assert!(daemon.computer("theirs--candidate").await.is_err());
    assert!(matches!(
        daemon
            .fork_environment("origin", "alice", fork_of("x--candidate"))
            .await,
        Err(EnvironmentError::Invalid(_))
    ));
    sh(&daemon, "origin", "ln -s /etc/passwd link").await;
    let refused = daemon
        .fork_environment("origin", "alice", fork_of("linked"))
        .await;
    assert!(
        matches!(&refused, Err(EnvironmentError::RuntimeUnavailable(reason)) if reason.contains("unsupported entry")),
        "{refused:?}"
    );
    assert!(daemon.computer("linked").await.is_err());
    assert!(daemon.computer("linked--candidate").await.is_err());
    sh(&daemon, "origin", "rm link").await;

    // A fork after the failures: the name and the leftover candidate are not poisoned.
    let report = daemon
        .fork_environment("origin", "alice", fork_of("branch"))
        .await
        .unwrap();
    assert!(report.workspace_verified);
    assert_eq!(digest_of(&daemon, "branch").await, digest);
    assert!(daemon.computer("branch--candidate").await.is_err());
    assert_eq!(
        daemon.computer("origin").await.unwrap().session_id,
        origin.session_id
    );
}

// ---- checkpoints ---------------------------------------------------------------

/// An artifact store that can be made to fail, corrupt, or hang.
struct FaultyArtifacts {
    inner: compute_state::StateArtifacts,
    fail_put: AtomicBool,
    corrupt_get: AtomicBool,
    hang_put: AtomicBool,
    put_started: AtomicBool,
    truncate_get: AtomicBool,
    /// Answers every read with these bytes instead: a store that lies.
    substitute: std::sync::Mutex<Option<Vec<u8>>>,
}

impl FaultyArtifacts {
    fn new(store: &Arc<dyn StateStore>) -> Arc<Self> {
        Arc::new(Self {
            inner: compute_state::StateArtifacts::new(compute_state::ControlState::new(
                store.clone(),
            )),
            fail_put: AtomicBool::new(false),
            corrupt_get: AtomicBool::new(false),
            hang_put: AtomicBool::new(false),
            put_started: AtomicBool::new(false),
            truncate_get: AtomicBool::new(false),
            substitute: std::sync::Mutex::new(None),
        })
    }
}

#[async_trait]
impl compute_state::ArtifactStore for FaultyArtifacts {
    async fn put(&self, kind: &str, bytes: &[u8]) -> Result<String, compute_state::StateError> {
        self.put_started.store(true, Ordering::SeqCst);
        while self.hang_put.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if self.fail_put.load(Ordering::SeqCst) {
            return Err(compute_state::StateError::Invalid(
                "artifact store is down".into(),
            ));
        }
        self.inner.put(kind, bytes).await
    }

    async fn get(&self, digest: &str) -> Result<Option<Vec<u8>>, compute_state::StateError> {
        let mut bytes = self.inner.get(digest).await?;
        if self.corrupt_get.load(Ordering::SeqCst)
            && let Some(bytes) = bytes.as_mut()
        {
            let middle = bytes.len() / 2;
            bytes[middle] ^= 0xff;
        }
        if self.truncate_get.load(Ordering::SeqCst)
            && let Some(bytes) = bytes.as_mut()
        {
            bytes.truncate(bytes.len() / 2);
        }
        if bytes.is_some()
            && let Some(lie) = self.substitute.lock().unwrap().clone()
        {
            return Ok(Some(lie));
        }
        Ok(bytes)
    }

    fn location(&self) -> String {
        self.inner.location()
    }
}

/// A state store that refuses to record a checkpoint while told to.
struct FailingState {
    inner: MemoryState,
    fail_checkpoint: AtomicBool,
}

impl FailingState {
    fn refuses(&self, writes: &[compute_state::Write]) -> bool {
        self.fail_checkpoint.load(Ordering::SeqCst)
            && writes.iter().any(|write| {
                matches!(write, compute_state::Write::Create { collection, .. }
                    if *collection == compute_state::Collection::Checkpoint)
            })
    }
}

#[async_trait]
impl StateStore for FailingState {
    fn backend(&self) -> compute_state::BackendInfo {
        self.inner.backend()
    }
    async fn get(
        &self,
        collection: compute_state::Collection,
        id: &str,
    ) -> Result<Option<compute_state::Record>, compute_state::StateError> {
        self.inner.get(collection, id).await
    }
    async fn query(
        &self,
        query: &compute_state::Query,
    ) -> Result<Vec<compute_state::Record>, compute_state::StateError> {
        self.inner.query(query).await
    }
    async fn commit(
        &self,
        writes: Vec<compute_state::Write>,
    ) -> Result<(), compute_state::StateError> {
        if self.refuses(&writes) {
            return Err(compute_state::StateError::Unavailable(
                "state is down".into(),
            ));
        }
        self.inner.commit(writes).await
    }
    async fn commit_tracked(
        &self,
        writes: Vec<compute_state::Write>,
    ) -> Result<Option<(u64, u64)>, compute_state::StateError> {
        if self.refuses(&writes) {
            return Err(compute_state::StateError::Unavailable(
                "state is down".into(),
            ));
        }
        self.inner.commit_tracked(writes).await
    }
    async fn revision(&self) -> Result<Option<compute_state::Revision>, compute_state::StateError> {
        self.inner.revision().await
    }
    fn take_transitions(&self) -> Option<Vec<compute_state::Transition>> {
        self.inner.take_transitions()
    }
}

fn no_parent() -> CheckpointRequest {
    CheckpointRequest::default()
}

async fn records(daemon: &Arc<Daemon>, name: &str) -> Vec<compute_state::CheckpointRecord> {
    daemon
        .checkpoints(name, "alice")
        .await
        .unwrap()
        .into_iter()
        .map(|view| view.checkpoint)
        .collect()
}

async fn artifact_bytes(artifacts: &Arc<FaultyArtifacts>, id: &str) -> Vec<u8> {
    compute_state::ArtifactStore::get(artifacts.as_ref(), id)
        .await
        .unwrap()
        .unwrap()
}

type World = (
    Arc<Daemon>,
    Arc<FaultyArtifacts>,
    Arc<FailingState>,
    Target,
    tempfile::TempDir,
    tempfile::TempDir,
    Arc<Steered>,
);

/// A daemon over a memory state with a faulty artifact store and a state
/// store that can refuse a checkpoint, and an environment holding state.
async fn checkpoint_world(source: &Path) -> World {
    checkpoint_world_gated(source, None).await
}

async fn checkpoint_world_gated(source: &Path, gate: Option<Arc<Semaphore>>) -> World {
    let workspaces = tempfile::tempdir().unwrap();
    let steered = Steered::new(workspaces.path(), full(), gate);
    let target = Target::start(steered.clone(), &[]);
    let state = Arc::new(FailingState {
        inner: MemoryState::new(),
        fail_checkpoint: AtomicBool::new(false),
    });
    let store: Arc<dyn StateStore> = state.clone();
    let artifacts = FaultyArtifacts::new(&store);
    let (daemon, node) =
        start_daemon_with(store, artifacts.clone(), pool(&[("target-a", &target)])).await;
    let mut origin = definition(
        "origin",
        ComputerLifecycle::Persistent,
        requirements(),
        contents(source, "v1"),
    );
    origin
        .env
        .insert("API_TOKEN".into(), "s3cret-token-value".into());
    daemon
        .create_computer_environment(origin, "alice")
        .await
        .unwrap();
    computer_where(&daemon, "origin", "the origin to converge", |view| {
        view.converged
    })
    .await;
    sh(
        &daemon,
        "origin",
        "mkdir -p data/nested empty && printf hello > data/notes.txt && printf deep > data/nested/x.bin && printf '#!/bin/sh\\necho hi\\n' > run.sh && chmod +x run.sh",
    )
    .await;
    (daemon, artifacts, state, target, workspaces, node, steered)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_checkpoint_is_immutable_captured_state_not_machine_state() {
    let (_repositories, source) = repository();
    let (daemon, artifacts, _state, _target, _workspaces, _node, _steered) =
        checkpoint_world(&source).await;
    let origin = daemon.computer("origin").await.unwrap();
    let digest = digest_of(&daemon, "origin").await;
    let pid = origin.observed.processes["api"].pid;

    let report = daemon
        .checkpoint_environment("origin", "alice", no_parent())
        .await
        .unwrap();

    // Captured state, tied to the environment it came from.
    assert_eq!(report.workspace, digest, "the verified source digest");
    assert!(report.verified && !report.existing);
    assert_eq!(report.format, "compute.checkpoint@1");
    assert!(report.checkpoint_id.starts_with("ckp_"));
    assert_eq!(report.contents_generation, origin.desired.generation);
    assert_eq!(report.computer_generation, origin.spec_generation);
    assert!(report.platform.contains('-'), "{report:?}");
    let listed = records(&daemon, "origin").await;
    assert_eq!(listed.len(), 1);
    let record = listed[0].clone();
    assert_eq!(record.checkpoint_id, report.checkpoint_id);
    assert_eq!(record.environment_id, origin.environment_id);
    assert_eq!(record.owner, "alice");
    assert_eq!(record.workspace_digest, digest);
    assert_eq!(record.artifact_id, report.artifact);
    assert_eq!(record.capture_job_id, report.job_id);

    // The artifact reopens and validates, and its identity is its content.
    let bytes = artifact_bytes(&artifacts, &record.artifact_id).await;
    let valid = compute_environment::checkpoint::validate(&bytes).unwrap();
    assert_eq!(valid.artifact_digest, record.artifact_id);
    assert_eq!(valid.checkpoint_id, record.checkpoint_id);
    assert_eq!(valid.manifest.tree_digest, digest);
    assert_eq!(valid.manifest.source.environment_id, origin.environment_id);
    let viewed = daemon
        .checkpoint("origin", "alice", &record.checkpoint_id)
        .await
        .unwrap();
    assert_eq!(viewed.valid, Some(true), "{viewed:?}");

    // Workspace state, not machine state: files, with the executable bit and
    // the empty directory; not process state, re-derived repositories,
    // controller state, or configuration values.
    let files = compute_environment::checkpoint::Checkpoint::read_files(&bytes).unwrap();
    assert_eq!(files["data/notes.txt"].1, b"hello");
    assert!(files["run.sh"].0, "executable bit");
    assert!(valid.manifest.entries.iter().any(|entry| matches!(entry,
        compute_environment::checkpoint::Entry::Directory { path } if path == "empty")));
    assert!(
        files
            .keys()
            .all(|path| !path.starts_with("repos/") && !path.starts_with(".compute/processes")),
        "{:?}",
        files.keys()
    );
    assert!(!String::from_utf8_lossy(&bytes).contains("s3cret-token-value"));
    assert_eq!(valid.manifest.exclusions.paths.len(), 2);

    // The environment was not disturbed, and the capture is evidenced.
    let still = daemon.computer("origin").await.unwrap();
    assert_eq!(still.session_id, origin.session_id);
    assert_eq!(
        still.observed.processes["api"].pid, pid,
        "the process was not touched"
    );
    assert!(still.converged);
    assert_eq!(digest_of(&daemon, "origin").await, digest);
    let recorded = events(&daemon, "origin").await;
    let captured = recorded
        .iter()
        .find(|(kind, _)| kind == "checkpoint.captured")
        .expect("the capture is evidenced");
    assert_eq!(captured.1["checkpoint_id"], record.checkpoint_id.as_str());
    assert_eq!(captured.1["job_id"], report.job_id.as_str());
    assert_eq!(captured.1["workspace"], digest.as_str());
    assert!(
        recorded
            .iter()
            .any(|(_, data)| data["command"] == "workspace.export"
                && data["job_id"] == report.job_id.as_str())
    );

    // The same state is the same checkpoint: named by content, never rewritten.
    let again = daemon
        .checkpoint_environment("origin", "alice", no_parent())
        .await
        .unwrap();
    assert!(again.existing);
    assert_eq!(again.checkpoint_id, report.checkpoint_id);
    assert_eq!(again.artifact, report.artifact);
    assert_eq!(records(&daemon, "origin").await, vec![record.clone()]);

    // Mutate the environment: the checkpoint does not move.
    sh(
        &daemon,
        "origin",
        "printf changed > data/notes.txt && printf new > data/new.txt && rm run.sh",
    )
    .await;
    assert_ne!(digest_of(&daemon, "origin").await, digest);
    assert_eq!(
        artifact_bytes(&artifacts, &record.artifact_id).await,
        bytes,
        "immutable"
    );
    assert_eq!(records(&daemon, "origin").await, vec![record.clone()]);
    assert_eq!(
        daemon
            .checkpoint("origin", "alice", &record.checkpoint_id)
            .await
            .unwrap()
            .valid,
        Some(true)
    );

    // Lineage: C1 has independent children, and no authority passes down.
    let child = |parent: &str| CheckpointRequest {
        parent: Some(parent.into()),
    };
    let c2 = daemon
        .checkpoint_environment("origin", "alice", child(&record.checkpoint_id))
        .await
        .unwrap();
    sh(&daemon, "origin", "printf more > data/more.txt").await;
    let c3 = daemon
        .checkpoint_environment("origin", "alice", child(&record.checkpoint_id))
        .await
        .unwrap();
    assert_ne!(c2.checkpoint_id, c3.checkpoint_id);
    assert_eq!(c2.parent.as_deref(), Some(record.checkpoint_id.as_str()));
    assert_eq!(c3.parent.as_deref(), Some(record.checkpoint_id.as_str()));
    assert_ne!(c2.workspace, c3.workspace);
    assert_eq!(records(&daemon, "origin").await.len(), 3);
    assert_eq!(artifact_bytes(&artifacts, &record.artifact_id).await, bytes);
    let unknown = daemon
        .checkpoint_environment("origin", "alice", child("ckp_nope"))
        .await;
    assert!(
        matches!(unknown, Err(EnvironmentError::NotFound(_))),
        "{unknown:?}"
    );

    // The same tree on another machine is the same tree: only provenance moves.
    daemon
        .replace_computer("origin", "alice", requirements())
        .await
        .unwrap();
    computer_where(&daemon, "origin", "the replacement to converge", |view| {
        view.converged
    })
    .await;
    let moved = daemon
        .checkpoint_environment("origin", "alice", no_parent())
        .await
        .unwrap();
    assert_eq!(moved.workspace, digest_of(&daemon, "origin").await);
    assert_eq!(moved.computer_generation, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_checkpoint_of_an_unsafe_workspace_is_refused_and_nothing_is_recorded() {
    let (_repositories, source) = repository();
    let (daemon, _artifacts, _state, _target, _workspaces, _node, _steered) =
        checkpoint_world(&source).await;
    for (what, make, remove) in [
        ("a symbolic link", "ln -s /etc/passwd link", "rm link"),
        ("a hard link", "ln data/notes.txt data/twin", "rm data/twin"),
        ("a special file", "mkfifo pipe", "rm pipe"),
        (
            "a control character",
            "printf x > \"$(printf 'a\\001b')\"",
            "rm -f a*b",
        ),
    ] {
        sh(&daemon, "origin", make).await;
        let refused = daemon
            .checkpoint_environment("origin", "alice", no_parent())
            .await;
        assert!(refused.is_err(), "{what}: {refused:?}");
        assert!(
            records(&daemon, "origin").await.is_empty(),
            "{what}: nothing is published"
        );
        let failure = events(&daemon, "origin")
            .await
            .into_iter()
            .rev()
            .find(|(kind, _)| kind == "checkpoint.failed")
            .expect("the refusal is evidenced");
        assert_eq!(failure.1["published"], false, "{what}");
        sh(&daemon, "origin", remove).await;
    }
    // The environment is not poisoned: the next capture succeeds.
    let report = daemon
        .checkpoint_environment("origin", "alice", no_parent())
        .await
        .unwrap();
    assert!(report.verified);
    assert_eq!(records(&daemon, "origin").await.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_the_owner_checkpoints_and_a_changing_workspace_is_refused() {
    let (_repositories, source) = repository();
    let (daemon, _artifacts, _state, _target, _workspaces, _node, _steered) =
        checkpoint_world(&source).await;
    let refused = daemon
        .checkpoint_environment("origin", "mallory", no_parent())
        .await;
    assert!(refused.is_err(), "{refused:?}");
    assert!(records(&daemon, "origin").await.is_empty());
    assert!(
        !events(&daemon, "origin")
            .await
            .iter()
            .any(|(kind, _)| kind.starts_with("checkpoint."))
    );
    assert!(daemon.checkpoints("origin", "mallory").await.is_err());
    assert!(
        daemon
            .checkpoint("origin", "mallory", "ckp_x")
            .await
            .is_err()
    );
    assert!(daemon.computer("origin").await.unwrap().converged);

    // A workspace written to while it is captured is not captured.
    let mut busy = EnvironmentContents::default();
    busy.processes.push(ProcessSpec {
        name: "writer".into(),
        kind: ProcessKind::Process,
        runtime: None,
        command: vec![
            "sh".into(),
            "-c".into(),
            "i=0; while [ $i -lt 3000 ]; do i=$((i+1)); : > \"tick$i\"; sleep 0.01; done".into(),
        ],
        repository: None,
        env: BTreeMap::new(),
        desired: ProcessDesired::Running,
        port: None,
        restart: 0,
        readiness: None,
        restart_policy: Default::default(),
        max_restarts: compute_core::DEFAULT_MAX_RESTARTS,
    });
    daemon
        .create_computer_environment(
            definition("busy", ComputerLifecycle::Persistent, requirements(), busy),
            "alice",
        )
        .await
        .unwrap();
    computer_where(&daemon, "busy", "the writer to run", |view| view.converged).await;
    eventually("the writer to be writing", async || {
        let count = sh(&daemon, "busy", "ls | grep -c '^tick' || true").await;
        (count.trim().parse::<u32>().unwrap_or(0) > 5).then_some(())
    })
    .await;
    let refused = daemon
        .checkpoint_environment("busy", "alice", no_parent())
        .await;
    assert!(
        matches!(&refused, Err(EnvironmentError::RuntimeUnavailable(reason)) if changed_while_captured(reason)),
        "{refused:?}"
    );
    assert!(
        records(&daemon, "busy").await.is_empty(),
        "no usable checkpoint"
    );
    assert!(
        daemon.computer("busy").await.unwrap().converged,
        "the source is unchanged"
    );
    daemon
        .set_process("busy", "alice", "writer", ProcessDesired::Stopped)
        .await
        .unwrap();
    eventually("the writer to stop", async || {
        let before = sh(&daemon, "busy", "ls | wc -l").await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        (sh(&daemon, "busy", "ls | wc -l").await == before).then_some(())
    })
    .await;
    assert!(
        daemon
            .checkpoint_environment("busy", "alice", no_parent())
            .await
            .unwrap()
            .verified
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_capture_publishes_nothing_and_never_poisons_the_next() {
    let (_repositories, source) = repository();
    let (daemon, artifacts, state, _target, _workspaces, _node, _steered) =
        checkpoint_world(&source).await;
    let failed_at = async |phase: &str, failed: Result<CheckpointReport, EnvironmentError>| {
        assert!(failed.is_err(), "{phase}: {failed:?}");
        assert!(
            records(&daemon, "origin").await.is_empty(),
            "{phase}: nothing is published"
        );
        let failure = events(&daemon, "origin")
            .await
            .into_iter()
            .rev()
            .find(|(kind, data)| kind == "checkpoint.failed" && data["phase"] == phase)
            .unwrap_or_else(|| panic!("{phase}: the failure is evidenced"));
        assert_eq!(failure.1["published"], false);
        assert!(
            daemon.computer("origin").await.unwrap().converged,
            "{phase}: the source is unharmed"
        );
    };

    // The artifact cannot be written.
    artifacts.fail_put.store(true, Ordering::SeqCst);
    failed_at(
        "storing the artifact",
        daemon
            .checkpoint_environment("origin", "alice", no_parent())
            .await,
    )
    .await;
    artifacts.fail_put.store(false, Ordering::SeqCst);

    // What was stored does not read back valid.
    artifacts.corrupt_get.store(true, Ordering::SeqCst);
    failed_at(
        "verifying the stored artifact",
        daemon
            .checkpoint_environment("origin", "alice", no_parent())
            .await,
    )
    .await;
    artifacts.corrupt_get.store(false, Ordering::SeqCst);

    // The record cannot be persisted: the artifact exists, unreferenced.
    state.fail_checkpoint.store(true, Ordering::SeqCst);
    failed_at(
        "publishing",
        daemon
            .checkpoint_environment("origin", "alice", no_parent())
            .await,
    )
    .await;
    state.fail_checkpoint.store(false, Ordering::SeqCst);

    // A retry succeeds, and is a first capture: nothing was ever published.
    let report = daemon
        .checkpoint_environment("origin", "alice", no_parent())
        .await
        .unwrap();
    assert!(report.verified && !report.existing);
    assert_eq!(records(&daemon, "origin").await.len(), 1);

    // A record never vouches for bytes that no longer validate.
    artifacts.corrupt_get.store(true, Ordering::SeqCst);
    let view = daemon
        .checkpoint("origin", "alice", &report.checkpoint_id)
        .await
        .unwrap();
    assert_eq!(view.valid, Some(false), "{view:?}");
    assert!(view.invalid_reason.is_some());
    artifacts.corrupt_get.store(false, Ordering::SeqCst);
    assert_eq!(
        daemon
            .checkpoint("origin", "alice", &report.checkpoint_id)
            .await
            .unwrap()
            .valid,
        Some(true)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_capture_interrupted_by_a_restart_publishes_nothing() {
    let (_repositories, source) = repository();
    let workspaces = tempfile::tempdir().unwrap();
    let target = Target::start(Steered::new(workspaces.path(), full(), None), &[]);
    let store: Arc<dyn StateStore> = Arc::new(MemoryState::new());
    let artifacts = FaultyArtifacts::new(&store);
    let (first, _node) = start_daemon_with(
        store.clone(),
        artifacts.clone(),
        pool(&[("target-a", &target)]),
    )
    .await;
    first
        .create_computer_environment(
            definition(
                "origin",
                ComputerLifecycle::Persistent,
                requirements(),
                contents(&source, "v1"),
            ),
            "alice",
        )
        .await
        .unwrap();
    computer_where(&first, "origin", "converge", |view| view.converged).await;

    // The capture is in flight (the artifact is being stored) when the
    // controller goes away.
    artifacts.hang_put.store(true, Ordering::SeqCst);
    let capture = {
        let first = first.clone();
        tokio::spawn(async move {
            first
                .checkpoint_environment("origin", "alice", no_parent())
                .await
        })
    };
    eventually("the capture to reach the store", async || {
        artifacts.put_started.load(Ordering::SeqCst).then_some(())
    })
    .await;
    first.shutdown().await;
    capture.abort();
    drop(first);
    artifacts.hang_put.store(false, Ordering::SeqCst);

    // A new controller finds no checkpoint, the environment intact, and can
    // capture.
    let (second, _node) =
        start_daemon_with(store, artifacts.clone(), pool(&[("target-a", &target)])).await;
    computer_where(&second, "origin", "the environment to converge", |view| {
        view.converged
    })
    .await;
    assert!(records(&second, "origin").await.is_empty());
    let report = second
        .checkpoint_environment("origin", "alice", no_parent())
        .await
        .unwrap();
    assert!(report.verified && !report.existing);
    assert_eq!(records(&second, "origin").await.len(), 1);
    second.shutdown().await;
}

// ---- restore -------------------------------------------------------------------

fn restore_of(name: &str) -> RestoreRequest {
    RestoreRequest {
        name: name.into(),
        target: None,
    }
}

/// A checkpoint of the origin, and the digest it captured.
async fn captured(daemon: &Arc<Daemon>) -> (CheckpointReport, String) {
    let digest = digest_of(daemon, "origin").await;
    let report = daemon
        .checkpoint_environment("origin", "alice", no_parent())
        .await
        .unwrap();
    assert_eq!(report.workspace, digest);
    (report, digest)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restoring_a_checkpoint_creates_independent_environments_from_immutable_state() {
    let (_repositories, source) = repository();
    let (daemon, artifacts, _state, _target, _workspaces, _node, _steered) =
        checkpoint_world(&source).await;
    let origin = daemon.computer("origin").await.unwrap();
    let pid = origin.observed.processes["api"].pid;
    let (checkpoint, digest) = captured(&daemon).await;
    let record = records(&daemon, "origin").await.remove(0);
    let bytes = artifact_bytes(&artifacts, &record.artifact_id).await;

    let report = daemon
        .restore_checkpoint(&checkpoint.checkpoint_id, "alice", restore_of("branch"))
        .await
        .unwrap();

    // A new environment, computer, and session: identity is fresh.
    let branch = report.computer.clone();
    assert_ne!(branch.environment_id, origin.environment_id);
    assert_eq!(report.environment_id, branch.environment_id);
    assert_ne!(
        report.computer_id,
        compute_state::ids::computer(&origin.environment_id)
    );
    assert_ne!(branch.session_id, origin.session_id);
    assert_ne!(
        branch
            .machine
            .as_ref()
            .map(|machine| machine.resource.clone()),
        origin
            .machine
            .as_ref()
            .map(|machine| machine.resource.clone())
    );
    assert_eq!(branch.owner, "alice");
    assert_eq!(report.checkpoint_id, checkpoint.checkpoint_id);
    assert_eq!(report.source, "origin");
    assert!(report.workspace_verified);
    assert!(
        report.jobs.len() >= 3,
        "upload, extract, measure: {report:?}"
    );

    // The workspace: the digest, the contents, the empty directory, the
    // executable bit.
    assert_eq!(report.workspace, digest);
    assert_eq!(digest_of(&daemon, "branch").await, digest);
    assert_eq!(
        sh(&daemon, "branch", "cat data/notes.txt data/nested/x.bin").await,
        "hellodeep"
    );
    assert_eq!(
        sh(&daemon, "branch", "test -d empty && echo yes")
            .await
            .trim(),
        "yes"
    );
    assert_eq!(
        sh(&daemon, "branch", "test -x run.sh && echo x")
            .await
            .trim(),
        "x"
    );

    // The declared process runs in the new computer because it reconciled its
    // declaration: not a restored process.
    assert_eq!(branch.desired.processes, origin.desired.processes);
    assert_eq!(branch.desired.repositories, origin.desired.repositories);
    assert_eq!(
        branch.observed.processes["api"].state,
        ProcessState::Running
    );
    assert_ne!(branch.observed.processes["api"].pid, pid);
    assert_eq!(report.declared_state, "matches the state at capture");
    assert_eq!(
        report.captured_contents_generation,
        report.applied_contents_generation
    );

    // Nothing of the source's authority or configuration came along.
    assert!(branch.config.is_empty());
    assert_eq!(
        report.omitted_config,
        vec!["API_TOKEN".to_string(), "APP_ENV".to_string()]
    );

    // Provenance is evidence on the new environment, and nothing was written
    // to the source's history.
    let branch_events = events(&daemon, "branch").await;
    let restored = branch_events
        .iter()
        .find(|(_, data)| data["command"] == "restore")
        .expect("the provenance is recorded");
    assert_eq!(
        restored.1["checkpoint_id"],
        checkpoint.checkpoint_id.as_str()
    );
    assert_eq!(restored.1["artifact"], record.artifact_id.as_str());
    assert_eq!(restored.1["workspace"], digest.as_str());
    assert_eq!(restored.1["declared_state"], "matches the state at capture");
    assert!(
        !events(&daemon, "origin")
            .await
            .iter()
            .any(|(_, data)| data["command"] == "restore")
    );
    assert!(daemon.computer("branch--candidate").await.is_err());

    // The source is untouched.
    let still = daemon.computer("origin").await.unwrap();
    assert_eq!(still.session_id, origin.session_id);
    assert_eq!(still.observed.processes["api"].pid, pid);
    assert!(still.converged);
    assert_eq!(digest_of(&daemon, "origin").await, digest);

    // Independence: a change to either side never reaches the other.
    sh(
        &daemon,
        "origin",
        "printf a-only > data/a.txt && printf changed > data/notes.txt",
    )
    .await;
    assert_eq!(
        digest_of(&daemon, "branch").await,
        digest,
        "B did not change"
    );
    let origin_changed = digest_of(&daemon, "origin").await;
    sh(
        &daemon,
        "branch",
        "printf b-only > data/b.txt && rm data/nested/x.bin",
    )
    .await;
    assert_eq!(
        digest_of(&daemon, "origin").await,
        origin_changed,
        "A did not change"
    );

    // The checkpoint is immutable through all of it.
    assert_eq!(artifact_bytes(&artifacts, &record.artifact_id).await, bytes);
    assert_eq!(records(&daemon, "origin").await, vec![record.clone()]);
    let viewed = daemon
        .checkpoint("origin", "alice", &record.checkpoint_id)
        .await
        .unwrap();
    assert_eq!(viewed.valid, Some(true));
    assert_eq!(viewed.checkpoint.workspace_digest, digest);

    // Durable, reusable state: the same checkpoint again is another
    // independent environment with the same initial workspace.
    let again = daemon
        .restore_checkpoint(&checkpoint.checkpoint_id, "alice", restore_of("branch-two"))
        .await
        .unwrap();
    assert_ne!(again.environment_id, report.environment_id);
    assert_ne!(again.computer_id, report.computer_id);
    assert_ne!(again.computer.session_id, report.computer.session_id);
    assert_eq!(again.workspace, report.workspace);
    assert_eq!(digest_of(&daemon, "branch-two").await, digest);
    assert_ne!(
        digest_of(&daemon, "branch").await,
        digest_of(&daemon, "branch-two").await,
        "B1 changed; B2 did not"
    );

    // Declarations are the environment's, so a changed declaration is applied
    // and said so, never silently: the checkpoint holds none.
    daemon
        .set_process("origin", "alice", "api", ProcessDesired::Stopped)
        .await
        .unwrap();
    let later = daemon
        .restore_checkpoint(
            &checkpoint.checkpoint_id,
            "alice",
            restore_of("branch-three"),
        )
        .await
        .unwrap();
    assert_eq!(later.declared_state, "changed since capture");
    assert_ne!(
        later.captured_contents_generation,
        later.applied_contents_generation
    );
    assert_eq!(later.workspace, digest);
    assert_eq!(artifact_bytes(&artifacts, &record.artifact_id).await, bytes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_checkpoint_that_cannot_be_trusted_never_becomes_an_environment() {
    let (_repositories, source) = repository();
    let (daemon, artifacts, state, _target, _workspaces, _node, _steered) =
        checkpoint_world(&source).await;
    let (checkpoint, _digest) = captured(&daemon).await;
    let good = records(&daemon, "origin").await.remove(0);
    let store: Arc<dyn StateStore> = state.clone();
    let control = compute_state::ControlState::new(store);
    let forge = async |id: &str, edit: &dyn Fn(&mut compute_state::CheckpointRecord)| {
        let mut record = good.clone();
        record.checkpoint_id = id.to_owned();
        edit(&mut record);
        control
            .transaction(compute_state::Batch::new().create(id, &record))
            .await
            .unwrap();
    };
    let nothing_was_created =
        async |what: &str, result: Result<RestoreReport, EnvironmentError>| {
            assert!(result.is_err(), "{what}: {result:?}");
            assert!(
                daemon.computer("branch").await.is_err(),
                "{what}: no environment"
            );
            assert!(
                daemon.computer("branch--candidate").await.is_err(),
                "{what}: no candidate, nothing was begun"
            );
            assert!(
                !events(&daemon, "origin")
                    .await
                    .iter()
                    .any(|(_, data)| data["command"] == "restore"),
                "{what}"
            );
        };

    nothing_was_created(
        "a checkpoint that does not exist",
        daemon
            .restore_checkpoint("ckp_nope", "alice", restore_of("branch"))
            .await,
    )
    .await;

    // Corrupted, then truncated bytes.
    artifacts.corrupt_get.store(true, Ordering::SeqCst);
    nothing_was_created(
        "a corrupted artifact",
        daemon
            .restore_checkpoint(&checkpoint.checkpoint_id, "alice", restore_of("branch"))
            .await,
    )
    .await;
    artifacts.corrupt_get.store(false, Ordering::SeqCst);
    artifacts.truncate_get.store(true, Ordering::SeqCst);
    nothing_was_created(
        "a truncated artifact",
        daemon
            .restore_checkpoint(&checkpoint.checkpoint_id, "alice", restore_of("branch"))
            .await,
    )
    .await;
    artifacts.truncate_get.store(false, Ordering::SeqCst);

    // An artifact the store does not have.
    forge("ckp_lost", &|record| {
        record.artifact_id = format!("sha256:{:0>64}", "1");
    })
    .await;
    nothing_was_created(
        "a missing artifact",
        daemon
            .restore_checkpoint("ckp_lost", "alice", restore_of("branch"))
            .await,
    )
    .await;

    // Valid bytes that are not the canonical encoding.
    let bytes = artifact_bytes(&artifacts, &good.artifact_id).await;
    let noncanonical = {
        let mut archive = tar::Archive::new(&bytes[..]);
        let mut builder = tar::Builder::new(Vec::new());
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let mut data = vec![];
            std::io::Read::read_to_end(&mut entry, &mut data).unwrap();
            if entry.path().unwrap().to_str() == Some("manifest.json") {
                let manifest: serde_json::Value = serde_json::from_slice(&data).unwrap();
                data = serde_json::to_vec(&manifest).unwrap();
            }
            let mut header = entry.header().clone();
            header.set_size(data.len() as u64);
            header.set_cksum();
            builder.append(&header, &data[..]).unwrap();
        }
        builder.into_inner().unwrap()
    };
    let stored = compute_state::ArtifactStore::put(artifacts.as_ref(), "checkpoint", &noncanonical)
        .await
        .unwrap();
    forge("ckp_loose", &|record| record.artifact_id = stored.clone()).await;
    nothing_was_created(
        "a non-canonical encoding",
        daemon
            .restore_checkpoint("ckp_loose", "alice", restore_of("branch"))
            .await,
    )
    .await;

    // A store that answers with a different, valid artifact.
    forge("ckp_swapped", &|record| {
        record.artifact_id = format!("sha256:{:0>64}", "2");
    })
    .await;
    *artifacts.substitute.lock().unwrap() = Some(bytes.clone());
    nothing_was_created(
        "an artifact digest mismatch",
        daemon
            .restore_checkpoint("ckp_swapped", "alice", restore_of("branch"))
            .await,
    )
    .await;
    *artifacts.substitute.lock().unwrap() = None;

    // Records that disagree with their own artifact.
    forge("ckp_forged", &|_| {}).await;
    nothing_was_created(
        "a checkpoint id mismatch",
        daemon
            .restore_checkpoint("ckp_forged", "alice", restore_of("branch"))
            .await,
    )
    .await;
    forge("ckp_treed", &|record| {
        record.checkpoint_id = good.checkpoint_id.clone();
        record.workspace_digest = "sha256:wrong".into();
    })
    .await;
    nothing_was_created(
        "a workspace digest mismatch",
        daemon
            .restore_checkpoint("ckp_treed", "alice", restore_of("branch"))
            .await,
    )
    .await;

    // Names and operators.
    assert!(matches!(
        daemon
            .restore_checkpoint(&checkpoint.checkpoint_id, "alice", restore_of("origin"))
            .await,
        Err(EnvironmentError::Conflict(_))
    ));
    assert!(matches!(
        daemon
            .restore_checkpoint(
                &checkpoint.checkpoint_id,
                "alice",
                restore_of("x--candidate")
            )
            .await,
        Err(EnvironmentError::Invalid(_))
    ));
    let stranger = daemon
        .restore_checkpoint(&checkpoint.checkpoint_id, "mallory", restore_of("theirs"))
        .await;
    assert!(
        matches!(stranger, Err(EnvironmentError::NotFound(_))),
        "{stranger:?}"
    );
    assert!(daemon.computer("theirs").await.is_err());

    // The good checkpoint was never harmed by any of it, and still restores.
    assert!(
        records(&daemon, "origin").await.contains(&good),
        "the original record is unchanged"
    );
    assert_eq!(
        daemon
            .checkpoint("origin", "alice", &good.checkpoint_id)
            .await
            .unwrap()
            .valid,
        Some(true)
    );
    assert!(daemon.computer("origin").await.unwrap().converged);
    let report = daemon
        .restore_checkpoint(&checkpoint.checkpoint_id, "alice", restore_of("branch"))
        .await
        .unwrap();
    assert!(report.workspace_verified);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_restore_publishes_nothing_and_a_retry_succeeds() {
    let (_repositories, source) = repository();
    let (daemon, artifacts, _state, _target, _workspaces, _node, steered) =
        checkpoint_world(&source).await;
    let origin = daemon.computer("origin").await.unwrap();
    let (checkpoint, digest) = captured(&daemon).await;
    let record = records(&daemon, "origin").await.remove(0);
    let bytes = artifact_bytes(&artifacts, &record.artifact_id).await;

    let failed_at = async |phase: &str, failed: Result<RestoreReport, EnvironmentError>| {
        let Err(EnvironmentError::Conflict(reason)) = &failed else {
            panic!("{phase}: {failed:?}")
        };
        assert!(reason.contains(&format!("while {phase}")), "{reason}");
        assert!(reason.contains("unverified"), "{reason}");
        // The checkpoint and the source are as they were.
        assert_eq!(artifact_bytes(&artifacts, &record.artifact_id).await, bytes);
        assert_eq!(records(&daemon, "origin").await, vec![record.clone()]);
        let still = daemon.computer("origin").await.unwrap();
        assert_eq!(still.session_id, origin.session_id);
        assert!(still.converged);
        assert_eq!(digest_of(&daemon, "origin").await, digest);
        assert!(
            !events(&daemon, "origin")
                .await
                .iter()
                .any(|(_, data)| data["command"] == "restore"),
            "{phase}: the source's history is not written to"
        );
        // Nothing usable exists, the name is free, and the candidate is inert.
        assert!(
            daemon.computer("branch").await.is_err(),
            "{phase}: the name is free"
        );
        let candidate = computer_where(
            &daemon,
            "branch--candidate",
            "the candidate to stop",
            |view| view.reality.observed == "stopped",
        )
        .await;
        assert_eq!(candidate.reality.desired, "stopped");
        let recorded = events(&daemon, "branch--candidate").await;
        let failure = recorded
            .iter()
            .rev()
            .find(|(_, data)| data["command"] == "restore" && data["outcome"] == "failed")
            .unwrap_or_else(|| panic!("{phase}: the failure is evidenced"));
        assert_eq!(failure.1["phase"], phase);
        assert_eq!(failure.1["workspace_verified"], false);
    };
    let attempt =
        || daemon.restore_checkpoint(&checkpoint.checkpoint_id, "alice", restore_of("branch"));

    // Seeding fails.
    *steered.fail_exec_containing.lock().unwrap() = Some("tar -xf".into());
    failed_at("seeding", attempt().await).await;
    *steered.fail_exec_containing.lock().unwrap() = None;

    // The seeded workspace is not the checkpoint's: verification refuses it.
    *steered.tamper.lock().unwrap() = Some((
        "workspace_check\ndigest".into(),
        Box::new(|workspace: &Path| std::fs::write(workspace.join("stray"), "x").unwrap()),
    ));
    failed_at("seeding", attempt().await).await;

    // The declared contents cannot be brought up.
    *steered.fail_exec_containing.lock().unwrap() = Some("git fetch".into());
    failed_at("reconciling", attempt().await).await;
    *steered.fail_exec_containing.lock().unwrap() = None;

    // A retry: the leftover candidate is cleared, the name was never poisoned.
    let report = attempt().await.unwrap();
    assert!(report.workspace_verified);
    assert_eq!(digest_of(&daemon, "branch").await, digest);
    assert!(daemon.computer("branch--candidate").await.is_err());
    assert_eq!(artifact_bytes(&artifacts, &record.artifact_id).await, bytes);
    assert_eq!(records(&daemon, "origin").await, vec![record]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restore_interrupted_by_a_restart_publishes_nothing_and_retries() {
    let (_repositories, source) = repository();
    // One permit: the origin provisions; the restore's machine waits.
    let gate = Arc::new(Semaphore::new(1));
    let (first, artifacts, state, target, _workspaces, _node, _steered) =
        checkpoint_world_gated(&source, Some(gate.clone())).await;
    let (checkpoint, digest) = captured(&first).await;
    let record = records(&first, "origin").await.remove(0);

    let restoring = {
        let first = first.clone();
        let id = checkpoint.checkpoint_id.clone();
        tokio::spawn(async move {
            first
                .restore_checkpoint(&id, "alice", restore_of("branch"))
                .await
        })
    };
    eventually("the candidate to be created", async || {
        first.computer("branch--candidate").await.ok()
    })
    .await;
    first.shutdown().await;
    restoring.abort();
    drop(first);

    // A new controller: nothing is authoritative under the name, the
    // checkpoint is intact, and the restore can be retried.
    gate.add_permits(64);
    let store: Arc<dyn StateStore> = state.clone();
    let (second, _node2) =
        start_daemon_with(store, artifacts.clone(), pool(&[("target-a", &target)])).await;
    computer_where(&second, "origin", "the origin to converge", |view| {
        view.converged
    })
    .await;
    assert!(second.computer("branch").await.is_err());
    assert_eq!(records(&second, "origin").await, vec![record.clone()]);
    assert_eq!(
        second
            .checkpoint("origin", "alice", &record.checkpoint_id)
            .await
            .unwrap()
            .valid,
        Some(true)
    );
    let report = second
        .restore_checkpoint(&checkpoint.checkpoint_id, "alice", restore_of("branch"))
        .await
        .unwrap();
    assert!(report.workspace_verified);
    assert_eq!(digest_of(&second, "branch").await, digest);
    assert!(second.computer("branch--candidate").await.is_err());
    second.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checkpoint_and_restore_work_over_http_and_ownership_holds() {
    let (_repositories, source) = repository();
    let workspaces = tempfile::tempdir().unwrap();
    let target = Target::start(Steered::new(workspaces.path(), full(), None), &[]);
    let store: Arc<dyn StateStore> = Arc::new(MemoryState::new());
    let artifacts = FaultyArtifacts::new(&store);
    let (daemon, _node) =
        start_daemon_tuned(store, artifacts, pool(&[("target-a", &target)]), |config| {
            config.security.legacy_token = Some("operator".into())
        })
        .await;
    // The token's principal owns `origin`; alice owns `theirs`.
    for (name, owner) in [("origin", "legacy-token"), ("theirs", "alice")] {
        daemon
            .create_computer_environment(
                definition(
                    name,
                    ComputerLifecycle::Persistent,
                    requirements(),
                    contents(&source, "v1"),
                ),
                owner,
            )
            .await
            .unwrap();
        computer_where(&daemon, name, "converge", |view| view.converged).await;
    }
    let their_checkpoint = daemon
        .checkpoint_environment("theirs", "alice", no_parent())
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(api::serve(listener, daemon.clone(), None));
    let anonymous = client::DaemonClient::new(&endpoint).unwrap();
    let operator = client::DaemonClient::new(&endpoint)
        .unwrap()
        .with_bearer_token("operator");

    // Nothing without the token.
    assert!(matches!(
        anonymous
            .post::<_, CheckpointReport>("/environments/origin/checkpoint", Some(&no_parent()))
            .await,
        Err(EnvironmentError::Unauthorized(_))
    ));
    assert!(matches!(
        anonymous
            .post::<_, RestoreReport>(
                &format!("/checkpoints/{}/restore", their_checkpoint.checkpoint_id),
                Some(&restore_of("stolen"))
            )
            .await,
        Err(EnvironmentError::Unauthorized(_))
    ));

    // Capture, list, inspect and restore, all over the wire.
    let captured: CheckpointReport = operator
        .post("/environments/origin/checkpoint", Some(&no_parent()))
        .await
        .unwrap();
    assert!(captured.verified);
    let listed: Vec<CheckpointView> = operator
        .get("/environments/origin/checkpoints")
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].checkpoint.checkpoint_id, captured.checkpoint_id);
    let shown: CheckpointView = operator
        .get(&format!(
            "/environments/origin/checkpoints/{}",
            captured.checkpoint_id
        ))
        .await
        .unwrap();
    assert_eq!(shown.valid, Some(true));
    let restored: RestoreReport = operator
        .post(
            &format!("/checkpoints/{}/restore", captured.checkpoint_id),
            Some(&restore_of("branch")),
        )
        .await
        .unwrap();
    assert!(restored.workspace_verified);
    assert_eq!(restored.workspace, captured.workspace);
    assert_eq!(restored.environment, "branch");
    assert!(daemon.computer("branch").await.unwrap().converged);

    // Ownership holds over HTTP: another operator's checkpoint is not
    // visible, restorable, or capturable.
    let refused = operator
        .post::<_, RestoreReport>(
            &format!("/checkpoints/{}/restore", their_checkpoint.checkpoint_id),
            Some(&restore_of("stolen")),
        )
        .await;
    assert!(
        matches!(refused, Err(EnvironmentError::NotFound(_))),
        "{refused:?}"
    );
    assert!(daemon.computer("stolen").await.is_err());
    assert!(
        operator
            .get::<Vec<CheckpointView>>("/environments/theirs/checkpoints")
            .await
            .is_err()
    );
    assert!(
        operator
            .post::<_, CheckpointReport>("/environments/theirs/checkpoint", Some(&no_parent()))
            .await
            .is_err()
    );
    let missing = operator
        .post::<_, RestoreReport>("/checkpoints/ckp_nope/restore", Some(&restore_of("ghost")))
        .await;
    assert!(
        matches!(missing, Err(EnvironmentError::NotFound(_))),
        "{missing:?}"
    );
    server.abort();
}

// ---- configuration -------------------------------------------------------------

const DATABASE_URL: &str = "postgres://app:S3cr3tPassw0rd@db.internal/app";

fn env_file(text: &str) -> ConfigFile {
    ConfigFile {
        name: ".env".into(),
        content: text.into(),
    }
}

fn import_of(files: Vec<ConfigFile>) -> ConfigImportRequest {
    ConfigImportRequest {
        files,
        public: vec![],
        secret: vec![],
    }
}

/// A process that only stays alive: its environment is read from `/proc`, so a
/// test never has to write a value into the workspace to see it.
fn sleeper() -> EnvironmentContents {
    let mut contents = EnvironmentContents::default();
    contents.processes.push(ProcessSpec {
        name: "app".into(),
        kind: ProcessKind::Process,
        runtime: None,
        command: vec!["sh".into(), "-c".into(), "sleep 3600".into()],
        repository: None,
        env: BTreeMap::new(),
        desired: ProcessDesired::Running,
        port: Some(41999),
        restart: 0,
        readiness: None,
        restart_policy: Default::default(),
        max_restarts: compute_core::DEFAULT_MAX_RESTARTS,
    });
    contents
}

/// The SHA-256 of a variable's value in the environment the process was
/// actually started with, or `None` when it has none. Computed inside the
/// computer, so neither the value nor a command holding it is ever sent.
async fn process_variable(daemon: &Arc<Daemon>, name: &str, variable: &str) -> Option<String> {
    let pid = daemon.computer(name).await.unwrap().observed.processes["app"]
        .pid
        .expect("the process runs");
    let out = sh(
        daemon,
        name,
        &format!(
            "if tr '\\0' '\\n' < /proc/{pid}/environ | grep -q '^{variable}='; then \
             tr '\\0' '\\n' < /proc/{pid}/environ | sed -n 's/^{variable}=//p' | tr -d '\\n' | sha256sum | cut -d' ' -f1; \
             else echo none; fi"
        ),
    )
    .await;
    let out = out.trim().to_owned();
    (out != "none").then(|| format!("sha256:{out}"))
}

fn digest_of_value(value: &str) -> Option<String> {
    Some(compute_core::sha256_identity(value.as_bytes()))
}

fn variable(view: &ConfigurationView, name: &str) -> ConfigurationInput {
    view.variables
        .iter()
        .find(|variable| variable.name == name)
        .unwrap_or_else(|| panic!("{name} is not configured: {view:?}"))
        .clone()
}

/// Every file under `dir` that contains `needle`.
fn files_containing(dir: &Path, needle: &str) -> Vec<PathBuf> {
    let mut found = vec![];
    let mut pending = vec![dir.to_path_buf()];
    while let Some(path) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if std::fs::read(&path)
                .is_ok_and(|bytes| bytes.windows(needle.len()).any(|w| w == needle.as_bytes()))
            {
                found.push(path);
            }
        }
    }
    found
}

/// Everything an operator can observe about an environment, as one string.
async fn observable(daemon: &Arc<Daemon>, name: &str) -> String {
    let mut all = serde_json::to_string(&daemon.computer(name).await.unwrap()).unwrap();
    all += &serde_json::to_string(&daemon.environment(name).await.unwrap()).unwrap();
    all += &serde_json::to_string(&daemon.configuration(name, "alice").await.unwrap()).unwrap();
    all += &serde_json::to_string(&events(daemon, name).await).unwrap();
    all
}

async fn created(daemon: &Arc<Daemon>, name: &str, source: &Path) -> ComputerView {
    let mut contents = contents(source, "v1");
    contents.processes = sleeper().processes;
    daemon
        .create_computer_environment(
            definition(
                name,
                ComputerLifecycle::Persistent,
                requirements(),
                contents,
            ),
            "alice",
        )
        .await
        .unwrap();
    computer_where(daemon, name, "the process to run", |view| {
        view.converged && view.observed.processes["app"].pid.is_some()
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn env_files_are_imported_as_configuration_and_reach_processes_without_leaking() {
    let (_repositories, source) = repository();
    let (daemon, _artifacts, _state, target, _workspaces, _node, _steered) =
        checkpoint_world(&source).await;
    let started = created(&daemon, "app", &source).await;
    let before = started.observed.processes["app"].pid;
    assert_eq!(process_variable(&daemon, "app", "DATABASE_URL").await, None);

    // The workspace asks for variables; discovery lists names only.
    sh(
        &daemon,
        "app",
        "printf 'DATABASE_URL=\\nSTRIPE_SECRET_KEY=\\nPORT=3000\\nREDIS_URL=\\n' > .env.example && \
         printf 'DATABASE_URL=from-the-workspace-file\\nAPP_MODE=test\\nPORT=3000\\n' > .env",
    )
    .await;
    let found = daemon.discover_configuration("app", "alice").await.unwrap();
    assert_eq!(
        found
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.kind.as_str()))
            .collect::<Vec<_>>(),
        vec![(".env", "values"), (".env.example", "requirements")]
    );
    let status = |name: &str| {
        let v = found.variables.iter().find(|v| v.name == name).unwrap();
        (v.status.clone(), v.sensitive)
    };
    assert_eq!(status("DATABASE_URL"), ("available".into(), true));
    assert_eq!(status("APP_MODE"), ("available".into(), false));
    assert_eq!(status("STRIPE_SECRET_KEY"), ("missing".into(), true));
    assert_eq!(status("REDIS_URL"), ("missing".into(), true));
    assert!(
        !serde_json::to_string(&found)
            .unwrap()
            .contains("S3cr3tPassw0rd")
    );

    // Import: parsed, classified, one generation, the file not copied anywhere.
    let report = daemon
        .import_configuration(
            "app",
            "alice",
            import_of(vec![env_file(&format!(
                "DATABASE_URL={DATABASE_URL}\nAPP_MODE=test\nPORT=3000\n"
            ))]),
        )
        .await
        .unwrap();
    assert!(!report.unchanged && report.generation > 0);
    assert_eq!(
        report
            .imported
            .iter()
            .map(|v| (v.name.as_str(), v.sensitive))
            .collect::<Vec<_>>(),
        vec![("APP_MODE", false), ("DATABASE_URL", true)]
    );
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(
        report.skipped[0].name, "PORT",
        "reserved: Compute gives a process its port"
    );
    let view = daemon.configuration("app", "alice").await.unwrap();
    assert_eq!(view.generation, report.generation);
    assert_eq!(variable(&view, "DATABASE_URL").source, ".env");
    assert!(variable(&view, "DATABASE_URL").value.is_none());
    assert_eq!(variable(&view, "APP_MODE").value.as_deref(), Some("test"));
    assert!(
        !sh(
            &daemon,
            "app",
            "test -e imported.env && echo copied || echo no"
        )
        .await
        .contains("copied")
    );

    // The reconciler restarts the process with the configuration: a new
    // process, started under this generation.
    let restarted = computer_where(&daemon, "app", "the process to restart", |view| {
        view.converged
            && view.observed.processes["app"].pid.is_some()
            && view.observed.processes["app"].pid != before
            && view.observed.processes["app"].config_generation == report.generation
    })
    .await;
    assert_eq!(
        restarted.reality.processes["app"].config_generation,
        report.generation
    );
    assert_eq!(
        process_variable(&daemon, "app", "DATABASE_URL").await,
        digest_of_value(DATABASE_URL)
    );
    assert_eq!(
        process_variable(&daemon, "app", "APP_MODE").await,
        digest_of_value("test")
    );
    assert_eq!(
        process_variable(&daemon, "app", "PORT").await,
        digest_of_value("41999"),
        "the declared port, not the file's"
    );

    // No surface returns the secret: views, configuration, events, the report,
    // and the target's job records and receipts.
    let everything = observable(&daemon, "app").await + &serde_json::to_string(&report).unwrap();
    assert!(
        !everything.contains("S3cr3tPassw0rd"),
        "an API view or event leaked the value"
    );
    assert!(everything.contains("APP_MODE"), "names are shown");
    assert_eq!(
        files_containing(target.stores.path(), "S3cr3tPassw0rd"),
        Vec::<PathBuf>::new(),
        "a job record or receipt holds the value"
    );

    // Importing the same thing again changes nothing, and no generation moves.
    let again = daemon
        .import_configuration(
            "app",
            "alice",
            import_of(vec![env_file(&format!(
                "DATABASE_URL={DATABASE_URL}\nAPP_MODE=test\n"
            ))]),
        )
        .await
        .unwrap();
    assert!(again.unchanged);
    assert_eq!(again.generation, report.generation);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn configuration_belongs_to_the_environment_not_the_controller() {
    let (_repositories, source) = repository();
    let (daemon, _artifacts, _state, _target, _workspaces, _node, _steered) =
        checkpoint_world(&source).await;
    created(&daemon, "alpha", &source).await;
    created(&daemon, "beta", &source).await;
    for (name, value) in [("alpha", "A"), ("beta", "B")] {
        daemon
            .import_configuration(
                name,
                "alice",
                import_of(vec![env_file(&format!("DATABASE_URL={value}\n"))]),
            )
            .await
            .unwrap();
    }
    for name in ["alpha", "beta"] {
        computer_where(&daemon, name, "configured", |view| {
            view.converged && view.observed.processes["app"].config_generation > 0
        })
        .await;
    }
    assert_eq!(
        process_variable(&daemon, "alpha", "DATABASE_URL").await,
        digest_of_value("A")
    );
    assert_eq!(
        process_variable(&daemon, "beta", "DATABASE_URL").await,
        digest_of_value("B")
    );
    let beta = daemon.computer("beta").await.unwrap();
    let beta_generation = beta.configuration.generation;
    let beta_pid = beta.observed.processes["app"].pid;

    // Change one: the other does not move.
    let alpha_pid = daemon.computer("alpha").await.unwrap().observed.processes["app"].pid;
    daemon
        .change_configuration(
            "alpha",
            "alice",
            ConfigChange {
                set: BTreeMap::from([("DATABASE_URL".into(), "A2".into())]),
                source: Some("api".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    computer_where(&daemon, "alpha", "the restart", |view| {
        view.converged
            && view.observed.processes["app"].pid != alpha_pid
            && view.observed.processes["app"].pid.is_some()
    })
    .await;
    assert_eq!(
        process_variable(&daemon, "alpha", "DATABASE_URL").await,
        digest_of_value("A2")
    );
    assert_eq!(
        process_variable(&daemon, "beta", "DATABASE_URL").await,
        digest_of_value("B")
    );
    let beta = daemon.computer("beta").await.unwrap();
    assert_eq!(beta.configuration.generation, beta_generation);
    assert_eq!(
        beta.observed.processes["app"].pid, beta_pid,
        "beta's process was not restarted"
    );
    let alpha = daemon.configuration("alpha", "alice").await.unwrap();
    assert_eq!(variable(&alpha, "DATABASE_URL").source, "api");
    assert!(alpha.generation > beta_generation || alpha.generation != beta_generation);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_import_is_atomic_authorized_and_deterministic() {
    let (_repositories, source) = repository();
    let (daemon, _artifacts, _state, _target, _workspaces, _node, _steered) =
        checkpoint_world(&source).await;
    let before = daemon.configuration("origin", "alice").await.unwrap();
    let watch = async |what: &str, result: Result<ConfigImportReport, EnvironmentError>| {
        assert!(result.is_err(), "{what}: {result:?}");
        assert_eq!(
            daemon.configuration("origin", "alice").await.unwrap(),
            before,
            "{what}: nothing was applied"
        );
    };

    // Good lines before a bad one apply nothing.
    let error = daemon
        .import_configuration(
            "origin",
            "alice",
            import_of(vec![env_file(
                "ONE=1\nTWO=2\nTHREE=3\nnot a line sk_live_TOPSECRET\n",
            )]),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains(".env, line 4") && !error.contains("sk_live_TOPSECRET"),
        "{error}"
    );
    watch(
        "a later malformed line",
        daemon
            .import_configuration(
                "origin",
                "alice",
                import_of(vec![env_file("ONE=1\nTWO=2\nbad\n")]),
            )
            .await,
    )
    .await;
    watch(
        "a name twice in a file",
        daemon
            .import_configuration("origin", "alice", import_of(vec![env_file("A=1\nA=2\n")]))
            .await,
    )
    .await;
    watch(
        "a bad second file",
        daemon
            .import_configuration(
                "origin",
                "alice",
                import_of(vec![
                    env_file("GOOD=1\n"),
                    ConfigFile {
                        name: ".env.local".into(),
                        content: "\"unclosed\n".into(),
                    },
                ]),
            )
            .await,
    )
    .await;
    watch(
        "a treatment for a variable the files do not define",
        daemon
            .import_configuration(
                "origin",
                "alice",
                ConfigImportRequest {
                    files: vec![env_file("A=1\n")],
                    public: vec!["OTHER".into()],
                    secret: vec![],
                },
            )
            .await,
    )
    .await;
    watch(
        "no files",
        daemon
            .import_configuration("origin", "alice", import_of(vec![]))
            .await,
    )
    .await;

    // Another operator cannot change or read configuration.
    let stranger = daemon
        .import_configuration("origin", "mallory", import_of(vec![env_file("X=1\n")]))
        .await;
    assert!(stranger.is_err());
    assert!(daemon.configuration("origin", "mallory").await.is_err());
    assert!(
        daemon
            .discover_configuration("origin", "mallory")
            .await
            .is_err()
    );
    assert!(
        daemon
            .change_configuration(
                "origin",
                "mallory",
                ConfigChange {
                    set: BTreeMap::from([("X".into(), "1".into())]),
                    ..Default::default()
                }
            )
            .await
            .is_err()
    );
    assert_eq!(
        daemon.configuration("origin", "alice").await.unwrap(),
        before
    );
    assert!(matches!(
        daemon
            .import_configuration("nowhere", "alice", import_of(vec![env_file("X=1\n")]))
            .await,
        Err(EnvironmentError::NotFound(_))
    ));

    // Files apply in the order given, deterministically; treatment is honoured.
    let report = daemon
        .import_configuration(
            "origin",
            "alice",
            ConfigImportRequest {
                files: vec![
                    env_file("SHARED=base\nLABEL=\"héllo\"\nEMPTY=\nQUOTED='a=b #c'\r\n"),
                    ConfigFile {
                        name: ".env.local".into(),
                        content: "SHARED=local\nCOMPUTE_INTERNAL=1\nMODE_FLAG=on\n".into(),
                    },
                ],
                public: vec!["LABEL".into(), "MODE_FLAG".into()],
                secret: vec!["QUOTED".into()],
            },
        )
        .await
        .unwrap();
    assert_eq!(report.overridden, vec!["SHARED".to_string()]);
    assert_eq!(
        report
            .skipped
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        vec!["COMPUTE_INTERNAL"]
    );
    let view = daemon.configuration("origin", "alice").await.unwrap();
    assert_eq!(variable(&view, "SHARED").source, ".env.local");
    assert_eq!(variable(&view, "LABEL").value.as_deref(), Some("héllo"));
    assert_eq!(variable(&view, "MODE_FLAG").value.as_deref(), Some("on"));
    assert!(variable(&view, "SHARED").sensitive && variable(&view, "SHARED").value.is_none());
    assert!(variable(&view, "QUOTED").sensitive);
    assert!(variable(&view, "EMPTY").configured);
    assert_eq!(
        sh(&daemon, "origin", "printenv QUOTED").await.trim(),
        "a=b #c"
    );
    assert!(view.generation > before.generation);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_process_that_fails_reports_evidence_without_its_environment() {
    let (_repositories, source) = repository();
    let (daemon, _artifacts, _state, target, _workspaces, _node, _steered) =
        checkpoint_world(&source).await;
    daemon
        .import_configuration(
            "origin",
            "alice",
            import_of(vec![env_file(&format!("DATABASE_URL={DATABASE_URL}\n"))]),
        )
        .await
        .unwrap();
    let mut contents = daemon.computer("origin").await.unwrap().desired;
    contents.processes = vec![ProcessSpec {
        name: "app".into(),
        kind: ProcessKind::Process,
        runtime: None,
        command: vec!["sh".into(), "-c".into(), "echo starting >&2; exit 3".into()],
        repository: None,
        env: BTreeMap::new(),
        desired: ProcessDesired::Running,
        port: None,
        restart: 0,
        readiness: None,
        restart_policy: Default::default(),
        max_restarts: 1,
    }];
    let contents_update = ContentsUpdate {
        contents,
        config: None,
        lifecycle: None,
        expected_generation: None,
    };
    daemon
        .set_contents("origin", "alice", contents_update)
        .await
        .unwrap();
    let failed = computer_where(&daemon, "origin", "the process to fail", |view| {
        view.observed
            .processes
            .get("app")
            .is_some_and(|seen| seen.last_failure.is_some())
    })
    .await;
    assert!(failed.observed.processes["app"].last_failure.is_some());
    assert!(
        !observable(&daemon, "origin")
            .await
            .contains("S3cr3tPassw0rd")
    );
    assert!(files_containing(target.stores.path(), "S3cr3tPassw0rd").is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_go_round_trip_of_the_masked_view_never_erases_what_it_could_not_see() {
    let (_repositories, source) = repository();
    let (daemon, _artifacts, _state, _target, _workspaces, _node, _steered) =
        checkpoint_world(&source).await;
    // The world's `API_TOKEN` is not a public name: its value is not in the view.
    let view = daemon.computer("origin").await.unwrap();
    assert!(!view.config.contains_key("API_TOKEN"));
    assert!(
        view.config.contains_key("APP_ENV"),
        "a public value is shown"
    );
    assert!(variable(&view.configuration, "API_TOKEN").sensitive);
    let generation = view.configuration.generation;

    // GO writes back what it could see: the secret survives, nothing changes.
    let update = ContentsUpdate {
        contents: view.desired.clone(),
        config: Some(view.config.clone()),
        lifecycle: None,
        expected_generation: None,
    };
    daemon
        .set_contents("origin", "alice", update)
        .await
        .unwrap();
    let after = daemon.computer("origin").await.unwrap();
    assert_eq!(
        after.configuration.generation, generation,
        "no configuration change"
    );
    assert_eq!(
        sh(&daemon, "origin", "printenv API_TOKEN").await.trim(),
        "s3cret-token-value"
    );

    // Removal is by name, explicitly.
    let view = daemon
        .change_configuration(
            "origin",
            "alice",
            ConfigChange {
                unset: vec!["API_TOKEN".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(view.variables.iter().all(|v| v.name != "API_TOKEN"));
    assert!(view.generation > generation);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fork_checkpoint_and_restore_carry_configuration_requirements_never_values() {
    let (_repositories, source) = repository();
    let (daemon, artifacts, _state, target, _workspaces, _node, _steered) =
        checkpoint_world(&source).await;
    let started = created(&daemon, "app", &source).await;
    let before = started.observed.processes["app"].pid;
    daemon
        .import_configuration(
            "app",
            "alice",
            import_of(vec![env_file(&format!(
                "DATABASE_URL={DATABASE_URL}\nAPP_MODE=test\n"
            ))]),
        )
        .await
        .unwrap();
    computer_where(&daemon, "app", "the configured process", |view| {
        view.converged
            && view.observed.processes["app"].pid != before
            && view.observed.processes["app"].config_generation > 0
    })
    .await;
    sh(
        &daemon,
        "app",
        "mkdir data && printf notes > data/notes.txt",
    )
    .await;
    assert_eq!(
        process_variable(&daemon, "app", "DATABASE_URL").await,
        digest_of_value(DATABASE_URL)
    );

    // Fork: the workspace and declarations, and no configuration values.
    let forked = daemon
        .fork_environment("app", "alice", fork_of("forked"))
        .await
        .unwrap();
    assert!(forked.omitted_config.contains(&"DATABASE_URL".to_string()));
    let forked_view = daemon.computer("forked").await.unwrap();
    assert!(forked_view.config.is_empty());
    assert!(forked_view.configuration.variables.is_empty());
    assert_eq!(
        process_variable(&daemon, "forked", "DATABASE_URL").await,
        None
    );
    assert!(
        !observable(&daemon, "forked")
            .await
            .contains("S3cr3tPassw0rd")
    );

    // Checkpoint: the manifest names the configuration, never a value.
    let (checkpoint, digest) = {
        let digest = digest_of(&daemon, "app").await;
        (
            daemon
                .checkpoint_environment("app", "alice", no_parent())
                .await
                .unwrap(),
            digest,
        )
    };
    assert_eq!(checkpoint.workspace, digest);
    let record = records(&daemon, "app").await.remove(0);
    let bytes = artifact_bytes(&artifacts, &record.artifact_id).await;
    let valid = compute_environment::checkpoint::validate(&bytes).unwrap();
    let configuration = valid
        .manifest
        .configuration
        .clone()
        .expect("provenance is recorded");
    let named = |name: &str| {
        configuration
            .variables
            .iter()
            .find(|variable| variable.name == name)
            .unwrap_or_else(|| panic!("{name}: {configuration:?}"))
            .clone()
    };
    assert!(named("DATABASE_URL").sensitive);
    assert_eq!(named("DATABASE_URL").source, ".env");
    assert!(!named("APP_MODE").sensitive);
    assert!(!String::from_utf8_lossy(&bytes).contains("S3cr3tPassw0rd"));
    assert!(
        !String::from_utf8_lossy(&bytes).contains("\"value\""),
        "the manifest holds no values"
    );

    // Restore: the workspace, and the names to supply; not the values.
    let restored = daemon
        .restore_checkpoint(&checkpoint.checkpoint_id, "alice", restore_of("revived"))
        .await
        .unwrap();
    assert_eq!(restored.workspace, digest);
    assert_eq!(digest_of(&daemon, "revived").await, digest);
    assert!(
        restored
            .configuration_required
            .iter()
            .any(|required| required.name == "DATABASE_URL" && required.sensitive)
    );
    let revived = daemon.computer("revived").await.unwrap();
    assert!(revived.config.is_empty() && revived.configuration.variables.is_empty());
    assert_eq!(
        process_variable(&daemon, "revived", "DATABASE_URL").await,
        None
    );
    assert!(
        !observable(&daemon, "revived")
            .await
            .contains("S3cr3tPassw0rd")
    );

    // Explicit configuration reaches the restored environment's process, and
    // only that one.
    let revived_pid = revived.observed.processes["app"].pid;
    daemon
        .import_configuration(
            "revived",
            "alice",
            import_of(vec![env_file("DATABASE_URL=postgres://revived/db\n")]),
        )
        .await
        .unwrap();
    computer_where(&daemon, "revived", "the restart", |view| {
        view.converged
            && view.observed.processes["app"].pid != revived_pid
            && view.observed.processes["app"].pid.is_some()
    })
    .await;
    assert_eq!(
        process_variable(&daemon, "revived", "DATABASE_URL").await,
        digest_of_value("postgres://revived/db")
    );
    assert_eq!(
        process_variable(&daemon, "app", "DATABASE_URL").await,
        digest_of_value(DATABASE_URL),
        "the source keeps its own"
    );
    assert_eq!(
        process_variable(&daemon, "forked", "DATABASE_URL").await,
        None
    );

    // Nothing of the secret reached any record the target keeps.
    assert!(files_containing(target.stores.path(), "S3cr3tPassw0rd").is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn configuration_works_over_http_and_never_returns_a_secret_or_crosses_owners() {
    let (_repositories, source) = repository();
    let workspaces = tempfile::tempdir().unwrap();
    let target = Target::start(Steered::new(workspaces.path(), full(), None), &[]);
    let store: Arc<dyn StateStore> = Arc::new(MemoryState::new());
    let artifacts = FaultyArtifacts::new(&store);
    let (daemon, _node) =
        start_daemon_tuned(store, artifacts, pool(&[("target-a", &target)]), |config| {
            config.security.legacy_token = Some("operator".into())
        })
        .await;
    for (name, owner) in [("mine", "legacy-token"), ("theirs", "alice")] {
        let mut contents = contents(&source, "v1");
        contents.processes = sleeper().processes;
        daemon
            .create_computer_environment(
                definition(
                    name,
                    ComputerLifecycle::Persistent,
                    requirements(),
                    contents,
                ),
                owner,
            )
            .await
            .unwrap();
        computer_where(&daemon, name, "converge", |view| view.converged).await;
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(api::serve(listener, daemon.clone(), None));
    let anonymous = client::DaemonClient::new(&endpoint).unwrap();
    let operator = client::DaemonClient::new(&endpoint)
        .unwrap()
        .with_bearer_token("operator");
    let request = import_of(vec![env_file(&format!(
        "DATABASE_URL={DATABASE_URL}\nAPP_MODE=test\n"
    ))]);

    // Nothing without the token.
    assert!(matches!(
        anonymous
            .get::<serde_json::Value>("/environments/mine/config")
            .await,
        Err(EnvironmentError::Unauthorized(_))
    ));
    assert!(matches!(
        anonymous
            .post::<_, serde_json::Value>("/environments/mine/config/import", Some(&request))
            .await,
        Err(EnvironmentError::Unauthorized(_))
    ));

    // Import, inspect, change, discover: every response is free of the value.
    let imported: serde_json::Value = operator
        .post("/environments/mine/config/import", Some(&request))
        .await
        .unwrap();
    assert_eq!(imported["imported"].as_array().unwrap().len(), 2);
    let shown: ConfigurationView = operator.get("/environments/mine/config").await.unwrap();
    assert_eq!(variable(&shown, "DATABASE_URL").sensitive, true);
    assert!(variable(&shown, "DATABASE_URL").value.is_none());
    assert_eq!(variable(&shown, "APP_MODE").value.as_deref(), Some("test"));
    let changed: ConfigurationView = operator
        .post(
            "/environments/mine/config/change",
            Some(&ConfigChange {
                set: BTreeMap::from([("EXTRA".into(), "x".into())]),
                source: Some("api".into()),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    assert!(changed.generation > shown.generation);
    let discovered: serde_json::Value = operator
        .post::<(), _>("/environments/mine/config/discover", None)
        .await
        .unwrap();
    let computer: serde_json::Value = operator.get("/environments/mine/computer").await.unwrap();
    let environment: serde_json::Value = operator.get("/environments/mine").await.unwrap();
    for body in [&imported, &discovered, &computer, &environment] {
        assert!(!body.to_string().contains("S3cr3tPassw0rd"), "{body}");
    }
    assert!(
        !serde_json::to_string(&shown)
            .unwrap()
            .contains("S3cr3tPassw0rd")
    );
    assert!(
        !serde_json::to_string(&changed)
            .unwrap()
            .contains("S3cr3tPassw0rd")
    );

    // Another operator's environment is neither visible nor changeable.
    assert!(matches!(
        operator
            .get::<serde_json::Value>("/environments/theirs/config")
            .await,
        Err(EnvironmentError::Forbidden(_))
    ));
    assert!(
        operator
            .post::<_, serde_json::Value>("/environments/theirs/config/import", Some(&request))
            .await
            .is_err()
    );
    assert!(
        operator
            .post::<_, serde_json::Value>(
                "/environments/theirs/config/change",
                Some(&ConfigChange {
                    set: BTreeMap::from([("X".into(), "1".into())]),
                    ..Default::default()
                })
            )
            .await
            .is_err()
    );
    assert!(
        daemon
            .configuration("theirs", "alice")
            .await
            .unwrap()
            .variables
            .iter()
            .all(|v| v.name != "X" && v.name != "DATABASE_URL")
    );

    // A malformed import over the wire applies nothing and echoes nothing.
    let before = daemon.configuration("mine", "legacy-token").await.unwrap();
    let error = operator
        .post::<_, serde_json::Value>(
            "/environments/mine/config/import",
            Some(&import_of(vec![env_file("OK=1\nsk_live_TOPSECRET\n")])),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("line 2") && !error.contains("sk_live_TOPSECRET"),
        "{error}"
    );
    assert_eq!(
        daemon.configuration("mine", "legacy-token").await.unwrap(),
        before
    );
    server.abort();
}
