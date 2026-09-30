//! Process ownership and confirmed termination, with real processes on the
//! real workspace provider.
//!
//! A command Compute runs in a session carries the session's workspace as its
//! owner marker; so does everything it starts. These tests start trees of
//! real processes (a child, a grandchild, a descendant in its own session)
//! and prove that cancel, stop, and destroy end them all, report a terminal
//! state only once it is true, and are idempotent.
#![cfg(target_os = "linux")]

use std::net::TcpListener as StdTcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use compute_core::{
    ComputeSession, JobStatus, NetworkPolicy, SessionCapabilities, SessionCommand,
    SessionResources, SessionSpec, SessionStatus,
};
use compute_provider::processes::{alive, owned_processes};
use compute_provider::{
    EnvironmentState, ProviderAuthorizer, ProviderConnection, ProviderError, ProviderErrorKind,
    ProviderOperation, ProviderRequest, ProvisionRequest, ProvisionedSession, RemoteProvider,
    ServerConfig, SessionCreateRequest, SessionEnvironment, SessionEnvironmentSpec,
    SessionProvider, WorkspaceSessionProvider,
};
use tokio::runtime::Runtime;

struct Open;

#[async_trait]
impl ProviderAuthorizer for Open {
    async fn authorize(&self, _: ProviderOperation, _: Option<&str>) -> Result<(), ProviderError> {
        Ok(())
    }
}

/// The real workspace provider, with faults a test can turn on: a provider
/// that cannot confirm termination, or cannot remove the machine.
struct Faulty {
    inner: WorkspaceSessionProvider,
    fail_termination: AtomicBool,
    fail_destruction: AtomicBool,
}

#[async_trait]
impl SessionProvider for Faulty {
    fn kind(&self) -> String {
        "workspace".into()
    }
    fn capabilities(&self) -> SessionCapabilities {
        self.inner.capabilities()
    }
    async fn provision(
        &self,
        request: &ProvisionRequest,
    ) -> Result<ProvisionedSession, ProviderError> {
        self.inner.provision(request).await
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
        self.inner.resume(id).await
    }
    async fn claim(&self, id: &str) -> Result<(), ProviderError> {
        self.inner.claim(id).await
    }
    async fn destroy(&self, id: &str) -> Result<(), ProviderError> {
        if self.fail_termination.load(Ordering::SeqCst) {
            return Err(ProviderError::new(
                ProviderErrorKind::TerminationFailed,
                "an injected survivor: 1 process is still alive",
            ));
        }
        if self.fail_destruction.load(Ordering::SeqCst) {
            return Err(ProviderError::new(
                ProviderErrorKind::RemoteExecutionFailure,
                "an injected failure removing the machine",
            ));
        }
        self.inner.destroy(id).await
    }
}

struct Node {
    _root: tempfile::TempDir,
    jobs: PathBuf,
    sessions: PathBuf,
    workspaces: PathBuf,
    provider: Arc<Faulty>,
    runtime: Option<Runtime>,
    endpoint: String,
}

