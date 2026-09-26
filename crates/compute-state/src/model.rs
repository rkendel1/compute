//! The durable Compute control model, `compute.state@1`.
//!
//! These records are desired state and evidence. Live process state (a
//! service's PID, its current health) is owned by the daemon and is never
//! written here; lifecycle events and execution records are.
//!
//! ```text
//! Project ── ProjectRevision (immutable content)
//!    │
//!    └── EnvironmentProject (membership: project in an environment)
//!           ├── Deployment (a revision deployed there, with its evidence)
//!           └── Workload   (a service or task, with its port bindings)
//!                 └── Execution ── Receipt (reference)
//! Environment, Provider, Service, Event
//! ```

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::store::Collection;

pub const STATE_VERSION: &str = "compute.state@1";

/// The generation of the control model within `compute.state@1`. Each
/// generation only adds (collections, optional fields, indexes), so a
/// controller of an earlier generation keeps working on a later model.
///
/// - 1: the hardening model (optional `OperatorCredential`, `Audit`,
///   `Execution.failure`).
/// - 2: FeltDB 0.11.8 consumption: an indexed `record_id` identity on
///   every collection, and indexes for the filters Compute's views use
///   (`Execution.project_id`, `Receipt.project_id`, `Event.environment`,
///   `Event.project`, `Event.deployment_id`).
pub const MODEL_GENERATION: u32 = 3;

/// A typed document of one collection.
pub trait Document: Serialize + DeserializeOwned + Clone + Send + Sync {
    const COLLECTION: Collection;
}

macro_rules! document {
    ($type:ty, $collection:ident) => {
        impl Document for $type {
            const COLLECTION: Collection = Collection::$collection;
        }
    };
}

/// What the operator wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DesiredState {
    #[default]
    Running,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadKind {
    /// Long-lived: an API, a worker, a web server. Runs until stopped.
    Service,
    /// Runs to completion on request: build, test, migration, lint.
    Task,
}

impl WorkloadKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::Task => "task",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    /// A service that exits stays down until explicitly started.
    Never,
    /// A service that fails or is killed is started again, with backoff.
    #[default]
    OnFailure,
}

/// Readiness: what proves a new service instance can take traffic.
/// Process existence alone is never enough for a service with ports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Readiness {
    pub check: ReadinessCheck,
    /// The declared port to check. Defaults to the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
    /// For `http`: the path to GET; 2xx and 3xx are ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// For `task`: a task of the same revision that exits 0 when ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// Give up after this long; the deployment fails and what serves keeps
    /// serving.
    #[serde(default = "Readiness::default_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "Readiness::default_interval")]
    pub interval_ms: u64,
}

impl Readiness {
    fn default_timeout() -> u64 {
        60_000
    }

    fn default_interval() -> u64 {
        250
    }

    /// The default: accepting connections on the first port for a service
    /// with ports, otherwise staying alive briefly.
    pub fn default_for(ports: &[PortSpec]) -> Self {
        Self {
            check: if ports.is_empty() {
                ReadinessCheck::Process
            } else {
                ReadinessCheck::Port
            },
            port: None,
            path: None,
            task: None,
            timeout_ms: Self::default_timeout(),
            interval_ms: Self::default_interval(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessCheck {
    /// The process is alive and stays alive for a second.
    Process,
    /// The port accepts TCP connections.
    Port,
    /// `GET path` on the port answers 2xx or 3xx.
    Http,
    /// A task of the same revision exits 0.
    Task,
}

/// A logical port a project declares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortSpec {
    pub name: String,
    pub port: u16,
}

/// A logical port and the host port the environment bound it to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortBinding {
    pub name: String,
    /// The port the project declares.
    pub logical: u16,
    /// The host port the environment layer bound it to.
    pub host: u16,
}

/// Software, independent of where it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRecord {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub created_at: DateTime<Utc>,
}
document!(ProjectRecord, Project);

/// One workload of an immutable project revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionWorkload {
    pub name: String,
    pub kind: WorkloadKind,
    /// The content identity of the workload's bundle. The bundle itself is
    /// a content-addressed artifact, not control state.
    pub bundle_id: String,
    /// The artifact digest of the bundle's canonical encoding.
    pub artifact: String,
    pub workload_identity: String,
    pub runtime: String,
    /// The pinned runtime version, when the workload declares one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_version: Option<String>,
    /// The dependency capsule, when the bundle carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependency: Option<String>,
    /// The Compute distribution the revision requires, when it pins one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distribution: Option<String>,
    /// How to tell that a new instance of this service is ready to serve.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness: Option<Readiness>,
    #[serde(default)]
    pub ports: Vec<PortSpec>,
    #[serde(default)]
    pub restart: RestartPolicy,
    #[serde(default)]
    pub desired_state: DesiredState,
}

