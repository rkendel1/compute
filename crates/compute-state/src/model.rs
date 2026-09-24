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
    #[default]
    Never,
    /// A service that exits unsuccessfully is started again after a delay.
    OnFailure,
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
    pub workload_identity: String,
    pub runtime: String,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentStatus {
    /// Recorded; nothing evaluated yet.
    Queued,
    /// Every workload was admitted by policy.
    Admitted,
    /// Every workload has a provider.
    Placed,
    /// The revision is current; its services are starting.
    Starting,
    /// Every service is running and healthy.
    Healthy,
    /// Admission, placement, or startup failed. The previous deployment,
    /// if any, stays current.
    Failed,
    /// The project was stopped while this deployment was current.
    Stopped,
    /// A later deployment replaced this one.
    Superseded,
}

impl DeploymentStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Admitted => "admitted",
            Self::Placed => "placed",
            Self::Starting => "starting",
            Self::Healthy => "healthy",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
            Self::Superseded => "superseded",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Failed | Self::Superseded)
    }
}

/// The admission and placement evidence of one workload in a deployment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentWorkload {
    pub name: String,
    pub kind: WorkloadKind,
    pub bundle_id: String,
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
}

/// A revision delivered to an environment, with its evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentRecord {
    pub environment_id: String,
    pub environment: String,
    pub project_id: String,
    pub project: String,
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
}
