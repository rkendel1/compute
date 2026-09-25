//! The persistent Compute daemon: control-plane API, lifecycle manager,
//! and reconciler.
//!
//! Desired state is durable and lives behind `compute-state` — in Managed
//! FeltDB for a production control plane. The daemon owns only live
//! process state. Its reconciler reads desired state every cycle, converges
//! the node toward it, and writes back what it observed (actual state,
//! executions, receipts, events). The daemon's memory can disappear; the
//! desired state cannot.
//!
//! Stopping a child never stops its parent or siblings: reconciliation
//! only starts what should run and is not running, and only stops what is
//! running and should not.

mod deploy;
mod execute;
mod lifecycle;
pub(crate) mod network;
mod operators;
mod processes;
mod reconcile;
mod release;
mod supervision;
mod upgrades;

pub use supervision::Recovery;
mod views;

pub use views::EventFilter;

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use compute_core::ExecutionControl;
use compute_network::acme::AcmeConfig;
use compute_network::dns::{DnsProvider, DnsProviderConfig};
use compute_network::{Endpoints, Ingress, SecretStore};
use compute_placement::{CapabilityCache, PoolConfig, ProviderConfig, ProviderKind, ProviderPool};
use compute_policy::Policy;
use compute_provider::{LocalProvider, RemoteProvider};
use compute_state::{
    ArtifactStore, Batch, CertificateRecord, Collection, ControlState, DeploymentRecord,
    DeploymentStatus, DnsRecordRecord, Document, DomainRecord, EnvironmentProjectRecord,
    EnvironmentRecord, EventRecord, InstanceState, ProjectRecord, ProjectRevisionRecord,
    ProviderRecord, Query, StateStore, Stored, TrafficAssignmentRecord, WorkloadInstanceRecord,
    WorkloadRecord, ids, short_digest,
};
use serde_json::Value;
use tokio::sync::{Mutex, Notify, broadcast};

use crate::EnvironmentError;
use crate::model::*;
use crate::status::*;

/// Output a service keeps in memory is capped; full output is in its logs.
const SERVICE_OUTPUT_BYTES: u64 = 1024 * 1024;
const RECENT_RECEIPTS: usize = 16;
const RECENT_EXECUTIONS: usize = 256;

pub struct DaemonConfig {
    /// Node-local data: the artifact cache, logs, and the daemon lock.
    /// Never authoritative.
    pub state_dir: PathBuf,
    /// Durable control state: desired state and evidence.
    pub state: Arc<dyn StateStore>,
    /// Durable artifacts: workload bundles and receipts.
    pub artifacts: Arc<dyn ArtifactStore>,
    /// The daemon's own node: it executes services and local tasks.
    pub provider: Arc<LocalProvider>,
    /// Additional providers for tasks. `None` means the local node only.
    pub pool: Option<PoolConfig>,
    /// Daemon-wide execution policy, intersected with each environment's.
    pub policy: Option<Policy>,
    /// Host ports for services' stable endpoints.
    pub port_range: (u16, u16),
    /// Host ports for individual service instances. Endpoints forward to
    /// them; two instances of a service run side by side during a release.
    pub instance_port_range: (u16, u16),
    /// How long a replaced instance may finish open connections before it
    /// is stopped.
    pub drain_timeout: Duration,
    /// How long traffic may take to verify on a new revision before the
    /// release is rolled back.
    pub switch_timeout: Duration,
    /// Endpoints, ingress, DNS, and certificates.
    pub network: NetworkConfig,
    /// The first restart delay of a failed service; later ones back off.
    pub restart_delay: Duration,
    /// How often the reconciler rereads desired state.
    pub reconcile_interval: Duration,
    /// How the API authenticates operators.
    pub security: crate::auth::SecurityConfig,
    /// The API's TLS, when it terminates TLS.
    pub api_tls: Option<Arc<crate::tls::ApiTls>>,
    /// Where services run and endpoints listen. `None` runs them in this
    /// process, sharing its fate; the CLI uses the node's supervisor
    /// process, which outlives controller restarts and upgrades.
    pub data_plane: Option<Arc<dyn crate::dataplane::DataPlane>>,
    /// How old desired state a read may be served from. The cache is
    /// dropped by every write this controller makes, so it never hides a
    /// change made here; changes made elsewhere show within this bound.
    pub read_cache: Duration,
    /// Refuse to start while durable control state is unreachable,
    /// instead of starting with a degraded control plane.
    pub require_state_at_start: bool,
}

impl DaemonConfig {
    pub fn new(
        state_dir: impl Into<PathBuf>,
        state: Arc<dyn StateStore>,
        artifacts: Arc<dyn ArtifactStore>,
    ) -> Self {
        Self {
            state_dir: state_dir.into(),
            state,
            artifacts,
            provider: Arc::new(LocalProvider::new()),
            pool: None,
            policy: None,
            port_range: (20000, 29999),
            instance_port_range: (30000, 39999),
            drain_timeout: Duration::from_secs(30),
            switch_timeout: Duration::from_secs(10),
            network: NetworkConfig::default(),
            restart_delay: Duration::from_secs(1),
            reconcile_interval: Duration::from_secs(5),
            security: crate::auth::SecurityConfig::default(),
            api_tls: None,
            data_plane: None,
            read_cache: Duration::from_millis(1000),
            require_state_at_start: false,
        }
    }
}

/// The node's network: where endpoints listen, the public entry, and the
/// providers that manage DNS and certificates.
#[derive(Debug, Clone)]
pub struct NetworkConfig {
    /// The address endpoints listen on. Loopback keeps services private to
    /// the node; ingress is the public entry.
    pub endpoint_address: IpAddr,
    /// Public HTTP: ACME HTTP-01, redirects, and plain routing.
    pub ingress_http: Option<SocketAddr>,
    /// Public HTTPS: TLS terminated by SNI.
    pub ingress_https: Option<SocketAddr>,
    /// The address DNS `A` records point at.
    pub public_ipv4: Option<String>,
    /// The address DNS `AAAA` records point at.
    pub public_ipv6: Option<String>,
    /// DNS providers by name.
    pub dns: BTreeMap<String, DnsProviderConfig>,
    /// Certificate issuance. `None` leaves domains on plain HTTP.
    pub acme: Option<AcmeConfig>,
    /// How often DNS records are read back to detect drift.
    pub dns_interval: Duration,
    /// How long to wait after a failed certificate order before retrying.
    pub certificate_retry: Duration,
    /// Where node-local secrets (certificate keys, the ACME account) live.
    /// Defaults to `secrets` in the state directory.
    pub secrets_dir: Option<PathBuf>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            endpoint_address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            ingress_http: None,
            ingress_https: None,
            public_ipv4: None,
            public_ipv6: None,
            dns: BTreeMap::new(),
            acme: None,
            dns_interval: Duration::from_secs(60),
            certificate_retry: Duration::from_secs(300),
            secrets_dir: None,
        }
    }
}

/// (environment, project, workload) names.
pub(crate) type Key = (String, String, String);

/// What runs: a workload at one deployment's revision. A service has one
/// unit per instance; during a release the serving instance and the
/// candidate run side by side.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Unit {
    pub key: Key,
    pub deployment_id: String,
}

impl Unit {
    pub fn new(key: &Key, deployment_id: &str) -> Self {
        Self {
            key: key.clone(),
            deployment_id: deployment_id.into(),
        }
    }
}

/// Whether an instance should run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Want {
    Run,
    /// Draining: keep it while it finishes, but never start it again.
    KeepIfRunning,
    Stop,
}

#[derive(Default)]
pub(crate) struct WorkloadRuntime {
    pub service: bool,
    pub state: Option<ActualState>,
    pub generation: u64,
    /// Held down after exiting on its own, failing without a restart
    /// policy, or being denied, until an explicit start.
    pub held: bool,
    pub restarts: u64,
    pub consecutive_failures: u32,
    pub control: Option<ExecutionControl>,
    pub handle: Option<tokio::task::JoinHandle<()>>,
    pub deployment_id: Option<String>,
    pub execution_id: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    pub placement: PlacementView,
    pub evidence: Evidence,
    pub log_directory: Option<PathBuf>,
    pub health: Option<Health>,
    /// When the process was last seen running, for process readiness.
    pub running_since: Option<DateTime<Utc>>,
    /// The data-plane unit running it, and its process.
    pub unit_id: Option<String>,
    pub pid: Option<u32>,
}

