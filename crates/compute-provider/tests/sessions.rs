//! Compute sessions against a provider-neutral fake: the full lifecycle,
//! restarts, expiry, failures, authorization, stale provider responses, and
//! providers that lack optional capabilities.
//!
//! A "restart" kills the server's whole runtime, as a process exit would:
//! every task it ran stops at its next await, and nothing it held in memory
//! survives. The fake provider outlives it, as a real provider does.

use std::collections::{BTreeMap, BTreeSet};
use std::net::TcpListener as StdTcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use compute_core::{
    ComputeSession, JobStatus, NetworkPolicy, SessionCapabilities, SessionCommand,
    SessionConnection, SessionConnectionMode, SessionEndpointRequest, SessionFailure,
    SessionOwnership, SessionPhase, SessionResources, SessionSpec, SessionStatus,
};
use compute_provider::{
    ComputeProvider, EnvironmentState, Policy, ProviderAuthorizer, ProviderConnection,
    ProviderError, ProviderErrorKind, ProviderOperation, ProviderRequest, ProvisionRequest,
    ProvisionedSession, RemoteProvider, ServerConfig, SessionCreateRequest, SessionEnvironment,
    SessionEnvironmentSpec, SessionProvider,
};
use tokio::runtime::Runtime;
use tokio::sync::Semaphore;

/// A session provider with no relation to any real one. Environments are
/// directories so commands really run in them.
struct Fake {
    root: PathBuf,
    capabilities: SessionCapabilities,
    environments: Mutex<BTreeMap<String, EnvironmentState>>,
    calls: Mutex<Vec<String>>,
    /// Provisioning waits for a permit when gated.
    gate: Option<Arc<Semaphore>>,
    fail_provision: AtomicBool,
    fail_destroy: AtomicBool,
    lose_environment_on_resume: AtomicBool,
}

impl Fake {
    fn new(root: &Path, capabilities: SessionCapabilities) -> Arc<Self> {
        Arc::new(Self {
            root: root.to_path_buf(),
            capabilities,
            environments: Mutex::new(BTreeMap::new()),
            calls: Mutex::new(vec![]),
            gate: None,
            fail_provision: AtomicBool::new(false),
            fail_destroy: AtomicBool::new(false),
            lose_environment_on_resume: AtomicBool::new(false),
        })
    }

    fn gated(root: &Path, gate: Arc<Semaphore>) -> Arc<Self> {
        let mut fake = Arc::into_inner(Self::new(root, everything())).unwrap();
        fake.gate = Some(gate);
        Arc::new(fake)
    }

    fn calls(&self, name: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.as_str() == name)
            .count()
    }

    fn state(&self, id: &str) -> Option<EnvironmentState> {
        self.environments.lock().unwrap().get(id).copied()
    }

    fn call(&self, name: &str) {
        self.calls.lock().unwrap().push(name.into());
    }
}

fn everything() -> SessionCapabilities {
    SessionCapabilities {
        exec: true,
        terminal: true,
        filesystem: true,
        network: true,
        public_endpoint: true,
        persistent_storage: false,
        suspend: true,
        resume: true,
        claim: true,
        process_tree_termination: true,
    }
}

fn minimal() -> SessionCapabilities {
    SessionCapabilities {
        exec: true,
        terminal: false,
        filesystem: true,
        network: true,
        public_endpoint: false,
        persistent_storage: false,
        suspend: false,
        resume: false,
        claim: false,
        process_tree_termination: true,
    }
}

#[async_trait]
impl SessionProvider for Fake {
    fn kind(&self) -> String {
        "fake".into()
    }

    fn capabilities(&self) -> SessionCapabilities {
        self.capabilities
    }

    async fn provision(
        &self,
        request: &ProvisionRequest,
    ) -> Result<ProvisionedSession, ProviderError> {
        self.call("provision");
        if let Some(gate) = &self.gate {
            gate.acquire().await.unwrap().forget();
        }
        if self.fail_provision.load(Ordering::SeqCst) {
            return Err(ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                "fake capacity exhausted",
            ));
        }
        // Idempotent per session, as the contract requires.
        let id = format!("fake-{}", &request.session_id.0[4..20]);
        std::fs::create_dir_all(self.root.join(&id)).unwrap();
        self.environments
            .lock()
            .unwrap()
            .insert(id.clone(), EnvironmentState::Ready);
        Ok(ProvisionedSession {
            provider_session_id: id,
            connection: SessionConnection {
                mode: if self.capabilities.terminal {
                    SessionConnectionMode::Terminal
                } else {
                    SessionConnectionMode::Exec
                },
                address: Some("fake.invalid".into()),
                port: None,
                details: BTreeMap::new(),
            },
            endpoints: vec![],
            capabilities: self.capabilities,
        })
    }

    async fn inspect(&self, id: &str) -> Result<EnvironmentState, ProviderError> {
        Ok(self.state(id).unwrap_or(EnvironmentState::Missing))
    }

    async fn exec(
        &self,
        environment: &SessionEnvironment,
        command: &SessionCommand,
    ) -> Result<ProviderRequest, ProviderError> {
        self.call("exec");
        compute_provider::command_in_directory(
            environment,
            &self.root.join(&environment.provider_session_id),
            command,
        )
    }

    async fn connect(
        &self,
        environment: &SessionEnvironment,
    ) -> Result<ProviderConnection, ProviderError> {
        self.call("connect");
        Ok(ProviderConnection {
            connection: None,
            command: vec![
                "fake-terminal".into(),
                environment.provider_session_id.clone(),
            ],
            credentials: BTreeMap::from([("token".into(), "short-lived-secret".into())]),
            expires_at: None,
        })
    }

    async fn stop(&self, id: &str) -> Result<(), ProviderError> {
        self.call("stop");
        if !self.capabilities.suspend {
            return Err(compute_provider::unsupported("fake", "stop"));
        }
        self.environments
            .lock()
            .unwrap()
            .insert(id.into(), EnvironmentState::Stopped);
        Ok(())
    }

    async fn resume(&self, id: &str) -> Result<(), ProviderError> {
        self.call("resume");
        if !self.capabilities.resume {
            return Err(compute_provider::unsupported("fake", "resume"));
        }
        if self.lose_environment_on_resume.load(Ordering::SeqCst) {
            self.environments.lock().unwrap().remove(id);
            return Err(ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                "the environment was reclaimed by the provider",
            ));
        }
        self.environments
            .lock()
            .unwrap()
            .insert(id.into(), EnvironmentState::Ready);
        Ok(())
    }

    async fn destroy(&self, id: &str) -> Result<(), ProviderError> {
        self.call("destroy");
        if self.fail_destroy.load(Ordering::SeqCst) {
            return Err(ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                "fake teardown failed",
            ));
        }
        self.environments.lock().unwrap().remove(id);
        let _ = std::fs::remove_dir_all(self.root.join(id));
        Ok(())
    }

    async fn claim(&self, _: &str) -> Result<(), ProviderError> {
        self.call("claim");
        if !self.capabilities.claim {
            return Err(compute_provider::unsupported("fake", "claim"));
        }
        Ok(())
    }
}

