//! Compute sessions: a durable, authorized handle to a temporary execution
//! environment.
//!
//! A session is not a VM. It is the Compute-level record of where work runs
//! (node and provider), how it is reached (connection and endpoints), what it
//! can do (capabilities), who owns it, how long it lives, and what happened to
//! it. Providers implement the environment behind it; they never become the
//! authority for it.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{JobId, JobStatus, NetworkPolicy, ProviderIdentity, ReceiptPlacement};

pub const SESSION_VERSION: &str = "compute.session@1";
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn generate() -> Self {
        let sequence = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
        let seed = format!(
            "session:{}:{}:{}",
            Utc::now().timestamp_nanos_opt().unwrap_or_default(),
            std::process::id(),
            sequence
        );
        Self(format!("ses_{:x}", Sha256::digest(seed.as_bytes())))
    }

    pub fn parse(value: impl Into<String>) -> crate::Result<Self> {
        let value = value.into();
        let valid = value.strip_prefix("ses_").is_some_and(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
        if !valid {
            return Err(crate::ComputeError::InvalidWorkload(
                "malformed session identity".into(),
            ));
        }
        Ok(Self(value))
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The durable session lifecycle. `requested`, `provisioning`, `stopping`,
/// `resuming`, `expiring`, and `destroying` are in-flight states that a
/// restarted server reconciles; `destroyed`, `expired`, and `failed` are
/// terminal and never left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Requested,
    Provisioning,
    /// The environment is up and idle.
    Ready,
    /// The environment is up and at least one execution is active.
    Running,
    Stopping,
    Stopped,
    Resuming,
    Expiring,
    Destroying,
    Destroyed,
    Expired,
    Failed,
}

impl SessionStatus {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Destroyed | Self::Expired | Self::Failed)
    }

    /// Whether the environment accepts executions and connections.
    pub const fn is_usable(self) -> bool {
        matches!(self, Self::Ready | Self::Running)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Provisioning => "provisioning",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Resuming => "resuming",
            Self::Expiring => "expiring",
            Self::Destroying => "destroying",
            Self::Destroyed => "destroyed",
            Self::Expired => "expired",
            Self::Failed => "failed",
        }
    }
}

impl std::fmt::Display for SessionStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What the actual environment supports. Reported by the provider for the
/// environment it runs, never inferred from the provider's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCapabilities {
    pub exec: bool,
    pub terminal: bool,
    pub filesystem: bool,
    pub network: bool,
    pub public_endpoint: bool,
    pub persistent_storage: bool,
    /// The provider can stop the environment's machine and later resume it
    /// with its storage kept: everything running in it is ended on stop. It
    /// does not mean memory or process state is kept (see `memory_suspend`).
    pub suspend: bool,
    pub resume: bool,
    pub claim: bool,
    /// `stop` and `destroy` return only once every process the environment
    /// owns, however deep in its process tree, is confirmed gone; otherwise
    /// they fail with `termination_failed`. A provider that cannot offer the
    /// guarantee says so here, so it can be required (or refused) before
    /// anything is acquired.
    #[serde(default)]
    pub process_tree_termination: bool,
    /// `stop` keeps the machine's memory, processes, and devices, and
    /// `resume` continues them exactly where they were, or fails: a provider
    /// with this capability never substitutes a cold start for a resume.
    /// Written only when true, so readers that predate it still parse every
    /// record that does not depend on it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub memory_suspend: bool,
    /// Machine fork: children continue from the running machine's memory,
    /// processes, devices, and disk captured at one instant, each with its
    /// own copy of everything mutable; the source keeps running. Not a copy
    /// of files: a provider that can only copy a filesystem does not offer
    /// it. Written only when true.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fork: bool,
}

impl SessionCapabilities {
    pub const NAMES: [&'static str; 12] = [
        "exec",
        "terminal",
        "filesystem",
        "network",
        "public_endpoint",
        "persistent_storage",
        "suspend",
        "resume",
        "claim",
        "process_tree_termination",
        "memory_suspend",
        "fork",
    ];