/// Exact, immutable project content. A revision label always names the
/// same content; promotion copies a revision, never rebuilds one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRevisionRecord {
    pub project_id: String,
    pub project: String,
    /// The operator's label, such as a commit.
    pub revision: String,
    /// Digest of every workload definition and bundle identity.
    pub revision_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub workloads: Vec<RevisionWorkload>,
    #[serde(with = "crate::time")]
    pub created_at: DateTime<Utc>,
}
document!(ProjectRevisionRecord, ProjectRevision);

/// A deployed, isolated place such as `preprod` or `prod`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentRecord {
    pub name: String,
    pub desired_state: DesiredState,
    /// Configuration visible to every workload in the environment.
    #[serde(default)]
    pub config: BTreeMap<String, String>,
    /// A `compute.policy@1` document intersected into every admission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<Value>,
    /// Pin tasks to one provider of the daemon's pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub created_at: DateTime<Utc>,
}
document!(EnvironmentRecord, Environment);

/// A project's membership in an environment: its desired state there, its
/// configuration there, and which deployment is current.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentProjectRecord {
    pub environment_id: String,
    pub environment: String,
    pub project_id: String,
    pub project: String,
    pub desired_state: DesiredState,
    #[serde(default)]
    pub config: BTreeMap<String, String>,
    /// The desired revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_id: Option<String>,
    /// The deployment that delivered the desired revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
document!(EnvironmentProjectRecord, EnvironmentProject);

/// A release, as a durable state machine:
///
/// ```text
/// pending → starting → ready → network_ready → switching → active → draining → complete
///    ╰──────────╰────────╰───────────╰── failed (the current revision keeps serving)
///                        active ── rolled_back (traffic returned to the previous revision)
/// ```
///
/// Traffic moves to the new revision only in `switching`, and only once it
/// is ready. Nothing currently serving is stopped before then.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentStatus {
    /// Recorded; admission and placement are being evaluated. (Records
    /// written before zero-downtime releases read `queued`, `admitted`, or
    /// `placed`.)
    #[serde(alias = "queued", alias = "admitted", alias = "placed")]
    Pending,
    /// Admitted and placed; the new revision's instances are starting next
    /// to whatever serves now.
    Starting,
    /// Every new service passed readiness; its endpoints, DNS, and TLS
    /// are verified next.
    Ready,
    /// Endpoints, DNS, and TLS are verified; traffic may move.
    NetworkReady,
    /// Traffic is moving to the new revision.
    Switching,
    /// The new revision serves; it is verified through its endpoints.
    Active,
    /// The previous revision stops accepting traffic and finishes what it
    /// has.
    Draining,
    /// The previous revision stopped; the release is done. (Earlier
    /// records read `healthy`, `stopped`, or `superseded`.)
    #[serde(alias = "healthy", alias = "stopped", alias = "superseded")]
    Complete,
    /// The release failed before traffic moved. What served before still
    /// serves.
    Failed,
    /// Traffic moved, verification failed, and traffic was returned to the
    /// previous revision.
    RolledBack,
}