/// Desired state as last read from control state.
#[derive(Default, Clone)]
pub(crate) struct Desired {
    pub environments: BTreeMap<String, Stored<EnvironmentRecord>>,
    pub projects: BTreeMap<String, Stored<ProjectRecord>>,
    pub memberships: BTreeMap<(String, String), Stored<EnvironmentProjectRecord>>,
    pub workloads: BTreeMap<Key, Stored<WorkloadRecord>>,
    /// Current deployments and every release in flight.
    pub deployments: BTreeMap<String, Stored<DeploymentRecord>>,
    pub revisions: BTreeMap<String, ProjectRevisionRecord>,
    pub instances: BTreeMap<String, Stored<WorkloadInstanceRecord>>,
    /// Traffic assignments by endpoint.
    pub traffic: BTreeMap<String, Stored<TrafficAssignmentRecord>>,
    /// Domains by name.
    pub domains: BTreeMap<String, Stored<DomainRecord>>,
    pub dns_records: BTreeMap<String, Stored<DnsRecordRecord>>,
    /// Certificates by domain.
    pub certificates: BTreeMap<String, Stored<CertificateRecord>>,
    /// Instance IDs by workload and deployment, so finding a unit's
    /// instance does not scan every instance.
    pub instance_index: BTreeMap<(Key, String), String>,
    /// The snapshot this was derived from, when it was derived whole.
    pub snapshot_id: Option<String>,
}

/// The durable state the controller works from, as one FeltDB snapshot
/// definition: explicit, bounded to desired state, and coherent.
///
/// - Environments, projects, memberships, workloads, instances, traffic,
///   domains, DNS records, and certificates: the controller converges every
///   one of them, so it holds each collection whole. These are desired
///   state, bounded by what the control plane runs, never history.
/// - Deployments: only releases in flight (an indexed equality per status),
///   plus the ones memberships and instances name, read by identity. The
///   history of finished releases is never read here.
/// - Revisions: only those deployments name, by identity; immutable, so a
///   revision the previous snapshot holds is reused.
pub fn desired_snapshot() -> compute_state::SnapshotDefinition {
    use compute_state::{SnapshotDefinition, SnapshotSource};
    SnapshotDefinition::new(
        "compute.controller.desired",
        vec![
            SnapshotSource::all(Collection::Environment),
            SnapshotSource::all(Collection::Project),
            SnapshotSource::all(Collection::EnvironmentProject),
            SnapshotSource::all(Collection::Workload),
            SnapshotSource::all(Collection::WorkloadInstance),
            SnapshotSource::all(Collection::TrafficAssignment),
            SnapshotSource::all(Collection::Domain),
            SnapshotSource::all(Collection::DnsRecord),
            SnapshotSource::all(Collection::Certificate),
            SnapshotSource::filtered(
                Query::all(Collection::Deployment)
                    .one_of("status", IN_FLIGHT.iter().map(|status| status.as_str())),
            ),
        ],
    )
    .reference(
        Collection::EnvironmentProject,
        "deployment_id",
        Collection::Deployment,
    )
    .reference(
        Collection::WorkloadInstance,
        "deployment_id",
        Collection::Deployment,
    )
    .immutable_reference(
        Collection::Deployment,
        "revision_id",
        Collection::ProjectRevision,
    )
    // A controller keeps working under constant writes: a snapshot whose
    // coherence could not be proven is used, marked unproven, and never
    // reused; the next cycle reads again.
    .on_incoherent(compute_state::Incoherent::PublishUnproven)
}

/// Observed state this controller writes back: one status per workload.
/// Bounded by the workloads the control plane has run.
pub fn observed_snapshot() -> compute_state::SnapshotDefinition {
    compute_state::SnapshotDefinition::new(
        "compute.controller.observed",
        vec![compute_state::SnapshotSource::all(
            Collection::WorkloadStatus,
        )],
    )
    .on_incoherent(compute_state::Incoherent::PublishUnproven)
}

impl Desired {
    /// Desired state from a published snapshot. No I/O.
    pub fn from_snapshot(
        snapshot: &compute_state::Snapshot,
    ) -> Result<Self, compute_state::StateError> {
        let mut desired = Desired::default();
        for environment in snapshot.typed::<EnvironmentRecord>()? {
            desired
                .environments
                .insert(environment.value.name.clone(), environment);
        }
        for project in snapshot.typed::<ProjectRecord>()? {
            desired.projects.insert(project.value.name.clone(), project);
        }
        for membership in snapshot.typed::<EnvironmentProjectRecord>()? {
            desired.memberships.insert(
                (
                    membership.value.environment.clone(),
                    membership.value.project.clone(),
                ),
                membership,
            );
        }
        for workload in snapshot.typed::<WorkloadRecord>()? {
            desired.workloads.insert(
                (
                    workload.value.environment.clone(),
                    workload.value.project.clone(),
                    workload.value.name.clone(),
                ),
                workload,
            );
        }
        for instance in snapshot.typed::<WorkloadInstanceRecord>()? {
            desired.instances.insert(instance.id.clone(), instance);
        }
        for assignment in snapshot.typed::<TrafficAssignmentRecord>()? {
            desired
                .traffic
                .insert(assignment.value.endpoint.clone(), assignment);
        }
        for domain in snapshot.typed::<DomainRecord>()? {
            desired.domains.insert(domain.value.name.clone(), domain);
        }
        for record in snapshot.typed::<DnsRecordRecord>()? {
            desired.dns_records.insert(record.id.clone(), record);
        }
        for certificate in snapshot.typed::<CertificateRecord>()? {
            desired
                .certificates
                .insert(certificate.value.domain.clone(), certificate);
        }
        for deployment in snapshot.typed::<DeploymentRecord>()? {
            desired
                .deployments
                .insert(deployment.id.clone(), deployment);
        }
        for revision in snapshot.typed::<ProjectRevisionRecord>()? {
            desired.revisions.insert(revision.id, revision.value);
        }
        desired.reindex();
        Ok(desired)
    }

    pub fn environment(&self, name_or_id: &str) -> Option<&Stored<EnvironmentRecord>> {
        self.environments.get(name_or_id).or_else(|| {
            self.environments
                .values()
                .find(|record| record.id == name_or_id)
        })
    }

    pub fn workloads_of<'a>(
        &'a self,
        environment: &'a str,
        project: &'a str,
    ) -> impl Iterator<Item = (&'a Key, &'a Stored<WorkloadRecord>)> + 'a {
        self.workloads
            .iter()
            .filter(move |(key, _)| key.0 == environment && key.1 == project)
    }

    /// Whether an environment and a project in it are running.
    pub fn project_runs(&self, environment: &str, project: &str) -> bool {
        self.memberships
            .get(&(environment.to_string(), project.to_string()))
            .is_some_and(|membership| membership.value.desired_state == DesiredState::Running)
            && self
                .environments
                .get(environment)
                .is_some_and(|record| record.value.desired_state == DesiredState::Running)
    }

    /// Whether a workload's services should run: the workload's own
    /// desired state, or, for a workload a release introduces, its
    /// revision's.
    pub fn workload_runs(&self, key: &Key, deployment_id: &str) -> bool {
        let desired = match self.workloads.get(key) {
            Some(workload) => workload.value.desired_state,
            None => self
                .revision_workload(deployment_id, &key.2)
                .map_or(DesiredState::Running, |workload| workload.desired_state),
        };
        desired == DesiredState::Running && self.project_runs(&key.0, &key.1)
    }

    pub fn instance_key(instance: &WorkloadInstanceRecord) -> Key {
        (
            instance.environment.clone(),
            instance.project.clone(),
            instance.workload.clone(),
        )
    }

    /// Whether an instance should run now.
    pub fn want(&self, instance: &WorkloadInstanceRecord) -> Want {
        let key = Self::instance_key(instance);
        if !self.workload_runs(&key, &instance.deployment_id) {
            return Want::Stop;
        }
        match instance.state {
            InstanceState::Starting | InstanceState::Ready | InstanceState::Serving => Want::Run,
            InstanceState::Draining => Want::KeepIfRunning,
            InstanceState::Stopped | InstanceState::Failed => Want::Stop,
        }
    }

    /// The instance of a workload at a deployment.
    pub fn instance(
        &self,
        key: &Key,
        deployment_id: &str,
    ) -> Option<&Stored<WorkloadInstanceRecord>> {
        self.instance_index
            .get(&(key.clone(), deployment_id.to_string()))
            .and_then(|id| self.instances.get(id))
    }

    /// Rebuild the indexes after the maps changed.
    pub fn reindex(&mut self) {
        self.instance_index = self
            .instances
            .values()
            .map(|instance| {
                (
                    (
                        Self::instance_key(&instance.value),
                        instance.value.deployment_id.clone(),
                    ),
                    instance.id.clone(),
                )
            })
            .collect();
    }

    pub fn revision_of(&self, deployment_id: &str) -> Option<&ProjectRevisionRecord> {
        self.deployments
            .get(deployment_id)
            .and_then(|deployment| self.revisions.get(&deployment.value.revision_id))
    }

    pub fn revision_workload(
        &self,
        deployment_id: &str,
        workload: &str,
    ) -> Option<&compute_state::RevisionWorkload> {
        self.revision_of(deployment_id)?
            .workloads
            .iter()
            .find(|candidate| candidate.name == workload)
    }

    /// Releases that have not finished.
    pub fn in_flight(&self) -> impl Iterator<Item = &Stored<DeploymentRecord>> {
        self.deployments
            .values()
            .filter(|deployment| !deployment.value.status.is_terminal())
    }
}

