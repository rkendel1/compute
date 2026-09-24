//! Deterministic, machine-readable views of the control plane: durable
//! desired state joined with the daemon's live observations. The CLI, the
//! UI, and AppPort all render these documents.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use compute_state::{
    BackendInfo, DeploymentRecord, DeploymentWorkload, EventRecord, ExecutionRecord,
    ProviderRecord, ReceiptRecord, RevisionWorkload, ServiceRecord,
};

pub use crate::model::PortBinding;
use crate::model::{ActualState, DeploymentStatus, DesiredState, RestartPolicy, WorkloadKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    Healthy,
    Unhealthy,
    Unknown,
}

impl Health {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Unhealthy => "unhealthy",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub version: String,
    pub instance_id: String,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    /// The node-local directory: artifact cache, logs, and the daemon lock.
    pub state_dir: String,
    /// Where durable control state lives.
    pub state: BackendInfo,
    /// Where durable artifacts (bundles, receipts) live.
    pub artifacts: String,
    /// Whether the last read of control state succeeded. When it did not,
    /// the daemon changes nothing until it does.
    pub state_available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reconciled_at: Option<DateTime<Utc>>,
    pub reconcile_interval_ms: u64,
    pub environments: usize,
    pub running_services: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Evidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_id: Option<String>,
    /// Receipt hashes, most recent last.
    pub receipt_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlacementView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The Compute node that executes: the provider's identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ResourceView {
    /// CPU usage is not measured by compute.environment@1.
    pub cpu: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_limit_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    pub disk_bytes: u64,
    pub network: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadView {
    pub workload_id: String,
    pub name: String,
    pub kind: WorkloadKind,
    pub desired_state: DesiredState,
    pub actual_state: ActualState,
    pub health: Health,
    pub restart: RestartPolicy,
    pub runtime: String,
    pub bundle_id: String,
    pub deployment_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
    pub ports: Vec<PortBinding>,
    pub restarts: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub placement: PlacementView,
    pub evidence: Evidence,
    pub resources: ResourceView,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_directory: Option<String>,
}

/// The current deployment of a project in an environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentSummary {
    pub deployment_id: String,
    pub status: DeploymentStatus,
    pub revision: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promoted_from: Option<String>,
}

/// A project as it is in one environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectView {
    pub project_id: String,
    pub name: String,
    pub environment: String,
    pub environment_id: String,
    pub revision: String,
    pub revision_id: String,
    pub revision_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub desired_state: DesiredState,
    pub actual_state: ActualState,
    pub health: Health,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment: Option<DeploymentSummary>,
    pub deployed_at: DateTime<Utc>,
    pub config: BTreeMap<String, String>,
    pub workload_count: usize,
    pub service_count: usize,
    /// The provider pin of the environment, or `local`.
    pub provider: String,
    pub workloads: Vec<WorkloadView>,
    pub disk_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentView {
    pub version: String,
    pub environment_id: String,
    pub name: String,
    pub desired_state: DesiredState,
    pub actual_state: ActualState,
    pub health: Health,
    pub created_at: DateTime<Utc>,
    /// The environment's effective policy (daemon ∩ environment ∩ baseline).
    pub policy_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub config: BTreeMap<String, String>,
    pub project_count: usize,
    pub workload_count: usize,
    pub service_count: usize,
    pub projects: Vec<ProjectView>,
    pub disk_bytes: u64,
}

/// Summary row for `compute environment list` and the UI's first screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentSummary {
    pub environment_id: String,
    pub name: String,
    pub desired_state: DesiredState,
    pub actual_state: ActualState,
    pub health: Health,
    pub project_count: usize,
    pub workload_count: usize,
    pub service_count: usize,
    pub provider: String,
}

/// A project in one environment, as listed from the project's side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectPlacement {
    pub environment: String,
    pub revision: String,
    pub revision_id: String,
    pub desired_state: DesiredState,
    pub actual_state: ActualState,
    pub health: Health,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment: Option<DeploymentSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionView {
    pub revision_id: String,
    pub project: String,
    pub revision: String,
    pub revision_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub workloads: Vec<RevisionWorkload>,
    pub created_at: DateTime<Utc>,
}

/// A project across every environment it is in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSummary {
    pub project_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub created_at: DateTime<Utc>,
    pub revision_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_revision: Option<String>,
    pub environments: Vec<ProjectPlacement>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectDetail {
    #[serde(flatten)]
    pub summary: ProjectSummary,
    /// Newest first.
    pub revisions: Vec<RevisionView>,
    /// Newest first.
    pub deployments: Vec<DeploymentView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentView {
    pub deployment_id: String,
    #[serde(flatten)]
    pub record: DeploymentRecord,
}

/// An execution record joined with its output when this node still has it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionView {
    #[serde(flatten)]
    pub record: ExecutionRecord,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stdout: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stderr: String,
}