impl DeploymentStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::NetworkReady => "network_ready",
            Self::Switching => "switching",
            Self::Active => "active",
            Self::Draining => "draining",
            Self::Complete => "complete",
            Self::Failed => "failed",
            Self::RolledBack => "rolled_back",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Failed | Self::RolledBack)
    }

    /// Whether this deployment's revision is (or is becoming) the one that
    /// serves.
    pub const fn serves(self) -> bool {
        matches!(
            self,
            Self::Switching | Self::Active | Self::Draining | Self::Complete
        )
    }
}

/// The admission and placement evidence of one workload in a deployment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentWorkload {
    pub name: String,
    pub kind: WorkloadKind,
    pub bundle_id: String,
    #[serde(default)]
    pub artifact: String,
    #[serde(default)]
    pub runtime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_runtime_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distribution: Option<String>,
    pub admitted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
    /// A service's stable endpoints: logical ports and the host ports that
    /// keep serving across releases.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<PortBinding>,
    /// How a caller's provider pool chose this node for the deployment,
    /// when one did (`compute deploy`). The node's own placement and
    /// admission are the fields above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool_placement: Option<PoolPlacement>,
    /// The portable application artifact the deployment released, when it
    /// came from one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_artifact: Option<ApplicationArtifactEvidence>,
}

/// The portable application artifact (`compute.application-artifact@1`)
/// a deployment released: its content identity, where the provider
/// fetched it, and what it declared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationArtifactEvidence {
    /// `sha256:` of the artifact's canonical bytes.
    pub artifact_id: String,
    /// The `file://` or `http(s)://` reference the provider fetched, when
    /// the artifact was not sent inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The developer's label for the build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
}

/// A caller's placement decision that sent a deployment to this node:
/// which pool member it selected and the placement that proves why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolPlacement {
    pub placement_id: String,
    /// The provider's name in the caller's pool.
    pub provider_id: String,
    /// `pool` when requirements chose it, `explicit` when the caller named it.
    pub selection_mode: String,
}

/// A revision delivered to an environment, with its evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentRecord {
    pub environment_id: String,
    pub environment: String,
    pub project_id: String,
    pub project: String,
    /// Monotonic deployment version scoped to the application/project.
    /// Legacy records predate versioning and deserialize as zero.
    #[serde(default)]
    pub version: u64,
    pub revision_id: String,
    pub revision: String,
    pub revision_digest: String,
    pub status: DeploymentStatus,
    /// The deployment this one was promoted from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promoted_from: Option<String>,
    /// The deployment this one replaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    #[serde(default)]
    pub workloads: Vec<DeploymentWorkload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    /// Receipts of executions this deployment started.
    #[serde(default)]
    pub receipt_ids: Vec<String>,
    /// The revision that served before this deployment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_revision: Option<String>,
    /// Digest of the environment and project configuration the release
    /// runs with. Configuration is per environment, never part of the
    /// revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_digest: Option<String>,
    /// The project configuration the release runs with in this
    /// environment; it becomes the membership's configuration at switch.
    #[serde(default)]
    pub config: BTreeMap<String, String>,
    /// Per-workload readiness evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness_result: Option<serde_json::Value>,
    /// Endpoint, DNS, and TLS verification evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_result: Option<serde_json::Value>,
    /// Which endpoints moved from which instance to which, and when.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traffic_switch_result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_reason: Option<String>,
    /// The deployment receipt: an artifact digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<String>,
    /// When the current status was entered; timeouts count from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_since: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(with = "crate::time")]
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
document!(DeploymentRecord, Deployment);

/// A service or task of a project in an environment. It survives
/// redeployments of the same workload name, and so do its port bindings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadRecord {
    pub environment_id: String,
    pub environment: String,
    pub project_id: String,
    pub project: String,
    pub name: String,
    pub kind: WorkloadKind,
    pub desired_state: DesiredState,
    pub restart: RestartPolicy,
    pub bundle_id: String,
    pub workload_identity: String,
    pub runtime: String,
    #[serde(default)]
    pub ports: Vec<PortBinding>,
    pub deployment_id: String,
}
document!(WorkloadRecord, Workload);

