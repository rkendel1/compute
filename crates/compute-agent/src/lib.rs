//! Agent as a Compute workload.
//!
//! **Compute provides the computer. Agents provide the agency.**
//!
//! An agent is something Compute can launch against a Compute environment: an identity, an
//! executable, arguments and an environment ([`AgentSpec`]). Everything above the execution
//! boundary (reasoning, goals, capabilities, memory, models, recovery, what "done" means) belongs
//! to the agent. Everything below it belongs to Compute: a session (an isolated, owned, ephemeral
//! workspace on a target), the project loaded into it, `exec(argv, env)` with captured output and
//! a durable receipt, and confirmed teardown.
//!
//! ```text
//!   Agent (any runtime)        ── owns intelligence and agency
//!        │ exec(argv, env)     ── the only thing it asks of Compute; no stdin
//!   AgentSession               ── owns the computer: one session, one project, one cleanup
//! ```
//!
//! This crate knows no agent. It has no dependency on Rust Chip, Chip/Eve, or any provider SDK,
//! and no code in it can tell which agent it is hosting. What Compute returns ([`Outcome`]) is
//! what happened in the environment: an exit code, captured output and the receipt's job id. It
//! is never read as the agent's success, a capability's success, or a goal's satisfaction; only
//! the agent can say that.
//!
//! **Isolation** is the session's: one agent work gets one session, so two works never share a
//! mutable project even when they start from the same source. Compute documents that the workspace
//! substrate is not a security boundary beyond the isolation profile requested; this crate adds no
//! sandbox and claims none.
//!
//! **Failure.** Acquisition fails closed: a session that was created but not made usable
//! (not ready, project not loaded) is destroyed and no agent runs. A failed destroy is counted in
//! [`HostStats::cleanup_failed`] and never raised; the TTL bounds what it leaves behind.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use compute_core::{
    JobStatus, NetworkPolicy, SessionCommand, SessionResources, SessionSpec, SessionStatus,
};
use compute_provider::{RemoteProvider, SessionCreateRequest, SessionEnvironmentSpec};
use sha2::{Digest, Sha256};

/// The agent a distribution launches when none is named.
pub const DEFAULT_AGENT: &str = "chip";

/// Where a loaded project lives inside a session, relative to its workspace.
pub const PROJECT_DIRECTORY: &str = "project";

/// What Compute needs to launch an agent: nothing about how it thinks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSpec {
    /// Identity: a short, stable name (`chip`, ...). Compute only carries it.
    pub name: String,
    /// The executable, as the target sees it.
    pub program: String,
    pub args: Vec<String>,
    /// Environment for the launch, on top of the host's command environment.
    pub env: BTreeMap<String, String>,
}

impl AgentSpec {
    pub fn new(name: impl Into<String>, program: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    pub fn argv(&self) -> Vec<String> {
        std::iter::once(self.program.clone())
            .chain(self.args.iter().cloned())
            .collect()
    }
}

/// Operator configuration for hosting agents. None of it comes from an agent, a model or a client.
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// The `compute serve` target (or daemon `/compute/` endpoint) that hosts the sessions.
    pub endpoint: String,
    pub token: Option<String>,
    /// Where the project is loaded from, as the target sees it (a git URL or a path). `None`
    /// leaves the session empty.
    pub project_source: Option<String>,
    /// Environment for every command in a session (`PATH`, tool locations the target needs).
    pub command_environment: BTreeMap<String, String>,
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

impl HostConfig {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            token: None,
            project_source: None,
            command_environment: BTreeMap::new(),
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

/// Counts and times, nothing derived.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostStats {
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

/// Why Compute could not give an agent a computer, or could not run a command in it. Never about
/// what the agent did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputeError(pub String);

impl fmt::Display for ComputeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ComputeError {}

/// What happened when a command ran in a session: reality, not a verdict. `job_id` is the durable
/// Compute receipt's job; it says Compute ran the command and nothing about any goal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub job_id: String,
}

/// Hosts agents on a Compute target: one [`AgentSession`] per agent work.
#[derive(Clone)]
pub struct AgentHost {
    remote: Arc<RemoteProvider>,
    config: Arc<HostConfig>,
    counters: Arc<Counters>,
}

impl AgentHost {
    pub fn new(config: HostConfig) -> Self {
        let mut remote = RemoteProvider::new(config.endpoint.clone())
            .with_request_timeout(Duration::from_secs(30));
        if let Some(token) = &config.token {
            remote = remote.with_bearer_token(token.clone());
        }
        Self {
            remote: Arc::new(remote),
            config: Arc::new(config),
            counters: Arc::default(),
        }
    }

