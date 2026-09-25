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
}

impl Desired {
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
        self.instances.values().find(|instance| {
            instance.value.deployment_id == deployment_id
                && instance.value.environment == key.0
                && instance.value.project == key.1
                && instance.value.workload == key.2
        })
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
    pub desired: Desired,
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
    events: broadcast::Sender<EventRecord>,
    endpoints: Endpoints,
    ingress: Option<Arc<Ingress>>,
    ingress_http: Option<SocketAddr>,
    ingress_https: Option<SocketAddr>,
    secrets: SecretStore,
    node_id: String,
    dns: BTreeMap<String, Result<Arc<dyn DnsProvider>, String>>,
    wake: Notify,
    shutdown: tokio::sync::watch::Sender<bool>,
    authority: crate::auth::Authority,
    _lock: std::fs::File,
}

pub(crate) enum Outcome {
    Denied(String, Option<Box<compute_policy::AdmissionDecision>>),
    /// Nothing executed. The error says why: a workload's own failure is
    /// never reported as an infrastructure failure, or the reverse.
    Failed(EnvironmentError),
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
        // Services a killed predecessor left running on this node would
        // otherwise run twice.
        let reaped = processes::reap(&config.state_dir).await;
        let control = ControlState::new(config.state.clone());
        // The first read proves the control state is reachable and ours.
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
            })?;
        let sequence = last.first().map_or(0, |event| event.value.sequence);
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
        let endpoints = Endpoints::new(config.network.endpoint_address);
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
        let (events, _) = broadcast::channel(1024);
        let daemon = Arc::new(Self {
            config,
            control,
            instance_id,
            started_at,
            inner: Mutex::new(Inner {
                desired: Desired::default(),
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
            }),
            reconciling: Mutex::new(()),
            pool,
            cache: Mutex::new(CapabilityCache::default()),
            sequence: AtomicU64::new(sequence),
            events,
            endpoints,
            ingress,
            ingress_http,
            ingress_https,
            secrets,
            node_id,
            dns,
            wake: Notify::new(),
            shutdown,
            authority,
            _lock: lock,
        });
        daemon.register_providers().await?;
        daemon.load_credentials().await?;
        daemon.bootstrap_admin().await?;
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
        daemon.apply(started).await?;
        daemon.reconcile().await;
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
        let change = self.event(
            Change::new(),
            compute_state::events::DAEMON_STOPPED,
            Scope::default(),
            format!("Compute daemon {} stopped", self.instance_id),
            Value::Null,
        );
        let _ = self.apply(change).await;
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

    /// The API's TLS acceptor, when it terminates TLS.
    pub fn api_tls(&self) -> Option<Arc<crate::tls::ApiTls>> {
        self.config.api_tls.clone()
    }

    /// Liveness without authentication: whether this controller answers,
    /// and whether its control plane is degraded. Nothing else.
    pub async fn health(&self) -> Value {
        let inner = self.inner.lock().await;
        serde_json::json!({
            "status": if inner.state_error.is_none() { "ok" } else { "degraded_control_plane" },
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
        let inner = self.inner.lock().await;
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
            },
            runtimes,
        }
    }

    // ---- Control-state changes --------------------------------------------

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
        self.control.transaction(change.batch).await?;
        for event in change.events {
            let _ = self.events.send(event);
        }
        Ok(())
    }

    /// Read desired state. On failure, the snapshot is kept and the error
    /// is reported; nothing is started or stopped from stale intent.
    pub(crate) async fn refresh(&self) -> Result<(), EnvironmentError> {
        let loaded = self.load_desired().await;
        let mut inner = self.inner.lock().await;
        match loaded {
            Ok(desired) => {
                inner.desired = desired;
                inner.state_error = None;
                Ok(())
            }
            Err(error) => {
                inner.state_error = Some(error.to_string());
                Err(error.into())
            }
        }
    }

    async fn load_desired(&self) -> Result<Desired, compute_state::StateError> {
        let mut desired = Desired::default();
        for environment in self.control.list::<EnvironmentRecord>().await? {
            desired
                .environments
                .insert(environment.value.name.clone(), environment);
        }
        for project in self.control.list::<ProjectRecord>().await? {
            desired.projects.insert(project.value.name.clone(), project);
        }
        for membership in self.control.list::<EnvironmentProjectRecord>().await? {
            desired.memberships.insert(
                (
                    membership.value.environment.clone(),
                    membership.value.project.clone(),
                ),
                membership,
            );
        }
        for workload in self.control.list::<WorkloadRecord>().await? {
            desired.workloads.insert(
                (
                    workload.value.environment.clone(),
                    workload.value.project.clone(),
                    workload.value.name.clone(),
                ),
                workload,
            );
        }
        for instance in self.control.list::<WorkloadInstanceRecord>().await? {
            desired.instances.insert(instance.id.clone(), instance);
        }
        for assignment in self.control.list::<TrafficAssignmentRecord>().await? {
            desired
                .traffic
                .insert(assignment.value.endpoint.clone(), assignment);
        }
        for domain in self.control.list::<DomainRecord>().await? {
            desired.domains.insert(domain.value.name.clone(), domain);
        }
        for record in self.control.list::<DnsRecordRecord>().await? {
            desired.dns_records.insert(record.id.clone(), record);
        }
        for certificate in self.control.list::<CertificateRecord>().await? {
            desired
                .certificates
                .insert(certificate.value.domain.clone(), certificate);
        }
        // Current deployments, the deployments instances belong to, and
        // every release in flight.
        let mut wanted = desired
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
        for status in IN_FLIGHT {
            for deployment in self
                .control
                .query::<DeploymentRecord>(
                    Query::all(Collection::Deployment).eq("status", status.as_str()),
                )
                .await?
            {
                wanted.remove(&deployment.id);
                desired
                    .deployments
                    .insert(deployment.id.clone(), deployment);
            }
        }
        for deployment_id in wanted {
            if let Some(deployment) = self.control.get::<DeploymentRecord>(&deployment_id).await? {
                desired.deployments.insert(deployment_id, deployment);
            }
        }
        // Revisions are immutable: reuse ones already read.
        let known = self.inner.lock().await.desired.revisions.clone();
        let revision_ids = desired
            .deployments
            .values()
            .map(|deployment| deployment.value.revision_id.clone())
            .collect::<BTreeSet<_>>();
        for revision_id in revision_ids {
            if let Some(revision) = known.get(&revision_id) {
                desired.revisions.insert(revision_id, revision.clone());
            } else if let Some(revision) = self
                .control
                .get::<ProjectRevisionRecord>(&revision_id)
                .await?
            {
                desired.revisions.insert(revision_id, revision.value);
            }
        }
        Ok(desired)
    }

    /// Read-your-writes: refresh desired state after a change, then
    /// reconcile.
    pub(crate) async fn changed(self: &Arc<Self>) {
        self.reconcile().await;
    }

    async fn register_providers(&self) -> Result<(), EnvironmentError> {
        let now = Utc::now();
        let mut change = Change::new();
        for (id, provider) in &self.pool.configs() {
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
            change.batch = match self.control.get::<ProviderRecord>(&record_id).await? {
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

    pub(crate) fn endpoints(&self) -> &Endpoints {
        &self.endpoints
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
