//! Rust Chip as a Compute workload.
//!
//! **Two agents, kept apart.** The configured distribution already ships an npm agent (Chip/Eve,
//! with the npm FX) behind `compute-configured-chip`. This crate is something else: the Rust
//! agent runtime from `chip-rs` ("Rust Chip"), with its own Rust FX, its own launcher
//! (`compute-configured-rust-chip`) and its own executable (`compute-rust-chip`). Nothing here
//! calls, replaces or shares a name with the npm agent.
//!
//! **The seam.** Rust Chip publishes a generic environment contract (`chip-core`:
//! `EnvironmentProvider` / `WorkEnvironment`). [`ComputeSessionEnvironments`] implements it with
//! the Compute primitive that already gives one workload an isolated computer: a *session*
//! (ephemeral, owned, placed on a target, a private workspace per session, every command a durable
//! job with a receipt). Rust Chip depends on nothing in Compute; Compute depends only on that
//! generic contract.
//!
//! ```text
//! work A ── acquire ──▶ Compute session A ◀─ exec(argv, env) ─ Rust Chip's worker ─ project A
//! work B ── acquire ──▶ Compute session B ◀─ exec(argv, env) ─ Rust Chip's worker ─ project B
//! ```
//!
//! **What Compute does and does not do.** Compute provides the computer: it creates the session,
//! runs commands in it (`exec(argv, env)`, captured output and a receipt, no stdin) and destroys
//! it. It does not decide what a capability means: Rust Chip validated `project.write`, and the
//! command Compute runs is Rust Chip's own executor (`compute-rust-chip capability-exec`) acting
//! on the project in that session. A Compute receipt says Compute ran a command. It is never a Rust
//! Chip receipt and never establishes that a goal was met.
//!
//! **Isolation.** It is the session's: a private workspace directory per session on the target
//! (or whatever substrate the target's session provider is). Compute documents that the workspace
//! substrate is not a security boundary beyond the isolation profile requested; this crate adds no
//! sandbox of its own and does not claim one.
//!
//! **Project loading.** A session starts empty. The project is loaded by a command in the session
//! (`git clone <source> project`), where `<source>` is operator configuration, never anything the
//! model or the HTTP client supplies. Compute sessions have no project loader of their own (the
//! daemon's environment *repositories* do, but they belong to a persistent environment, not to
//! one ephemeral session).

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use chip_core::{EnvironmentError, EnvironmentId, EnvironmentProvider, WorkEnvironment, WorkId};
use chip_remote_env::{
    CommandOutput, CommandRunner, RemoteEnvironment, RunnerError, WorkerCommand,
};
use compute_core::{
    JobStatus, NetworkPolicy, SessionCommand, SessionResources, SessionSpec, SessionStatus,
};
use compute_provider::{RemoteProvider, SessionCreateRequest, SessionEnvironmentSpec};
use sha2::{Digest, Sha256};

/// Where the project lives inside a session, relative to its workspace.
pub const PROJECT_DIRECTORY: &str = "project";

/// Operator configuration for the Compute-backed environments. None of it comes from a client or a
/// model.
#[derive(Debug, Clone)]
pub struct ComputeSessionConfig {
    /// The `compute serve` target (or daemon `/compute/` endpoint) that hosts the sessions.
    pub endpoint: String,
    pub token: Option<String>,
    /// Where the project is loaded from, as the target sees it (a git URL or a path).
    pub project_source: String,
    /// Rust Chip's executable as the target sees it: it is what runs in each session.
    pub worker_program: String,
    /// Environment for every command in a session (`PATH`, tool locations the target needs).
    pub command_environment: BTreeMap<String, String>,
    /// How many sessions may be owned at once: the isolation capacity this provider declares.
    pub max_environments: usize,
    pub session_ttl: Duration,
    pub resources: SessionResources,
    /// The network the session asks for. `network` is Compute's own session default: a narrower
    /// policy needs a target whose session provider can enforce it, and is refused otherwise.
    pub network: NetworkPolicy,
    /// The longest one command may take.
    pub command_timeout: Duration,
    /// The longest to wait for a session to be ready.
    pub ready_timeout: Duration,
}

impl ComputeSessionConfig {
    pub fn new(
        endpoint: impl Into<String>,
        project_source: impl Into<String>,
        worker_program: impl Into<String>,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            token: None,
            project_source: project_source.into(),
            worker_program: worker_program.into(),
            command_environment: BTreeMap::new(),
            max_environments: 2,
            session_ttl: Duration::from_secs(30 * 60),
            resources: SessionResources {
                cpu_count: Some(1),
                memory_bytes: Some(1 << 30),
                disk_bytes: None,
            },
            network: NetworkPolicy::Network,
            command_timeout: Duration::from_secs(600),
            ready_timeout: Duration::from_secs(60),
        }
    }
}

