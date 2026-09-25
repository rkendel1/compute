//! Deterministic, machine-readable views of the control plane: durable
//! desired state joined with the daemon's live observations. The CLI, the
//! UI, and AppPort all render these documents.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use compute_state::{
    BackendInfo, CertificateRecord, DeploymentRecord, DeploymentWorkload, DnsRecordRecord,
    DomainRecord, EventRecord, ExecutionRecord, ProviderRecord, ReceiptRecord, Reconciliation,
    RevisionWorkload, ServiceRecord, TrafficAssignmentRecord, WorkloadInstanceRecord,
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

/// Which controller is running, how its API is secured, and what it can
/// run: `GET /info`, `compute node info`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControllerInfo {
    pub api: String,
    pub controller: crate::identity::ControllerIdentity,
    pub instance_id: String,
    pub node_id: String,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub security: SecurityView,
    pub control_plane: ControlPlaneView,
    /// Where workloads run and endpoints listen.
    pub data_plane: DataPlaneView,
    pub reconcile: ReconcileMetrics,
    /// Runtimes this node can execute, as its provider reports them.
    pub runtimes: serde_json::Value,
    /// What each host isolation profile enforces on this node, per
    /// dimension, or why it is unsupported.
    #[serde(default)]
    pub isolation: Option<compute_core::host::HostIsolationReport>,
    /// Workloads and endpoints on this node, as last observed.
    #[serde(default)]
    pub workloads: WorkloadSummary,
    /// The upgrade this node last ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade: Option<crate::upgrade::UpgradeRecord>,
}

/// Counts for `compute doctor`: what runs here and what is not well.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WorkloadSummary {
    pub total: usize,
    pub running: usize,
    pub failed: usize,
    /// `environment/project/workload` of each running workload whose
    /// health check fails.
    pub unhealthy: Vec<String>,
    /// Host ports routed to a workload.
    pub endpoints: usize,
    /// Host ports that could not listen, with why.
    pub endpoint_errors: std::collections::BTreeMap<u16, String>,
}

/// Reconciliation, measured: the last cycle and totals since start.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ReconcileMetrics {
    pub cycles: u64,
    pub errors_total: u64,
    pub duration_seconds_total: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<ReconcileCycle>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReconcileCycle {
    pub started_at: DateTime<Utc>,
    pub duration_ms: f64,
    /// Workloads, instances, releases, and endpoints the cycle looked at.
    pub resources_examined: usize,
    /// What it changed: units started or stopped, release steps, status
    /// records written, routes assigned.
    pub resources_changed: usize,
    pub errors: usize,
    /// Milliseconds per phase.
    pub phases_ms: std::collections::BTreeMap<String, f64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataPlaneView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info: Option<crate::dataplane::DataPlaneInfo>,
    /// Whether workloads outlive this controller.
    pub independent: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// What this controller found when it started.
    pub recovery: crate::daemon::Recovery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityView {
    pub mode: crate::auth::SecurityMode,
    pub reason: String,
    /// Whether requests without a credential are refused.
    pub authentication_required: bool,
    pub tls: crate::tls::TlsStatus,
    pub active_credentials: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlPlaneView {
    pub state: BackendInfo,
    /// `normal`, or `degraded_control_plane` while durable control state is
    /// unreachable: workloads keep running, reads are served from the last
    /// snapshot, and mutations are refused with `state_unavailable`.
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reconciled_at: Option<DateTime<Utc>>,
    /// The durable-state boundary: absent from controllers older than it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority: Option<AuthorityView>,
}

/// The boundary between Compute and its durable authority (FeltDB), for
/// diagnosis. Counters and timestamps only: never credentials, tokens, or
/// record contents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthorityView {
    /// `healthy`, `degraded_control_plane` (unreachable; reads served from
    /// the last snapshot, marked stale), `state_unavailable` (unreachable
    /// with nothing read yet: reads and changes fail), or `recovered`
    /// (answering again; the recovery sequence has not finished).
    pub state: String,
    /// The `@feltdb/core` release this build is certified against, when
    /// the backend is FeltDB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certified_feltdb: Option<String>,
    /// The Compute control model: its format and generation.
    pub model: String,
    pub model_generation: u32,
    /// What the backend reports at its boundary (FeltDB: server version,
    /// connection, query plans, transactions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<compute_state::AccessReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_durable_read: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_durable_mutation: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_since: Option<DateTime<Utc>>,
    /// The last outage that ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_recovery: Option<RecoveryView>,
    pub cache: CacheView,
    /// Controller writes not yet read back, evidence and audit waiting for
    /// durable state.
    pub pending: PendingView,
    pub snapshots: Vec<compute_state::SnapshotReport>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecoveryView {
    pub began: DateTime<Utc>,
    pub ended: DateTime<Utc>,
    pub outage_seconds: f64,
}

/// The controller's working copy of desired state. Never authority.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheView {
    /// Writes this controller has committed; a read cached before the
    /// latest is not served.
    pub generation: u64,
    /// The durable state the working copy was read at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_ms: Option<u64>,
    /// How old a read may be served from it.
    pub max_age_ms: u64,
    /// `current` (read at the latest generation within `max_age_ms`),
    /// `expired`, `stale` (durable state unreachable), or `empty`.
    pub freshness: String,
    /// The snapshot it was derived from, when derived whole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
    /// The durable revision it provably represents, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    /// Refreshes answered by that revision alone (nothing else read).
    #[serde(default)]
    pub reused: u64,
    /// This controller's own commits carried onto it without a re-read.
    #[serde(default)]
    pub rolled_forward: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingView {
    pub targeted_refresh: usize,
    pub evidence: usize,
    pub audit: usize,
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

/// A release and the instances it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentView {
    pub deployment_id: String,
    #[serde(flatten)]
    pub record: DeploymentRecord,
    /// The release's instances while they exist.
    #[serde(default)]
    pub instances: Vec<InstanceView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceView {
    pub instance_id: String,
    #[serde(flatten)]
    pub record: WorkloadInstanceRecord,
    /// What runs on this node for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_state: Option<ActualState>,
    /// Connections open to it through its endpoints.
    #[serde(default)]
    pub open_connections: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainView {
    pub domain_id: String,
    #[serde(flatten)]
    pub record: DomainRecord,
    /// `environment/project/workload/port`.
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serving_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serving_deployment: Option<String>,
    #[serde(default)]
    pub dns_records: Vec<DnsRecordView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certificate: Option<CertificateView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsRecordView {
    pub record_id: String,
    #[serde(flatten)]
    pub record: DnsRecordRecord,
}

/// A certificate's public facts. Its key is never in a view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertificateView {
    pub certificate_id: String,
    #[serde(flatten)]
    pub record: CertificateRecord,
    /// Whether this node holds the key.
    pub held_here: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointView {
    pub endpoint: String,
    pub host_port: u16,
    pub instance_id: String,
    pub target_port: u16,
    pub revision: String,
    pub listening: bool,
    pub open_connections: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsProviderView {
    pub name: String,
    pub kind: String,
    pub zone: String,
    /// Why the provider cannot be used, such as a missing token variable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkStatus {
    pub node_id: String,
    pub endpoint_address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress_http: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress_https: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_ipv4: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_ipv6: Option<String>,
    pub dns_providers: Vec<DnsProviderView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acme_directory: Option<String>,
    pub endpoints: Vec<EndpointView>,
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