/// One invocation of a workload. Output stays in the daemon's logs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionRecord {
    pub execution_id: String,
    pub environment_id: String,
    pub environment: String,
    pub project_id: String,
    pub project: String,
    pub workload_id: String,
    pub workload: String,
    pub kind: WorkloadKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(with = "crate::time")]
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Why it did not succeed, as a failure kind: `workload_failed` when
    /// the workload itself failed, never for a failure of Compute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}
document!(ExecutionRecord, Execution);

/// A reference to receipt evidence. The receipt itself is an artifact the
/// daemon serves on request; the control model keeps only its identity and
/// what it binds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptRecord {
    pub receipt_id: String,
    pub execution_id: String,
    pub environment_id: String,
    pub project_id: String,
    pub workload_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_id: Option<String>,
    /// The artifact holding the encoded receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_digest: Option<String>,
    #[serde(with = "crate::time")]
    pub created_at: DateTime<Utc>,
}
document!(ReceiptRecord, Receipt);

/// A shared service other projects consume, such as an LLM gateway or a
/// database. This is the model boundary only: Compute records who provides
/// which capabilities; it does not yet manage a service catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceRecord {
    pub name: String,
    /// Capabilities such as `llm.generate@1`.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// The provider it runs on, such as `local`.
    pub provider: String,
    /// The workload that implements it, when Compute runs it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
document!(ServiceRecord, Service);

/// A member of the daemon's provider pool, as last observed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRecord {
    pub provider_id: String,
    /// `local` or `remote`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    pub priority: i64,
    /// The daemon instance that registered it.
    pub registered_by: String,
    pub observed_at: DateTime<Utc>,
}
document!(ProviderRecord, Provider);

/// A persisted lifecycle event. Sequences increase; consumers such as the
/// UI or Attn read events after the last sequence they saw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventRecord {
    pub sequence: u64,
    pub kind: String,
    pub at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
    pub message: String,
    #[serde(default)]
    pub data: Value,
}
document!(EventRecord, Event);

/// Actual state: what the daemon last observed of a workload. Written on
/// transitions, never read back as intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadStatusRecord {
    pub workload_id: String,
    pub environment: String,
    pub project: String,
    pub workload: String,
    /// `pending`, `starting`, `running`, `stopping`, `stopped`,
    /// `completed`, `failed`, or `denied`.
    pub actual_state: String,
    /// `healthy`, `unhealthy`, or `unknown`.
    pub health: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
    pub restarts: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The daemon instance that observed it.
    pub observed_by: String,
    pub observed_at: DateTime<Utc>,
}
document!(WorkloadStatusRecord, WorkloadStatus);

/// A content-addressed artifact: a workload bundle or a receipt. Its bytes
/// are stored in chunks so every backend's request limits are respected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRecord {
    /// `sha256:<hex>` of the bytes.
    pub digest: String,
    /// `bundle` or `receipt`.
    pub kind: String,
    pub size: u64,
    pub chunks: u64,
    pub created_at: DateTime<Utc>,
}
document!(ArtifactRecord, Artifact);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactChunkRecord {
    pub digest: String,
    /// The chunk's position in the artifact, from 0.
    pub position: u64,
    /// Standard base64 of this chunk's bytes.
    pub data: String,
}
document!(ArtifactChunkRecord, ArtifactChunk);

/// The state of one running instance of a service revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceState {
    Starting,
    /// Passed readiness; not yet receiving traffic.
    Ready,
    /// The active target of its endpoints.
    Serving,
    /// No longer a target; finishing open connections.
    Draining,
    Stopped,
    Failed,
}

impl InstanceState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Serving => "serving",
            Self::Draining => "draining",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