pub(crate) struct Inner {
    /// The last read of desired state. Shared, never mutated in place: a
    /// refresh replaces it, so taking a snapshot costs nothing.
    pub desired: Arc<Desired>,
    pub runtime: BTreeMap<Unit, WorkloadRuntime>,
    /// Output of recent executions, which control state does not keep.
    pub outputs: BTreeMap<String, (String, String)>,
    pub state_error: Option<String>,
    pub last_reconciled_at: Option<DateTime<Utc>>,
    /// Endpoints that could not listen, by host port.
    pub endpoint_errors: BTreeMap<u16, String>,
    /// Whether a release is in flight: the reconciler then runs often.
    pub releasing: bool,
    pub network: network::NetworkRuntime,
    /// Executions terminalized recently, so terminalizing one again is a
    /// no-op.
    pub terminal: TerminalLog,
    /// Execution records whose evidence could not be written yet because
    /// control state was unreachable; written once it is.
    pub pending_evidence: Vec<execute::PendingEvidence>,
    /// Audit records not yet written to control state.
    pub pending_audit: Vec<compute_state::AuditRecord>,
    /// Refused requests recorded as events this minute: (minute, count).
    pub refusals: (i64, u32),
    /// When desired state was last read, and how many writes this
    /// controller had made by then.
    pub loaded: Option<Loaded>,
    /// Since when durable control state has been unreachable.
    pub degraded_since: Option<DateTime<Utc>>,
    /// An outage that ended, to be recorded: when it began.
    pub recovered_from: Option<DateTime<Utc>>,
    /// Whether the current outage has been announced.
    pub outage_announced: bool,
    pub reconcile: ReconcileMetrics,
    /// What the current cycle changed, counted by the phases.
    pub cycle_changes: usize,
    /// The snapshot `desired` was derived from whole; `None` once a
    /// targeted refresh patched it.
    pub desired_from: Option<String>,
    /// The durable revision `desired` provably represents: the snapshot's,
    /// carried forward by this controller's own commits. `None` when
    /// unknown (another writer, an unproven snapshot, an outage).
    pub desired_revision: Option<compute_state::Revision>,
    /// Refreshes answered by `desired_revision` alone, and own commits
    /// chained onto it.
    pub desired_reused: u64,
    pub rolled_forward: u64,
    /// When durable state last answered a read.
    pub last_durable_read: Option<DateTime<Utc>>,
    /// The last outage that ended: (began, ended).
    pub last_recovery: Option<(DateTime<Utc>, DateTime<Utc>)>,
}

/// When desired state was read.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Loaded {
    pub writes: u64,
    pub at: std::time::Instant,
    pub as_of: DateTime<Utc>,
}

/// The most recently terminalized executions, by execution ID.
#[derive(Default)]
pub(crate) struct TerminalLog {
    records: std::collections::HashMap<String, ExecutionRecord>,
    order: std::collections::VecDeque<String>,
}

impl TerminalLog {
    const CAPACITY: usize = 4096;

    pub fn get(&self, execution_id: &str) -> Option<&ExecutionRecord> {
        self.records.get(execution_id)
    }

    pub fn insert(&mut self, record: ExecutionRecord) {
        if self
            .records
            .insert(record.execution_id.clone(), record.clone())
            .is_none()
        {
            self.order.push_back(record.execution_id);
        }
        while self.order.len() > Self::CAPACITY {
            if let Some(oldest) = self.order.pop_front() {
                self.records.remove(&oldest);
            }
        }
    }
}

pub struct Daemon {
    config: DaemonConfig,
    control: ControlState,
    instance_id: String,
    started_at: DateTime<Utc>,
    inner: Mutex<Inner>,
    /// Serializes reconciliation cycles.
    reconciling: Mutex<()>,
    pool: ProviderPool,
    cache: Mutex<CapabilityCache>,
    sequence: AtomicU64,
    /// Writes this controller has committed: the read cache's generation.
    writes: AtomicU64,
    /// When durable state last accepted a change, in Unix milliseconds.
    last_mutation_ms: std::sync::atomic::AtomicI64,
    /// Documents this controller wrote since desired state was last read,
    /// for a targeted refresh.
    dirty: std::sync::Mutex<Vec<(Collection, String)>>,
    /// The revisions just before and after each of this controller's
    /// commits since, to carry its working copy's revision forward.
    transitions: std::sync::Mutex<Vec<(u64, u64)>>,
    /// What each bundle declares, by bundle identity.
    declared:
        std::sync::Mutex<std::collections::HashMap<String, (Option<u64>, Option<u64>, String)>>,
    events: broadcast::Sender<EventRecord>,
    data_plane: Arc<dyn crate::dataplane::DataPlane>,
    /// The routes this controller assigned, mirrored so a reconcile that
    /// changes nothing sends nothing to the data plane.
    routes: std::sync::Mutex<BTreeMap<u16, compute_network::Route>>,
    ingress: Option<Arc<Ingress>>,
    ingress_http: Option<SocketAddr>,
    ingress_https: Option<SocketAddr>,
    secrets: SecretStore,
    node_id: String,
    dns: BTreeMap<String, Result<Arc<dyn DnsProvider>, String>>,
    wake: Notify,
    shutdown: tokio::sync::watch::Sender<bool>,
    /// Set once a shutdown or detach has finished its work.
    stopped: tokio::sync::watch::Sender<bool>,
    authority: crate::auth::Authority,
    /// What this controller found on the data plane when it started.
    recovery: std::sync::Mutex<Recovery>,
    /// A hand-over this controller agreed to.
    upgrade: std::sync::Mutex<Option<crate::upgrade::UpgradeRecord>>,
    /// The node lock: held until the controller hands the node over.
    lock: std::sync::Mutex<Option<std::fs::File>>,
}

pub(crate) enum Outcome {
    Denied(String, Option<Box<compute_policy::AdmissionDecision>>),
    /// Nothing executed. The error says why: a workload's own failure is
    /// never reported as an infrastructure failure, or the reverse.
    Failed(EnvironmentError),
    /// The data plane lost it (its supervisor died): its result is
    /// unknown, and it is started again whatever its restart policy,
    /// because the failure was Compute's, not the workload's.
    Lost(String),
    Executed(
        Box<compute_core::ExecutionResult>,
        Option<String>,
        PlacementView,
    ),
}

/// Who a lifecycle event is about.
#[derive(Debug, Clone, Default)]
pub(crate) struct Scope {
    pub environment: Option<String>,
    pub project: Option<String>,
    pub workload: Option<String>,
    pub deployment_id: Option<String>,
    pub execution_id: Option<String>,
}

impl Scope {
    pub fn environment(environment: &str) -> Self {
        Self {
            environment: Some(environment.into()),
            ..Self::default()
        }
    }

    pub fn project(environment: &str, project: &str) -> Self {
        Self {
            project: Some(project.into()),
            ..Self::environment(environment)
        }
    }

    pub fn workload(key: &Key) -> Self {
        Self {
            workload: Some(key.2.clone()),
            ..Self::project(&key.0, &key.1)
        }
    }

    pub fn deployment(mut self, deployment_id: &str) -> Self {
        self.deployment_id = Some(deployment_id.into());
        self
    }

    pub fn execution(mut self, execution_id: &str) -> Self {
        self.execution_id = Some(execution_id.into());
        self
    }
}

/// An atomic control-state change and the events it records.
#[derive(Default)]
pub(crate) struct Change {
    pub batch: Batch,
    pub events: Vec<EventRecord>,
}

impl Change {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, apply: impl FnOnce(Batch) -> Batch) -> Self {
        self.batch = apply(self.batch);
        self
    }
}