    pub fn stats(&self) -> HostStats {
        let c = &self.counters;
        HostStats {
            acquired: c.acquired.load(Ordering::SeqCst),
            acquire_failed: c.acquire_failed.load(Ordering::SeqCst),
            acquire_ms_total: c.acquire_micros.load(Ordering::SeqCst) as f64 / 1000.0,
            released: c.released.load(Ordering::SeqCst),
            cleanup_failed: c.cleanup_failed.load(Ordering::SeqCst),
            commands_run: c.commands.load(Ordering::SeqCst),
        }
    }

    /// The Compute client, for operators and tests.
    pub fn remote(&self) -> &RemoteProvider {
        &self.remote
    }

    /// Creates a session, waits until it is ready, and loads the project into it. Fails closed:
    /// whatever was created and not made usable is destroyed, and nothing runs.
    pub async fn acquire(&self) -> Result<AgentSession, ComputeError> {
        let started = Instant::now();
        let outcome = self.provision().await;
        self.counters
            .acquire_micros
            .fetch_add(started.elapsed().as_micros() as u64, Ordering::SeqCst);
        match outcome {
            Ok(session) => {
                self.counters.acquired.fetch_add(1, Ordering::SeqCst);
                Ok(session)
            }
            Err(e) => {
                self.counters.acquire_failed.fetch_add(1, Ordering::SeqCst);
                Err(e)
            }
        }
    }

    async fn provision(&self) -> Result<AgentSession, ComputeError> {
        let session_id = self.create_session().await?;
        let session = AgentSession {
            host: self.clone(),
            session_id,
        };
        if let Some(source) = &self.config.project_source {
            let load = vec![
                "git".to_string(),
                "clone".into(),
                "--quiet".into(),
                "--".into(),
                source.clone(),
                PROJECT_DIRECTORY.into(),
            ];
            match session.exec(load, BTreeMap::new()).await {
                Ok(o) if o.exit_code == Some(0) => {}
                Ok(o) => {
                    session.release().await;
                    return Err(ComputeError(format!(
                        "the project could not be loaded into the session (exit {:?})",
                        o.exit_code
                    )));
                }
                Err(e) => {
                    session.release().await;
                    return Err(ComputeError(format!(
                        "the project could not be loaded into the session: {e}"
                    )));
                }
            }
        }
        Ok(session)
    }

    async fn create_session(&self) -> Result<String, ComputeError> {
        let environment = SessionEnvironmentSpec {
            resources: self.config.resources.clone(),
            network: self.config.network.clone(),
            ..SessionEnvironmentSpec::default()
        };
        let spec = SessionSpec {
            ttl_seconds: Some(self.config.session_ttl.as_secs().max(1)),
            ..SessionSpec::default()
        };
        let create =
            SessionCreateRequest::new(&environment, spec).map_err(|e| ComputeError(e.message))?;
        let session = self.remote.create_session(&create).await.map_err(|e| {
            ComputeError(format!("compute could not create a session: {}", e.message))
        })?;
        let id = session.session_id.to_string();
        let deadline = Instant::now() + self.config.ready_timeout;
        loop {
            let current = match self.remote.session(&id).await {
                Ok(current) => current,
                Err(e) => {
                    self.destroy(&id).await;
                    return Err(ComputeError(format!(
                        "compute could not report the session: {}",
                        e.message
                    )));
                }
            };
            match current.status {
                SessionStatus::Ready | SessionStatus::Running => return Ok(id),
                status if status.is_terminal() => {
                    return Err(ComputeError(format!(
                        "the session ended before it was ready ({status:?})"
                    )));
                }
                _ if Instant::now() > deadline => {
                    self.destroy(&id).await;
                    return Err(ComputeError("the session was not ready in time".into()));
                }
                _ => tokio::time::sleep(Duration::from_millis(25)).await,
            }
        }
    }