/// One instance of a service at one deployment's revision. During a
/// release two instances of a workload exist: the one serving and the
/// candidate. Each binds its own host ports; the workload's stable
/// endpoints forward to whichever is serving.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadInstanceRecord {
    pub environment_id: String,
    pub environment: String,
    pub project_id: String,
    pub project: String,
    pub workload: String,
    pub workload_id: String,
    pub deployment_id: String,
    pub revision: String,
    pub state: InstanceState,
    /// Logical ports and the instance's own host ports.
    #[serde(default)]
    pub ports: Vec<PortBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub updated_at: DateTime<Utc>,
}
document!(WorkloadInstanceRecord, WorkloadInstance);

/// Which instance an endpoint sends traffic to. One record per endpoint,
/// so exactly one revision serves it: switching is replacing this record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrafficAssignmentRecord {
    /// `environment/project/workload/port`.
    pub endpoint: String,
    pub environment: String,
    pub project: String,
    pub workload: String,
    pub port: String,
    /// The endpoint's stable host port.
    pub host_port: u16,
    /// Domains routed to this endpoint.
    #[serde(default)]
    pub domains: Vec<String>,
    pub deployment_id: String,
    pub revision: String,
    pub instance_id: String,
    /// The serving instance's own host port.
    pub target_port: u16,
    /// `active`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_deployment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_instance_id: Option<String>,
    pub switched_at: DateTime<Utc>,
}
document!(TrafficAssignmentRecord, TrafficAssignment);

/// How a network resource stands: what is wanted, what is, and what went
/// wrong last.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Reconciliation {
    /// `pending`, `healthy`, `degraded`, or `failed`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reconciled_at: Option<DateTime<Utc>>,
}

/// A domain belongs to one environment and routes to one workload port of
/// one project there. It can never route anywhere else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainRecord {
    pub name: String,
    pub environment_id: String,
    pub environment: String,
    pub project_id: String,
    pub project: String,
    pub workload: String,
    pub port: String,
    /// The DNS provider that holds its records, by configured name.
    pub dns_provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certificate_id: Option<String>,
    /// `pending`, `healthy`, `degraded`, or `failed`.
    pub status: String,
    pub dns: Reconciliation,
    pub tls: Reconciliation,
    pub routing: Reconciliation,
    pub created_at: DateTime<Utc>,
}
document!(DomainRecord, Domain);

/// A DNS record Compute wants a provider to hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsRecordRecord {
    pub domain: String,
    pub provider: String,
    pub zone: String,
    /// The record name relative to the zone (`@` for the apex).
    pub name: String,
    /// `A`, `AAAA`, or `CNAME`.
    pub record_type: String,
    pub value: String,
    pub ttl: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_record_id: Option<String>,
    pub state: Reconciliation,
}
document!(DnsRecordRecord, DnsRecord);

/// A TLS certificate. Its private key and chain never enter control state:
/// `secret_reference` names where the node that holds them keeps them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertificateRecord {
    pub domain: String,
    /// The ACME directory that issues it.
    pub issuer: String,
    /// `pending`, `issuing`, `valid`, `renewing`, `failed`, or `expired`.
    pub status: String,
    /// `not_due`, `due`, `renewing`, or `failed`.
    pub renewal_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    /// SHA-256 of the leaf certificate (public).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_reference: Option<String>,
    /// The daemon instance whose node holds the key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reconciled_at: Option<DateTime<Utc>>,
}
document!(CertificateRecord, Certificate);

/// An operator's credential for the Compute API. The secret is shown once,
/// when the credential is created or rotated; control state keeps only its
/// SHA-256 verifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatorCredentialRecord {
    pub credential_id: String,
    pub operator_id: String,
    /// `compute.read`, `compute.execute`, `compute.deploy`,
    /// `compute.operate`, `compute.admin`.
    pub scopes: Vec<String>,
    /// Hex SHA-256 of the secret. Never the secret.
    pub verifier: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<DateTime<Utc>>,
    /// The credential this one replaced by rotation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotated_from: Option<String>,
    /// Who created it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
}
document!(OperatorCredentialRecord, OperatorCredential);

