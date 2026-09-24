//! Deterministic, machine-readable views of daemon, environment, project,
//! workload, and execution state.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::model::{ActualState, DesiredState, WorkloadKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    Healthy,
    Unhealthy,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub version: String,
    pub instance_id: String,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub state_dir: String,
    pub environments: usize,
    pub running_services: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortBinding {
    pub name: String,
    /// The port the project declares.
    pub logical: u16,
    /// The host port the environment layer bound it to.
    pub host: u16,
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
    pub runtime: String,
    pub bundle_id: String,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectView {
    pub project_id: String,
    pub name: String,
    pub revision: String,
    pub revision_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub desired_state: DesiredState,
    pub actual_state: ActualState,
    pub health: Health,
    pub deployed_at: DateTime<Utc>,
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
    pub project_count: usize,
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
}

/// One invocation of a workload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionRecord {
    pub execution_id: String,
    pub environment: String,
    pub project: String,
    pub workload: String,
    pub kind: WorkloadKind,
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
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stdout: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stderr: String,
}
