//! The persistent Compute daemon: environment registry, lifecycle manager,
//! and reconciler.
//!
//! Every lifecycle operation changes desired state and then reconciles.
//! Reconciliation only starts what should run and is not running, and only
//! stops what is running and should not; it never touches anything else.
//! Stopping a child therefore never stops its parent or siblings.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use compute_core::{
    ExecutionControl, ExecutionResult, ExecutionStatus, ProviderIdentity, ReceiptScope,
    WorkloadBundle,
};
use compute_placement::{
    AdmissionContext, CapabilityCache, DiscoveryMode, PlacementReport, PlacementRequirements,
    PoolConfig, ProviderConfig, ProviderKind, ProviderPool, RequirementOptions, SubmissionMode,
    place,
};
use compute_policy::{EffectivePolicy, ExecutionContract, Policy, PolicySourceKind};
use compute_provider::{ComputeProvider, LocalProvider, ProviderRequest, RemoteProvider};
use tokio::sync::Mutex;

use crate::EnvironmentError;
use crate::model::*;
use crate::status::*;

/// Output a service keeps in memory is capped; full output is in its logs.
const SERVICE_OUTPUT_BYTES: u64 = 1024 * 1024;
const RECENT_RECEIPTS: usize = 16;

pub struct DaemonConfig {
    pub state_dir: PathBuf,
    /// The daemon's own node: it executes services and local tasks.
    pub provider: Arc<LocalProvider>,
    /// Additional providers for tasks. `None` means the local node only.
    pub pool: Option<PoolConfig>,
    /// Daemon-wide execution policy, intersected with each environment's.
    pub policy: Option<Policy>,
    /// Host ports available for logical port bindings.
    pub port_range: (u16, u16),
    pub restart_delay: Duration,
}

impl DaemonConfig {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
            provider: Arc::new(LocalProvider::new()),
            pool: None,
            policy: None,
            port_range: (20000, 29999),
            restart_delay: Duration::from_secs(1),
        }
    }
}

type Key = (String, String, String);

#[derive(Default)]
struct WorkloadRuntime {
    state: Option<ActualState>,
    generation: u64,
    /// Held down after exiting on its own or being denied, until an
    /// explicit start.
    held: bool,
    restarts: u64,
    control: Option<ExecutionControl>,
    handle: Option<tokio::task::JoinHandle<()>>,
    execution_id: Option<String>,
    started_at: Option<chrono::DateTime<Utc>>,
    finished_at: Option<chrono::DateTime<Utc>>,
    exit_code: Option<i32>,
    error: Option<String>,
    placement: PlacementView,
    evidence: Evidence,
    log_directory: Option<PathBuf>,
}

#[derive(Default)]
struct Inner {
    environments: BTreeMap<String, EnvironmentRecord>,
    workloads: BTreeMap<Key, WorkloadRuntime>,
    ports: BTreeMap<String, u16>,
    executions: BTreeMap<String, ExecutionRecord>,
}

pub struct Daemon {
    config: DaemonConfig,
    instance_id: String,
    started_at: chrono::DateTime<Utc>,
    inner: Mutex<Inner>,
    pool: ProviderPool,
    cache: Mutex<CapabilityCache>,
    shutdown: tokio::sync::watch::Sender<bool>,
}

enum Outcome {
    Denied(String, Option<Box<compute_policy::AdmissionDecision>>),
    Failed(String),
    Executed(Box<ExecutionResult>, Option<String>, PlacementView),
}

impl Daemon {
    /// Load persisted environments and reconcile them.
    pub async fn start(config: DaemonConfig) -> Result<Arc<Self>, EnvironmentError> {
        std::fs::create_dir_all(config.state_dir.join("environments"))?;
        let mut inner = Inner::default();
        for entry in std::fs::read_dir(config.state_dir.join("environments"))? {
            let path = entry?.path().join("environment.json");
            if path.is_file() {
                let record: EnvironmentRecord = serde_json::from_slice(&std::fs::read(&path)?)?;
                if record.version != ENVIRONMENT_VERSION {
                    return Err(EnvironmentError::Invalid(format!(
                        "unsupported environment record version {}",
                        record.version
                    )));
                }
                inner.environments.insert(record.name.clone(), record);
            }
        }
        let ports_path = config.state_dir.join("ports.json");
        if ports_path.is_file() {
            inner.ports = serde_json::from_slice(&std::fs::read(&ports_path)?)?;
        }
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
        let daemon = Arc::new(Self {
            config,
            instance_id,
            started_at,
            inner: Mutex::new(inner),
            pool,
            cache: Mutex::new(CapabilityCache::default()),
            shutdown,
        });
        daemon.reconcile().await;
        Ok(daemon)
    }