/// Bearer tokens name principals; operations can be revoked at runtime.
#[derive(Default)]
struct Tokens {
    revoked: Mutex<BTreeSet<(String, String)>>,
}

impl Tokens {
    fn revoke(&self, token: &str, operation: ProviderOperation) {
        self.revoked
            .lock()
            .unwrap()
            .insert((token.into(), format!("{operation:?}")));
    }
}

#[async_trait]
impl ProviderAuthorizer for Tokens {
    async fn authorize(
        &self,
        operation: ProviderOperation,
        authorization: Option<&str>,
    ) -> Result<(), ProviderError> {
        let token = authorization.ok_or_else(|| {
            ProviderError::new(ProviderErrorKind::Unauthorized, "credential required")
        })?;
        if self
            .revoked
            .lock()
            .unwrap()
            .contains(&(token.into(), format!("{operation:?}")))
        {
            return Err(ProviderError::new(
                ProviderErrorKind::Unauthorized,
                format!("{operation:?} is not permitted"),
            ));
        }
        Ok(())
    }
}

/// On-disk state that survives a restart.
struct Stores {
    _root: tempfile::TempDir,
    jobs: PathBuf,
    sessions: PathBuf,
    environments: PathBuf,
}

impl Stores {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let jobs = root.path().join("jobs");
        let sessions = root.path().join("sessions");
        let environments = root.path().join("environments");
        std::fs::create_dir_all(&environments).unwrap();
        Self {
            _root: root,
            jobs,
            sessions,
            environments,
        }
    }
}

/// A server process: its own runtime, killed on `kill`.
struct Server {
    runtime: Option<Runtime>,
    endpoint: String,
}

impl Server {
    fn start(
        stores: &Stores,
        provider: Arc<dyn SessionProvider>,
        authorizer: Arc<dyn ProviderAuthorizer>,
    ) -> Self {
        let socket = StdTcpListener::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", socket.local_addr().unwrap());
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let mut config = ServerConfig::local(endpoint.clone());
        config.job_store = stores.jobs.clone();
        config.session_store = stores.sessions.clone();
        config.session_provider = Some(provider);
        config.authorizer = authorizer;
        config.execution.sessions = true;
        config.session_sweep = Duration::from_millis(50);
        runtime.spawn(async move {
            let listener = tokio::net::TcpListener::from_std(socket).unwrap();
            let _ = compute_provider::serve_listener(listener, config).await;
        });
        Self {
            runtime: Some(runtime),
            endpoint,
        }
    }

    fn client(&self, token: &str) -> RemoteProvider {
        RemoteProvider::new(self.endpoint.clone()).with_bearer_token(token)
    }