impl Daemon {
    /// Acquire the node, connect to control state, and reconcile. Fails
    /// closed: a daemon that cannot read its control state does not start.
    pub async fn start(config: DaemonConfig) -> Result<Arc<Self>, EnvironmentError> {
        std::fs::create_dir_all(&config.state_dir)?;
        let lock = lock_state_dir(&config.state_dir)?;
        // Services a killed in-process predecessor left running on this
        // node would otherwise run twice. A supervisor keeps its own.
        let reaped = if config
            .data_plane
            .as_ref()
            .is_some_and(|plane| plane.independent())
        {
            vec![]
        } else {
            processes::reap(&config.state_dir).await
        };
        let control = ControlState::new(config.state.clone());
        // The first read proves the control state is reachable and ours.
        // Without it the controller still starts — its data plane keeps
        // serving and it reattaches to its workloads — with a degraded
        // control plane: nothing changes until durable state answers.
        let last = control
            .query::<EventRecord>(
                Query::all(Collection::Event)
                    .descending("sequence")
                    .limit(1),
            )
            .await
            .map_err(|error| {
                EnvironmentError::Unavailable(format!(
                    "cannot read control state at {}: {error}",
                    config.state.backend().location
                ))
            });
        let (sequence, degraded) = match last {
            Ok(last) => (last.first().map_or(0, |event| event.value.sequence), None),
            Err(error) if !config.require_state_at_start => (0, Some(error)),
            Err(error) => return Err(error),
        };
        let pool = build_pool(&config)?;
        let started_at = Utc::now();
        let instance_id = format!(
            "daemon_{}",
            short_digest(&[
                &std::process::id().to_string(),
                &started_at
                    .timestamp_nanos_opt()
                    .unwrap_or_default()
                    .to_string(),
            ])
        );
        let node_id = node_id(&config.state_dir)?;
        let secrets = SecretStore::open(
            config
                .network
                .secrets_dir
                .clone()
                .unwrap_or_else(|| config.state_dir.join("secrets")),
            node_id.clone(),
        )?;
        let (ingress, ingress_http, ingress_https) =
            network::start_ingress(&config.network).await?;
        let dns = config
            .network
            .dns
            .iter()
            .map(|(name, provider)| {
                (
                    name.clone(),
                    provider
                        .build()
                        .map(Arc::from)
                        .map_err(|error| error.to_string()),
                )
            })
            .collect();
        let data_plane: Arc<dyn crate::dataplane::DataPlane> = match &config.data_plane {
            Some(plane) => plane.clone(),
            None => Arc::new(crate::dataplane::LocalDataPlane::in_process(
                config.provider.clone(),
                Endpoints::new(config.network.endpoint_address),
            )),
        };
        // The data plane's current routes: after a controller restart they
        // are still being served.
        let routes = data_plane
            .routes()
            .await?
            .into_iter()
            .map(|route| {
                (
                    route.port,
                    compute_network::Route {
                        instance_id: route.instance_id,
                        target_port: route.target_port,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        if config.security.mode == crate::auth::SecurityMode::Production
            && config.security.legacy_token.is_some()
        {
            return Err(EnvironmentError::Invalid(
                "a shared token is not a production credential; issue operator credentials with `compute auth create`".into(),
            ));
        }
        let authority = crate::auth::Authority::new(
            config.security.clone(),
            config.state_dir.join("credentials.json"),
        );
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let (stopped, _) = tokio::sync::watch::channel(false);
        let (events, _) = broadcast::channel(1024);
        let daemon = Arc::new(Self {
            config,
            control,
            instance_id,
            started_at,
            inner: Mutex::new(Inner {
                desired: Arc::new(Desired::default()),
                runtime: BTreeMap::new(),
                outputs: BTreeMap::new(),
                state_error: None,
                last_reconciled_at: None,
                endpoint_errors: BTreeMap::new(),
                releasing: false,
                network: network::NetworkRuntime::default(),
                terminal: TerminalLog::default(),
                pending_evidence: Vec::new(),
                pending_audit: Vec::new(),
                refusals: (0, 0),
                loaded: None,
                degraded_since: None,
                recovered_from: None,
                outage_announced: false,
                reconcile: ReconcileMetrics::default(),
                cycle_changes: 0,
                desired_from: None,
                desired_revision: None,
                desired_reused: 0,
                rolled_forward: 0,
                last_durable_read: None,
                last_recovery: None,
            }),
            reconciling: Mutex::new(()),
            pool,
            cache: Mutex::new(CapabilityCache::default()),
            sequence: AtomicU64::new(sequence),
            writes: AtomicU64::new(0),
            last_mutation_ms: std::sync::atomic::AtomicI64::new(0),
            dirty: std::sync::Mutex::new(Vec::new()),
            transitions: std::sync::Mutex::new(Vec::new()),
            declared: std::sync::Mutex::new(std::collections::HashMap::new()),
            events,
            data_plane,
            routes: std::sync::Mutex::new(routes),
            ingress,
            ingress_http,
            ingress_https,
            secrets,
            node_id,
            dns,
            wake: Notify::new(),
            shutdown,
            stopped,
            authority,
            recovery: std::sync::Mutex::new(Recovery::default()),
            upgrade: std::sync::Mutex::new(None),
            lock: std::sync::Mutex::new(Some(lock)),
        });
        if let Some(error) = &degraded {
            let mut inner = daemon.inner.lock().await;
            inner.state_error = Some(error.message());
            inner.degraded_since = Some(Utc::now());
        }
        if degraded.is_none() {
            // Records an older controller wrote get the indexed identity
            // this one looks them up by (a no-op once upgraded).
            let upgraded = daemon.config.state.upgrade_records().await?;
            if !upgraded.is_empty() {
                let change = daemon.event(
                    Change::new(),
                    compute_state::events::CONTROL_MODEL_UPGRADED,
                    Scope::default(),
                    format!(
                        "gave {} records an indexed identity",
                        upgraded.values().sum::<u64>()
                    ),
                    serde_json::json!({ "records": upgraded }),
                );
                daemon.apply(change).await?;
            }
            daemon.register_providers().await?;
            daemon.load_credentials().await?;
            daemon.bootstrap_admin().await?;
        } else {
            // Verifiers from this node's last snapshot; nothing new.
            let _ = daemon.load_credentials().await;
        }
        let started = Change::new();
        let started = daemon.event(
            started,
            compute_state::events::DAEMON_STARTED,
            Scope::default(),
            format!("Compute daemon {} started", daemon.instance_id),
            serde_json::json!({
                "state": daemon.config.state.backend(),
                "artifacts": daemon.config.artifacts.location(),
                "reaped": reaped,
            }),
        );
        if degraded.is_none() {
            daemon.apply(started).await?;
        }
        // Supervise again whatever the data plane still runs before
        // reconciling, so nothing healthy is started twice or restarted.
        let _ = daemon.refresh().await;
        let recovery = daemon.reattach().await?;
        let plane = daemon.data_plane().info().await?;
        let change = daemon.event(
            Change::new(),
            compute_state::events::CONTROLLER_STARTED,
            Scope::default(),
            format!(
                "Compute controller {} started ({} data plane, pid {}); reattached {}, collected {}, orphaned {}",
                daemon.instance_id,
                plane.kind,
                plane.pid,
                recovery.reattached.len(),
                recovery.collected.len(),
                recovery.orphaned.len()
            ),
            serde_json::json!({
                "controller": crate::identity::ControllerIdentity::current(),
                "data_plane": plane,
                "recovery": recovery,
            }),
        );
        let _ = daemon.apply(change).await;
        *daemon.recovery.lock().expect("recovery") = recovery;
        // Part of an upgrade: take over only with every workload accounted
        // for.
        if let Err(error) = daemon.resume_upgrade().await {
            for runtime in daemon.inner.lock().await.runtime.values_mut() {
                if let Some(handle) = runtime.handle.take() {
                    handle.abort();
                }
            }
            daemon.release_node();
            return Err(error);
        }
        daemon.reconcile().await;
        let change = daemon.event(
            Change::new(),
            compute_state::events::CONTROLLER_READY,
            Scope::default(),
            format!("Compute controller {} is ready", daemon.instance_id),
            serde_json::json!({}),
        );
        let _ = daemon.apply(change).await;
        daemon.spawn_reconciler();
        Ok(daemon)
    }

    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub(crate) fn control(&self) -> &ControlState {
        &self.control
    }

    /// Lifecycle events as they are recorded.
    pub fn subscribe(&self) -> broadcast::Receiver<EventRecord> {
        self.events.subscribe()
    }

    /// Resolves when a shutdown was requested.
    pub async fn wait_for_shutdown(&self) {
        let mut receiver = self.shutdown.subscribe();
        while !*receiver.borrow_and_update() {
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }

    /// Resolves once a shutdown (or detach) has finished: workloads are
    /// stopped (or left running), and the final events are written. A
    /// process hosting the daemon waits for this before it exits.
    pub async fn wait_stopped(&self) {
        let mut receiver = self.stopped.subscribe();
        while !*receiver.borrow_and_update() {
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }

    /// Whether a shutdown or detach has begun.
    pub fn is_stopping(&self) -> bool {
        self.is_shutting_down()
    }

    fn is_shutting_down(&self) -> bool {
        *self.shutdown.borrow()
    }

    /// Stop every service (desired state is kept, so they return when a
    /// daemon starts again) and signal shutdown.
    pub async fn shutdown(self: &Arc<Self>) {
        let _ = self.shutdown.send(true);
        let _guard = self.reconciling.lock().await;
        let units = self
            .inner
            .lock()
            .await
            .runtime
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        self.stop_units(&units).await;
        let _ = self.observe().await;
        // Everything stopped: the supervisor process goes too.
        if self.data_plane().independent() {
            let _ = self.data_plane().shutdown().await;
        }
        let change = self.event(
            Change::new(),
            compute_state::events::DAEMON_STOPPED,
            Scope::default(),
            format!("Compute daemon {} stopped", self.instance_id),
            Value::Null,
        );
        let _ = self.apply(change).await;
        // Nothing runs under this controller any more: the node is free
        // for the next one, whoever still holds a reference to this one.
        self.release_node();
        let _ = self.stopped.send(true);
    }

    pub async fn status(&self) -> DaemonStatus {
        let inner = self.inner.lock().await;
        DaemonStatus {
            version: env!("CARGO_PKG_VERSION").into(),
            instance_id: self.instance_id.clone(),
            pid: std::process::id(),
            started_at: self.started_at,
            state_dir: self.config.state_dir.display().to_string(),
            state: self.config.state.backend(),
            artifacts: self.config.artifacts.location(),
            state_available: inner.state_error.is_none(),
            state_error: inner.state_error.clone(),
            last_reconciled_at: inner.last_reconciled_at,
            reconcile_interval_ms: u64::try_from(self.config.reconcile_interval.as_millis())
                .unwrap_or(u64::MAX),
            environments: inner.desired.environments.len(),
            running_services: inner
                .runtime
                .values()
                .filter(|runtime| runtime.state == Some(ActualState::Running))
                .count(),
        }
    }

    /// Metrics in the Prometheus text format: reconciliation, workloads,
    /// the control plane's availability, and evidence not yet written.
    pub async fn metrics(&self) -> String {
        use std::fmt::Write;
        let inner = self.inner.lock().await;
        let metrics = &inner.reconcile;
        let last = metrics.last.as_ref();
        let running = inner
            .runtime
            .values()
            .filter(|runtime| runtime.state == Some(ActualState::Running))
            .count();
        let unhealthy = inner
            .runtime
            .values()
            .filter(|runtime| runtime.health == Some(Health::Unhealthy))
            .count();
        let mut out = String::new();
        let mut metric = |name: &str, kind: &str, help: &str, value: f64| {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} {kind}");
            let _ = writeln!(out, "{name} {value}");
        };
        metric(
            "compute_reconcile_cycles_total",
            "counter",
            "Reconciliation cycles run.",
            metrics.cycles as f64,
        );
        metric(
            "compute_reconcile_errors_total",
            "counter",
            "Cycles that could not complete.",
            metrics.errors_total as f64,
        );
        metric(
            "compute_reconcile_duration_seconds_total",
            "counter",
            "Time spent reconciling.",
            metrics.duration_seconds_total,
        );
        metric(
            "compute_reconcile_duration_seconds",
            "gauge",
            "The last cycle's duration.",
            last.map_or(0.0, |cycle| cycle.duration_ms / 1000.0),
        );
        metric(
            "compute_reconcile_resources_examined",
            "gauge",
            "Resources the last cycle examined.",
            last.map_or(0.0, |cycle| cycle.resources_examined as f64),
        );
        metric(
            "compute_reconcile_resources_changed",
            "gauge",
            "Resources the last cycle changed.",
            last.map_or(0.0, |cycle| cycle.resources_changed as f64),
        );
        metric(
            "compute_workloads_running",
            "gauge",
            "Units running on this node.",
            running as f64,
        );
        metric(
            "compute_workloads_unhealthy",
            "gauge",
            "Running services failing their health check.",
            unhealthy as f64,
        );
        metric(
            "compute_control_state_available",
            "gauge",
            "1 when durable control state answers.",
            f64::from(u8::from(inner.state_error.is_none())),
        );
        metric(
            "compute_evidence_pending",
            "gauge",
            "Execution evidence waiting for control state.",
            inner.pending_evidence.len() as f64,
        );
        metric(
            "compute_audit_pending",
            "gauge",
            "Audit records waiting for control state.",
            inner.pending_audit.len() as f64,
        );
        out
    }

    /// The durable-state boundary as this controller sees it.
    fn authority_view(&self, inner: &Inner) -> AuthorityView {
        let generation = self.writes.load(Ordering::SeqCst);
        let max_age_ms = u64::try_from(self.config.read_cache.as_millis()).unwrap_or(u64::MAX);
        let state = match (&inner.state_error, inner.loaded, inner.recovered_from) {
            (Some(_), None, _) => "state_unavailable",
            (Some(_), Some(_), _) => "degraded_control_plane",
            (None, _, Some(_)) => "recovered",
            (None, _, None) => "healthy",
        };
        let cache = CacheView {
            generation,
            as_of: inner.loaded.map(|loaded| loaded.as_of),
            age_ms: inner
                .loaded
                .map(|loaded| u64::try_from(loaded.at.elapsed().as_millis()).unwrap_or(u64::MAX)),
            max_age_ms,
            freshness: match inner.loaded {
                None => "empty",
                Some(_) if inner.state_error.is_some() => "stale",
                Some(loaded)
                    if loaded.writes == generation
                        && loaded.at.elapsed() < self.config.read_cache =>
                {
                    "current"
                }
                Some(_) => "expired",
            }
            .into(),
            snapshot_id: inner.desired_from.clone(),
            revision: inner
                .desired_revision
                .as_ref()
                .map(|revision| revision.value),
            reused: inner.desired_reused,
            rolled_forward: inner.rolled_forward,
        };
        let mutation_ms = self.last_mutation_ms.load(Ordering::SeqCst);
        AuthorityView {
            state: state.into(),
            certified_feltdb: self
                .config
                .state
                .access()
                .and_then(|access| access.certified_version),
            model: compute_state::STATE_VERSION.into(),
            model_generation: compute_state::MODEL_GENERATION,
            access: self.config.state.access(),
            last_durable_read: inner.last_durable_read,
            last_durable_mutation: (mutation_ms > 0)
                .then(|| DateTime::from_timestamp_millis(mutation_ms))
                .flatten(),
            degraded_since: inner.degraded_since,
            last_recovery: inner.last_recovery.map(|(began, ended)| RecoveryView {
                began,
                ended,
                outage_seconds: (ended - began).num_milliseconds() as f64 / 1000.0,
            }),
            cache,
            pending: PendingView {
                targeted_refresh: self.dirty.lock().expect("dirty").len(),
                evidence: inner.pending_evidence.len(),
                audit: inner.pending_audit.len(),
            },
            snapshots: self.control.snapshots().reports(),
        }
    }

    /// What this controller found on the data plane when it started.
    pub fn recovery(&self) -> Recovery {
        self.recovery.lock().expect("recovery").clone()
    }

    /// The API's TLS acceptor, when it terminates TLS.
    pub fn api_tls(&self) -> Option<Arc<crate::tls::ApiTls>> {
        self.config.api_tls.clone()
    }

    /// Liveness without authentication: whether this controller answers,
    /// and whether its control plane is degraded. Nothing else.
    pub async fn health(&self) -> Value {
        let inner = self.inner.lock().await;
        serde_json::json!({
            "status": if self.is_shutting_down() {
                "stopping"
            } else if inner.state_error.is_none() {
                "ok"
            } else {
                "degraded_control_plane"
            },
            "instance_id": self.instance_id,
            "pid": std::process::id(),
        })
    }

    pub async fn info(&self) -> ControllerInfo {
        let runtimes =
            match compute_provider::ComputeProvider::capabilities(self.config.provider.as_ref())
                .await
            {
                Ok(capabilities) => {
                    serde_json::to_value(&capabilities.inventory).unwrap_or_default()
                }
                Err(error) => serde_json::json!({ "error": error.to_string() }),
            };
        let plane = self.data_plane.info().await;
        let data_plane = DataPlaneView {
            independent: self.data_plane.independent(),
            error: plane.as_ref().err().map(|error| error.message()),
            info: plane.ok(),
            recovery: self.recovery(),
        };
        let endpoints = self.routes_snapshot().len();
        let inner = self.inner.lock().await;
        let workloads = {
            let mut summary = crate::status::WorkloadSummary {
                endpoints,
                endpoint_errors: inner.endpoint_errors.clone(),
                ..Default::default()
            };
            let mut seen = std::collections::BTreeSet::new();
            for (unit, runtime) in &inner.runtime {
                seen.insert(unit.key.clone());
                match runtime.state {
                    Some(ActualState::Running) => {
                        summary.running += 1;
                        if runtime.health == Some(crate::status::Health::Unhealthy) {
                            let (environment, project, workload) = &unit.key;
                            summary
                                .unhealthy
                                .push(format!("{environment}/{project}/{workload}"));
                        }
                    }
                    Some(ActualState::Failed) => summary.failed += 1,
                    _ => {}
                }
            }
            summary.total = seen.len();
            summary
        };
        let security = &self.authority.config;
        ControllerInfo {
            api: crate::api::API_VERSION.into(),
            controller: crate::identity::ControllerIdentity::current(),
            instance_id: self.instance_id.clone(),
            node_id: self.node_id.clone(),
            pid: std::process::id(),
            started_at: self.started_at,
            security: SecurityView {
                mode: security.mode,
                reason: security.reason.clone(),
                authentication_required: security.mode == crate::auth::SecurityMode::Production
                    || security.legacy_token.is_some(),
                tls: self
                    .config
                    .api_tls
                    .as_ref()
                    .map_or_else(crate::tls::TlsStatus::disabled, |tls| tls.status()),
                active_credentials: self
                    .authority
                    .records()
                    .iter()
                    .filter(|record| {
                        crate::auth::CredentialView::of(record, Utc::now()).status == "active"
                    })
                    .count(),
            },
            control_plane: ControlPlaneView {
                state: self.config.state.backend(),
                mode: if inner.state_error.is_none() {
                    "normal".into()
                } else {
                    "degraded_control_plane".into()
                },
                error: inner.state_error.clone(),
                last_reconciled_at: inner.last_reconciled_at,
                authority: Some(self.authority_view(&inner)),
            },
            data_plane,
            reconcile: inner.reconcile.clone(),
            runtimes,
            isolation: Some(compute_core::host::host_isolation_report()),
            workloads,
            upgrade: self.upgrade_record(),
        }
    }

    // ---- Control-state changes --------------------------------------------

    pub(crate) fn next_sequence(&self) -> u64 {
        self.sequence.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Add an event to a change. Sequences increase across daemon restarts.
    pub(crate) fn event(
        &self,
        mut change: Change,
        kind: &str,
        scope: Scope,
        message: String,
        data: Value,
    ) -> Change {
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        // Who asked for it, when a request caused it.
        let data = match (crate::auth::RequestContext::current(), data) {
            (Some(request), Value::Object(mut map)) => {
                map.entry("request_id")
                    .or_insert_with(|| request.request_id.clone().into());
                map.entry("operator_id")
                    .or_insert_with(|| request.operator_id.clone().into());
                if let Some(credential) = request.credential_id {
                    map.entry("credential_id").or_insert(credential.into());
                }
                Value::Object(map)
            }
            (Some(request), Value::Null) => serde_json::json!({
                "request_id": request.request_id,
                "operator_id": request.operator_id,
            }),
            (_, data) => data,
        };
        let record = EventRecord {
            sequence,
            kind: kind.into(),
            at: Utc::now(),
            environment: scope.environment,
            project: scope.project,
            workload: scope.workload,
            deployment_id: scope.deployment_id,
            execution_id: scope.execution_id,
            message,
            data: if data.is_null() {
                serde_json::json!({})
            } else {
                data
            },
        };
        change.batch = change.batch.create(&ids::event(sequence), &record);
        change.events.push(record);
        change
    }

    /// Commit a change, then publish its events. Nothing is published for
    /// a change that did not commit.
    pub(crate) async fn apply(&self, change: Change) -> Result<(), EnvironmentError> {
        let targets = change.batch.targets();
        let result = self.control.transaction_tracked(change.batch).await;
        if let Ok(transition) = &result {
            self.last_mutation_ms
                .store(Utc::now().timestamp_millis(), Ordering::SeqCst);
            let mut dirty = self.dirty.lock().expect("dirty");
            if !targets.is_empty() {
                // A commit whose revisions are not stated cannot be chained.
                self.transitions
                    .lock()
                    .expect("transitions")
                    .push(transition.unwrap_or((u64::MAX - 1, u64::MAX)));
            }
            dirty.extend(targets);
        } else {
            // Unknown what landed: the next refresh reads everything.
            self.dirty
                .lock()
                .expect("dirty")
                .push((Collection::Environment, String::new()));
        }
        // Whatever the outcome, a write was attempted: cached reads must
        // look again.
        self.writes.fetch_add(1, Ordering::SeqCst);
        result?;
        for event in change.events {
            let _ = self.events.send(event);
        }
        Ok(())
    }

    /// Read desired state. On failure, the snapshot is kept and the error
    /// is reported; nothing is started or stopped from stale intent.
    ///
    /// First this controller's own writes are read back and its own
    /// commits are chained onto the revision its working copy represents
    /// ([`Daemon::catch_up`]). Then one revision read decides: unchanged
    /// means no other writer committed, and nothing else is read;
    /// changed means the desired-state snapshot is rebuilt.
    pub(crate) async fn refresh(&self) -> Result<(), EnvironmentError> {
        let writes = self.writes.load(Ordering::SeqCst);
        // After an outage nothing derived before it is trusted: the
        // snapshot is rebuilt from durable state, whatever its revision.
        let outage = self.inner.lock().await.degraded_since.is_some();
        let mut dirty = vec![];
        if outage {
            self.control.snapshots().invalidate_all();
            dirty = std::mem::take(&mut *self.dirty.lock().expect("dirty"));
            self.transitions.lock().expect("transitions").clear();
            self.inner.lock().await.desired_revision = None;
        }
        let loaded = match if outage {
            Ok(false)
        } else {
            self.catch_up(writes).await
        } {
            Ok(_) => self.load_desired(outage).await,
            Err(error) => Err(error),
        };
        if loaded.is_err() {
            self.dirty.lock().expect("dirty").extend(dirty);
        }
        let mut inner = self.inner.lock().await;
        match loaded {
            Ok(desired) => {
                if let Some((desired, revision)) = desired {
                    inner.desired_from = desired.snapshot_id.clone();
                    inner.desired = Arc::new(desired);
                    inner.desired_revision = revision;
                }
                inner.last_durable_read = Some(Utc::now());
                inner.state_error = None;
                inner.loaded = Some(Loaded {
                    writes,
                    at: std::time::Instant::now(),
                    as_of: Utc::now(),
                });
                if let Some(since) = inner.degraded_since.take() {
                    inner.recovered_from = Some(since);
                    inner.outage_announced = false;
                }
                Ok(())
            }
            Err(error) => {
                inner.state_error = Some(error.to_string());
                if inner.degraded_since.is_none() {
                    inner.degraded_since = Some(Utc::now());
                }
                Err(error.into())
            }
        }
    }

    /// Desired state for a read: this controller's last read when it is
    /// recent (its own writes since are read back by identity), otherwise
    /// a refresh. When durable state is unreachable, the last read is
    /// served and the response says how old it is; with no read at all,
    /// the request fails with `state_unavailable`.
    pub(crate) async fn refresh_for_read(&self) -> Result<(), EnvironmentError> {
        let (cached, degraded) = {
            let inner = self.inner.lock().await;
            (inner.loaded, inner.state_error.is_some())
        };
        if let Some(loaded) = cached
            && !degraded
            && loaded.at.elapsed() < self.config.read_cache
        {
            let writes = self.writes.load(Ordering::SeqCst);
            let current =
                loaded.writes == writes || matches!(self.catch_up(writes).await, Ok(false));
            if current {
                crate::auth::set_freshness("cached", loaded.as_of);
                return Ok(());
            }
        }
        match self.refresh().await {
            Ok(()) => {
                crate::auth::set_freshness("live", Utc::now());
                Ok(())
            }
            Err(EnvironmentError::Unavailable(message)) => match cached {
                Some(loaded) => {
                    crate::auth::set_freshness("stale", loaded.as_of);
                    Ok(())
                }
                None => Err(EnvironmentError::Unavailable(format!(
                    "{message}; this controller has no earlier read to serve"
                ))),
            },
            Err(error) => Err(error),
        }
    }

    /// Re-read only what this controller wrote since the last read: the
    /// records themselves, and deployments and revisions they now name.
    /// Changes made elsewhere are picked up by the next full refresh,
    /// which every reconciliation cycle begins with.
    pub(crate) async fn refresh_targeted(&self) -> Result<(), EnvironmentError> {
        let writes = self.writes.load(Ordering::SeqCst);
        match self.catch_up(writes).await {
            Ok(false) => Ok(()),
            // An unknown write, or too many to be worth it: read everything.
            Ok(true) => self.refresh().await,
            Err(error) => {
                self.inner.lock().await.state_error = Some(error.to_string());
                Err(error.into())
            }
        }
    }

    /// Bring the working copy up to this controller's own writes, without
    /// reading anything else: read back, by identity, the desired-state
    /// records it wrote, then chain its commits onto the revision the
    /// working copy represents. A commit that began exactly where the
    /// working copy stood moves it to where that commit ended; any gap is
    /// another writer, and the working copy's revision becomes unknown.
    ///
    /// Returns whether a full read is needed (a write whose outcome is
    /// unknown, or too many to read back).
    async fn catch_up(&self, writes: u64) -> Result<bool, compute_state::StateError> {
        let dirty = std::mem::take(&mut *self.dirty.lock().expect("dirty"));
        let transitions = std::mem::take(&mut *self.transitions.lock().expect("transitions"));
        let relevant = dirty
            .iter()
            .filter(|(collection, _)| {
                matches!(
                    collection,
                    Collection::Environment
                        | Collection::Project
                        | Collection::EnvironmentProject
                        | Collection::Workload
                        | Collection::WorkloadInstance
                        | Collection::TrafficAssignment
                        | Collection::Domain
                        | Collection::DnsRecord
                        | Collection::Certificate
                        | Collection::Deployment
                )
            })
            .cloned()
            .collect::<BTreeSet<_>>();
        if relevant.iter().any(|(_, id)| id.is_empty()) || relevant.len() > 64 {
            self.inner.lock().await.desired_revision = None;
            return Ok(true);
        }
        if !relevant.is_empty() {
            match self.apply_targets(&relevant).await {
                Ok(desired) => {
                    let mut inner = self.inner.lock().await;
                    inner.desired_from = None;
                    inner.last_durable_read = Some(Utc::now());
                    inner.desired = Arc::new(desired);
                }
                Err(error) => {
                    self.dirty.lock().expect("dirty").extend(dirty);
                    self.transitions
                        .lock()
                        .expect("transitions")
                        .extend(transitions);
                    return Err(error);
                }
            }
        }
        let mut inner = self.inner.lock().await;
        if let Some(loaded) = &mut inner.loaded {
            loaded.writes = loaded.writes.max(writes);
        }
        if let Some(revision) = inner.desired_revision.clone() {
            let mut at = revision.value;
            let mut sorted = transitions;
            sorted.sort_unstable();
            for (before, after) in sorted {
                if after <= at {
                    continue;
                }
                if before != at {
                    inner.desired_revision = None;
                    return Ok(false);
                }
                at = after;
            }
            if at != revision.value {
                inner.desired_revision = Some(compute_state::Revision {
                    value: at,
                    scope: revision.scope,
                });
                inner.rolled_forward += 1;
            }
        }
        Ok(false)
    }

    async fn apply_targets(
        &self,
        targets: &BTreeSet<(Collection, String)>,
    ) -> Result<Desired, compute_state::StateError> {
        let control = &self.control;
        let mut desired = (*self.inner.lock().await.desired).clone();
        // The records written, by identity: one bounded read per
        // collection, issued together.
        let mut by_collection = BTreeMap::<Collection, Vec<String>>::new();
        for (collection, id) in targets {
            by_collection
                .entry(*collection)
                .or_default()
                .push(id.clone());
        }
        let mut reads = tokio::task::JoinSet::new();
        for (collection, ids) in by_collection {
            let control = control.clone();
            reads.spawn(async move {
                let found = control.store().get_many(collection, &ids).await?;
                let mut found = found
                    .into_iter()
                    .map(|record| (record.id.clone(), record))
                    .collect::<BTreeMap<_, _>>();
                Ok::<_, compute_state::StateError>(
                    ids.into_iter()
                        .map(|id| {
                            let record = found.remove(&id);
                            (collection, id, record)
                        })
                        .collect::<Vec<_>>(),
                )
            });
        }
        let mut read = vec![];
        while let Some(result) = reads.join_next().await {
            read.extend(
                result
                    .map_err(|error| compute_state::StateError::Unavailable(error.to_string()))??,
            );
        }
        fn decode<T: Document>(
            id: &str,
            record: Option<compute_state::Record>,
        ) -> Result<Option<Stored<T>>, compute_state::StateError> {
            record
                .map(|record| {
                    Ok(Stored {
                        id: id.to_string(),
                        version: record.version,
                        value: serde_json::from_value(serde_json::Value::Object(record.value))
                            .map_err(|error| {
                                compute_state::StateError::Invalid(error.to_string())
                            })?,
                    })
                })
                .transpose()
        }
        for (collection, id, record) in read {
            match collection {
                Collection::Environment => {
                    desired.environments.retain(|_, stored| stored.id != id);
                    if let Some(stored) = decode::<EnvironmentRecord>(&id, record)? {
                        desired
                            .environments
                            .insert(stored.value.name.clone(), stored);
                    }
                }
                Collection::Project => {
                    desired.projects.retain(|_, stored| stored.id != id);
                    if let Some(stored) = decode::<ProjectRecord>(&id, record)? {
                        desired.projects.insert(stored.value.name.clone(), stored);
                    }
                }
                Collection::EnvironmentProject => {
                    desired.memberships.retain(|_, stored| stored.id != id);
                    if let Some(stored) = decode::<EnvironmentProjectRecord>(&id, record)? {
                        desired.memberships.insert(
                            (
                                stored.value.environment.clone(),
                                stored.value.project.clone(),
                            ),
                            stored,
                        );
                    }
                }
                Collection::Workload => {
                    desired.workloads.retain(|_, stored| stored.id != id);
                    if let Some(stored) = decode::<WorkloadRecord>(&id, record)? {
                        desired.workloads.insert(
                            (
                                stored.value.environment.clone(),
                                stored.value.project.clone(),
                                stored.value.name.clone(),
                            ),
                            stored,
                        );
                    }
                }
                Collection::WorkloadInstance => {
                    desired.instances.remove(&id);
                    if let Some(stored) = decode::<WorkloadInstanceRecord>(&id, record)? {
                        desired.instances.insert(id.clone(), stored);
                    }
                }
                Collection::TrafficAssignment => {
                    desired.traffic.retain(|_, stored| stored.id != id);
                    if let Some(stored) = decode::<TrafficAssignmentRecord>(&id, record)? {
                        desired
                            .traffic
                            .insert(stored.value.endpoint.clone(), stored);
                    }
                }
                Collection::Domain => {
                    desired.domains.retain(|_, stored| stored.id != id);
                    if let Some(stored) = decode::<DomainRecord>(&id, record)? {
                        desired.domains.insert(stored.value.name.clone(), stored);
                    }
                }
                Collection::DnsRecord => {
                    desired.dns_records.remove(&id);
                    if let Some(stored) = decode::<DnsRecordRecord>(&id, record)? {
                        desired.dns_records.insert(id.clone(), stored);
                    }
                }
                Collection::Certificate => {
                    desired.certificates.retain(|_, stored| stored.id != id);
                    if let Some(stored) = decode::<CertificateRecord>(&id, record)? {
                        desired
                            .certificates
                            .insert(stored.value.domain.clone(), stored);
                    }
                }
                Collection::Deployment => {
                    desired.deployments.remove(&id);
                    if let Some(stored) = decode::<DeploymentRecord>(&id, record)? {
                        desired.deployments.insert(id.clone(), stored);
                    }
                }
                _ => {}
            }
        }
        // Keep exactly the deployments a full read keeps: in flight, or
        // named by a membership or an instance.
        let wanted = desired
            .memberships
            .values()
            .filter_map(|membership| membership.value.deployment_id.clone())
            .chain(
                desired
                    .instances
                    .values()
                    .map(|instance| instance.value.deployment_id.clone()),
            )
            .collect::<BTreeSet<_>>();
        desired.deployments.retain(|id, deployment| {
            wanted.contains(id) || IN_FLIGHT.contains(&deployment.value.status)
        });
        // Deployments now named, then their revisions: each an indexed
        // identity read, issued together.
        let missing = wanted
            .iter()
            .filter(|id| !desired.deployments.contains_key(*id))
            .cloned()
            .collect::<Vec<_>>();
        for deployment in control.get_many::<DeploymentRecord>(&missing).await? {
            desired
                .deployments
                .insert(deployment.id.clone(), deployment);
        }
        let missing = desired
            .deployments
            .values()
            .map(|deployment| deployment.value.revision_id.clone())
            .filter(|id| !desired.revisions.contains_key(id))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        for revision in control.get_many::<ProjectRevisionRecord>(&missing).await? {
            desired.revisions.insert(revision.id, revision.value);
        }
        desired.reindex();
        Ok(desired)
    }

    /// Mutations need durable state: while it is unreachable they fail
    /// with `state_unavailable` before anything is attempted.
    pub(crate) async fn require_state(&self) -> Result<(), EnvironmentError> {
        let (degraded, recovering) = {
            let inner = self.inner.lock().await;
            (inner.state_error.is_some(), inner.recovered_from.is_some())
        };
        if degraded {
            self.refresh().await?;
        }
        // Durable authority answers again: finish recovering before a
        // change is accepted, so nothing is decided on pre-outage state.
        if degraded || recovering {
            self.recover().await;
            self.wake();
        }
        Ok(())
    }

    /// The recovery sequence once durable state answers after an outage.
    /// `refresh` has already reconnected, re-established the current
    /// durable state, and dropped every snapshot derived before the
    /// outage. Then, in order: continue the event sequence from durable
    /// state, reload credentials, write the evidence and audit held during
    /// the outage, and record the transition. Reconciliation against the
    /// re-established state follows in the same cycle (or, from a
    /// mutation, in the cycle it wakes). Idempotent: only the first caller
    /// after an outage records it.
    pub(crate) async fn recover(&self) {
        let Some(since) = self.inner.lock().await.recovered_from.take() else {
            self.flush_pending_evidence().await;
            self.flush_pending_audit().await;
            return;
        };
        if let Ok(last) = self
            .control
            .query::<EventRecord>(
                Query::all(Collection::Event)
                    .descending("sequence")
                    .limit(1),
            )
            .await
            && let Some(last) = last.first()
        {
            self.sequence
                .fetch_max(last.value.sequence, Ordering::SeqCst);
        }
        let _ = self.load_credentials().await;
        self.flush_pending_evidence().await;
        self.flush_pending_audit().await;
        let now = Utc::now();
        let seconds = (now - since).num_milliseconds() as f64 / 1000.0;
        self.inner.lock().await.last_recovery = Some((since, now));
        let change = self.event(
            Change::new(),
            compute_state::events::FELTDB_RECOVERED,
            Scope::default(),
            format!("durable control state is reachable again after {seconds:.1}s"),
            serde_json::json!({ "since": since, "outage_seconds": seconds }),
        );
        let _ = self.apply(change).await;
    }

    /// Whether the control plane is degraded, and since when.
    pub fn degraded_since(&self) -> Option<DateTime<Utc>> {
        self.inner
            .try_lock()
            .ok()
            .and_then(|inner| inner.degraded_since)
    }

    /// Desired state as one coherent, bounded snapshot of durable state
    /// (see [`desired_snapshot`]), with the revision it represents (none
    /// when coherence could not be proven). `None` when the authoritative
    /// revision is the one the working copy already represents: then one
    /// revision read is the whole cost.
    #[allow(clippy::type_complexity)]
    async fn load_desired(
        &self,
        force: bool,
    ) -> Result<Option<(Desired, Option<compute_state::Revision>)>, compute_state::StateError> {
        let current = self.control.store().revision().await?;
        if !force && current.is_some() {
            let mut inner = self.inner.lock().await;
            if inner.desired_revision == current {
                inner.desired_reused += 1;
                return Ok(None);
            }
        }
        let handle = self.control.snapshot(desired_snapshot())?;
        let (snapshot, _) = handle.refresh_at(force, current).await?;
        let mut desired = Desired::from_snapshot(&snapshot)?;
        desired.snapshot_id = Some(snapshot.identity.id.clone());
        Ok(Some((desired, snapshot.basis.revision.clone())))
    }

    /// Read-your-writes: refresh desired state after a change, then
    /// reconcile.
    /// Act on a change this controller just made to desired state, which
    /// it read in full before making it.
    pub(crate) async fn changed(self: &Arc<Self>) {
        self.reconcile_targeted().await;
    }

    async fn register_providers(&self) -> Result<(), EnvironmentError> {
        let now = Utc::now();
        let mut change = Change::new();
        let configs = self.pool.configs();
        // Every provider's record in one bounded identity read.
        let ids = configs
            .keys()
            .map(|id| ids::provider(id))
            .collect::<Vec<_>>();
        let mut existing = self
            .control
            .get_many::<ProviderRecord>(&ids)
            .await?
            .into_iter()
            .map(|stored| (stored.id.clone(), stored))
            .collect::<BTreeMap<_, _>>();
        for (id, provider) in &configs {
            let record = ProviderRecord {
                provider_id: id.clone(),
                kind: match provider.kind {
                    ProviderKind::Local => "local".into(),
                    ProviderKind::Remote => "remote".into(),
                },
                endpoint: provider.endpoint.clone(),
                priority: provider.priority,
                registered_by: self.instance_id.clone(),
                observed_at: now,
            };
            let record_id = ids::provider(id);
            change.batch = match existing.remove(&record_id) {
                Some(existing) => change.batch.replace(&existing, &record),
                None => change.batch.create(&record_id, &record),
            };
        }
        self.apply(change).await
    }

    pub(crate) async fn get_required<T: Document>(
        &self,
        id: &str,
    ) -> Result<Stored<T>, EnvironmentError> {
        self.control
            .get::<T>(id)
            .await?
            .ok_or_else(|| EnvironmentError::NotFound(format!("{} {id}", T::COLLECTION.name())))
    }

    fn logs_dir(&self, key: &Key) -> PathBuf {
        self.config
            .state_dir
            .join("logs")
            .join(&key.0)
            .join(&key.1)
            .join(&key.2)
    }

    /// Whether this controller's workloads outlive it.
    pub fn data_plane_independent(&self) -> bool {
        self.data_plane.independent()
    }

    pub(crate) fn data_plane(&self) -> &Arc<dyn crate::dataplane::DataPlane> {
        &self.data_plane
    }

    /// Where an endpoint sends new connections, as this controller last
    /// assigned it.
    pub(crate) fn route(&self, host_port: u16) -> Option<compute_network::Route> {
        self.routes.lock().expect("routes").get(&host_port).cloned()
    }

    pub(crate) fn routes_snapshot(&self) -> BTreeMap<u16, compute_network::Route> {
        self.routes.lock().expect("routes").clone()
    }

    /// Connections open to an instance through any endpoint. Unknown when
    /// the data plane does not answer: treated as open, so nothing that
    /// may still serve traffic is stopped.
    pub(crate) async fn open_connections(&self, instance_id: &str) -> usize {
        self.data_plane
            .connections(instance_id)
            .await
            .map(|(open, _)| open)
            .unwrap_or(usize::MAX)
    }

    fn cache_path(&self, digest: &str) -> PathBuf {
        self.config
            .state_dir
            .join("cache")
            .join(digest.trim_start_matches("sha256:"))
    }

    /// Bundle bytes by artifact digest: from the node cache, else from
    /// durable artifacts (then cached).
    pub(crate) async fn artifact(&self, digest: &str) -> Result<Vec<u8>, EnvironmentError> {
        let path = self.cache_path(digest);
        if let Ok(bytes) = std::fs::read(&path)
            && compute_state::artifacts::digest(&bytes) == digest
        {
            return Ok(bytes);
        }
        let bytes = self.config.artifacts.get(digest).await?.ok_or_else(|| {
            EnvironmentError::NotFound(format!(
                "artifact {digest} is not in {}",
                self.config.artifacts.location()
            ))
        })?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("tmp");
        std::fs::write(&temporary, &bytes)?;
        std::fs::rename(temporary, &path)?;
        Ok(bytes)
    }

    fn spawn_reconciler(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let interval = self.config.reconcile_interval;
        let mut shutdown = self.shutdown.subscribe();
        tokio::spawn(async move {
            loop {
                let Some(daemon) = weak.upgrade() else {
                    return;
                };
                // A release in flight advances every tick.
                let interval = if daemon.inner.lock().await.releasing {
                    interval.min(RELEASE_TICK)
                } else {
                    interval
                };
                tokio::select! {
                    () = tokio::time::sleep(interval) => {}
                    () = daemon.wake.notified() => {}
                    _ = shutdown.changed() => return,
                }
                if daemon.is_shutting_down() {
                    return;
                }
                daemon.reconcile().await;
            }
        });
    }

    /// Ask the reconciler to run soon, without waiting for it.
    pub(crate) fn wake(&self) {
        self.wake.notify_one();
    }
}

/// The statuses of a release in flight.
const IN_FLIGHT: [DeploymentStatus; 7] = [
    DeploymentStatus::Pending,
    DeploymentStatus::Starting,
    DeploymentStatus::Ready,
    DeploymentStatus::NetworkReady,
    DeploymentStatus::Switching,
    DeploymentStatus::Active,
    DeploymentStatus::Draining,
];

/// How often the reconciler runs while a release is in flight.
const RELEASE_TICK: Duration = Duration::from_millis(100);

/// This node's stable identity: it names the secrets the node holds.
fn node_id(state_dir: &std::path::Path) -> Result<String, EnvironmentError> {
    let path = state_dir.join("node-id");
    if let Ok(id) = std::fs::read_to_string(&path)
        && !id.trim().is_empty()
    {
        return Ok(id.trim().to_string());
    }
    let id = format!(
        "node_{}",
        short_digest(&[
            &std::process::id().to_string(),
            &Utc::now()
                .timestamp_nanos_opt()
                .unwrap_or_default()
                .to_string(),
            &state_dir.display().to_string(),
        ])
    );
    std::fs::write(&path, &id)?;
    Ok(id)
}

/// One daemon per node directory.
fn lock_state_dir(state_dir: &std::path::Path) -> Result<std::fs::File, EnvironmentError> {
    let path = state_dir.join("daemon.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: flock on a descriptor this function owns.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(EnvironmentError::Conflict(format!(
                "another Compute daemon owns {}",
                state_dir.display()
            )));
        }
    }
    Ok(file)
}

fn identity_label(identity: &compute_core::ProviderIdentity) -> String {
    match identity {
        compute_core::ProviderIdentity::Local { id } => id.clone(),
        compute_core::ProviderIdentity::Remote { id, .. } => id.clone(),
    }
}

/// The daemon's provider pool. `local` is always the daemon's own node.
fn build_pool(config: &DaemonConfig) -> Result<ProviderPool, EnvironmentError> {
    let pool_config = config.pool.clone().unwrap_or_else(PoolConfig::local_only);
    pool_config
        .validate()
        .map_err(|error| EnvironmentError::Invalid(error.to_string()))?;
    let mut pool = ProviderPool::new(pool_config.pool.clone());
    let invalid =
        |error: compute_placement::PlacementError| EnvironmentError::Invalid(error.to_string());
    let mut has_local = false;
    for (id, provider) in &pool_config.providers {
        match provider.kind {
            ProviderKind::Local => {
                has_local = true;
                pool.add(id.clone(), provider.clone(), config.provider.clone())
                    .map_err(invalid)?;
            }
            ProviderKind::Remote => {
                let endpoint = provider.endpoint.clone().expect("validated");
                pool.add_remote(
                    id.clone(),
                    provider.clone(),
                    Arc::new(RemoteProvider::new(endpoint)),
                )
                .map_err(invalid)?;
            }
        }
    }
    if !has_local {
        pool.add(
            "local",
            ProviderConfig {
                kind: ProviderKind::Local,
                endpoint: None,
                priority: 0,
                token_env: None,
            },
            config.provider.clone(),
        )
        .map_err(invalid)?;
    }
    Ok(pool)
}