    /// A failure is recorded, never raised: cleanup does not rewrite a work's result, and the
    /// session's TTL bounds what a failed destroy leaves behind.
    async fn destroy(&self, session_id: &str) {
        match self.remote.destroy_session(session_id).await {
            Ok(_) => self.counters.released.fetch_add(1, Ordering::SeqCst),
            Err(_) => self.counters.cleanup_failed.fetch_add(1, Ordering::SeqCst),
        };
    }
}

/// One agent work's computer. Cloneable handle; release it once.
#[derive(Clone)]
pub struct AgentSession {
    host: AgentHost,
    session_id: String,
}

impl AgentSession {
    /// Compute's own session id: for operators and tests. Never given to an agent.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// An identity safe to show an agent: a hash of the session id, never the id, a path or a host.
    pub fn opaque_id(&self) -> String {
        let digest = Sha256::digest(self.session_id.as_bytes());
        let hex: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
        format!("env_{hex}")
    }

    /// Launches the agent in this session: `exec(program + args, env)`.
    pub async fn launch(&self, agent: &AgentSpec) -> Result<Outcome, ComputeError> {
        self.exec(agent.argv(), agent.env.clone()).await
    }

    /// `exec(argv, env)` in this session: a durable job with captured output and a receipt. A
    /// non-zero exit is an [`Outcome`], not an error; Compute does not judge it.
    pub async fn exec(
        &self,
        argv: Vec<String>,
        env: BTreeMap<String, String>,
    ) -> Result<Outcome, ComputeError> {
        let (remote, config) = (&self.host.remote, &self.host.config);
        let fail = |what: &str, e: &dyn fmt::Display| ComputeError(format!("{what}: {e}"));
        let mut command = SessionCommand::new(argv);
        command.env = config.command_environment.clone();
        command.env.extend(env);
        command.timeout = Some(config.command_timeout);
        let submitted = remote
            .session_exec(&self.session_id, &command)
            .await
            .map_err(|e| fail("compute refused the command", &e.message))?;
        self.host.counters.commands.fetch_add(1, Ordering::SeqCst);
        let job_id = submitted.job_id.to_string();
        let deadline = Instant::now() + config.command_timeout + Duration::from_secs(30);
        loop {
            let job = remote
                .job_status(&job_id)
                .await
                .map_err(|e| fail("compute could not report the command", &e.message))?;
            match job.status {
                JobStatus::Succeeded | JobStatus::Failed => break,
                JobStatus::Cancelled | JobStatus::TimedOut | JobStatus::Rejected => {
                    return Err(ComputeError(format!(
                        "compute ended the command as {:?}",
                        job.status
                    )));
                }
                _ if Instant::now() > deadline => {
                    return Err(ComputeError("the command did not finish in time".into()));
                }
                _ => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
        let result = remote
            .job_result(&job_id)
            .await
            .map_err(|e| fail("compute could not return the result", &e.message))?
            .result;
        // Output Compute truncated is not the agent's answer: refuse it.
        if result.stdout.truncated {
            return Err(ComputeError(
                "the command's output exceeded what compute captures".into(),
            ));
        }
        Ok(Outcome {
            exit_code: result.exit_code,
            stdout: result.stdout.text,
            stderr: result.stderr.text,
            job_id,
        })
    }

    /// Destroys the session (confirmed teardown; the record stays as evidence).
    pub async fn release(&self) {
        self.host.destroy(&self.session_id).await;
    }
}