    /// Stop the server as a process exit would.
    fn kill(mut self) {
        self.runtime.take().unwrap().shutdown_background();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

fn client_runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

fn request(network: NetworkPolicy, spec: SessionSpec) -> SessionCreateRequest {
    SessionCreateRequest::new(
        &SessionEnvironmentSpec {
            resources: SessionResources {
                cpu_count: Some(1),
                memory_bytes: Some(64 * 1024 * 1024),
                disk_bytes: None,
            },
            network,
            isolation: Default::default(),
            architecture: None,
        },
        spec,
    )
    .unwrap()
}

fn ttl(seconds: u64) -> SessionSpec {
    SessionSpec {
        ttl_seconds: Some(seconds),
        ..Default::default()
    }
}

async fn wait_for(
    client: &RemoteProvider,
    session_id: &str,
    wanted: impl Fn(&ComputeSession) -> bool,
) -> ComputeSession {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        // The server may still be starting after a restart.
        if let Ok(session) = client.session(session_id).await {
            if wanted(&session) {
                return session;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "session never reached the expected state: {session:#?}"
            );
        } else {
            assert!(
                tokio::time::Instant::now() < deadline,
                "server never answered"
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn ready(client: &RemoteProvider, session_id: &str) -> ComputeSession {
    wait_for(client, session_id, |session| {
        session.status == SessionStatus::Ready
    })
    .await
}

/// Run a command and wait for its durable job.
async fn run(
    client: &RemoteProvider,
    session_id: &str,
    command: &[&str],
) -> (compute_core::SessionExecSubmission, compute_core::JobResult) {
    let submission = client
        .session_exec(
            session_id,
            &SessionCommand::new(command.iter().map(|part| part.to_string()).collect()),
        )
        .await
        .unwrap();
    let job_id = submission.job_id.0.clone();
    loop {
        let job = client.job_status(&job_id).await.unwrap();
        if job.status.is_terminal() {
            return (submission, client.job_result(&job_id).await.unwrap());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn session_file(stores: &Stores, session: &ComputeSession) -> PathBuf {
    stores
        .sessions
        .join("sessions")
        .join(&session.session_id.0)
        .join("session.json")
}

#[test]
fn a_session_lives_its_whole_lifecycle_on_any_provider() {
    let stores = Stores::new();
    let fake = Fake::new(&stores.environments, everything());
    let server = Server::start(&stores, fake.clone(), Arc::new(Tokens::default()));
    let client = server.client("alice");
    client_runtime().block_on(async {
        let created = client
            .create_session(&request(NetworkPolicy::Network, ttl(3600)))
            .await
            .unwrap();
        // The durable identities are there before the provider has answered.
        assert!(created.session_id.0.starts_with("ses_"));
        assert!(created.job_id.0.starts_with("job_"));
        assert!(compute_core::is_execution_id(&created.execution_id));
        assert!(created.expires_at.is_some());
        assert_eq!(created.ownership, SessionOwnership::Ephemeral);
        let id = created.session_id.0.clone();

        let session = ready(&client, &id).await;
        assert_eq!(session.provider_kind, "fake");
        assert_eq!(session.capabilities, everything());
        assert_eq!(
            session.connection.as_ref().unwrap().mode,
            SessionConnectionMode::Terminal
        );
        // The readiness execution is the session's own durable job, under
        // the identities returned at creation.
        let readiness = client.job_result(&created.job_id.0).await.unwrap();
        assert_eq!(readiness.status, JobStatus::Succeeded);
        assert_eq!(readiness.result.execution_id, created.execution_id);
        assert_eq!(readiness.result.stdout.text, "compute-session-ready\n");

        let (first, result) = run(&client, &id, &["sh", "-c", "echo hello > note; cat note"]).await;
        assert_eq!(result.status, JobStatus::Succeeded);
        assert_eq!(result.result.stdout.text, "hello\n");
        assert_eq!(result.result.execution_id, first.execution_id);
        let job = client.job_status(&first.job_id.0).await.unwrap();
        assert_eq!(job.session_id.as_ref(), Some(&created.session_id));
        assert_eq!(
            job.execution_id.as_deref(),
            Some(first.execution_id.as_str())
        );
        // Its evidence is a normal, verifiable receipt.
        let receipt = client.job_receipt(&first.job_id.0).await.unwrap();
        receipt.receipt.verify().unwrap();
        assert_eq!(receipt.receipt.execution_id.0, first.execution_id);

        let logs = client.session_logs(&id).await.unwrap();
        assert_eq!(logs.executions.len(), 2);
        assert_eq!(logs.executions[0].purpose, "provision");
        assert_eq!(logs.executions[1].stdout, "hello\n");
        assert_eq!(
            logs.executions[1].command,
            ["sh", "-c", "echo hello > note; cat note"]
        );

        let grant = client.connect_session(&id).await.unwrap();
        assert_eq!(grant.connection.mode, SessionConnectionMode::Terminal);
        assert_eq!(grant.credentials["token"], "short-lived-secret");

        let stopped = client.stop_session(&id).await.unwrap();
        assert_eq!(stopped.status, SessionStatus::Stopped);
        let refused = client
            .session_exec(&id, &SessionCommand::new(vec!["true".into()]))
            .await
            .unwrap_err();
        assert_eq!(refused.kind, ProviderErrorKind::SessionConflict);
        assert!(refused.message.contains("resume"), "{refused:?}");

        let resumed = client.resume_session(&id).await.unwrap();
        assert_eq!(resumed.status, SessionStatus::Ready);
        // The same environment: the file written before the stop is there.
        let (_, again) = run(&client, &id, &["cat", "note"]).await;
        assert_eq!(again.result.stdout.text, "hello\n");
        wait_for(&client, &id, |session| {
            session.status == SessionStatus::Ready && session.active_executions().next().is_none()
        })
        .await;

        let destroyed = client.destroy_session(&id).await.unwrap();
        assert_eq!(destroyed.status, SessionStatus::Destroyed);
        assert!(destroyed.ended_at.is_some());
        let provider_id = destroyed.provider_session_id.clone().unwrap();
        assert_eq!(fake.state(&provider_id), None);
        // The record remains as evidence; destroying again changes nothing.
        let evidence = client.session(&id).await.unwrap();
        assert_eq!(evidence, destroyed);
        assert_eq!(client.destroy_session(&id).await.unwrap(), destroyed);
        assert_eq!(fake.calls("provision"), 1);

        let events = client.session_events(&id).await.unwrap();
        let kinds = events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>();
        for expected in [
            "requested",
            "provisioning",
            "provisioned",
            "ready",
            "connected",
            "stopping",
            "stopped",
            "resuming",
            "resumed",
            "destroying",
            "destroyed",
        ] {
            assert!(kinds.contains(&expected), "missing {expected} in {kinds:?}");
        }
        // Connection material is handed to the caller, never written down.
        let stored = std::fs::read_to_string(session_file(&stores, &destroyed)).unwrap();
        let events = std::fs::read_to_string(
            session_file(&stores, &destroyed).with_file_name("events.json"),
        )
        .unwrap();
        assert!(!stored.contains("short-lived-secret"));
        assert!(!events.contains("short-lived-secret"));
    });
    server.kill();
}

#[test]
fn a_session_is_durable_before_the_provider_answers_and_survives_a_restart_while_provisioning() {
    let stores = Stores::new();
    let gate = Arc::new(Semaphore::new(0));
    let fake = Fake::gated(&stores.environments, gate.clone());
    let tokens: Arc<Tokens> = Arc::new(Tokens::default());
    let client_runtime = client_runtime();

    let server = Server::start(&stores, fake.clone(), tokens.clone());
    let client = server.client("alice");
    let created = client_runtime.block_on(async {
        let created = client
            .create_session(&request(NetworkPolicy::Network, ttl(3600)))
            .await
            .unwrap();
        // Provisioning has started and cannot finish: the session is
        // already durable.
        let pending = wait_for(&client, &created.session_id.0, |session| {
            session.status == SessionStatus::Provisioning
        })
        .await;
        assert!(pending.provider_session_id.is_none());
        let on_disk: ComputeSession =
            serde_json::from_slice(&std::fs::read(session_file(&stores, &created)).unwrap())
                .unwrap();
        assert_eq!(on_disk.session_id, created.session_id);
        assert_eq!(on_disk.job_id, created.job_id);
        created
    });
    server.kill();

    // The provider finishes after Compute went away; a new process picks the
    // session up and completes it under the same identities.
    gate.add_permits(16);
    let server = Server::start(&stores, fake.clone(), tokens);
    let client = server.client("alice");
    client_runtime.block_on(async {
        let session = ready(&client, &created.session_id.0).await;
        assert_eq!(session.session_id, created.session_id);
        assert_eq!(session.job_id, created.job_id);
        assert_eq!(session.execution_id, created.execution_id);
        assert_eq!(session.created_at, created.created_at);
        assert_eq!(session.expires_at, created.expires_at);
        let readiness = client.job_result(&created.job_id.0).await.unwrap();
        assert_eq!(readiness.result.execution_id, created.execution_id);
        let (_, result) = run(&client, &created.session_id.0, &["echo", "after-restart"]).await;
        assert_eq!(result.result.stdout.text, "after-restart\n");
    });
    // Provisioning is idempotent per session: one environment exists.
    assert_eq!(fake.environments.lock().unwrap().len(), 1);
    server.kill();
}

#[test]
fn a_ready_session_and_its_evidence_survive_a_restart() {
    let stores = Stores::new();
    let fake = Fake::new(&stores.environments, everything());
    let tokens: Arc<Tokens> = Arc::new(Tokens::default());
    let client_runtime = client_runtime();

    let server = Server::start(&stores, fake.clone(), tokens.clone());
    let client = server.client("alice");
    let (before, submission) = client_runtime.block_on(async {
        let created = client
            .create_session(&request(NetworkPolicy::Network, ttl(3600)))
            .await
            .unwrap();
        ready(&client, &created.session_id.0).await;
        let (submission, _) = run(
            &client,
            &created.session_id.0,
            &["sh", "-c", "echo kept > state"],
        )
        .await;
        let settled = wait_for(&client, &created.session_id.0, |session| {
            session.status == SessionStatus::Ready && session.active_executions().next().is_none()
        })
        .await;
        (settled, submission)
    });
    server.kill();

    let server = Server::start(&stores, fake.clone(), tokens);
    let client = server.client("alice");
    client_runtime.block_on(async {
        let after = wait_for(&client, &before.session_id.0, |_| true).await;
        assert_eq!(after, before, "a restart does not change the session");
        let listed = client.sessions().await.unwrap();
        assert_eq!(listed, vec![before.clone()]);
        // The execution's durable record, result, and receipt are intact.
        let job = client.job_status(&submission.job_id.0).await.unwrap();
        assert_eq!(job.status, JobStatus::Succeeded);
        client
            .job_receipt(&submission.job_id.0)
            .await
            .unwrap()
            .receipt
            .verify()
            .unwrap();
        let (_, result) = run(&client, &before.session_id.0, &["cat", "state"]).await;
        assert_eq!(result.result.stdout.text, "kept\n");
    });
    assert_eq!(fake.calls("provision"), 1);
    server.kill();
}

#[test]
fn destruction_is_durable_and_a_failed_teardown_is_retried() {
    let stores = Stores::new();
    let fake = Fake::new(&stores.environments, everything());
    let tokens: Arc<Tokens> = Arc::new(Tokens::default());
    let client_runtime = client_runtime();

    let server = Server::start(&stores, fake.clone(), tokens.clone());
    let client = server.client("alice");
    let session_id = client_runtime.block_on(async {
        let created = client
            .create_session(&request(NetworkPolicy::Network, ttl(3600)))
            .await
            .unwrap();
        let id = created.session_id.0.clone();
        let session = ready(&client, &id).await;
        // The provider cannot tear down: the failure is recorded, with its
        // phase, and the session stays `destroying`.
        fake.fail_destroy.store(true, Ordering::SeqCst);
        let error = client.destroy_session(&id).await.unwrap_err();
        assert!(error.message.contains("fake teardown failed"), "{error:?}");
        let pending = client.session(&id).await.unwrap();
        assert_eq!(pending.status, SessionStatus::Destroying);
        let failure = pending.failure.unwrap();
        assert_eq!(failure.phase, SessionPhase::Teardown);
        assert_eq!(failure.provider.as_deref(), Some("fake"));
        assert_eq!(failure.code, "provider_unavailable");
        assert!(failure.retryable);
        let refused = client
            .session_exec(&id, &SessionCommand::new(vec!["true".into()]))
            .await
            .unwrap_err();
        assert_eq!(refused.kind, ProviderErrorKind::SessionConflict);
        assert_eq!(
            fake.state(session.provider_session_id.as_deref().unwrap()),
            Some(EnvironmentState::Ready)
        );
        id
    });
    server.kill();

    // After a restart the teardown is retried until the provider succeeds.
    fake.fail_destroy.store(false, Ordering::SeqCst);
    let server = Server::start(&stores, fake.clone(), tokens.clone());
    let client = server.client("alice");
    let destroyed = client_runtime.block_on(async {
        wait_for(&client, &session_id, |session| {
            session.status == SessionStatus::Destroyed
        })
        .await
    });
    assert!(destroyed.failure.is_none());
    assert_eq!(
        fake.state(destroyed.provider_session_id.as_deref().unwrap()),
        None
    );
    server.kill();

    // And it stays destroyed across another restart.
    let server = Server::start(&stores, fake.clone(), tokens);
    let client = server.client("alice");
    client_runtime.block_on(async {
        let again = wait_for(&client, &session_id, |_| true).await;
        assert_eq!(again, destroyed);
    });
    server.kill();
}

#[test]
fn expired_sessions_are_reconciled_even_across_a_restart() {
    let stores = Stores::new();
    let fake = Fake::new(&stores.environments, everything());
    let tokens: Arc<Tokens> = Arc::new(Tokens::default());
    let client_runtime = client_runtime();

    let server = Server::start(&stores, fake.clone(), tokens.clone());
    let client = server.client("alice");
    let (live, down) = client_runtime.block_on(async {
        let live = client
            .create_session(&request(NetworkPolicy::Network, ttl(1)))
            .await
            .unwrap();
        let expired = wait_for(&client, &live.session_id.0, |session| {
            session.status == SessionStatus::Expired
        })
        .await;
        assert!(expired.ended_at.is_some());
        assert_eq!(
            fake.state(expired.provider_session_id.as_deref().unwrap()),
            None
        );
        let events = client.session_events(&live.session_id.0).await.unwrap();
        let kinds = events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>();
        assert!(kinds.ends_with(&["expiring", "expired"]), "{kinds:?}");

        let down = client
            .create_session(&request(NetworkPolicy::Network, ttl(2)))
            .await
            .unwrap();
        ready(&client, &down.session_id.0).await;
        (expired, down)
    });
    server.kill();
    // The TTL passes while Compute is down; the session is not forgotten.
    std::thread::sleep(Duration::from_millis(2_200));
    let server = Server::start(&stores, fake.clone(), tokens);
    let client = server.client("alice");
    client_runtime.block_on(async {
        let expired = wait_for(&client, &down.session_id.0, |session| {
            session.status == SessionStatus::Expired
        })
        .await;
        assert_eq!(expired.session_id, down.session_id);
        assert_eq!(expired.job_id, down.job_id);
        // Expired sessions reject work.
        let refused = client
            .session_exec(
                &live.session_id.0,
                &SessionCommand::new(vec!["true".into()]),
            )
            .await
            .unwrap_err();
        assert_eq!(refused.kind, ProviderErrorKind::SessionConflict);
    });
    server.kill();
}

#[test]
fn a_claimed_session_does_not_expire_and_expiry_needs_authority() {
    struct NoExpiry(AtomicBool);
    #[async_trait]
    impl ProviderAuthorizer for NoExpiry {
        async fn authorize(
            &self,
            _: ProviderOperation,
            _: Option<&str>,
        ) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn authorize_expiry(&self, _: &str) -> Result<(), ProviderError> {
            if self.0.load(Ordering::SeqCst) {
                Err(ProviderError::new(
                    ProviderErrorKind::Unauthorized,
                    "expiry is not authorized",
                ))
            } else {
                Ok(())
            }
        }
    }
    let stores = Stores::new();
    let fake = Fake::new(&stores.environments, everything());
    let authority = Arc::new(NoExpiry(AtomicBool::new(true)));
    let server = Server::start(&stores, fake.clone(), authority.clone());
    let client = server.client("alice");
    client_runtime().block_on(async {
        let claimed = client
            .create_session(&request(NetworkPolicy::Network, ttl(3)))
            .await
            .unwrap();
        ready(&client, &claimed.session_id.0).await;
        let after = client.claim_session(&claimed.session_id.0).await.unwrap();
        assert_eq!(after.ownership, SessionOwnership::Claimed);
        assert_eq!(
            after.owner, claimed.owner,
            "claiming never changes the owner"
        );
        assert!(after.expires_at.is_none());

        let ephemeral = client
            .create_session(&request(NetworkPolicy::Network, ttl(1)))
            .await
            .unwrap();
        // Without authority, nothing is torn down: the session waits in
        // `expiring` with the refusal recorded.
        let waiting = wait_for(&client, &ephemeral.session_id.0, |session| {
            session.status == SessionStatus::Expiring && session.failure.is_some()
        })
        .await;
        let failure = waiting.failure.unwrap();
        assert_eq!(failure.phase, SessionPhase::Authorization);
        assert_eq!(fake.calls("destroy"), 0);
        authority.0.store(false, Ordering::SeqCst);
        wait_for(&client, &ephemeral.session_id.0, |session| {
            session.status == SessionStatus::Expired
        })
        .await;
        // Well past the TTL it was created with.
        let past = claimed.expires_at.unwrap() + chrono::Duration::milliseconds(500);
        while chrono::Utc::now() < past {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        let kept = client.session(&claimed.session_id.0).await.unwrap();
        assert_eq!(kept.status, SessionStatus::Ready);
    });
    server.kill();
}

#[test]
fn provider_failures_are_the_providers_and_keep_their_evidence() {
    let stores = Stores::new();
    let fake = Fake::new(&stores.environments, everything());
    fake.fail_provision.store(true, Ordering::SeqCst);
    let server = Server::start(&stores, fake.clone(), Arc::new(Tokens::default()));
    let client = server.client("alice");
    client_runtime().block_on(async {
        let created = client
            .create_session(&request(NetworkPolicy::Network, ttl(3600)))
            .await
            .unwrap();
        let failed = wait_for(&client, &created.session_id.0, |session| {
            session.status == SessionStatus::Failed
        })
        .await;
        let SessionFailure {
            phase,
            provider,
            code,
            message,
            retryable,
            ..
        } = failed.failure.clone().unwrap();
        assert_eq!(phase, SessionPhase::Provisioning);
        assert_eq!(provider.as_deref(), Some("fake"));
        assert_eq!(code, "provider_unavailable");
        assert!(message.contains("fake capacity exhausted"));
        assert!(retryable);
        // The record keeps every identity and the lifecycle that led here.
        assert_eq!(failed.job_id, created.job_id);
        assert!(failed.ended_at.is_some());
        let kinds = client
            .session_events(&created.session_id.0)
            .await
            .unwrap()
            .into_iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>();
        assert_eq!(kinds, ["requested", "provisioning", "failed"]);
        // A terminal session stays terminal.
        let refused = client
            .resume_session(&created.session_id.0)
            .await
            .unwrap_err();
        assert_eq!(refused.kind, ProviderErrorKind::SessionConflict);
        assert_eq!(
            client.destroy_session(&created.session_id.0).await.unwrap(),
            failed
        );
    });
    server.kill();
}

#[test]
fn every_operation_is_authorized_and_bound_to_its_owner() {
    let stores = Stores::new();
    let fake = Fake::new(&stores.environments, everything());
    let tokens = Arc::new(Tokens::default());
    let server = Server::start(&stores, fake.clone(), tokens.clone());
    let alice = server.client("alice");
    let mallory = server.client("mallory");
    let anonymous = RemoteProvider::new(server.endpoint.clone());
    client_runtime().block_on(async {
        let denied = anonymous
            .create_session(&request(NetworkPolicy::Network, ttl(3600)))
            .await
            .unwrap_err();
        assert_eq!(denied.kind, ProviderErrorKind::Unauthorized);

        let created = alice
            .create_session(&request(NetworkPolicy::Network, ttl(3600)))
            .await
            .unwrap();
        let id = created.session_id.0.clone();
        ready(&alice, &id).await;
        // Ownership is the authority's principal, not anything the client
        // said.
        assert_eq!(
            created.owner,
            compute_core::sha256_identity(b"Bearer alice")
        );

        // Another principal can neither see nor touch the session.
        assert!(mallory.sessions().await.unwrap().is_empty());
        for error in [
            mallory.session(&id).await.unwrap_err(),
            mallory.connect_session(&id).await.unwrap_err(),
            mallory
                .session_exec(&id, &SessionCommand::new(vec!["true".into()]))
                .await
                .unwrap_err(),
            mallory.session_logs(&id).await.unwrap_err(),
            mallory.stop_session(&id).await.unwrap_err(),
            mallory.destroy_session(&id).await.unwrap_err(),
            mallory.claim_session(&id).await.unwrap_err(),
        ] {
            // Indistinguishable from a session that does not exist.
            assert_eq!(error.kind, ProviderErrorKind::UnknownSession, "{error:?}");
            assert_eq!(error.message, "unknown session");
        }
        // Nor the jobs the session runs.
        let (submission, _) = run(&alice, &id, &["true"]).await;
        assert_eq!(
            mallory
                .job_status(&submission.job_id.0)
                .await
                .unwrap_err()
                .kind,
            ProviderErrorKind::Unauthorized
        );

        // Authority is checked on every operation, not once at creation.
        tokens.revoke("Bearer alice", ProviderOperation::SessionExec);
        let revoked = alice
            .session_exec(&id, &SessionCommand::new(vec!["true".into()]))
            .await
            .unwrap_err();
        assert_eq!(revoked.kind, ProviderErrorKind::Unauthorized);
        tokens.revoke("Bearer alice", ProviderOperation::SessionDestroy);
        assert_eq!(
            alice.destroy_session(&id).await.unwrap_err().kind,
            ProviderErrorKind::Unauthorized
        );
        assert!(alice.session(&id).await.unwrap().status.is_usable());

        // Exposing an endpoint is its own decision; refused, nothing is
        // created.
        tokens.revoke("Bearer alice", ProviderOperation::SessionExpose);
        let before = alice.sessions().await.unwrap().len();
        let exposed = alice
            .create_session(&request(
                NetworkPolicy::Network,
                SessionSpec {
                    endpoints: vec![SessionEndpointRequest {
                        port: 8080,
                        protocol: "http".into(),
                        public: true,
                    }],
                    ..ttl(3600)
                },
            ))
            .await
            .unwrap_err();
        assert_eq!(exposed.kind, ProviderErrorKind::Unauthorized);
        assert_eq!(alice.sessions().await.unwrap().len(), before);
        assert_eq!(fake.calls("provision"), 1);
    });
    server.kill();
}

#[test]
fn policy_admission_decides_before_anything_is_provisioned() {
    let stores = Stores::new();
    let fake = Fake::new(&stores.environments, everything());
    let server = Server::start(&stores, fake.clone(), Arc::new(Tokens::default()));
    let client = server.client("alice");
    client_runtime().block_on(async {
        let mut create = request(NetworkPolicy::Network, ttl(3600));
        create.request.execution.policy =
            Some(Policy::from_json(br#"{"version": 1, "allowed_networks": ["none"]}"#).unwrap());
        let denied = client.create_session(&create).await.unwrap_err();
        assert_eq!(denied.kind, ProviderErrorKind::AdmissionDenied);
        assert_eq!(denied.admission.unwrap().codes(), ["network_denied"]);
        assert!(client.sessions().await.unwrap().is_empty());
        assert_eq!(fake.calls("provision"), 0);
    });
    server.kill();
}

#[test]
fn a_stale_provider_response_cannot_resurrect_a_destroyed_session() {
    let stores = Stores::new();
    let gate = Arc::new(Semaphore::new(0));
    let fake = Fake::gated(&stores.environments, gate.clone());
    let tokens: Arc<Tokens> = Arc::new(Tokens::default());
    let client_runtime = client_runtime();
    let server = Server::start(&stores, fake.clone(), tokens.clone());
    let client = server.client("alice");
    let destroyed = client_runtime.block_on(async {
        let created = client
            .create_session(&request(NetworkPolicy::Network, ttl(3600)))
            .await
            .unwrap();
        let id = created.session_id.0.clone();
        wait_for(&client, &id, |session| {
            session.status == SessionStatus::Provisioning
        })
        .await;
        // Destroyed while the provider is still provisioning.
        let destroyed = client.destroy_session(&id).await.unwrap();
        assert_eq!(destroyed.status, SessionStatus::Destroyed);
        // The provider now answers, late.
        gate.add_permits(16);
        // The late answer is discarded and what it built is torn down.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let kinds = client
                .session_events(&id)
                .await
                .unwrap()
                .into_iter()
                .map(|event| event.event_type)
                .collect::<Vec<_>>();
            if kinds.iter().any(|kind| kind == "orphan_destroyed") {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "orphan never torn down: {kinds:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(fake.calls("destroy"), 1);
        let after = client.session(&id).await.unwrap();
        assert_eq!(after.status, SessionStatus::Destroyed);
        assert!(
            after.provider_session_id.is_none(),
            "the late answer is not adopted"
        );
        assert!(fake.environments.lock().unwrap().is_empty());
        after
    });
    server.kill();
    // A restart does not revive it either.
    let server = Server::start(&stores, fake.clone(), tokens);
    let client = server.client("alice");
    client_runtime.block_on(async {
        let again = wait_for(&client, &destroyed.session_id.0, |_| true).await;
        assert_eq!(again, destroyed);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            client.session(&destroyed.session_id.0).await.unwrap(),
            destroyed
        );
    });
    assert_eq!(fake.calls("provision"), 1);
    server.kill();
}

#[test]
fn a_provider_without_optional_capabilities_is_still_a_complete_provider() {
    let stores = Stores::new();
    let fake = Fake::new(&stores.environments, minimal());
    let server = Server::start(&stores, fake.clone(), Arc::new(Tokens::default()));
    let client = server.client("alice");
    client_runtime().block_on(async {
        // Requirements the provider cannot meet are refused up front.
        for (spec, network) in [
            (
                SessionSpec {
                    required_capabilities: vec!["terminal".into()],
                    ..ttl(60)
                },
                NetworkPolicy::Network,
            ),
            (
                SessionSpec {
                    endpoints: vec![SessionEndpointRequest {
                        port: 80,
                        protocol: "http".into(),
                        public: true,
                    }],
                    ..ttl(60)
                },
                NetworkPolicy::Network,
            ),
        ] {
            let error = client
                .create_session(&request(network, spec))
                .await
                .unwrap_err();
            assert_eq!(
                error.kind,
                ProviderErrorKind::OperationUnsupported,
                "{error:?}"
            );
        }
        let unknown = client
            .create_session(&request(
                NetworkPolicy::Network,
                SessionSpec {
                    required_capabilities: vec!["teleport".into()],
                    ..ttl(60)
                },
            ))
            .await
            .unwrap_err();
        assert_eq!(unknown.kind, ProviderErrorKind::CapabilityMismatch);

        let created = client
            .create_session(&request(NetworkPolicy::Network, ttl(3600)))
            .await
            .unwrap();
        let id = created.session_id.0.clone();
        let session = ready(&client, &id).await;
        assert_eq!(session.capabilities, minimal());
        assert_eq!(
            session.connection.as_ref().unwrap().mode,
            SessionConnectionMode::Exec
        );
        let (_, result) = run(&client, &id, &["echo", "minimal"]).await;
        assert_eq!(result.result.stdout.text, "minimal\n");

        // Stop works everywhere: it stops executions. Without suspend, the
        // provider is not asked to suspend anything.
        let stopped = client.stop_session(&id).await.unwrap();
        assert_eq!(stopped.status, SessionStatus::Stopped);
        assert_eq!(fake.calls("stop"), 0);
        for error in [
            client.resume_session(&id).await.unwrap_err(),
            client.claim_session(&id).await.unwrap_err(),
        ] {
            assert_eq!(error.kind, ProviderErrorKind::OperationUnsupported);
        }
        assert_eq!(fake.calls("resume") + fake.calls("claim"), 0);
        let still = client.session(&id).await.unwrap();
        assert_eq!(still.status, SessionStatus::Stopped);
        assert_eq!(still.ownership, SessionOwnership::Ephemeral);
        let destroyed = client.destroy_session(&id).await.unwrap();
        assert_eq!(destroyed.status, SessionStatus::Destroyed);
    });
    server.kill();
}

#[test]
fn a_provider_that_cannot_resume_fails_explicitly_and_never_substitutes() {
    let stores = Stores::new();
    let fake = Fake::new(&stores.environments, everything());
    let server = Server::start(&stores, fake.clone(), Arc::new(Tokens::default()));
    let client = server.client("alice");
    client_runtime().block_on(async {
        let created = client
            .create_session(&request(NetworkPolicy::Network, ttl(3600)))
            .await
            .unwrap();
        let id = created.session_id.0.clone();
        ready(&client, &id).await;
        client.stop_session(&id).await.unwrap();
        fake.lose_environment_on_resume
            .store(true, Ordering::SeqCst);
        let error = client.resume_session(&id).await.unwrap_err();
        assert!(error.message.contains("reclaimed"), "{error:?}");
        // The environment is gone: the session says so the next time it is
        // inspected, instead of claiming a stopped machine that no longer
        // exists. Nothing is recreated in its place.
        let session = client.session(&id).await.unwrap();
        assert_eq!(session.status, SessionStatus::Failed);
        let failure = session.failure.unwrap();
        assert_eq!(failure.code, "environment_lost");
        assert_eq!(failure.phase, SessionPhase::Reconciliation);
        let events = client.session_events(&id).await.unwrap();
        assert!(
            events.iter().any(|event| event.event_type == "failed"),
            "{events:?}"
        );
        assert_eq!(fake.calls("provision"), 1, "no replacement environment");
    });
    server.kill();
}

#[test]
fn the_workspace_provider_runs_commands_in_a_private_persistent_directory() {
    let stores = Stores::new();
    let provider = Arc::new(compute_provider::WorkspaceSessionProvider::new(
        stores.environments.join("workspaces"),
    ));
    let server = Server::start(&stores, provider, Arc::new(Tokens::default()));
    let client = server.client("alice");
    client_runtime().block_on(async {
        let created = client
            .create_session(&request(NetworkPolicy::Network, ttl(3600)))
            .await
            .unwrap();
        let id = created.session_id.0.clone();
        let session = ready(&client, &id).await;
        assert_eq!(session.provider_kind, "workspace");
        assert!(session.capabilities.network);
        run(
            &client,
            &id,
            &["sh", "-c", "mkdir -p src && echo one > src/a"],
        )
        .await;
        let (_, result) = run(&client, &id, &["sh", "-c", "cat src/a; pwd; echo $HOME"]).await;
        let lines = result.result.stdout.text.lines().collect::<Vec<_>>();
        assert_eq!(lines[0], "one");
        assert_eq!(lines[1], lines[2], "the workspace is the command's home");
        let workspace = PathBuf::from(lines[1]);
        assert!(workspace.join("src/a").is_file());
        let (_, failed) = run(&client, &id, &["sh", "-c", "exit 3"]).await;
        assert_eq!(failed.status, JobStatus::Failed);
        assert_eq!(failed.result.exit_code, Some(3));
        client.destroy_session(&id).await.unwrap();
        assert!(!workspace.exists());
    });
    server.kill();
}

#[test]
fn a_persistent_session_never_expires_and_a_reference_never_makes_a_second_one() {
    let stores = Stores::new();
    let fake = Fake::new(&stores.environments, everything());
    let tokens: Arc<Tokens> = Arc::new(Tokens::default());
    let client_runtime = client_runtime();
    let server = Server::start(&stores, fake.clone(), tokens.clone());
    let client = server.client("alice");
    let keyed = SessionSpec {
        persistent: true,
        reference: Some("cmp_1:1".into()),
        ..Default::default()
    };
    let created = client_runtime.block_on(async {
        let created = client
            .create_session(&request(NetworkPolicy::Network, keyed.clone()))
            .await
            .unwrap();
        assert_eq!(created.ownership, SessionOwnership::Claimed);
        assert!(created.expires_at.is_none() && created.ttl_seconds.is_none());
        assert_eq!(created.reference.as_deref(), Some("cmp_1:1"));
        // Repeating the creation returns the same session.
        let again = client
            .create_session(&request(NetworkPolicy::Network, keyed.clone()))
            .await
            .unwrap();
        assert_eq!(again.session_id, created.session_id);
        // The reference belongs to its owner: another principal gets its own.
        let other = server
            .client("bob")
            .create_session(&request(NetworkPolicy::Network, keyed.clone()))
            .await
            .unwrap();
        assert_ne!(other.session_id, created.session_id);
        ready(&client, &created.session_id.0).await;
        created
    });
    server.kill();
    // After a restart the same reference still finds it.
    let server = Server::start(&stores, fake.clone(), tokens);
    let client = server.client("alice");
    client_runtime.block_on(async {
        wait_for(&client, &created.session_id.0, |_| true).await;
        let again = client
            .create_session(&request(NetworkPolicy::Network, keyed.clone()))
            .await
            .unwrap();
        assert_eq!(again.session_id, created.session_id);
        // A destroyed session frees its reference.
        client.destroy_session(&created.session_id.0).await.unwrap();
        let fresh = client
            .create_session(&request(NetworkPolicy::Network, keyed.clone()))
            .await
            .unwrap();
        assert_ne!(fresh.session_id, created.session_id);
        // A TTL contradicts persistence; a malformed reference is refused.
        for spec in [
            SessionSpec {
                ttl_seconds: Some(60),
                ..keyed.clone()
            },
            SessionSpec {
                reference: Some("has space".into()),
                ..keyed.clone()
            },
        ] {
            let error = client
                .create_session(&request(NetworkPolicy::Network, spec))
                .await
                .unwrap_err();
            assert_eq!(error.kind, ProviderErrorKind::PolicyRejected, "{error:?}");
        }
    });
    server.kill();
    // A provider that cannot keep sessions cannot host a persistent one.
    let stores = Stores::new();
    let server = Server::start(
        &stores,
        Fake::new(&stores.environments, minimal()),
        Arc::new(Tokens::default()),
    );
    client_runtime.block_on(async {
        let error = server
            .client("alice")
            .create_session(&request(NetworkPolicy::Network, keyed))
            .await
            .unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::OperationUnsupported);
    });
    server.kill();
}

/// A Docker-compatible CLI that keeps "containers" as directories, so the
/// container adapter's translation is tested without a container runtime.
fn fake_container_runtime(root: &Path) -> PathBuf {
    let state = root.join("containers");
    std::fs::create_dir_all(&state).unwrap();
    let script = format!(
        r#"#!/bin/sh
state='{state}'
verb="$1"; shift
case "$verb" in
  inspect)
    name="$3"
    if [ -f "$state/$name/status" ]; then cat "$state/$name/status"; else echo "Error: No such object: $name" >&2; exit 1; fi ;;
  run)
    printf '%s\n' "$@" > "$state/last-run"
    while [ $# -gt 0 ]; do
      case "$1" in
        --name) name="$2"; shift 2 ;;
        --volume) volume="${{2%%:*}}"; shift 2 ;;
        --label|--workdir|--cpus|--memory|--network) shift 2 ;;
        --detach) shift ;;
        *) break ;;
      esac
    done
    mkdir -p "$state/$name"; echo running > "$state/$name/status"; echo "$volume" > "$state/$name/volume" ;;
  exec)
    set --  "$@"
    envs=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --workdir) shift 2 ;;
        --env) envs="$envs $2"; shift 2 ;;
        *) break ;;
      esac
    done
    name="$1"; shift
    [ "$(cat "$state/$name/status" 2>/dev/null)" = running ] || {{ echo "container $name is not running" >&2; exit 1; }}
    cd "$(cat "$state/$name/volume")" && exec env $envs "$@" ;;
  stop) echo exited > "$state/$1/status" ;;
  start) [ -d "$state/$1" ] || exit 1; echo running > "$state/$1/status" ;;
  rm) rm -rf "$state/$2" ;;
  *) echo "unknown verb $verb" >&2; exit 2 ;;
