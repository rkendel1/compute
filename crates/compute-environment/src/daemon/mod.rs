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
mod reconcile;
mod views;

pub use views::EventFilter;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use compute_core::ExecutionControl;
use compute_placement::{CapabilityCache, PoolConfig, ProviderConfig, ProviderKind, ProviderPool};
use compute_policy::Policy;
use compute_provider::{LocalProvider, RemoteProvider};
use compute_state::{
    ArtifactStore, Batch, Collection, ControlState, DeploymentRecord, Document,
    EnvironmentProjectRecord, EnvironmentRecord, EventRecord, ProjectRecord, ProjectRevisionRecord,
    ProviderRecord, Query, StateStore, Stored, WorkloadRecord, ids, short_digest,
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
    /// Host ports available for logical port bindings.
    pub port_range: (u16, u16),
    /// The first restart delay of a failed service; later ones back off.
    pub restart_delay: Duration,
    /// How often the reconciler rereads desired state.
    pub reconcile_interval: Duration,
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
            restart_delay: Duration::from_secs(1),
            reconcile_interval: Duration::from_secs(5),
        }
    }
}

/// (environment, project, workload) names.
pub(crate) type Key = (String, String, String);

#[derive(Default)]
pub(crate) struct WorkloadRuntime {
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
}

/// Desired state as last read from control state.
#[derive(Default, Clone)]
pub(crate) struct Desired {
    pub environments: BTreeMap<String, Stored<EnvironmentRecord>>,
    pub projects: BTreeMap<String, Stored<ProjectRecord>>,
    pub memberships: BTreeMap<(String, String), Stored<EnvironmentProjectRecord>>,
    pub workloads: BTreeMap<Key, Stored<WorkloadRecord>>,
    pub deployments: BTreeMap<String, Stored<DeploymentRecord>>,
    pub revisions: BTreeMap<String, ProjectRevisionRecord>,
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

    /// Whether a service should be running now: its environment, project,
    /// and own desired state are all running.
    pub fn should_run(&self, key: &Key) -> bool {
        let Some(workload) = self.workloads.get(key) else {
            return false;
        };
        workload.value.kind == WorkloadKind::Service
            && workload.value.desired_state == DesiredState::Running
            && self
                .memberships
                .get(&(key.0.clone(), key.1.clone()))
                .is_some_and(|membership| {
                    membership.value.desired_state == DesiredState::Running
                        && membership.value.deployment_id.as_deref()
                            == Some(workload.value.deployment_id.as_str())
                })
            && self
                .environments
                .get(&key.0)
                .is_some_and(|environment| environment.value.desired_state == DesiredState::Running)
    }
}

pub(crate) struct Inner {
    pub desired: Desired,
    pub runtime: BTreeMap<Key, WorkloadRuntime>,
    /// Output of recent executions, which control state does not keep.
    pub outputs: BTreeMap<String, (String, String)>,
    pub state_error: Option<String>,
    pub last_reconciled_at: Option<DateTime<Utc>>,
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
    wake: Notify,
    shutdown: tokio::sync::watch::Sender<bool>,
    _lock: std::fs::File,
}

pub(crate) enum Outcome {
    Denied(String, Option<Box<compute_policy::AdmissionDecision>>),
    Failed(String),
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
            }),
            reconciling: Mutex::new(()),
            pool,
            cache: Mutex::new(CapabilityCache::default()),
            sequence: AtomicU64::new(sequence),
            events,
            wake: Notify::new(),
            shutdown,
            _lock: lock,
        });
        daemon.register_providers().await?;
        let started = Change::new();
        let started = daemon.event(
            started,
            compute_state::events::DAEMON_STARTED,
            Scope::default(),
            format!("Compute daemon {} started", daemon.instance_id),
            serde_json::json!({
                "state": daemon.config.state.backend(),
                "artifacts": daemon.config.artifacts.location(),
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
        let keys = self
            .inner
            .lock()
            .await
            .runtime
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        self.stop_keys(&keys).await;
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
        // Revisions are immutable: reuse ones already read.
        let known = self.inner.lock().await.desired.revisions.clone();
        for membership in desired.memberships.values() {
            if let Some(deployment_id) = &membership.value.deployment_id
                && let Some(deployment) =
                    self.control.get::<DeploymentRecord>(deployment_id).await?
            {
                desired
                    .deployments
                    .insert(deployment_id.clone(), deployment);
            }
            if let Some(revision_id) = &membership.value.revision_id {
                if let Some(revision) = known.get(revision_id) {
                    desired
                        .revisions
                        .insert(revision_id.clone(), revision.clone());
                } else if let Some(revision) = self
                    .control
                    .get::<ProjectRevisionRecord>(revision_id)
                    .await?
                {
                    desired
                        .revisions
                        .insert(revision_id.clone(), revision.value);
                }
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
