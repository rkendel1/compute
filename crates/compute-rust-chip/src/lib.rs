//! Rust Chip as a Compute workload.
//!
//! **Two agents, kept apart.** The configured distribution already ships an npm agent (Chip/Eve,
//! with the npm FX) behind `compute-configured-chip`. This crate is something else: the Rust
//! agent runtime from `chip-rs` ("Rust Chip"), with its own Rust FX, its own launcher
//! (`compute-configured-rust-chip`) and its own executable (`compute-rust-chip`). Nothing here
//! calls, replaces or shares a name with the npm agent.
//!
//! **Rust Chip is an agent; Compute hosts agents.** The agent-neutral half (a session per work,
//! project loading, `exec(argv, env)`, receipts, teardown, failure isolation) is the
//! `compute-agent` crate, which knows no agent. This crate is the thin adapter that lets *Rust
//! Chip's* environment contract be served by it, and the `chip` launcher entry (`main.rs`). The
//! adapter carries no Compute logic and no Chip logic: Chip's semantics stay in Chip.
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
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chip_core::{EnvironmentError, EnvironmentId, EnvironmentProvider, WorkEnvironment, WorkId};
use chip_remote_env::{
    CommandOutput, CommandRunner, RemoteEnvironment, RunnerError, WorkerCommand,
};
use compute_agent::{AgentHost, AgentSession, HostConfig};
use compute_core::{NetworkPolicy, SessionResources};
use compute_provider::RemoteProvider;

pub use compute_agent::PROJECT_DIRECTORY;

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
        let defaults = HostConfig::new("");
        Self {
            endpoint: endpoint.into(),
            token: None,
            project_source: project_source.into(),
            worker_program: worker_program.into(),
            command_environment: BTreeMap::new(),
            max_environments: 2,
            session_ttl: defaults.session_ttl,
            resources: defaults.resources,
            network: defaults.network,
            command_timeout: defaults.command_timeout,
            ready_timeout: defaults.ready_timeout,
        }
    }

    fn host(&self) -> HostConfig {
        let mut host = HostConfig::new(self.endpoint.clone());
        host.token = self.token.clone();
        host.project_source = Some(self.project_source.clone());
        host.command_environment = self.command_environment.clone();
        host.session_ttl = self.session_ttl;
        host.resources = self.resources.clone();
        host.network = self.network.clone();
        host.command_timeout = self.command_timeout;
        host.ready_timeout = self.ready_timeout;
        host
    }
}

pub use compute_agent::HostStats as EnvironmentStats;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Rust Chip's `exec(argv, env)`, served by one Compute session.
struct SessionRunner(AgentSession);

#[async_trait::async_trait]
impl CommandRunner for SessionRunner {
    async fn run(
        &self,
        argv: Vec<String>,
        env: BTreeMap<String, String>,
    ) -> Result<CommandOutput, RunnerError> {
        let outcome = self.0.exec(argv, env).await.map_err(|e| RunnerError(e.0))?;
        Ok(CommandOutput {
            exit_code: outcome.exit_code,
            stdout: outcome.stdout,
            stderr: outcome.stderr,
        })
    }
}

/// An environment provider that gives each Rust Chip work its own Compute session.
pub struct ComputeSessionEnvironments {
    host: AgentHost,
    config: ComputeSessionConfig,
    live: Mutex<HashMap<EnvironmentId, AgentSession>>,
    runtime: tokio::runtime::Handle,
}

impl ComputeSessionEnvironments {
    /// Must be called inside a Tokio runtime: releases run on it.
    pub fn new(config: ComputeSessionConfig) -> Self {
        Self {
            host: AgentHost::new(config.host()),
            config,
            live: Mutex::new(HashMap::new()),
            runtime: tokio::runtime::Handle::current(),
        }
    }

    pub fn stats(&self) -> EnvironmentStats {
        self.host.stats()
    }

    /// The Compute session behind an environment, for operators and tests. Never given to Chip's
    /// model, and not part of Chip's contract.
    pub fn session_of(&self, environment: &EnvironmentId) -> Option<String> {
        lock(&self.live)
            .get(environment)
            .map(|s| s.session_id().to_string())
    }

    pub fn remote(&self) -> &RemoteProvider {
        self.host.remote()
    }
}

#[async_trait::async_trait]
impl EnvironmentProvider for ComputeSessionEnvironments {
    fn isolation_capacity(&self) -> usize {
        self.config.max_environments
    }

    async fn acquire(&self, _work: &WorkId) -> Result<Arc<dyn WorkEnvironment>, EnvironmentError> {
        // Never a fallback: not another session, not the host.
        let session = self
            .host
            .acquire()
            .await
            .map_err(|e| EnvironmentError::Unavailable(e.0))?;
        let id = EnvironmentId::new(session.opaque_id());
        let worker = WorkerCommand {
            program: self.config.worker_program.clone(),
            root: PROJECT_DIRECTORY.to_string(),
        };
        let runner = Arc::new(SessionRunner(session.clone()));
        match RemoteEnvironment::connect(id.clone(), runner, worker).await {
            Ok(environment) => {
                lock(&self.live).insert(id, session);
                Ok(Arc::new(environment))
            }
            Err(e) => {
                session.release().await;
                Err(EnvironmentError::Unavailable(e.to_string()))
            }
        }
    }

    fn release(&self, _work: &WorkId, environment: &EnvironmentId) {
        let Some(session) = lock(&self.live).remove(environment) else {
            return;
        };
        self.runtime.spawn(async move {
            session.release().await;
        });
    }
}