    /// Whether the named capability is present; `None` for an unknown name.
    pub fn get(&self, name: &str) -> Option<bool> {
        Some(match name {
            "exec" => self.exec,
            "terminal" => self.terminal,
            "filesystem" => self.filesystem,
            "network" => self.network,
            "public_endpoint" => self.public_endpoint,
            "persistent_storage" => self.persistent_storage,
            "suspend" => self.suspend,
            "resume" => self.resume,
            "claim" => self.claim,
            "process_tree_termination" => self.process_tree_termination,
            "memory_suspend" => self.memory_suspend,
            "fork" => self.fork,
            _ => return None,
        })
    }

    /// Every named capability, in a stable order.
    pub fn entries(&self) -> Vec<(&'static str, bool)> {
        Self::NAMES
            .iter()
            .map(|name| (*name, self.get(name).expect("known capability")))
            .collect()
    }

    /// Reject names Compute does not define, so a typo is never read as
    /// "not required".
    pub fn validate_names(names: &[String]) -> crate::Result<()> {
        for name in names {
            if !Self::NAMES.contains(&name.as_str()) {
                return Err(crate::ComputeError::InvalidWorkload(format!(
                    "unknown session capability `{name}`: expected one of {}",
                    Self::NAMES.join(", ")
                )));
            }
        }
        Ok(())
    }

    /// Required capabilities this environment lacks.
    pub fn missing(&self, required: &[String]) -> Vec<String> {
        required
            .iter()
            .filter(|name| self.get(name) != Some(true))
            .cloned()
            .collect()
    }
}

/// How a session is reached. SSH is one transport among several; none is the
/// Compute protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionConnectionMode {
    /// Commands through `compute session exec` (the durable job protocol).
    Exec,
    Ssh,
    Websocket,
    Terminal,
    PortForward,
    Appport,
    LocalProcess,
}

impl SessionConnectionMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exec => "exec",
            Self::Ssh => "ssh",
            Self::Websocket => "websocket",
            Self::Terminal => "terminal",
            Self::PortForward => "port_forward",
            Self::Appport => "appport",
            Self::LocalProcess => "local_process",
        }
    }
}

/// The durable, non-secret description of how to reach a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionConnection {
    pub mode: SessionConnectionMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Non-secret connection details (user name, host key fingerprint, ...).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub details: BTreeMap<String, String>,
}

/// What `connect` hands the caller: the connection plus any short-lived
/// material the provider issued for this one connection. It is returned to
/// the authorized caller and never persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionConnectionGrant {
    pub session_id: SessionId,
    pub connection: SessionConnection,
    /// A command that establishes the connection, when one exists.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// Short-lived, connection-scoped credential material. Never stored.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub credentials: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionEndpoint {
    pub id: String,
    pub protocol: String,
    pub address: String,
    pub port: u16,
    pub public: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

/// An endpoint the caller asks the environment to expose. Each one is an
/// explicit authorization decision (`session_expose`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionEndpointRequest {
    pub port: u16,
    #[serde(default = "default_endpoint_protocol")]
    pub protocol: String,
    #[serde(default)]
    pub public: bool,
}

fn default_endpoint_protocol() -> String {
    "http".into()
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionResources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_bytes: Option<u64>,
}

/// Where a lifecycle failure happened. A provider's failure is never reported
/// as Compute's, and a Compute lifecycle state is never a provider error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    Placement,
    Authorization,
    Admission,
    Provisioning,
    Connection,
    Execution,
    Stopping,
    Resuming,
    Claim,
    Teardown,
    Expiration,
    Reconciliation,
}

