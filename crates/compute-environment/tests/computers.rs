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
    let node = tempfile::tempdir().unwrap();
    let artifacts = Arc::new(compute_state::StateArtifacts::new(
        compute_state::ControlState::new(store.clone()),
    ));
    let mut config = DaemonConfig::new(node.path(), store, artifacts);
    config.provider = Arc::new(common::provider());
    config.pool = Some(pool);
    config.reconcile_interval = Duration::from_millis(200);
    config.computer_probe = Duration::from_millis(400);
    config.computer_liveness = Duration::from_millis(300);
    config.computer_liveness_timeout = Duration::from_secs(3);
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
    assert_eq!(
        restarted.config.get("GREETING").map(String::as_str),
        Some("hi")
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cloning_an_environment_seeds_a_new_computer_with_the_same_workload_state() {
    let (_repositories, source) = repository();
    let workspaces = tempfile::tempdir().unwrap();
    let target = Target::start(Steered::new(workspaces.path(), full(), None), &[]);
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
    let origin = computer_where(&daemon, "origin", "the origin to converge", |view| {
        view.converged
    })
    .await;

    // State a workload wrote that no declared content owns.
    run(
        &daemon,
        "origin",
        "alice",
        &[
            "sh",
            "-c",
            "mkdir -p data/nested && printf hello > data/notes.txt && printf deep > data/nested/x.bin && mkdir empty",
        ],
    )
    .await;

    let report = daemon
        .clone_environment(
            "origin",
            "alice",
            CloneRequest {
                name: "copy".into(),
                target: None,
                copy_config: false,
            },
        )
        .await
        .unwrap();

    // The seed was proven inside the new computer, before anything started.
    assert!(report.workspace_verified);
    assert!(report.workspace.starts_with("sha256:"));
    assert!(report.files >= 3, "{report:#?}");
    assert!(report.jobs.len() >= 4, "export, upload, extract, verify");

    // Same workload state, in a different computer.
    let copy = report.computer;
    assert!(copy.converged);
    assert_ne!(copy.session_id, origin.session_id, "a new machine");
    assert_ne!(
        copy.machine
            .as_ref()
            .map(|machine| machine.resource.clone()),
        origin
            .machine
            .as_ref()
            .map(|machine| machine.resource.clone())
    );
    let (notes, _) = run(&daemon, "copy", "alice", &["cat", "data/notes.txt"]).await;
    assert_eq!(notes, "hello");
    let (deep, _) = run(&daemon, "copy", "alice", &["cat", "data/nested/x.bin"]).await;
    assert_eq!(deep, "deep");
    let (empty, _) = run(
        &daemon,
        "copy",
        "alice",
        &["sh", "-c", "test -d empty && echo yes"],
    )
    .await;
    assert_eq!(empty.trim(), "yes");

    // Declared contents were re-derived, not copied: same commit, and the
    // process runs in the clone under its own pid.
    let (from, to) = &report.repositories["app"];
    assert!(from.is_some() && from == to, "{:?}", report.repositories);
    assert_eq!(copy.observed.processes["api"].state, ProcessState::Running);
    assert_ne!(
        copy.observed.processes["api"].pid,
        origin.observed.processes["api"].pid
    );
    let (version, _) = run(&daemon, "copy", "alice", &["cat", "running-version"]).await;
    assert_eq!(version, "v1");

    // Configuration values stay behind unless asked for; their names are reported.
    assert_eq!(report.omitted_config, vec!["APP_ENV".to_string()]);
    assert!(copy.config.is_empty());

    // The clone is evidenced, and the origin was not disturbed.
    let recorded = events(&daemon, "copy").await;
    let cloned = recorded
        .iter()
        .find(|(_, data)| data["command"] == "clone")
        .expect("a clone event");
    assert_eq!(cloned.1["source"], "origin");
    assert_eq!(cloned.1["workspace"], report.workspace.as_str());
    let still = daemon.computer("origin").await.unwrap();
    assert_eq!(still.session_id, origin.session_id);
    assert!(still.converged);

    // Only the owner clones; a name is not reused.
    assert!(
        daemon
            .clone_environment(
                "origin",
                "mallory",
                CloneRequest {
                    name: "theirs".into(),
                    target: None,
                    copy_config: false
                }
            )
            .await
            .is_err()
    );
    assert!(daemon.computer("theirs").await.is_err());
    assert!(matches!(
        daemon
            .clone_environment(
                "origin",
                "alice",
                CloneRequest {
                    name: "copy".into(),
                    target: None,
                    copy_config: false
                }
            )
            .await,
        Err(EnvironmentError::Conflict(_))
    ));

    // What cannot travel is refused before anything is created.
    run(
        &daemon,
        "origin",
        "alice",
        &["ln", "-s", "/etc/passwd", "link"],
    )
    .await;
    let refused = daemon
        .clone_environment(
            "origin",
            "alice",
            CloneRequest {
                name: "linked".into(),
                target: None,
                copy_config: false,
            },
        )
        .await;
    assert!(
        matches!(&refused, Err(EnvironmentError::RuntimeUnavailable(reason)) if reason.contains("unsupported entry")),
        "{refused:?}"
    );
    assert!(
        daemon.computer("linked").await.is_err(),
        "nothing was created"
    );
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
async fn workspace_state_is_exported_seeded_and_verified_without_clone() {
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
        matches!(&refused, Err(EnvironmentError::RuntimeUnavailable(reason)) if reason.contains("changed while it was captured")),
        "{refused:?}"
    );
    // A workspace process outlives its test unless it is stopped.
    daemon
        .set_process("busy", "alice", "writer", ProcessDesired::Stopped)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_clone_never_leaves_an_unverified_environment_running() {
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
    computer_where(&daemon, "origin", "converge", |view| view.converged).await;

    // The seed's extraction fails on the new computer.
    *steered.fail_exec_containing.lock().unwrap() = Some("tar -xf".into());
    let failed = daemon
        .clone_environment(
            "origin",
            "alice",
            CloneRequest {
                name: "copy".into(),
                target: None,
                copy_config: false,
            },
        )
        .await;
    let Err(EnvironmentError::Conflict(reason)) = &failed else {
        panic!("{failed:?}")
    };
    assert!(
        reason.contains("while seeding") && reason.contains("unverified"),
        "{reason}"
    );

    // The environment exists, its workspace is unverified, and reality says
    // it is not running: no contents ever ran.
    let view = computer_where(&daemon, "copy", "the clone to stop", |view| {
        view.reality.observed == "stopped"
    })
    .await;
    assert_eq!(view.reality.desired, "stopped");
    assert!(view.desired.processes.is_empty(), "nothing was applied");
    let recorded = events(&daemon, "copy").await;
    let failure = recorded
        .iter()
        .find(|(_, data)| data["command"] == "clone" && data["outcome"] == "failed")
        .expect("the failure is recorded");
    assert_eq!(failure.1["phase"], "seeding");
    assert_eq!(failure.1["workspace_verified"], false);
    // The source is untouched.
    assert!(daemon.computer("origin").await.unwrap().converged);
}