    pub fn instance_id(&self) -> &str {
        &self.instance_id
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

    /// Stop every service (desired state is kept, so they return when the
    /// daemon starts again) and signal shutdown.
    pub async fn shutdown(self: &Arc<Self>) {
        let keys = self
            .inner
            .lock()
            .await
            .workloads
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        self.stop_keys(&keys).await;
        let _ = self.shutdown.send(true);
    }

    pub async fn status(&self) -> DaemonStatus {
        let inner = self.inner.lock().await;
        DaemonStatus {
            version: env!("CARGO_PKG_VERSION").into(),
            instance_id: self.instance_id.clone(),
            pid: std::process::id(),
            started_at: self.started_at,
            state_dir: self.config.state_dir.display().to_string(),
            environments: inner.environments.len(),
            running_services: inner
                .workloads
                .values()
                .filter(|runtime| runtime.state == Some(ActualState::Running))
                .count(),
        }
    }

    // ---- Environment lifecycle -------------------------------------------

    pub async fn create_environment(
        self: &Arc<Self>,
        definition: EnvironmentDefinition,
    ) -> Result<EnvironmentView, EnvironmentError> {
        validate_name("environment", &definition.name)?;
        validate_env("environment", &definition.env)?;
        if let Some(policy) = &definition.policy {
            policy
                .validate()
                .map_err(|error| EnvironmentError::Invalid(error.to_string()))?;
        }
        if let Some(provider) = &definition.provider
            && self.pool.member(provider).is_none()
        {
            return Err(EnvironmentError::Invalid(format!(
                "provider {provider} is not in the daemon's pool"
            )));
        }
        {
            let mut inner = self.inner.lock().await;
            if inner.environments.contains_key(&definition.name) {
                return Err(EnvironmentError::Conflict(format!(
                    "environment {} already exists",
                    definition.name
                )));
            }
            let created_at = Utc::now();
            let record = EnvironmentRecord {
                version: ENVIRONMENT_VERSION.into(),
                environment_id: format!(
                    "env_{}",
                    short_digest(&[
                        &definition.name,
                        &created_at
                            .timestamp_nanos_opt()
                            .unwrap_or_default()
                            .to_string(),
                        &self.instance_id,
                    ])
                ),
                name: definition.name.clone(),
                desired_state: definition.desired_state,
                env: definition.env,
                policy: definition.policy.map(Policy::canonical),
                provider: definition.provider,
                created_at,
                projects: BTreeMap::new(),
            };
            self.persist(&record)?;
            inner.environments.insert(record.name.clone(), record);
        }
        self.reconcile().await;
        self.environment(&definition.name).await
    }

    pub async fn destroy_environment(self: &Arc<Self>, name: &str) -> Result<(), EnvironmentError> {
        let name = self.resolve_environment(name).await?;
        let keys = self.keys_where(|key| key.0 == name).await;
        self.stop_keys(&keys).await;
        let mut inner = self.inner.lock().await;
        inner.environments.remove(&name);
        inner.workloads.retain(|key, _| key.0 != name);
        let prefix = format!("{name}/");
        inner.ports.retain(|key, _| !key.starts_with(&prefix));
        self.persist_ports(&inner.ports)?;
        let directory = self.environment_dir(&name);
        if directory.exists() {
            std::fs::remove_dir_all(directory)?;
        }
        Ok(())
    }

    pub async fn set_environment_state(
        self: &Arc<Self>,
        name: &str,
        desired: DesiredState,
        restart: bool,
    ) -> Result<EnvironmentView, EnvironmentError> {
        let name = self.resolve_environment(name).await?;
        let keys = self.keys_where(|key| key.0 == name).await;
        if restart {
            self.stop_keys(&keys).await;
        }
        {
            let mut inner = self.inner.lock().await;
            let record = inner.environments.get_mut(&name).expect("resolved");
            record.desired_state = desired;
            self.persist(record)?;
            if desired == DesiredState::Running {
                release_holds(&mut inner, &keys);
            }
        }
        self.reconcile().await;
        self.environment(&name).await
    }

    // ---- Project lifecycle -----------------------------------------------

    /// Add a project, or replace it with a new revision. Only this
    /// project's workloads are stopped and started.
    pub async fn add_project(
        self: &Arc<Self>,
        environment: &str,
        definition: ProjectDefinition,
    ) -> Result<ProjectView, EnvironmentError> {
        let environment = self.resolve_environment(environment).await?;
        validate_name("project", &definition.name)?;
        validate_env("project", &definition.env)?;
        if definition.revision.is_empty()
            || definition.revision.len() > 128
            || !definition
                .revision
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:@".contains(&byte))
        {
            return Err(EnvironmentError::Invalid(
                "revision must be 1-128 letters, digits, '-', '_', '.', ':', or '@'".into(),
            ));
        }
        if definition.workloads.is_empty() {
            return Err(EnvironmentError::Invalid(
                "a project needs at least one workload".into(),
            ));
        }
        let mut names = BTreeSet::new();
        let mut bundles = vec![];
        for workload in &definition.workloads {
            validate_name("workload", &workload.name)?;
            if !names.insert(workload.name.clone()) {
                return Err(EnvironmentError::Invalid(format!(
                    "workload {} is declared twice",
                    workload.name
                )));
            }
            let mut ports = BTreeSet::new();
            for port in &workload.ports {
                validate_name("port", &port.name)?;
                if port.port == 0 || !ports.insert(port.name.clone()) {
                    return Err(EnvironmentError::Invalid(format!(
                        "workload {} declares an invalid or duplicate port {}",
                        workload.name, port.name
                    )));
                }
            }
            if workload.kind == WorkloadKind::Task && !workload.ports.is_empty() {
                return Err(EnvironmentError::Invalid(format!(
                    "task {} cannot declare ports; only services listen",
                    workload.name
                )));
            }
            let bundle = WorkloadBundle::from_bytes(&workload.bundle)?;
            bundles.push(bundle);
        }
        let keys = self
            .keys_where(|key| key.0 == environment && key.1 == definition.name)
            .await;
        self.stop_keys(&keys).await;
        {
            let mut inner = self.inner.lock().await;
            let record = inner
                .environments
                .get(&environment)
                .ok_or_else(|| EnvironmentError::NotFound(format!("environment {environment}")))?;
            let project_id = project_id(&record.environment_id, &definition.name);
            let directory = self.project_dir(&environment, &definition.name);
            let bundle_dir = directory.join("bundles");
            if bundle_dir.exists() {
                std::fs::remove_dir_all(&bundle_dir)?;
            }
            std::fs::create_dir_all(&bundle_dir)?;
            let mut workloads = vec![];
            let mut digests = vec![];
            for (workload, bundle) in definition.workloads.iter().zip(&bundles) {
                std::fs::write(
                    bundle_dir.join(format!("{}.compute", workload.name)),
                    &workload.bundle,
                )?;
                let bundle_id = bundle.bundle_id()?;
                digests.push(format!("{}={bundle_id}", workload.name));
                workloads.push(WorkloadRecord {
                    workload_id: workload_id(&project_id, &workload.name),
                    name: workload.name.clone(),
                    kind: workload.kind,
                    bundle_id,
                    workload_identity: bundle.workload_id()?,
                    ports: workload.ports.clone(),
                    restart: workload.restart,
                    desired_state: workload.desired_state,
                });
            }
            workloads.sort_by(|left, right| left.name.cmp(&right.name));
            digests.sort();
            let project = ProjectRecord {
                project_id,
                name: definition.name.clone(),
                revision: definition.revision,
                revision_digest: compute_core::sha256_identity(digests.join("\n").as_bytes()),
                source: definition.source,
                desired_state: definition.desired_state,
                env: definition.env,
                workloads,
                deployed_at: Utc::now(),
            };
            let record = inner.environments.get_mut(&environment).expect("checked");
            record.projects.insert(project.name.clone(), project);
            let record = record.clone();
            self.persist(&record)?;
            inner
                .workloads
                .retain(|key, _| !(key.0 == environment && key.1 == definition.name));
        }
        self.reconcile().await;
        self.project(&environment, &definition.name).await
    }

    pub async fn remove_project(
        self: &Arc<Self>,
        environment: &str,
        project: &str,
    ) -> Result<(), EnvironmentError> {
        let environment = self.resolve_environment(environment).await?;
        self.require_project(&environment, project).await?;
        let keys = self
            .keys_where(|key| key.0 == environment && key.1 == project)
            .await;
        self.stop_keys(&keys).await;
        let mut inner = self.inner.lock().await;
        let record = inner.environments.get_mut(&environment).expect("resolved");
        record.projects.remove(project);
        let record = record.clone();
        self.persist(&record)?;
        inner
            .workloads
            .retain(|key, _| !(key.0 == environment && key.1 == project));
        let prefix = format!("{environment}/{project}/");
        inner.ports.retain(|key, _| !key.starts_with(&prefix));
        self.persist_ports(&inner.ports)?;
        let directory = self.project_dir(&environment, project);
        if directory.exists() {
            std::fs::remove_dir_all(directory)?;
        }
        Ok(())
    }

    pub async fn set_project_state(
        self: &Arc<Self>,
        environment: &str,
        project: &str,
        desired: DesiredState,
        restart: bool,
    ) -> Result<ProjectView, EnvironmentError> {
        let environment = self.resolve_environment(environment).await?;
        self.require_project(&environment, project).await?;
        let keys = self
            .keys_where(|key| key.0 == environment && key.1 == project)
            .await;
        if restart {
            self.stop_keys(&keys).await;
        }
        {
            let mut inner = self.inner.lock().await;
            let record = inner.environments.get_mut(&environment).expect("resolved");
            record
                .projects
                .get_mut(project)
                .expect("checked")
                .desired_state = desired;
            let record = record.clone();
            self.persist(&record)?;
            if desired == DesiredState::Running {
                release_holds(&mut inner, &keys);
            }
        }
        self.reconcile().await;
        self.project(&environment, project).await
    }

    // ---- Workload lifecycle ----------------------------------------------

    pub async fn set_workload_state(
        self: &Arc<Self>,
        environment: &str,
        project: &str,
        workload: &str,
        desired: DesiredState,
        restart: bool,
    ) -> Result<WorkloadView, EnvironmentError> {
        let environment = self.resolve_environment(environment).await?;
        let record = self
            .require_workload(&environment, project, workload)
            .await?;
        if record.kind == WorkloadKind::Task {
            return Err(EnvironmentError::Invalid(format!(
                "{workload} is a task; run it instead of starting or stopping it"
            )));
        }
        let key = (
            environment.clone(),
            project.to_string(),
            workload.to_string(),
        );
        if restart {
            self.stop_keys(std::slice::from_ref(&key)).await;
        }
        {
            let mut inner = self.inner.lock().await;
            let record = inner.environments.get_mut(&environment).expect("resolved");
            let project_record = record.projects.get_mut(project).expect("checked");
            project_record
                .workloads
                .iter_mut()
                .find(|candidate| candidate.name == workload)
                .expect("checked")
                .desired_state = desired;
            let record = record.clone();
            self.persist(&record)?;
            if desired == DesiredState::Running {
                release_holds(&mut inner, std::slice::from_ref(&key));
            }
        }
        self.reconcile().await;
        self.workload(&environment, project, workload).await
    }

    /// Run a task to completion. The result, receipt, and admission are
    /// recorded; the task's failure never affects any other workload.
    pub async fn run_task(
        self: &Arc<Self>,
        environment: &str,
        project: &str,
        workload: &str,
    ) -> Result<ExecutionRecord, EnvironmentError> {
        let environment = self.resolve_environment(environment).await?;
        let record = self
            .require_workload(&environment, project, workload)
            .await?;
        if record.kind != WorkloadKind::Task {
            return Err(EnvironmentError::Invalid(format!(
                "{workload} is a service; start it instead of running it"
            )));
        }
        let key = (
            environment.clone(),
            project.to_string(),
            workload.to_string(),
        );
        let generation = {
            let mut inner = self.inner.lock().await;
            let runtime = inner.workloads.entry(key.clone()).or_default();
            runtime.generation += 1;
            runtime.state = Some(ActualState::Running);
            runtime.started_at = Some(Utc::now());
            runtime.finished_at = None;
            runtime.generation
        };
        let outcome = self.execute(&key, None).await;
        self.finish(&key, generation, outcome, false).await;
        let inner = self.inner.lock().await;
        let execution_id = inner
            .workloads
            .get(&key)
            .and_then(|runtime| runtime.execution_id.clone());
        match execution_id.and_then(|id| inner.executions.get(&id).cloned()) {
            Some(record) => Ok(record),
            None => Err(EnvironmentError::Denied(
                inner
                    .workloads
                    .get(&key)
                    .and_then(|runtime| runtime.error.clone())
                    .unwrap_or_else(|| "the task did not execute".into()),
            )),
        }
    }

    // ---- Reconciliation ----------------------------------------------------

    /// Converge actual state toward desired state.
    pub async fn reconcile(self: &Arc<Self>) {
        let (to_start, to_stop) = {
            let inner = self.inner.lock().await;
            let mut to_start = vec![];
            let mut to_stop = vec![];
            for (environment, record) in &inner.environments {
                for (project, project_record) in &record.projects {
                    for workload in &project_record.workloads {
                        if workload.kind != WorkloadKind::Service {
                            continue;
                        }
                        let key = (environment.clone(), project.clone(), workload.name.clone());
                        let should_run = record.desired_state == DesiredState::Running
                            && project_record.desired_state == DesiredState::Running
                            && workload.desired_state == DesiredState::Running;
                        let runtime = inner.workloads.get(&key);
                        let active = runtime.is_some_and(|runtime| {
                            matches!(
                                runtime.state,
                                Some(ActualState::Starting | ActualState::Running)
                            )
                        });
                        let held = runtime.is_some_and(|runtime| runtime.held);
                        if should_run && !active && !held {
                            to_start.push(key);
                        } else if !should_run && active {
                            to_stop.push(key);
                        }
                    }
                }
            }
            (to_start, to_stop)
        };
        self.stop_keys(&to_stop).await;
        for key in to_start {
            self.start_service(key).await;
        }
    }

    /// Reconcile again after `delay`. Boxed: reconciliation starts services
    /// whose completion may schedule another reconciliation.
    fn reconcile_after(self: &Arc<Self>, delay: Duration) {
        let daemon = self.clone();
        let future: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                daemon.reconcile().await;
            });
        tokio::spawn(future);
    }

    async fn start_service(self: &Arc<Self>, key: Key) {
        let log_directory;
        let control;
        let generation;
        {
            let mut inner = self.inner.lock().await;
            let sequence = inner
                .workloads
                .get(&key)
                .map_or(0, |runtime| runtime.generation)
                + 1;
            log_directory = self
                .workload_dir(&key.0, &key.1, &key.2)
                .join("logs")
                .join(format!("{sequence:06}"));
            control = ExecutionControl::new().with_log_directory(&log_directory);
            let runtime = inner.workloads.entry(key.clone()).or_default();
            runtime.generation = sequence;
            runtime.state = Some(ActualState::Starting);
            runtime.control = Some(control.clone());
            runtime.started_at = Some(Utc::now());
            runtime.finished_at = None;
            runtime.exit_code = None;
            runtime.error = None;
            runtime.log_directory = Some(log_directory);
            generation = sequence;
        }
        let daemon = self.clone();
        let task_key = key.clone();
        let handle = tokio::spawn(async move {
            let outcome = daemon.execute(&task_key, Some((generation, control))).await;
            daemon.finish(&task_key, generation, outcome, true).await;
        });
        let mut inner = self.inner.lock().await;
        if let Some(runtime) = inner.workloads.get_mut(&key)
            && runtime.generation == generation
        {
            runtime.handle = Some(handle);
        }
    }

    /// Stop services and wait until each has stopped.
    async fn stop_keys(self: &Arc<Self>, keys: &[Key]) {
        let mut handles = vec![];
        {
            let mut inner = self.inner.lock().await;
            for key in keys {
                if let Some(runtime) = inner.workloads.get_mut(key)
                    && matches!(
                        runtime.state,
                        Some(ActualState::Starting | ActualState::Running)
                    )
                {
                    runtime.state = Some(ActualState::Stopping);
                    if let Some(control) = &runtime.control {
                        control.cancel();
                    }
                    if let Some(handle) = runtime.handle.take() {
                        handles.push((key.clone(), handle));
                    }
                }
            }
        }
        for (key, handle) in handles {
            if tokio::time::timeout(Duration::from_secs(30), handle)
                .await
                .is_err()
            {
                let mut inner = self.inner.lock().await;
                if let Some(runtime) = inner.workloads.get_mut(&key) {
                    runtime.error = Some("service did not stop within 30s".into());
                }
            }
            let mut inner = self.inner.lock().await;
            if let Some(runtime) = inner.workloads.get_mut(&key)
                && runtime.state == Some(ActualState::Stopping)
            {
                runtime.state = Some(ActualState::Stopped);
            }
        }
    }

    async fn keys_where(&self, predicate: impl Fn(&Key) -> bool) -> Vec<Key> {
        let inner = self.inner.lock().await;
        let mut keys = BTreeSet::new();
        for (environment, record) in &inner.environments {
            for (project, project_record) in &record.projects {
                for workload in &project_record.workloads {
                    let key = (environment.clone(), project.clone(), workload.name.clone());
                    if predicate(&key) {
                        keys.insert(key);
                    }
                }
            }
        }
        keys.into_iter().collect()
    }

    // ---- Execution ---------------------------------------------------------

    /// Admission, placement, and execution of one workload invocation.
    async fn execute(
        self: &Arc<Self>,
        key: &Key,
        service: Option<(u64, ExecutionControl)>,
    ) -> Outcome {
        let prepared = match self.prepare(key).await {
            Ok(prepared) => prepared,
            Err(error) => return Outcome::Failed(error.to_string()),
        };
        let Prepared {
            request,
            report,
            scope,
        } = prepared;
        let Some(selected) = report.selected.clone() else {
            let failure = report
                .failure
                .as_ref()
                .map(|failure| format!("{}: {}", failure.code, failure.message))
                .unwrap_or_else(|| "placement_failed".into());
            let decision = report.providers.iter().find_map(|provider| {
                provider
                    .admission
                    .clone()
                    .filter(|decision| !decision.admitted)
            });
            let reasons = decision
                .iter()
                .flat_map(|decision| decision.reasons.iter())
                .map(|reason| reason.message.clone())
                .collect::<Vec<_>>();
            let message = if reasons.is_empty() {
                failure
            } else {
                format!("{failure}: {}", reasons.join("; "))
            };
            return Outcome::Denied(message, decision.map(Box::new));
        };
        let placement = PlacementView {
            placement_id: Some(report.placement_id.clone()),
            provider: Some(selected.provider_id.clone()),
            node: Some(identity_label(&selected.provider_identity)),
        };
        let mut request = request;
        request.execution.scope = Some(scope);
        match service {
            Some((generation, control)) => {
                if selected.provider_kind != ProviderKind::Local {
                    return Outcome::Failed(format!(
                        "services run on the daemon's own node; placement selected {}",
                        selected.provider_id
                    ));
                }
                let binding = report.receipt_binding().expect("placed");
                request.expected.distribution_id = report
                    .requirements
                    .distribution
                    .as_ref()
                    .map(|distribution| distribution.id.clone());
                request.execution.isolation = Some(report.requirements.isolation);
                request.execution.placement = Some(binding);
                request.execution.policy = report.admission.request_policy.clone();
                let admission = match self.config.provider.admit(request.clone()).await {
                    Ok(admission) => admission,
                    Err(error) => return Outcome::Failed(error.to_string()),
                };
                if !admission.decision.admitted {
                    let reasons = admission
                        .decision
                        .reasons
                        .iter()
                        .map(|reason| reason.message.clone())
                        .collect::<Vec<_>>()
                        .join("; ");
                    return Outcome::Denied(reasons, Some(Box::new(admission.decision)));
                }
                {
                    let mut inner = self.inner.lock().await;
                    if let Some(runtime) = inner.workloads.get_mut(key)
                        && runtime.generation == generation
                    {
                        if control.is_cancelled() {
                            return Outcome::Failed("stopped before starting".into());
                        }
                        runtime.state = Some(ActualState::Running);
                        runtime.evidence.policy_id = Some(admission.decision.policy_id.clone());
                        runtime.evidence.admission_id =
                            Some(admission.decision.admission_id.clone());
                        runtime.placement = placement.clone();
                    }
                }
                match self
                    .config
                    .provider
                    .execute_controlled(request, admission, &control)
                    .await
                {
                    Ok(response) => {
                        let receipt_check = response
                            .result
                            .receipt
                            .as_ref()
                            .map(|receipt| report.verify_receipt(receipt));
                        let warning = match receipt_check {
                            Some(Err(error)) => Some(error),
                            None => Some("execution returned no receipt".into()),
                            Some(Ok(())) => None,
                        };
                        Outcome::Executed(Box::new(response.result), warning, placement)
                    }
                    Err(error) if error.admission.is_some() => {
                        Outcome::Denied(error.message.clone(), error.admission)
                    }
                    Err(error) => Outcome::Failed(error.to_string()),
                }
            }
            None => {
                match compute_placement::dispatch::execute(&self.pool, &report, request).await {
                    Ok(response) => Outcome::Executed(Box::new(response.result), None, placement),
                    Err(error) if error.admission.is_some() => {
                        Outcome::Denied(error.message.clone(), error.admission)
                    }
                    Err(error) => Outcome::Failed(error.to_string()),
                }
            }
        }
    }

    /// Record an outcome, unless a newer invocation superseded it.
    async fn finish(self: &Arc<Self>, key: &Key, generation: u64, outcome: Outcome, service: bool) {
        let mut restart = false;
        {
            let mut inner = self.inner.lock().await;
            let restart_policy = inner
                .environments
                .get(&key.0)
                .and_then(|record| record.projects.get(&key.1))
                .and_then(|project| project.workloads.iter().find(|w| w.name == key.2))
                .map(|workload| workload.restart);
            let Some(runtime) = inner.workloads.get_mut(key) else {
                return;
            };
            if runtime.generation != generation {
                return;
            }
            let stopping = runtime
                .control
                .as_ref()
                .is_some_and(ExecutionControl::is_cancelled);
            runtime.finished_at = Some(Utc::now());
            runtime.control = None;
            runtime.handle = None;
            let mut execution = None;
            match outcome {
                Outcome::Denied(message, decision) => {
                    runtime.state = Some(ActualState::Denied);
                    runtime.held = true;
                    runtime.error = Some(message);
                    if let Some(decision) = decision {
                        runtime.evidence.policy_id = Some(decision.policy_id.clone());
                        runtime.evidence.admission_id = Some(decision.admission_id.clone());
                    }
                }
                Outcome::Failed(message) => {
                    runtime.state = Some(if stopping {
                        ActualState::Stopped
                    } else {
                        ActualState::Failed
                    });
                    runtime.held = !stopping;
                    runtime.error = Some(message);
                }
                Outcome::Executed(result, warning, placement) => {
                    let succeeded = result.status == ExecutionStatus::Completed
                        && result.exit_code.is_none_or(|code| code == 0);
                    runtime.state = Some(match (service, stopping, succeeded) {
                        (_, true, _) => ActualState::Stopped,
                        (false, _, true) => ActualState::Completed,
                        (true, _, true) => ActualState::Stopped,
                        (_, _, false) => ActualState::Failed,
                    });
                    runtime.held = service && !stopping;
                    runtime.exit_code = result.exit_code;
                    runtime.error = warning.or_else(|| {
                        (!succeeded && !stopping).then(|| {
                            result
                                .error
                                .as_ref()
                                .map(|error| error.message.clone())
                                .unwrap_or_else(|| {
                                    format!("exited with status {:?}", result.exit_code)
                                })
                        })
                    });
                    runtime.execution_id = Some(result.execution_id.clone());
                    runtime.placement = placement.clone();
                    if let Some(admission) = &result.admission {
                        runtime.evidence.policy_id = Some(admission.policy_id.clone());
                        runtime.evidence.admission_id = Some(admission.admission_id.clone());
                    }
                    let receipt_id = result
                        .receipt
                        .as_ref()
                        .map(|receipt| receipt.receipt_hash.0.clone());
                    if let (Some(receipt), Some(receipt_id)) = (&result.receipt, &receipt_id) {
                        let directory = self.environment_dir(&key.0).join("receipts");
                        let _ = std::fs::create_dir_all(&directory);
                        if let Ok(bytes) = receipt.encoded_bytes() {
                            let _ = std::fs::write(
                                directory.join(format!("{}.json", result.execution_id)),
                                bytes,
                            );
                        }
                        runtime.evidence.receipt_ids.push(receipt_id.clone());
                        if runtime.evidence.receipt_ids.len() > RECENT_RECEIPTS {
                            runtime.evidence.receipt_ids.remove(0);
                        }
                    }
                    restart = service
                        && !stopping
                        && !succeeded
                        && restart_policy == Some(RestartPolicy::OnFailure);
                    if restart {
                        runtime.held = false;
                        runtime.restarts += 1;
                    }
                    execution = Some(ExecutionRecord {
                        execution_id: result.execution_id.clone(),
                        environment: key.0.clone(),
                        project: key.1.clone(),
                        workload: key.2.clone(),
                        kind: if service {
                            WorkloadKind::Service
                        } else {
                            WorkloadKind::Task
                        },
                        status: serde_json::to_value(&result.status)
                            .ok()
                            .and_then(|value| value.as_str().map(str::to_owned))
                            .unwrap_or_default(),
                        exit_code: result.exit_code,
                        started_at: runtime.started_at.unwrap_or_else(Utc::now),
                        finished_at: runtime.finished_at,
                        receipt_id,
                        policy_id: result
                            .admission
                            .as_ref()
                            .map(|value| value.policy_id.clone()),
                        admission_id: result
                            .admission
                            .as_ref()
                            .map(|value| value.admission_id.clone()),
                        placement_id: placement.placement_id.clone(),
                        provider: placement.provider.clone(),
                        stdout: result.stdout.text.clone(),
                        stderr: result.stderr.text.clone(),
                    });
                }
            }
            if let Some(execution) = execution {
                inner
                    .executions
                    .insert(execution.execution_id.clone(), execution);
            }
        }
        if restart {
            self.reconcile_after(self.config.restart_delay);
        }
    }

    async fn prepare(self: &Arc<Self>, key: &Key) -> Result<Prepared, EnvironmentError> {
        let (record, project, workload, ports) = {
            let mut inner = self.inner.lock().await;
            let record = inner
                .environments
                .get(&key.0)
                .cloned()
                .ok_or_else(|| EnvironmentError::NotFound(format!("environment {}", key.0)))?;
            let project = record
                .projects
                .get(&key.1)
                .cloned()
                .ok_or_else(|| EnvironmentError::NotFound(format!("project {}", key.1)))?;
            let workload = project
                .workloads
                .iter()
                .find(|workload| workload.name == key.2)
                .cloned()
                .ok_or_else(|| EnvironmentError::NotFound(format!("workload {}", key.2)))?;
            let ports = self.allocate_ports(&mut inner, key, &workload.ports)?;
            (record, project, workload, ports)
        };
        let stored = std::fs::read(
            self.project_dir(&key.0, &key.1)
                .join("bundles")
                .join(format!("{}.compute", key.2)),
        )?;
        let mut bundle = WorkloadBundle::from_bytes(&stored)?;
        // Configuration layering: workload < environment < project <
        // Compute-owned port bindings.
        for (name, value) in record.env.iter().chain(project.env.iter()) {
            bundle.workload.env.insert(name.clone(), value.clone());
        }
        for binding in &ports {
            bundle.workload.env.insert(
                format!(
                    "COMPUTE_PORT_{}",
                    binding.name.to_ascii_uppercase().replace('-', "_")
                ),
                binding.host.to_string(),
            );
        }
        if ports.len() == 1 {
            bundle
                .workload
                .env
                .insert("PORT".into(), ports[0].host.to_string());
        }
        if workload.kind == WorkloadKind::Service {
            let resources = &mut bundle.workload.resources;
            resources.stdout_bytes.get_or_insert(SERVICE_OUTPUT_BYTES);
            resources.stderr_bytes.get_or_insert(SERVICE_OUTPUT_BYTES);
        }
        bundle.validate()?;
        let mut request = ProviderRequest::bundle(bundle.to_bytes()?);
        request.expected.workload_id = Some(bundle.workload_id()?);
        request.expected.bundle_id = Some(bundle.bundle_id()?);
        let request_bytes = serde_json::to_vec(&request)?.len() as u64;
        let submission = SubmissionMode::Synchronous;
        let requirements = PlacementRequirements::from_bundle(
            &bundle,
            request_bytes,
            submission,
            &RequirementOptions::default(),
        )
        .map_err(|error| EnvironmentError::Invalid(error.to_string()))?;
        let contract = ExecutionContract::from_bundle(&bundle, Some(requirements.isolation))
            .map_err(|error| EnvironmentError::Invalid(error.to_string()))?;
        let context = AdmissionContext::new(&self.policy_sources(&record), contract);
        let explicit = record.provider.as_deref();
        let records = {
            let mut cache = self.cache.lock().await;
            self.pool
                .capabilities(&mut cache, DiscoveryMode::PreferCache, explicit, Utc::now())
                .await
        };
        let report = place(
            &self.pool.configs(),
            self.pool.policy(),
            &records,
            &requirements,
            &context,
            explicit,
        );
        Ok(Prepared {
            request,
            report,
            scope: ReceiptScope {
                environment_id: record.environment_id.clone(),
                environment: record.name.clone(),
                project_id: project.project_id.clone(),
                project: project.name.clone(),
                revision: project.revision.clone(),
                workload_id: workload.workload_id.clone(),
                workload: workload.name.clone(),
                workload_kind: workload.kind.as_str().into(),
            },
        })
    }

    fn policy_sources(&self, record: &EnvironmentRecord) -> Vec<(PolicySourceKind, Policy)> {
        let mut sources = vec![];
        if let Some(policy) = &self.config.policy {
            sources.push((PolicySourceKind::Local, policy.clone()));
        }
        if let Some(policy) = &record.policy {
            sources.push((PolicySourceKind::Environment, policy.clone()));
        }
        sources
    }

    /// Stable host ports per (environment, project, workload, port).
    fn allocate_ports(
        &self,
        inner: &mut Inner,
        key: &Key,
        ports: &[PortSpec],
    ) -> Result<Vec<PortBinding>, EnvironmentError> {
        let mut bindings = vec![];
        let mut changed = false;
        for port in ports {
            let name = format!("{}/{}/{}/{}", key.0, key.1, key.2, port.name);
            let host = match inner.ports.get(&name) {
                Some(host) => *host,
                None => {
                    let used = inner.ports.values().copied().collect::<BTreeSet<_>>();
                    let (low, high) = self.config.port_range;
                    let host = (low..=high)
                        .find(|candidate| {
                            !used.contains(candidate)
                                && std::net::TcpListener::bind(("127.0.0.1", *candidate)).is_ok()
                        })
                        .ok_or_else(|| {
                            EnvironmentError::Invalid(
                                "no free host port in the daemon's range".into(),
                            )
                        })?;
                    inner.ports.insert(name, host);
                    changed = true;
                    host
                }
            };
            bindings.push(PortBinding {
                name: port.name.clone(),
                logical: port.port,
                host,
            });
        }
        if changed {
            self.persist_ports(&inner.ports)?;
        }
        Ok(bindings)
    }

    // ---- Views -------------------------------------------------------------

    pub async fn environments(&self) -> Vec<EnvironmentSummary> {
        let names = self
            .inner
            .lock()
            .await
            .environments
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let mut summaries = vec![];
        for name in names {
            if let Ok(view) = self.environment(&name).await {
                summaries.push(EnvironmentSummary {
                    environment_id: view.environment_id,
                    name: view.name,
                    desired_state: view.desired_state,
                    actual_state: view.actual_state,
                    health: view.health,
                    project_count: view.project_count,
                });
            }
        }
        summaries
    }

    pub async fn environment(&self, name: &str) -> Result<EnvironmentView, EnvironmentError> {
        let name = self.resolve_environment(name).await?;
        let record = self
            .inner
            .lock()
            .await
            .environments
            .get(&name)
            .cloned()
            .expect("resolved");
        let mut projects = vec![];
        for project in record.projects.keys() {
            projects.push(self.project(&name, project).await?);
        }
        let actual_state = aggregate(
            record.desired_state,
            projects
                .iter()
                .map(|project| (project.desired_state, project.actual_state)),
        );
        let health = combine_health(projects.iter().map(|project| project.health));
        Ok(EnvironmentView {
            version: ENVIRONMENT_VERSION.into(),
            environment_id: record.environment_id.clone(),
            name: record.name.clone(),
            desired_state: record.desired_state,
            actual_state,
            health,
            created_at: record.created_at,
            policy_id: EffectivePolicy::compose(&self.policy_sources(&record)).policy_id,
            provider: record.provider.clone(),
            project_count: projects.len(),
            disk_bytes: directory_size(&self.environment_dir(&name)),
            projects,
        })
    }

    pub async fn project(
        &self,
        environment: &str,
        project: &str,
    ) -> Result<ProjectView, EnvironmentError> {
        let environment = self.resolve_environment(environment).await?;
        let (record, project_record) = {
            let inner = self.inner.lock().await;
            let record = inner.environments.get(&environment).expect("resolved");
            let project_record = record.projects.get(project).cloned().ok_or_else(|| {
                EnvironmentError::NotFound(format!("project {project} in {environment}"))
            })?;
            (record.clone(), project_record)
        };
        let mut workloads = vec![];
        for workload in &project_record.workloads {
            workloads.push(self.workload(&environment, project, &workload.name).await?);
        }
        let services = workloads
            .iter()
            .filter(|workload| workload.kind == WorkloadKind::Service)
            .collect::<Vec<_>>();
        let effective_desired = if record.desired_state == DesiredState::Stopped {
            DesiredState::Stopped
        } else {
            project_record.desired_state
        };
        let actual_state = aggregate(
            effective_desired,
            services
                .iter()
                .map(|workload| (workload.desired_state, workload.actual_state)),
        );
        let health = combine_health(services.iter().map(|workload| workload.health));
        Ok(ProjectView {
            project_id: project_record.project_id.clone(),
            name: project_record.name.clone(),
            revision: project_record.revision.clone(),
            revision_digest: project_record.revision_digest.clone(),
            source: project_record.source.clone(),
            desired_state: project_record.desired_state,
            actual_state,
            health,
            deployed_at: project_record.deployed_at,
            disk_bytes: directory_size(&self.project_dir(&environment, project)),
            workloads,
        })
    }

    pub async fn workload(
        &self,
        environment: &str,
        project: &str,
        workload: &str,
    ) -> Result<WorkloadView, EnvironmentError> {
        let environment = self.resolve_environment(environment).await?;
        let record = self
            .require_workload(&environment, project, workload)
            .await?;
        let key = (
            environment.clone(),
            project.to_string(),
            workload.to_string(),
        );
        let (state, runtime_view, ports) = {
            let inner = self.inner.lock().await;
            let ports = record
                .ports
                .iter()
                .filter_map(|port| {
                    inner
                        .ports
                        .get(&format!("{environment}/{project}/{workload}/{}", port.name))
                        .map(|host| PortBinding {
                            name: port.name.clone(),
                            logical: port.port,
                            host: *host,
                        })
                })
                .collect::<Vec<_>>();
            let runtime = inner.workloads.get(&key);
            let state = runtime
                .and_then(|runtime| runtime.state)
                .unwrap_or(match record.kind {
                    WorkloadKind::Task => ActualState::Pending,
                    WorkloadKind::Service => ActualState::Stopped,
                });
            let view = runtime.map(|runtime| {
                (
                    runtime.execution_id.clone(),
                    runtime.restarts,
                    runtime.started_at,
                    runtime.finished_at,
                    runtime.exit_code,
                    runtime.error.clone(),
                    runtime.placement.clone(),
                    runtime.evidence.clone(),
                    runtime
                        .log_directory
                        .as_ref()
                        .map(|path| path.display().to_string()),
                )
            });
            (state, view, ports)
        };
        let (
            execution_id,
            restarts,
            started_at,
            finished_at,
            exit_code,
            error,
            placement,
            evidence,
            log_directory,
        ) = runtime_view.unwrap_or_default();
        let health = match (record.kind, state) {
            (WorkloadKind::Service, ActualState::Running) if ports.is_empty() => Health::Healthy,
            (WorkloadKind::Service, ActualState::Running) => {
                let mut healthy = true;
                for binding in &ports {
                    let reachable = tokio::time::timeout(
                        Duration::from_millis(300),
                        tokio::net::TcpStream::connect(("127.0.0.1", binding.host)),
                    )
                    .await
                    .is_ok_and(|result| result.is_ok());
                    healthy &= reachable;
                }
                if healthy {
                    Health::Healthy
                } else {
                    Health::Unhealthy
                }
            }
            (WorkloadKind::Service, ActualState::Failed | ActualState::Denied) => Health::Unhealthy,
            (WorkloadKind::Task, ActualState::Failed | ActualState::Denied) => Health::Unhealthy,
            (WorkloadKind::Task, _) => Health::Healthy,
            _ => Health::Unknown,
        };
        let stored = std::fs::read(
            self.project_dir(&environment, project)
                .join("bundles")
                .join(format!("{workload}.compute")),
        )
        .ok()
        .and_then(|bytes| WorkloadBundle::from_bytes(&bytes).ok());
        let resources = ResourceView {
            cpu: "not_measured".into(),
            memory_limit_bytes: stored
                .as_ref()
                .and_then(|bundle| bundle.workload.resources.memory_bytes),
            timeout_ms: stored.as_ref().and_then(|bundle| {
                bundle
                    .workload
                    .resources
                    .wall_time
                    .map(|value| u64::try_from(value.as_millis()).unwrap_or(u64::MAX))
            }),
            disk_bytes: directory_size(&self.workload_dir(&environment, project, workload)),
            network: stored
                .as_ref()
                .map(|bundle| bundle.workload.network.to_string())
                .unwrap_or_default(),
        };
        Ok(WorkloadView {
            workload_id: record.workload_id.clone(),
            name: record.name.clone(),
            kind: record.kind,
            desired_state: record.desired_state,
            actual_state: state,
            health,
            runtime: stored
                .as_ref()
                .map(|bundle| bundle.workload.runtime.to_string())
                .unwrap_or_default(),
            bundle_id: record.bundle_id.clone(),
            execution_id,
            ports,
            restarts,
            started_at,
            finished_at,
            exit_code,
            error,
            placement,
            evidence,
            resources,
            log_directory,
        })
    }

    pub async fn execution(&self, execution_id: &str) -> Result<ExecutionRecord, EnvironmentError> {
        self.inner
            .lock()
            .await
            .executions
            .get(execution_id)
            .cloned()
            .ok_or_else(|| EnvironmentError::NotFound(format!("execution {execution_id}")))
    }

    /// The most recent log output of a workload.
    pub async fn logs(
        &self,
        environment: &str,
        project: &str,
        workload: &str,
    ) -> Result<(String, String), EnvironmentError> {
        let view = self.workload(environment, project, workload).await?;
        let Some(directory) = view.log_directory else {
            return Ok((String::new(), String::new()));
        };
        let read = |name: &str| {
            std::fs::read(Path::new(&directory).join(name))
                .map(|bytes| {
                    let start = bytes.len().saturating_sub(64 * 1024);
                    String::from_utf8_lossy(&bytes[start..]).into_owned()
                })
                .unwrap_or_default()
        };
        Ok((read("stdout.log"), read("stderr.log")))
    }

    // ---- Helpers -----------------------------------------------------------

    async fn resolve_environment(&self, name_or_id: &str) -> Result<String, EnvironmentError> {
        let inner = self.inner.lock().await;
        if inner.environments.contains_key(name_or_id) {
            return Ok(name_or_id.to_string());
        }
        inner
            .environments
            .values()
            .find(|record| record.environment_id == name_or_id)
            .map(|record| record.name.clone())
            .ok_or_else(|| EnvironmentError::NotFound(format!("environment {name_or_id}")))
    }

    async fn require_project(
        &self,
        environment: &str,
        project: &str,
    ) -> Result<(), EnvironmentError> {
        let inner = self.inner.lock().await;
        inner
            .environments
            .get(environment)
            .and_then(|record| record.projects.get(project))
            .map(|_| ())
            .ok_or_else(|| {
                EnvironmentError::NotFound(format!("project {project} in {environment}"))
            })
    }

    async fn require_workload(
        &self,
        environment: &str,
        project: &str,
        workload: &str,
    ) -> Result<WorkloadRecord, EnvironmentError> {
        let inner = self.inner.lock().await;
        inner
            .environments
            .get(environment)
            .and_then(|record| record.projects.get(project))
            .and_then(|project| project.workloads.iter().find(|w| w.name == workload))
            .cloned()
            .ok_or_else(|| {
                EnvironmentError::NotFound(format!(
                    "workload {workload} in {environment}/{project}"
                ))
            })
    }

    fn environment_dir(&self, environment: &str) -> PathBuf {
        self.config.state_dir.join("environments").join(environment)
    }

    fn project_dir(&self, environment: &str, project: &str) -> PathBuf {
        self.environment_dir(environment)
            .join("projects")
            .join(project)
    }

    fn workload_dir(&self, environment: &str, project: &str, workload: &str) -> PathBuf {
        self.project_dir(environment, project)
            .join("workloads")
            .join(workload)
    }

    fn persist(&self, record: &EnvironmentRecord) -> Result<(), EnvironmentError> {
        let directory = self.environment_dir(&record.name);
        std::fs::create_dir_all(&directory)?;
        let temporary = directory.join(".environment.json.tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(record)?)?;
        std::fs::rename(temporary, directory.join("environment.json"))?;
        Ok(())
    }

    fn persist_ports(&self, ports: &BTreeMap<String, u16>) -> Result<(), EnvironmentError> {
        std::fs::write(
            self.config.state_dir.join("ports.json"),
            serde_json::to_vec_pretty(ports)?,
        )?;
        Ok(())
    }
}