impl SessionPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Placement => "placement",
            Self::Authorization => "authorization",
            Self::Admission => "admission",
            Self::Provisioning => "provisioning",
            Self::Connection => "connection",
            Self::Execution => "execution",
            Self::Stopping => "stopping",
            Self::Resuming => "resuming",
            Self::Claim => "claim",
            Self::Teardown => "teardown",
            Self::Expiration => "expiration",
            Self::Reconciliation => "reconciliation",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionFailure {
    pub phase: SessionPhase,
    /// The session provider that failed, when a provider did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Stable, machine-readable code (`provider_error`, `admission_denied`,
    /// `environment_lost`, `provider_interrupted`, ...).
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub at: DateTime<Utc>,
}

/// Who holds the session. Ownership is always the authenticated principal
/// that created it; claiming changes how long it lives, never who owns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionOwnership {
    /// Temporary: the session expires at `expires_at`.
    Ephemeral,
    /// Claimed: persistent until explicitly destroyed.
    Claimed,
}

/// One execution run in the session. The job store holds the execution's
/// lifecycle, result, and receipt; the session records the association.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionExecution {
    pub job_id: JobId,
    pub execution_id: String,
    /// `provision` for the readiness execution, `exec` for caller commands.
    pub purpose: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    pub submitted_at: DateTime<Utc>,
    /// Last job status the session observed. The job store is authoritative.
    pub status: JobStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputeSession {
    pub version: String,
    pub session_id: SessionId,
    pub status: SessionStatus,
    /// Pool identifier of the node that runs the session.
    pub node_id: String,
    pub provider: ProviderIdentity,
    /// The session provider's implementation name (descriptive only).
    pub provider_kind: String,
    /// The provider's own handle for the environment. It is never a Compute
    /// identity and never authorizes anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_session_id: Option<String>,
    /// The durable job that provisions the environment.
    pub job_id: JobId,
    /// The execution that proves the environment ran under admission.
    pub execution_id: String,
    /// Owner identity, resolved by the Compute authority at creation. Never
    /// supplied by the client.
    pub owner: String,
    pub ownership: SessionOwnership,
    pub resources: SessionResources,
    pub network: NetworkPolicy,
    pub capabilities: SessionCapabilities,
    /// Capabilities the caller required at creation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_capabilities: Vec<String>,
    /// The caller's key for the session (see [`SessionSpec::reference`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<SessionConnection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<SessionEndpoint>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requested_endpoints: Vec<SessionEndpointRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<ReceiptPlacement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission: Option<crate::ExecutionAdmission>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
    /// Increases with every transition. A provider response is applied only
    /// to the generation it answers, so a stale response cannot change a
    /// session that has moved on (or resurrect a destroyed one).
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<SessionFailure>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub executions: Vec<SessionExecution>,
    /// Where a forked session came from. Absent for one that was created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lineage: Option<SessionLineage>,
}

/// A forked session's origin: the session it was forked from and the
/// machine checkpoint it continues from. Durable with the session, and the
/// checkpoint's evidence also appears in its receipts' substrate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionLineage {
    pub parent_session_id: SessionId,
    /// The checkpoint every child of one fork shares.
    pub checkpoint: String,
    /// What the checkpoint consists of (digests, sizes, backend), once it
    /// exists.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub evidence: BTreeMap<String, String>,
    pub forked_at: DateTime<Utc>,
}

/// A running machine captured at one instant, from which children are
/// provisioned. Immutable; released once no child needs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineCheckpoint {
    pub id: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub evidence: BTreeMap<String, String>,
}

/// A request to fork a running session into `count` children.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionForkRequest {
    pub count: u32,
}

/// The most children one fork may create.
pub const MAX_FORK_CHILDREN: u32 = 16;

/// What a fork did: the checkpoint, and every child it created, each in the
/// state it reached (ready, or failed with its reason). Children the policy
/// refused were not created; `refused` says why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionFork {
    pub source_session_id: SessionId,
    pub checkpoint: MachineCheckpoint,
    pub children: Vec<ComputeSession>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
}