/// One remote operation: who asked for what, and what happened. It never
/// carries secret material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    pub request_id: String,
    pub operator_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_id: Option<String>,
    /// The API operation, `METHOD /route/{template}`.
    pub operation: String,
    /// The kind of resource acted on, and which one.
    pub resource: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<String>,
    /// `accepted`, `rejected` (authentication or authorization), or
    /// `failed` (the operation itself).
    pub result: String,
    pub status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
    /// Identifiers the operation produced or named: deployment, revision,
    /// execution, credential.
    #[serde(default)]
    pub detail: serde_json::Map<String, serde_json::Value>,
    #[serde(with = "crate::time")]
    pub at: DateTime<Utc>,
}
document!(AuditRecord, Audit);

/// Lifecycle event kinds.
pub mod events {
    pub const ENVIRONMENT_CREATED: &str = "environment.created";
    pub const ENVIRONMENT_DESTROYED: &str = "environment.destroyed";
    pub const ENVIRONMENT_STARTED: &str = "environment.started";
    pub const ENVIRONMENT_STOPPED: &str = "environment.stopped";
    pub const ENVIRONMENT_RESTARTED: &str = "environment.restarted";
    pub const PROJECT_REGISTERED: &str = "project.registered";
    pub const PROJECT_REVISION_CREATED: &str = "project.revision_created";
    pub const PROJECT_ADDED: &str = "project.added";
    pub const PROJECT_REMOVED: &str = "project.removed";
    pub const PROJECT_STARTED: &str = "project.started";
    pub const PROJECT_STOPPED: &str = "project.stopped";
    pub const PROJECT_RESTARTED: &str = "project.restarted";
    pub const DEPLOYMENT_STARTED: &str = "deployment.started";
    pub const DEPLOYMENT_ADMITTED: &str = "deployment.admitted";
    pub const DEPLOYMENT_PLACED: &str = "deployment.placed";
    pub const DEPLOYMENT_ACTIVATED: &str = "deployment.activated";
    pub const DEPLOYMENT_COMPLETED: &str = "deployment.completed";
    pub const DEPLOYMENT_FAILED: &str = "deployment.failed";
    pub const DEPLOYMENT_PROMOTED: &str = "deployment.promoted";
    pub const SERVICE_STARTED: &str = "service.started";
    pub const SERVICE_HEALTHY: &str = "service.healthy";
    pub const SERVICE_UNHEALTHY: &str = "service.unhealthy";
    pub const SERVICE_STOPPED: &str = "service.stopped";
    pub const SERVICE_FAILED: &str = "service.failed";
    pub const SERVICE_DENIED: &str = "service.denied";
    pub const WORKLOAD_STARTED: &str = "workload.started";
    pub const WORKLOAD_STOPPED: &str = "workload.stopped";
    pub const TASK_COMPLETED: &str = "task.completed";
    pub const TASK_FAILED: &str = "task.failed";
    pub const TASK_DENIED: &str = "task.denied";
    pub const SHARED_SERVICE_REGISTERED: &str = "shared_service.registered";
    pub const SHARED_SERVICE_REMOVED: &str = "shared_service.removed";
    pub const DEPLOYMENT_READY: &str = "deployment.ready";
    pub const DEPLOYMENT_SWITCHED: &str = "deployment.switched";
    pub const DEPLOYMENT_DRAINING: &str = "deployment.draining";
    pub const DEPLOYMENT_ROLLED_BACK: &str = "deployment.rolled_back";
    pub const INSTANCE_READY: &str = "instance.ready";
    pub const INSTANCE_FAILED: &str = "instance.failed";
    pub const INSTANCE_STOPPED: &str = "instance.stopped";
    pub const DOMAIN_CREATED: &str = "domain.created";
    pub const DOMAIN_REMOVED: &str = "domain.removed";
    pub const DNS_APPLIED: &str = "network.dns.applied";
    pub const DNS_DRIFTED: &str = "network.dns.drifted";
    pub const DNS_FAILED: &str = "network.dns.failed";
    pub const CERTIFICATE_ISSUED: &str = "network.certificate.issued";
    pub const CERTIFICATE_RENEWED: &str = "network.certificate.renewed";
    pub const CERTIFICATE_FAILED: &str = "network.certificate.failed";
    pub const ROUTE_SWITCHED: &str = "network.route.switched";
    pub const DAEMON_STARTED: &str = "daemon.started";
    pub const DAEMON_STOPPED: &str = "daemon.stopped";
    // The controller: its lifecycle, recovery, and upgrades.
    pub const CONTROLLER_STARTED: &str = "controller.started";
    pub const CONTROLLER_READY: &str = "controller.ready";
    pub const CONTROLLER_DEGRADED: &str = "controller.degraded";
    pub const CONTROLLER_STOPPED: &str = "controller.stopped";
    pub const WORKLOAD_DISCOVERED: &str = "workload.discovered";
    pub const WORKLOAD_REATTACHED: &str = "workload.reattached";
    pub const WORKLOAD_RESTARTED: &str = "workload.restarted";
    pub const WORKLOAD_ORPHANED: &str = "workload.orphaned";
    pub const DATA_PLANE_RESTARTED: &str = "data_plane.restarted";
    pub const RECONCILE_STARTED: &str = "reconcile.started";
    pub const RECONCILE_FINISHED: &str = "reconcile.finished";
    pub const UPGRADE_STARTED: &str = "upgrade.started";
    pub const UPGRADE_READY: &str = "upgrade.ready";
    pub const UPGRADE_COMPLETED: &str = "upgrade.completed";
    pub const UPGRADE_FAILED: &str = "upgrade.failed";
    pub const UPGRADE_ROLLED_BACK: &str = "upgrade.rolled_back";
    pub const ENDPOINT_UNAVAILABLE: &str = "endpoint.unavailable";
    pub const FELTDB_UNAVAILABLE: &str = "feltdb.unavailable";
    pub const FELTDB_RECOVERED: &str = "feltdb.recovered";
    pub const CONTROL_MODEL_UPGRADED: &str = "control_model.upgraded";
    // Operators.
    pub const AUTHENTICATION_FAILED: &str = "auth.authentication_failed";
    pub const AUTHORIZATION_DENIED: &str = "auth.authorization_denied";
    pub const CREDENTIAL_CREATED: &str = "credential.created";
    pub const CREDENTIAL_REVOKED: &str = "credential.revoked";
    pub const CREDENTIAL_ROTATED: &str = "credential.rotated";
    pub const CREDENTIAL_BOOTSTRAPPED: &str = "credential.bootstrapped";
}