struct Prepared {
    request: ProviderRequest,
    report: PlacementReport,
    scope: ReceiptScope,
}

fn release_holds(inner: &mut Inner, keys: &[Key]) {
    for key in keys {
        if let Some(runtime) = inner.workloads.get_mut(key) {
            runtime.held = false;
        }
    }
}

/// Aggregate child states into a parent state.
fn aggregate(
    desired: DesiredState,
    children: impl Iterator<Item = (DesiredState, ActualState)>,
) -> ActualState {
    let children = children
        .filter(|(desired, _)| *desired == DesiredState::Running)
        .map(|(_, actual)| actual)
        .collect::<Vec<_>>();
    if desired == DesiredState::Stopped {
        return if children
            .iter()
            .any(|state| matches!(state, ActualState::Running | ActualState::Stopping))
        {
            ActualState::Stopping
        } else {
            ActualState::Stopped
        };
    }
    if children.is_empty() {
        return ActualState::Running;
    }
    let running = children
        .iter()
        .filter(|state| **state == ActualState::Running)
        .count();
    if running == children.len() {
        ActualState::Running
    } else if children
        .iter()
        .all(|state| matches!(state, ActualState::Starting | ActualState::Pending))
    {
        ActualState::Starting
    } else if running == 0
        && children
            .iter()
            .all(|state| matches!(state, ActualState::Failed | ActualState::Denied))
    {
        ActualState::Failed
    } else {
        ActualState::Degraded
    }
}

fn combine_health(values: impl Iterator<Item = Health>) -> Health {
    let values = values.collect::<Vec<_>>();
    if values.contains(&Health::Unhealthy) {
        Health::Unhealthy
    } else if values.iter().all(|value| *value == Health::Healthy) {
        Health::Healthy
    } else {
        Health::Unknown
    }
}

fn directory_size(path: &Path) -> u64 {
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.metadata().ok())
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
        .sum()
}

fn identity_label(identity: &ProviderIdentity) -> String {
    match identity {
        ProviderIdentity::Local { id } => id.clone(),
        ProviderIdentity::Remote { id, .. } => id.clone(),
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