esac
"#,
        state = state.display()
    );
    let path = root.join("fake-docker");
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn the_container_adapter_translates_the_session_contract() {
    let stores = Stores::new();
    let runtime = fake_container_runtime(&stores.environments);
    let provider = Arc::new(compute_provider::ContainerSessionProvider::new(
        runtime.display().to_string(),
        "debian:stable-slim",
        stores.environments.join("volumes"),
    ));
    let server = Server::start(&stores, provider, Arc::new(Tokens::default()));
    let client = server.client("alice");
    let state = stores.environments.join("containers");
    client_runtime().block_on(async {
        let created = client
            .create_session(&request(NetworkPolicy::Network, ttl(3600)))
            .await
            .unwrap();
        let id = created.session_id.0.clone();
        let session = ready(&client, &id).await;
        assert_eq!(session.provider_kind, "container");
        let container = session.provider_session_id.clone().unwrap();
        assert!(container.starts_with("compute-"));
        let launched = std::fs::read_to_string(state.join("last-run")).unwrap();
        for expected in [
            "--cpus\n1\n",
            "--memory\n67108864b\n",
            "debian:stable-slim\nsleep\ninfinity",
        ] {
            assert!(
                launched.contains(expected),
                "{expected:?} missing from {launched}"
            );
        }
        // Commands are durable jobs that enter the container.
        let (_, result) = run(
            &client,
            &id,
            &[
                "sh",
                "-c",
                "echo $COMPUTE_SESSION_WORKSPACE > where; echo kept > note",
            ],
        )
        .await;
        assert_eq!(result.status, JobStatus::Succeeded, "{result:?}");
        client.stop_session(&id).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(state.join(&container).join("status"))
                .unwrap()
                .trim(),
            "exited"
        );
        client.resume_session(&id).await.unwrap();
        let (_, again) = run(&client, &id, &["cat", "note", "where"]).await;
        assert_eq!(again.result.stdout.text, "kept\n/workspace\n");
        client.destroy_session(&id).await.unwrap();
        assert!(!state.join(&container).exists());
        // Resuming a container that is gone fails; it is never recreated.
        let provider = compute_provider::ContainerSessionProvider::new(
            runtime.display().to_string(),
            "debian:stable-slim",
            stores.environments.join("volumes"),
        );
        use compute_provider::SessionProvider;
        assert!(provider.resume(&container).await.is_err());
        assert_eq!(
            provider.inspect(&container).await.unwrap(),
            EnvironmentState::Missing
        );
    });
    server.kill();
}