impl Node {
    fn start() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspaces = root.path().join("workspaces");
        std::fs::create_dir_all(&workspaces).unwrap();
        let provider = Arc::new(Faulty {
            inner: WorkspaceSessionProvider::new(&workspaces),
            fail_termination: AtomicBool::new(false),
            fail_destruction: AtomicBool::new(false),
        });
        let mut node = Self {
            jobs: root.path().join("jobs"),
            sessions: root.path().join("sessions"),
            workspaces,
            provider,
            runtime: None,
            endpoint: String::new(),
            _root: root,
        };
        node.serve();
        node
    }

    fn serve(&mut self) {
        let socket = StdTcpListener::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        self.endpoint = format!("http://{}", socket.local_addr().unwrap());
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let mut config = ServerConfig::local(self.endpoint.clone());
        config.job_store = self.jobs.clone();
        config.session_store = self.sessions.clone();
        config.session_provider = Some(self.provider.clone());
        config.authorizer = Arc::new(Open);
        config.execution.sessions = true;
        config.session_sweep = Duration::from_millis(50);
        runtime.spawn(async move {
            let listener = tokio::net::TcpListener::from_std(socket).unwrap();
            let _ = compute_provider::serve_listener(listener, config).await;
        });
        self.runtime = Some(runtime);
    }

    /// The server process exits: its tasks stop; the machines and their
    /// processes, and its stores, remain.
    fn restart(&mut self) {
        self.runtime.take().unwrap().shutdown_background();
        self.serve();
    }

    fn client(&self) -> RemoteProvider {
        RemoteProvider::new(self.endpoint.clone()).with_bearer_token("t")
    }

    fn workspace(&self, session: &ComputeSession) -> PathBuf {
        self.workspaces
            .join(session.provider_session_id.as_ref().expect("provisioned"))
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        // Nothing a test started outlives it.
        for entry in std::fs::read_dir(&self.workspaces)
            .into_iter()
            .flatten()
            .flatten()
        {
            for process in owned_processes(&entry.path()) {
                unsafe { libc::kill(process.pid as i32, libc::SIGKILL) };
            }
        }
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

fn spec() -> SessionCreateRequest {
    SessionCreateRequest::new(
        &SessionEnvironmentSpec {
            resources: SessionResources {
                cpu_count: Some(1),
                memory_bytes: Some(64 << 20),
                disk_bytes: None,
            },
            network: NetworkPolicy::Network,
            isolation: Default::default(),
            architecture: None,
        },
        SessionSpec {
            ttl_seconds: Some(3600),
            ..Default::default()
        },
    )
    .unwrap()
}

async fn wait_for(
    client: &RemoteProvider,
    id: &str,
    wanted: impl Fn(&ComputeSession) -> bool,
) -> ComputeSession {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(session) = client.session(id).await {
            if wanted(&session) {
                return session;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "never reached the state: {session:#?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn session(node: &Node) -> ComputeSession {
    let client = node.client();
    let created = client.create_session(&spec()).await.unwrap();
    wait_for(&client, &created.session_id.0, |session| {
        session.status == SessionStatus::Ready
    })
    .await
}

/// Run a command as a durable job and wait for its terminal state.
async fn run(
    client: &RemoteProvider,
    session: &ComputeSession,
    command: &[&str],
    timeout: Option<Duration>,
) -> compute_core::ExecutionJob {
    let mut command = SessionCommand::new(command.iter().map(|part| part.to_string()).collect());
    command.timeout = timeout;
    let submitted = client
        .session_exec(&session.session_id.0, &command)
        .await
        .unwrap();
    wait_job(client, &submitted.job_id.0).await
}

async fn wait_job(client: &RemoteProvider, job_id: &str) -> compute_core::ExecutionJob {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let job = client.job_status(job_id).await.unwrap();
        if job.status.is_terminal() {
            return job;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the job never ended"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A tree that outlives the command that started it: a child, a grandchild,
/// and a descendant that left its parent's process group and session.
/// Returns once all are running; the command itself has ended.
const DETACHED_TREE: &str = "\
(sleep 300 & (sleep 301 & sleep 302) & sleep 303) >/dev/null 2>&1 &
setsid sleep 304 >/dev/null 2>&1 &
sleep 0.4";

async fn owned_count(workspace: &Path, at_least: usize) -> usize {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let count = owned_processes(workspace).len();
        if count >= at_least || tokio::time::Instant::now() >= deadline {
            return count;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn the_workspace_provider_declares_the_termination_guarantee_it_can_keep() {
    let capabilities = WorkspaceSessionProvider::new("/unused").capabilities();
    assert!(capabilities.process_tree_termination);
    assert_eq!(capabilities.get("process_tree_termination"), Some(true));
}

// ---- completion, failure, timeout, cancellation ------------------------------

#[test]
fn a_command_completes_fails_or_times_out_and_each_says_so() {
    let node = Node::start();
    runtime().block_on(async {
        let session = session(&node).await;
        let client = node.client();
        assert_eq!(
            run(&client, &session, &["sh", "-c", "exit 0"], None)
                .await
                .status,
            JobStatus::Succeeded
        );
        assert_eq!(
            run(&client, &session, &["sh", "-c", "exit 7"], None)
                .await
                .status,
            JobStatus::Failed
        );
        let slow = run(
            &client,
            &session,
            &["sleep", "300"],
            Some(Duration::from_millis(600)),
        )
        .await;
        assert_eq!(slow.status, JobStatus::TimedOut);
        assert!(
            !slow.cancellation.requested,
            "a timeout is not a cancellation"
        );
    });
}

#[test]
fn cancel_is_confirmed_before_it_is_reported_and_is_idempotent() {
    let node = Node::start();
    runtime().block_on(async {
        let session = session(&node).await;
        let client = node.client();
        // A process with a child and a grandchild, all in the job's tree.
        let submitted = client
            .session_exec(
                &session.session_id.0,
                &SessionCommand::new(
                    [
                        "sh",
                        "-c",
                        "sleep 300 & sh -c 'sleep 301 & sleep 302' & wait",
                    ]
                    .map(String::from)
                    .to_vec(),
                ),
            )
            .await
            .unwrap();
        let job_id = submitted.job_id.0.clone();
        assert!(owned_count(&node.workspace(&session), 5).await >= 5);

        // A request is not a cancellation: the job is still running, and
        // nothing says the cancellation took effect.
        let first = client.cancel_job(&job_id).await.unwrap();
        assert!(first.cancellation.requested);
        if !first.status.is_terminal() {
            assert!(
                !first.cancellation.effective,
                "cancel was reported effective before the tree was confirmed gone"
            );
        }
        // Repeating it changes nothing, whether the job has ended or not.
        let second = client.cancel_job(&job_id).await.unwrap();
        let ended = wait_job(&client, &job_id).await;
        assert_eq!(ended.status, JobStatus::Cancelled);
        assert!(ended.cancellation.requested && ended.cancellation.effective);
        assert_eq!(
            ended.cancellation.phase.as_deref(),
            Some("execution_terminated")
        );
        let again = client.cancel_job(&job_id).await.unwrap();
        assert_eq!(again.status, JobStatus::Cancelled);
        assert_eq!(
            again.cancellation, ended.cancellation,
            "a repeat rewrote the record"
        );
        assert_eq!(second.job_id, first.job_id);
        // One terminal result: the same receipt however often it was asked.
        let result = client.job_result(&job_id).await.unwrap();
        assert_eq!(result.status, JobStatus::Cancelled);
        let events = client.job_events(&job_id).await.unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "cancellation_requested")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "terminal")
                .count(),
            1
        );
        // The whole tree ended with the job: nothing is left behind.
        assert_eq!(owned_count(&node.workspace(&session), 0).await, 0);
    });
}

#[test]
fn cancelling_a_finished_job_leaves_its_result_alone() {
    let node = Node::start();
    runtime().block_on(async {
        let session = session(&node).await;
        let client = node.client();
        let done = run(&client, &session, &["sh", "-c", "exit 0"], None).await;
        let after = client.cancel_job(&done.job_id.0).await.unwrap();
        assert_eq!(after.status, JobStatus::Succeeded);
        assert!(
            !after.cancellation.requested,
            "a finished job was marked cancelled"
        );
        assert_eq!(after, client.job_status(&done.job_id.0).await.unwrap());
    });
}

// ---- stop and destroy ------------------------------------------------------

#[test]
fn destroy_ends_a_detached_tree_before_it_removes_the_workspace() {
    let node = Node::start();
    runtime().block_on(async {
        let session = session(&node).await;
        let client = node.client();
        let workspace = node.workspace(&session);
        assert_eq!(
            run(&client, &session, &["sh", "-c", DETACHED_TREE], None)
                .await
                .status,
            JobStatus::Succeeded
        );
        // The command ended; what it started did not, and it is owned.
        assert!(
            owned_count(&workspace, 5).await >= 5,
            "{:?}",
            owned_processes(&workspace)
        );
        let pids = owned_processes(&workspace);

        let destroyed = client.destroy_session(&session.session_id.0).await.unwrap();
        assert_eq!(destroyed.status, SessionStatus::Destroyed);
        assert!(!workspace.exists());
        assert!(
            pids.iter().all(|process| !alive(process.pid)),
            "an orphan survived"
        );
        // A repeat is a success that changes nothing.
        let again = client.destroy_session(&session.session_id.0).await.unwrap();
        assert_eq!(again.status, SessionStatus::Destroyed);
    });
}

#[test]
fn stop_ends_processes_and_keeps_state_and_resume_starts_none() {
    let node = Node::start();
    runtime().block_on(async {
        let session = session(&node).await;
        let client = node.client();
        let workspace = node.workspace(&session);
        run(
            &client,
            &session,
            &["sh", "-c", "echo durable > kept.txt"],
            None,
        )
        .await;
        run(&client, &session, &["sh", "-c", DETACHED_TREE], None).await;
        assert!(owned_count(&workspace, 5).await >= 5);

        let stopped = client.stop_session(&session.session_id.0).await.unwrap();
        assert_eq!(stopped.status, SessionStatus::Stopped);
        assert!(
            owned_processes(&workspace).is_empty(),
            "a stopped session has processes"
        );
        // State survives the stop; transient execution does not.
        assert_eq!(
            std::fs::read_to_string(workspace.join("kept.txt")).unwrap(),
            "durable\n"
        );

        let resumed = client.resume_session(&session.session_id.0).await.unwrap();
        assert_eq!(resumed.status, SessionStatus::Ready);
        assert!(
            owned_processes(&workspace).is_empty(),
            "resume brought processes back"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("kept.txt")).unwrap(),
            "durable\n"
        );
        let out = run(&client, &session, &["sh", "-c", "cat kept.txt"], None).await;
        assert_eq!(out.status, JobStatus::Succeeded);
    });
}

// ---- termination that cannot be confirmed -----------------------------------

#[test]
fn a_destroy_that_cannot_confirm_termination_is_not_reported_destroyed() {
    let node = Node::start();
    runtime().block_on(async {
        let session = session(&node).await;
        let client = node.client();
        node.provider.fail_termination.store(true, Ordering::SeqCst);
        let error = client
            .destroy_session(&session.session_id.0)
            .await
            .expect_err("the destroy was reported successful");
        assert_eq!(error.kind, ProviderErrorKind::TerminationFailed);
        // The record says exactly what is true: still being destroyed, and why.
        let held = client.session(&session.session_id.0).await.unwrap();
        assert_eq!(held.status, SessionStatus::Destroying);
        assert_eq!(held.failure.as_ref().unwrap().code, "termination_failed");
        assert!(node.workspace(&session).exists());

        // When the provider can confirm, the same destroy completes.
        node.provider
            .fail_termination
            .store(false, Ordering::SeqCst);
        let done = wait_for(&client, &session.session_id.0, |session| {
            session.status == SessionStatus::Destroyed
        })
        .await;
        assert!(done.failure.is_none());
        assert!(!node.workspace(&session).exists());
    });
}

#[test]
fn an_interrupted_destroy_is_not_lost_by_a_restart_and_never_falsely_finished() {
    let mut node = Node::start();
    runtime().block_on(async {
        let session = session(&node).await;
        let workspace = node.workspace(&session);
        run(&node.client(), &session, &["sh", "-c", DETACHED_TREE], None).await;
        assert!(owned_count(&workspace, 5).await >= 5);
        node.provider.fail_destruction.store(true, Ordering::SeqCst);
        let error = node
            .client()
            .destroy_session(&session.session_id.0)
            .await
            .expect_err("reported destroyed");
        assert_ne!(error.kind, ProviderErrorKind::TerminationFailed);

        // The server exits mid-destroy and comes back: the destroy is still
        // owed, the workspace is still there, and nothing was claimed gone.
        node.restart();
        let client = node.client();
        let held = wait_for(&client, &session.session_id.0, |_| true).await;
        assert!(!held.status.is_terminal(), "{:?}", held.status);
        assert!(workspace.exists());

        // The provider recovers; the restarted server finishes the job, and
        // only then is anything reported gone.
        node.provider
            .fail_destruction
            .store(false, Ordering::SeqCst);
        let done = wait_for(&client, &session.session_id.0, |session| {
            session.status == SessionStatus::Destroyed
        })
        .await;
        assert!(done.ended_at.is_some());
        assert!(!workspace.exists());
        assert!(owned_processes(&workspace).is_empty());
    });
}

#[test]
fn a_cancel_interrupted_by_a_restart_never_reports_a_cancellation_that_did_not_happen() {
    let mut node = Node::start();
    runtime().block_on(async {
        let session = session(&node).await;
        let workspace = node.workspace(&session);
        let submitted = node
            .client()
            .session_exec(
                &session.session_id.0,
                &SessionCommand::new(["sleep", "300"].map(String::from).to_vec()),
            )
            .await
            .unwrap();
        assert!(owned_count(&workspace, 1).await >= 1);
        node.client().cancel_job(&submitted.job_id.0).await.unwrap();
        node.restart();
        let job = wait_job(&node.client(), &submitted.job_id.0).await;
        // Whatever the recorded end is, it is a recorded end: either the
        // cancellation was confirmed, or the interruption is named. It is
        // never `succeeded`, and `effective` is only ever true with `cancelled`.
        assert_ne!(job.status, JobStatus::Succeeded);
        if job.cancellation.effective {
            assert_eq!(job.status, JobStatus::Cancelled);
        }
        // Destroying the session ends what is left, whichever way it ended.
        let destroyed = node
            .client()
            .destroy_session(&session.session_id.0)
            .await
            .unwrap();
        assert_eq!(destroyed.status, SessionStatus::Destroyed);
        assert!(owned_processes(&workspace).is_empty());
    });
}

// ---- the primitive itself -----------------------------------------------------

fn marked(workspace: &Path, script: &str) -> std::process::Child {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .env(compute_provider::processes::OWNER_MARKER, workspace)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap()
}

#[test]
fn only_the_workspaces_own_processes_are_owned_and_terminated() {
    runtime().block_on(async {
        let workspace = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        // parent -> child -> grandchild, plus a descendant in its own session.
        let mut parent = marked(
            workspace.path(),
            "sh -c 'sh -c \"sleep 300\" & sleep 300' & (setsid sleep 300 &) ; sleep 300",
        );
        let mut stranger = marked(other.path(), "sleep 300");
        assert!(owned_count(workspace.path(), 5).await >= 5);

        let count = compute_provider::processes::terminate_owned(workspace.path())
            .await
            .unwrap();
        assert!(count >= 5, "{count}");
        assert!(owned_processes(workspace.path()).is_empty());
        let _ = parent.wait();
        // Another workspace's process is not this one's.
        assert!(alive(stranger.id()));
        let _ = stranger.kill();
        let _ = stranger.wait();
        // Terminating again is a no-op that still succeeds.
        assert_eq!(
            compute_provider::processes::terminate_owned(workspace.path())
                .await
                .unwrap(),
            0
        );
    });
}