impl ComputeSession {
    /// Executions whose jobs have not reached a terminal state.
    pub fn active_executions(&self) -> impl Iterator<Item = &SessionExecution> {
        self.executions
            .iter()
            .filter(|execution| !execution.status.is_terminal())
    }

    pub fn is_expired_at(&self, now: DateTime<Utc>) -> bool {
        self.ownership == SessionOwnership::Ephemeral
            && self.expires_at.is_some_and(|expires_at| expires_at <= now)
    }
}

/// A caller's request for a session: the environment's execution contract
/// travels as a normal provider request (so placement, policy, and admission
/// evaluate it exactly as they evaluate any workload), plus the session's own
/// lifetime and capability requirements.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<SessionEndpointRequest>,
    /// Keep the session until it is destroyed: created claimed, with no
    /// TTL. Needs the provider's `claim` capability.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub persistent: bool,
    /// The caller's key for this session. Creating a session with a
    /// reference the owner already uses for a live session returns that
    /// session, so a caller that repeats a creation after a restart never
    /// gets a second environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

/// Whether a session reference is well formed: 1-128 letters, digits, and
/// `.:_-`.
pub fn valid_session_reference(reference: &str) -> bool {
    !reference.is_empty()
        && reference.len() <= 128
        && reference
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b':' | b'_' | b'-'))
}

/// A command to run inside a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCommand {
    pub command: Vec<String>,
    /// Target-issued resolution for a runtime-aware Computer process. The
    /// target seals it into the command job's canonical receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<crate::RuntimeResolution>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::duration_option_millis"
    )]
    pub timeout: Option<std::time::Duration>,
    /// Written to the command's standard input, which is then closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
    /// The most bytes kept of each of stdout and stderr; the rest is
    /// dropped and the result says it was truncated. Defaults to
    /// [`SESSION_OUTPUT_BYTES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_bytes: Option<u64>,
    /// Run it on a terminal of this size: its output is the terminal's (one
    /// stream), and input, resizes, and signals may be sent while it runs
    /// (`POST /compute/sessions/{id}/input`). Needs `terminal`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalSize>,
    /// Keep its standard input open after `stdin`, for input sent while it
    /// runs. Needs `terminal` (the provider's interactive channel).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub interactive: bool,
}

/// A terminal's size, in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}

/// Bytes on the wire as base64 text.
pub mod base64_bytes {
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        base64::engine::general_purpose::STANDARD
            .decode(text)
            .map_err(serde::de::Error::custom)
    }
}

/// Input for an interactive execution that is running: any of more bytes,
/// a terminal resize, a signal to its process group, or the end of its
/// input. Delivered to the running command; never starts one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionInput {
    pub job_id: crate::JobId,
    #[serde(default, skip_serializing_if = "Vec::is_empty", with = "base64_bytes")]
    pub data: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resize: Option<TerminalSize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub close: bool,
}

/// The most input one [`SessionInput`] carries.
pub const MAX_SESSION_INPUT_BYTES: usize = 64 * 1024;

/// A read of a job's output from a byte offset: the durable log itself, so
/// a client that reconnects reads from where it was, and nothing is lost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobOutputRequest {
    /// `stdout` or `stderr`.
    pub stream: String,
    pub offset: u64,
    /// Wait up to this long for output past `offset` (bounded by the
    /// server).
    #[serde(default)]
    pub wait_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobOutput {
    pub job_id: crate::JobId,
    pub stream: String,
    /// Where `data` starts: the requested offset, or the log's end if the
    /// request was past it.
    pub offset: u64,
    #[serde(with = "base64_bytes")]
    pub data: Vec<u8>,
    /// Where the next read starts.
    pub next_offset: u64,
    /// The job has ended; with `data` empty, nothing more will come.
    pub complete: bool,
}