/// A `compute serve` target trusts only the control planes it issued
/// credentials to, owns what they create by control-plane identity, keeps
/// that across a restart, and lets no revoked or unknown credential back
/// in.
#[test]
fn a_target_is_controlled_only_by_the_control_planes_it_trusts() {
    use compute_provider::{TargetAuthorizer, TargetCredentials};
    let stores = Stores::new();
    let trust = stores
        .jobs
        .parent()
        .unwrap()
        .join("target-credentials.json");
    let mut credentials = TargetCredentials::default();
    let (daemon_credential, daemon) = credentials.issue("control-plane-a").unwrap();
    let (_, other) = credentials.issue("control-plane-b").unwrap();
    credentials.save(&trust).unwrap();
    let fake = Fake::new(&stores.environments, everything());
    let server = Server::start(
        &stores,
        fake.clone(),
        Arc::new(TargetAuthorizer::from_file(&trust)),
    );
    let anonymous = RemoteProvider::new(server.endpoint.clone());
    let wrong = server.client("cmpt_tcred_0000000000000000_00");
    let forged = server.client(&forge(&daemon));
    let authenticated = server.client(&daemon);
    let intruder = server.client(&other);
    let id = client_runtime().block_on(async {
        // 1. No credential: rejected, for reads too.
        for error in [
            anonymous.health().await.unwrap_err(),
            anonymous.sessions().await.unwrap_err(),
            anonymous
                .create_session(&request(NetworkPolicy::Network, ttl(3600)))
                .await
                .unwrap_err(),
        ] {
            assert_eq!(error.kind, ProviderErrorKind::Unauthorized, "{error:?}");
        }
        // 2. A wrong or forged credential: rejected.
        for client in [&wrong, &forged] {
            assert_eq!(
                client.sessions().await.unwrap_err().kind,
                ProviderErrorKind::Unauthorized
            );
        }
        // 3. The control plane's credential: accepted, and the target says
        // how it authenticates.
        let capabilities = authenticated.capabilities().await.unwrap();
        assert_eq!(capabilities.authentication.as_deref(), Some("credential"));
        // 4. The authenticated control plane lists, creates, execs in, and
        // owns its sessions, as its identity.
        let created = authenticated
            .create_session(&request(NetworkPolicy::Network, ttl(3600)))
            .await
            .unwrap();
        let id = created.session_id.0.clone();
        assert_eq!(created.owner, "control-plane:control-plane-a");
        ready(&authenticated, &id).await;
        let (submission, result) = run(&authenticated, &id, &["true"]).await;
        assert_eq!(result.status, JobStatus::Succeeded);
        assert_eq!(authenticated.sessions().await.unwrap().len(), 1);
        // 5. Another control plane's valid credential reaches none of it:
        // its sessions are unknown to it, its jobs refused.
        assert!(intruder.sessions().await.unwrap().is_empty());
        for error in [
            intruder.session(&id).await.unwrap_err(),
            intruder
                .session_exec(&id, &SessionCommand::new(vec!["true".into()]))
                .await
                .unwrap_err(),
            intruder.destroy_session(&id).await.unwrap_err(),
        ] {
            assert_eq!(error.kind, ProviderErrorKind::UnknownSession, "{error:?}");
        }
        assert_eq!(
            intruder
                .job_status(&submission.job_id.0)
                .await
                .unwrap_err()
                .kind,
            ProviderErrorKind::Unauthorized
        );
        id
    });
    server.kill();

    // 6. A restart keeps the relationship: the same trust file, the same
    // control plane, the same session.
    let server = Server::start(
        &stores,
        fake.clone(),
        Arc::new(TargetAuthorizer::from_file(&trust)),
    );
    let authenticated = server.client(&daemon);
    let intruder = server.client(&other);
    client_runtime().block_on(async {
        let session = ready(&authenticated, &id).await;
        assert_eq!(session.owner, "control-plane:control-plane-a");
        assert_eq!(
            intruder.session(&id).await.unwrap_err().kind,
            ProviderErrorKind::UnknownSession
        );

        // A rotated credential for the same control plane keeps what it
        // owns; the one it replaced is revoked.
        let mut credentials = TargetCredentials::load(&trust).unwrap();
        let (_, rotated) = credentials.issue("control-plane-a").unwrap();
        credentials
            .revoke(&daemon_credential.credential_id)
            .unwrap();
        credentials.save(&trust).unwrap();
        bump(&trust);
        let rotated = RemoteProvider::new(server.endpoint.clone()).with_bearer_token(&rotated);
        assert_eq!(rotated.session(&id).await.unwrap().session_id.0, id);

        // 7. A revoked credential cannot revive access, nor can one the
        // target no longer lists at all.
        let revoked = authenticated.session(&id).await.unwrap_err();
        assert_eq!(revoked.kind, ProviderErrorKind::Unauthorized);
        assert!(revoked.message.contains("revoked"), "{revoked:?}");
        let mut credentials = TargetCredentials::load(&trust).unwrap();
        credentials
            .credentials
            .retain(|record| record.control_plane != "control-plane-b");
        credentials.save(&trust).unwrap();
        bump(&trust);
        assert_eq!(
            intruder.sessions().await.unwrap_err().kind,
            ProviderErrorKind::Unauthorized
        );
        rotated.destroy_session(&id).await.unwrap();
    });
    server.kill();
}