/// A 24-hex-digit digest of length-prefixed parts.
pub fn short_digest(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    format!("{:x}", hasher.finalize())[..24].to_string()
}

pub mod ids {
    use super::short_digest;

    pub fn project(name: &str) -> String {
        format!("prj_{}", short_digest(&[name]))
    }

    pub fn revision(project_id: &str, revision_digest: &str) -> String {
        format!("rev_{}", short_digest(&[project_id, revision_digest]))
    }

    pub fn membership(environment_id: &str, project_id: &str) -> String {
        format!("ep_{}", short_digest(&[environment_id, project_id]))
    }

    pub fn workload(environment_id: &str, project_id: &str, name: &str) -> String {
        format!("wl_{}", short_digest(&[environment_id, project_id, name]))
    }

    /// Executions keep the engine's execution ID.
    pub fn execution(execution_id: &str) -> String {
        execution_id.to_string()
    }

    pub fn receipt(receipt_hash: &str) -> String {
        let hex = receipt_hash.strip_prefix("sha256:").unwrap_or(receipt_hash);
        format!("rcpt_{}", &hex[..hex.len().min(32)])
    }

    pub fn event(sequence: u64) -> String {
        format!("evt_{sequence:020}")
    }

    pub fn service(name: &str) -> String {
        format!("svc_{name}")
    }