/// What a session command keeps of each output stream unless it asks for
/// another bound.
pub const SESSION_OUTPUT_BYTES: u64 = 16 * 1024 * 1024;

impl SessionCommand {
    pub fn new(command: Vec<String>) -> Self {
        Self {
            command,
            runtime: None,
            env: BTreeMap::new(),
            timeout: None,
            stdin: None,
            output_bytes: None,
            terminal: None,
            interactive: false,
        }
    }

    /// The bound kept of each output stream.
    pub fn output_limit(&self) -> u64 {
        self.output_bytes.unwrap_or(SESSION_OUTPUT_BYTES)
    }

    pub fn validate(&self) -> crate::Result<()> {
        let invalid = |message: String| Err(crate::ComputeError::InvalidWorkload(message));
        if self.command.is_empty() || self.command[0].is_empty() {
            return invalid("a session command needs a program".into());
        }
        if self.command.iter().any(|part| part.contains('\0')) {
            return invalid("a session command cannot contain NUL".into());
        }
        for (key, value) in &self.env {
            if key.is_empty() || key.contains('=') || key.contains('\0') || value.contains('\0') {
                return invalid(format!("invalid environment variable {key:?}"));
            }
        }
        if self.output_bytes == Some(0) {
            return invalid("a session command must keep some output".into());
        }
        Ok(())
    }
}

/// The accepted execution of a session command: the same durable identities
/// a remote job reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionExecSubmission {
    pub session_id: SessionId,
    pub job_id: JobId,
    pub execution_id: String,
    pub status: JobStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionExecutionLogs {
    pub job_id: JobId,
    pub execution_id: String,
    pub purpose: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    pub status: JobStatus,
    pub stdout: String,
    pub stderr: String,
    pub complete: bool,
}

/// Session output, read from the durable job store. `environment` is the
/// provider's own environment log, where the provider offers one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionLogs {
    pub session_id: SessionId,
    pub executions: Vec<SessionExecutionLogs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionEvent {
    pub session_id: SessionId,
    pub sequence: u64,
    #[serde(rename = "type")]
    pub event_type: String,
    pub status: SessionStatus,
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub timestamp: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_are_distinct_and_validated() {
        let first = SessionId::generate();
        let second = SessionId::generate();
        assert_ne!(first, second);
        assert_eq!(SessionId::parse(first.0.clone()).unwrap(), first);
        assert!(SessionId::parse("ses_nothex").is_err());
        assert!(SessionId::parse("job_".to_owned() + &"a".repeat(64)).is_err());
        assert!(SessionId::parse("ses_../../etc").is_err());
    }

    #[test]
    fn capability_names_are_checked_not_guessed() {
        let capabilities = SessionCapabilities {
            exec: true,
            resume: false,
            ..Default::default()
        };
        assert_eq!(capabilities.get("exec"), Some(true));
        assert_eq!(capabilities.get("resume"), Some(false));
        assert_eq!(capabilities.get("reusme"), None);
        assert!(SessionCapabilities::validate_names(&["reusme".into()]).is_err());
        assert_eq!(
            capabilities.missing(&["exec".into(), "resume".into()]),
            vec!["resume".to_string()]
        );
    }

    #[test]
    fn terminal_states_are_final() {
        for status in [
            SessionStatus::Destroyed,
            SessionStatus::Expired,
            SessionStatus::Failed,
        ] {
            assert!(status.is_terminal());
            assert!(!status.is_usable());
        }
        assert!(SessionStatus::Ready.is_usable());
        assert!(SessionStatus::Running.is_usable());
        assert!(!SessionStatus::Stopped.is_usable());
    }

    #[test]
    fn commands_are_validated() {
        assert!(SessionCommand::new(vec![]).validate().is_err());
        assert!(SessionCommand::new(vec!["a\0b".into()]).validate().is_err());
        assert!(
            SessionCommand::new(vec!["echo".into(), "hi".into()])
                .validate()
                .is_ok()
        );
    }
}