/// The same credential with a different secret.
fn forge(token: &str) -> String {
    let last = if token.ends_with('0') { '1' } else { '0' };
    format!("{}{last}", &token[..token.len() - 1])
}

/// Make a rewritten trust file visibly newer, whatever the filesystem's
/// timestamp resolution.
fn bump(path: &Path) {
    let file = std::fs::File::options().append(true).open(path).unwrap();
    let modified = file.metadata().unwrap().modified().unwrap();
    file.set_modified(modified + Duration::from_secs(2))
        .unwrap();
}

/// An endpoint that nobody configured an authority for accepts nothing.
#[test]
fn a_server_without_an_authority_fails_closed() {
    let socket = StdTcpListener::bind("127.0.0.1:0").unwrap();
    socket.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", socket.local_addr().unwrap());
    let stores = Stores::new();
    let mut config = ServerConfig::local(endpoint.clone());
    config.job_store = stores.jobs.clone();
    let runtime = client_runtime();
    runtime.spawn(async move {
        let listener = tokio::net::TcpListener::from_std(socket).unwrap();
        let _ = compute_provider::serve_listener(listener, config).await;
    });
    runtime.block_on(async {
        let error = RemoteProvider::new(endpoint.clone())
            .with_bearer_token("anything")
            .health()
            .await
            .unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::Unauthorized);
    });
    runtime.shutdown_background();
}