    pub fn provider(id: &str) -> String {
        format!("pvd_{id}")
    }

    pub fn workload_status(workload_id: &str) -> String {
        format!("ws_{}", workload_id.trim_start_matches("wl_"))
    }

    pub fn instance(workload_id: &str, deployment_id: &str) -> String {
        format!("wi_{}", short_digest(&[workload_id, deployment_id]))
    }

    pub fn endpoint(environment: &str, project: &str, workload: &str, port: &str) -> String {
        format!("{environment}/{project}/{workload}/{port}")
    }

    pub fn traffic(endpoint: &str) -> String {
        format!("ta_{}", short_digest(&[endpoint]))
    }

    pub fn domain(name: &str) -> String {
        format!("dom_{}", short_digest(&[name]))
    }

    /// Credentials keep their generated ID.
    pub fn credential(credential_id: &str) -> String {
        credential_id.to_string()
    }

    pub fn audit(request_id: &str) -> String {
        format!("aud_{}", request_id.trim_start_matches("req_"))
    }

    pub fn dns_record(domain: &str, record_type: &str) -> String {
        format!("dns_{}", short_digest(&[domain, record_type]))
    }

    pub fn certificate(domain: &str) -> String {
        format!("cert_{}", short_digest(&[domain]))
    }

    pub fn artifact(digest: &str) -> String {
        format!("art_{}", digest.strip_prefix("sha256:").unwrap_or(digest))
    }

    pub fn artifact_chunk(digest: &str, index: u64) -> String {
        format!("{}_{index:06}", artifact(digest))
    }
}

/// Decode a stored document as its collection's typed record: whether
/// this build can read it. Used to prove existing state readable after a
/// model change.
pub fn decode_document(
    collection: crate::Collection,
    value: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), crate::StateError> {
    fn decode<T: Document>(
        value: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), crate::StateError> {
        serde_json::from_value::<T>(serde_json::Value::Object(value.clone()))
            .map(|_| ())
            .map_err(|error| {
                crate::StateError::Invalid(format!(
                    "a {} record does not decode: {error}",
                    T::COLLECTION.name()
                ))
            })
    }
    use crate::Collection as C;
    match collection {
        C::Project => decode::<ProjectRecord>(value),
        C::ProjectRevision => decode::<ProjectRevisionRecord>(value),
        C::Environment => decode::<EnvironmentRecord>(value),
        C::EnvironmentProject => decode::<EnvironmentProjectRecord>(value),
        C::Deployment => decode::<DeploymentRecord>(value),
        C::Workload => decode::<WorkloadRecord>(value),
        C::Execution => decode::<ExecutionRecord>(value),
        C::Service => decode::<ServiceRecord>(value),
        C::Provider => decode::<ProviderRecord>(value),
        C::Receipt => decode::<ReceiptRecord>(value),
        C::Event => decode::<EventRecord>(value),
        C::WorkloadStatus => decode::<WorkloadStatusRecord>(value),
        C::Artifact => decode::<ArtifactRecord>(value),
        C::ArtifactChunk => decode::<ArtifactChunkRecord>(value),
        C::WorkloadInstance => decode::<WorkloadInstanceRecord>(value),
        C::TrafficAssignment => decode::<TrafficAssignmentRecord>(value),
        C::Domain => decode::<DomainRecord>(value),
        C::DnsRecord => decode::<DnsRecordRecord>(value),
        C::Certificate => decode::<CertificateRecord>(value),
        C::OperatorCredential => decode::<OperatorCredentialRecord>(value),
        C::Audit => decode::<AuditRecord>(value),
    }
}