/// What the provider has measured about acquiring and releasing sessions. Counts and times, nothing
/// derived.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EnvironmentStats {
    pub acquired: u64,
    pub acquire_failed: u64,
    pub acquire_ms_total: f64,
    pub released: u64,
    pub cleanup_failed: u64,
    /// Compute jobs (commands) run in sessions.
    pub commands_run: u64,
}

#[derive(Default)]
struct Counters {
    acquired: AtomicU64,
    acquire_failed: AtomicU64,
    acquire_micros: AtomicU64,
    released: AtomicU64,
    cleanup_failed: AtomicU64,
    commands: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn opaque(session_id: &str) -> EnvironmentId {
    let digest = Sha256::digest(session_id.as_bytes());
    let hex: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
    EnvironmentId::new(format!("env_{hex}"))
}

/// `exec(argv, env)` in one Compute session.
struct SessionRunner {
    remote: Arc<RemoteProvider>,
    config: Arc<ComputeSessionConfig>,
    session_id: String,
    counters: Arc<Counters>,
}

#[async_trait::async_trait]
impl CommandRunner for SessionRunner {
    async fn run(
        &self,
        argv: Vec<String>,
        env: BTreeMap<String, String>,
    ) -> Result<CommandOutput, RunnerError> {
        let fail = |what: &str, e: &dyn std::fmt::Display| RunnerError(format!("{what}: {e}"));
        let mut command = SessionCommand::new(argv);
        command.env = self.config.command_environment.clone();
        command.env.extend(env);
        command.timeout = Some(self.config.command_timeout);
        let submitted = self
            .remote
            .session_exec(&self.session_id, &command)
            .await
            .map_err(|e| fail("compute refused the command", &e.message))?;
        self.counters.commands.fetch_add(1, Ordering::SeqCst);
        let job_id = submitted.job_id.to_string();
        let deadline = Instant::now() + self.config.command_timeout + Duration::from_secs(30);
        loop {
            let job = self
                .remote
                .job_status(&job_id)
                .await
                .map_err(|e| fail("compute could not report the command", &e.message))?;
            match job.status {
                JobStatus::Succeeded | JobStatus::Failed => break,
                JobStatus::Cancelled | JobStatus::TimedOut | JobStatus::Rejected => {
                    return Err(RunnerError(format!(
                        "compute ended the command as {:?}",
                        job.status
                    )));
                }
                _ if Instant::now() > deadline => {
                    return Err(RunnerError("the command did not finish in time".into()));
                }
                _ => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
        let result = self
            .remote
            .job_result(&job_id)
            .await
            .map_err(|e| fail("compute could not return the result", &e.message))?
            .result;
        // A captured output Compute truncated is not the worker's answer: refuse it.
        if result.stdout.truncated {
            return Err(RunnerError(
                "the command's output exceeded what compute captures".into(),
            ));
        }
        Ok(CommandOutput {
            exit_code: result.exit_code,
            stdout: result.stdout.text,
            stderr: result.stderr.text,
        })
    }
}

/// An environment provider that gives each Rust Chip work its own Compute session.
pub struct ComputeSessionEnvironments {
    remote: Arc<RemoteProvider>,
    config: Arc<ComputeSessionConfig>,
    live: Mutex<HashMap<EnvironmentId, String>>,
    counters: Arc<Counters>,
    runtime: tokio::runtime::Handle,
}

impl ComputeSessionEnvironments {
    /// Must be called inside a Tokio runtime: releases run on it.
    pub fn new(config: ComputeSessionConfig) -> Self {
        let mut remote = RemoteProvider::new(config.endpoint.clone())
            .with_request_timeout(Duration::from_secs(30));
        if let Some(token) = &config.token {
            remote = remote.with_bearer_token(token.clone());
        }
        Self {
            remote: Arc::new(remote),
            config: Arc::new(config),
            live: Mutex::new(HashMap::new()),
            counters: Arc::default(),
            runtime: tokio::runtime::Handle::current(),
        }
    }

    pub fn stats(&self) -> EnvironmentStats {
        let c = &self.counters;
        EnvironmentStats {
            acquired: c.acquired.load(Ordering::SeqCst),
            acquire_failed: c.acquire_failed.load(Ordering::SeqCst),
            acquire_ms_total: c.acquire_micros.load(Ordering::SeqCst) as f64 / 1000.0,
            released: c.released.load(Ordering::SeqCst),
            cleanup_failed: c.cleanup_failed.load(Ordering::SeqCst),
            commands_run: c.commands.load(Ordering::SeqCst),
        }
    }

    /// The Compute session behind an environment, for operators and tests. Never given to Chip's
    /// model, and not part of Chip's contract.
    pub fn session_of(&self, environment: &EnvironmentId) -> Option<String> {
        lock(&self.live).get(environment).cloned()
    }

    /// Runs a command in the environment's session, as the environment's own commands run.
    pub fn runner_for(&self, session_id: &str) -> Arc<dyn CommandRunner> {
        Arc::new(SessionRunner {
            remote: self.remote.clone(),
            config: self.config.clone(),
            session_id: session_id.to_string(),
            counters: self.counters.clone(),
        })
    }

    pub fn remote(&self) -> &RemoteProvider {
        &self.remote
    }

    async fn create_session(&self) -> Result<String, String> {
        let environment = SessionEnvironmentSpec {
            resources: self.config.resources.clone(),
            network: self.config.network.clone(),
            ..SessionEnvironmentSpec::default()
        };
        let spec = SessionSpec {
            ttl_seconds: Some(self.config.session_ttl.as_secs().max(1)),
            ..SessionSpec::default()
        };
        let create = SessionCreateRequest::new(&environment, spec).map_err(|e| e.message)?;
        let session = self
            .remote
            .create_session(&create)
            .await
            .map_err(|e| format!("compute could not create a session: {}", e.message))?;
        let id = session.session_id.to_string();
        let deadline = Instant::now() + self.config.ready_timeout;
        loop {
            let current = self
                .remote
                .session(&id)
                .await
                .map_err(|e| format!("compute could not report the session: {}", e.message))?;
            match current.status {
                SessionStatus::Ready | SessionStatus::Running => return Ok(id),
                status if status.is_terminal() => {
                    return Err(format!(
                        "the session ended before it was ready ({status:?})"
                    ));
                }
                _ if Instant::now() > deadline => {
                    self.destroy(&id).await;
                    return Err("the session was not ready in time".into());
                }
                _ => tokio::time::sleep(Duration::from_millis(25)).await,
            }
        }
    }

    /// Destroys a session. A failure is recorded, never raised: the work's result is not rewritten
    /// by cleanup, and the slot is free either way. The session's TTL bounds what a failed
    /// destroy leaves behind.
    async fn destroy(&self, session_id: &str) {
        destroy(&self.remote, &self.counters, session_id).await;
    }

    async fn provision(&self) -> Result<(String, Arc<dyn CommandRunner>), String> {
        let session_id = self.create_session().await?;
        let runner = self.runner_for(&session_id);
        let load = vec![
            "git".to_string(),
            "clone".into(),
            "--quiet".into(),
            "--".into(),
            self.config.project_source.clone(),
            PROJECT_DIRECTORY.into(),
        ];
        let loaded = runner.run(load, BTreeMap::new()).await;
        match loaded {
            Ok(output) if output.exit_code == Some(0) => Ok((session_id, runner)),
            Ok(output) => {
                self.destroy(&session_id).await;
                Err(format!(
                    "the project could not be loaded into the session (exit {:?})",
                    output.exit_code
                ))
            }
            Err(e) => {
                self.destroy(&session_id).await;
                Err(format!(
                    "the project could not be loaded into the session: {e}"
                ))
            }
        }
    }
}

async fn destroy(remote: &RemoteProvider, counters: &Counters, session_id: &str) {
    match remote.destroy_session(session_id).await {
        Ok(_) => {
            counters.released.fetch_add(1, Ordering::SeqCst);
        }
        Err(_) => {
            counters.cleanup_failed.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[async_trait::async_trait]
impl EnvironmentProvider for ComputeSessionEnvironments {
    fn isolation_capacity(&self) -> usize {
        self.config.max_environments
    }

    async fn acquire(&self, _work: &WorkId) -> Result<Arc<dyn WorkEnvironment>, EnvironmentError> {
        let started = Instant::now();
        let outcome = async {
            let (session_id, runner) = self.provision().await?;
            let id = opaque(&session_id);
            let worker = WorkerCommand {
                program: self.config.worker_program.clone(),
                root: PROJECT_DIRECTORY.to_string(),
            };
            match RemoteEnvironment::connect(id.clone(), runner, worker).await {
                Ok(environment) => Ok((id, session_id, environment)),
                Err(e) => {
                    self.destroy(&session_id).await;
                    Err(e.to_string())
                }
            }
        }
        .await;
        self.counters
            .acquire_micros
            .fetch_add(started.elapsed().as_micros() as u64, Ordering::SeqCst);
        match outcome {
            Ok((id, session_id, environment)) => {
                self.counters.acquired.fetch_add(1, Ordering::SeqCst);
                lock(&self.live).insert(id, session_id);
                Ok(Arc::new(environment))
            }
            Err(why) => {
                self.counters.acquire_failed.fetch_add(1, Ordering::SeqCst);
                // Never a fallback: not another session, not the host.
                Err(EnvironmentError::Unavailable(why))
            }
        }
    }

    fn release(&self, _work: &WorkId, environment: &EnvironmentId) {
        let Some(session_id) = lock(&self.live).remove(environment) else {
            return;
        };
        let (remote, counters) = (self.remote.clone(), self.counters.clone());
        self.runtime.spawn(async move {
            destroy(&remote, &counters, &session_id).await;
        });
    }
}
