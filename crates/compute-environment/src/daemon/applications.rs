//! Applications: a compatibility view over the one deployment model.
//!
//! ```text
//! compute application …            (this module: resolve, invoke, adapt)
//!   → Environment `application-<name>`, owned by the caller
//!   → Computer            placed and provisioned by the computer controller
//!   → Project `<name>`    its source imported into the computer (durable jobs)
//!   → Version             publish_version: source, package digest, artifact
//!   → Rollout             deploy_version / rollback_version: the deployment
//!   → target job          the process start, run in the target session
//!   → Endpoint            the computer's endpoint for the process's port
//!   → Receipt             the target's receipt for that job
//! ```
//!
//! Nothing here is a controller, a store, a supervisor, or a state machine.
//! Every operation resolves the application to its canonical records,
//! invokes the canonical operation — the same one the computer, version, and
//! rollout APIs invoke, with the same owner-bound authorization — and
//! describes the result in application terms. An application version is a
//! rollout; its number counts the project's rollouts in its environment.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use compute_core::{
    ApplicationIdentity, ComputerLifecycle, ComputerRequirements, ComputerStatus, InputSource,
    ProcessDesired, ProcessKind, ProcessSpec, ProcessState, ProjectSpec,
    ProviderRuntimeRequirement, RepositorySpec, RuntimeKind, WorkloadBundle,
};
use compute_state::{RolloutKind, RolloutRecord, RolloutStatus, VersionRecord, VersionStatus};

use super::Daemon;
use super::computers::imported_source;
use crate::EnvironmentError;
use crate::model::*;
use crate::status::*;

/// How long a deploy waits for each canonical step it drives: the computer
/// to run, to hold the imported source, and the version to publish.
const STEP_DEADLINE: Duration = Duration::from_secs(5 * 60);
const POLL: Duration = Duration::from_millis(100);
/// The lines of the process's log a logs request returns.
const LOG_LINES: usize = 1000;

/// The environment an application is: one computer of its own.
pub fn application_environment(name: &str) -> String {
    format!("application-{name}")
}

impl Daemon {
    /// Every application on this node: every environment that is one.
    pub async fn applications(&self) -> Result<Vec<ApplicationView>, EnvironmentError> {
        let mut applications = vec![];
        for environment in self.environments().await? {
            let Some(name) = environment.name.strip_prefix("application-") else {
                continue;
            };
            if let Ok(application) = self.application(name).await {
                applications.push(application);
            }
        }
        Ok(applications)
    }

    /// One application: its computer, endpoint, and versions.
    pub async fn application(&self, name: &str) -> Result<ApplicationView, EnvironmentError> {
        let identity = ApplicationIdentity::new(name, None)?;
        let computer = self.application_computer(name).await?;
        let deployments = self.application_deployments_of(name, &computer).await?;
        let active = deployments
            .iter()
            .find(|deployment| deployment.active)
            .cloned();
        let deploying = deployments
            .iter()
            .find(|deployment| deployment.state == ApplicationDeploymentState::Deploying)
            .cloned();
        let process = computer
            .desired
            .processes
            .iter()
            .find(|process| process.name == name);
        let observed = computer.observed.processes.get(name);
        // The computer's reality first: an application is never more alive
        // than the machine it runs on.
        let status = match computer.status {
            ComputerStatus::Running => {
                if process.is_some_and(|process| process.desired == ProcessDesired::Stopped) {
                    if observed.is_some_and(|seen| seen.state == ProcessState::Running) {
                        "stopping"
                    } else {
                        "stopped"
                    }
                } else if deploying.is_some() && active.is_none() {
                    "deploying"
                } else {
                    match observed.map(|seen| seen.state) {
                        Some(ProcessState::Running) => "running",
                        Some(ProcessState::Failed | ProcessState::Exited) => "failed",
                        Some(ProcessState::Stopped) => "stopped",
                        Some(ProcessState::Starting) | None => "starting",
                    }
                }
            }
            status => status.observed(),
        }
        .to_owned();
        let endpoint = self.application_endpoint(name, &computer);
        Ok(ApplicationView {
            application: identity,
            status,
            node: self.node_url(),
            endpoint,
            active,
            deploying,
            deployments,
            environment: Some(computer.environment.clone()),
            computer: Some(computer),
        })
    }

    /// An application's versions, newest first: its project's rollouts in
    /// its environment.
    pub async fn application_deployments(
        &self,
        name: &str,
    ) -> Result<Vec<ApplicationDeploymentView>, EnvironmentError> {
        let computer = self.application_computer(name).await?;
        self.application_deployments_of(name, &computer).await
    }

    /// One version, by number (`3`, `v3`) or deployment (rollout) ID.
    pub async fn application_deployment(
        &self,
        name: &str,
        target: &str,
    ) -> Result<ApplicationDeploymentView, EnvironmentError> {
        let deployments = self.application_deployments(name).await?;
        find_version(&deployments, target)
            .cloned()
            .ok_or_else(|| EnvironmentError::NotFound(format!("{name} {target}")))
    }

    /// The receipt of the execution that made a version run, exactly as
    /// the computer's target issued it for the job: canonical bytes.
    pub async fn application_receipt(
        &self,
        name: &str,
        target: &str,
    ) -> Result<Vec<u8>, EnvironmentError> {
        let deployment = self.application_deployment(name, target).await?;
        let records = deployment
            .canonical
            .as_ref()
            .expect("an application deployment is a rollout");
        let (Some(target), Some(job_id)) = (&records.target, &records.job_id) else {
            return Err(EnvironmentError::NotFound(format!(
                "{name} v{} has no execution yet",
                deployment.version
            )));
        };
        let receipt = self
            .target_client(target)?
            .job_receipt(job_id)
            .await
            .map_err(|error| EnvironmentError::RuntimeUnavailable(error.to_string()))?;
        Ok(receipt.receipt.encoded_bytes()?)
    }

    /// Release a new version: import the artifact's source into the
    /// application's computer, publish it as a version of its project, and
    /// deploy that version. The first deployment creates the environment,
    /// placed and owned like any other computer.
    pub async fn deploy_application(
        self: &Arc<Self>,
        name: &str,
        operator: &str,
        request: ApplicationDeployRequest,
    ) -> Result<ApplicationDeploymentView, EnvironmentError> {
        if !self.config.execution.deployments {
            return Err(EnvironmentError::Invalid(
                "this node does not host application deployments".into(),
            ));
        }
        let resolved = resolve_application(name, &request).await?;
        let bundle = WorkloadBundle::from_bytes(&resolved.bundle)?;
        let command = process_command(&bundle)?;
        let archive = source_archive(&bundle)?;
        let environment = application_environment(name);
        // The environment contract is checked before anything is recorded.
        let existing = self.stored_environment(&environment).await?;
        let config = match (&request.env, &existing) {
            (Some(env), _) => env.clone(),
            (None, Some(record)) => record.value.config.clone(),
            (None, None) => BTreeMap::new(),
        };
        let missing = resolved
            .required_env
            .iter()
            .filter(|key| !config.contains_key(*key))
            .cloned()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(EnvironmentError::Invalid(format!(
                "{name} requires configuration it was not given: {}",
                missing.join(", ")
            )));
        }
        // The application's computer: placed now, refused with every
        // target's reasons when none can host it.
        if existing.is_none() {
            let created = self
                .create_computer_environment(
                    ComputerEnvironmentDefinition {
                        name: environment.clone(),
                        desired_state: DesiredState::Running,
                        env: config.clone(),
                        policy: None,
                        computer: ComputerRequest {
                            lifecycle: ComputerLifecycle::Persistent,
                            requirements: requirements(&bundle),
                            target: None,
                            ttl_seconds: None,
                        },
                        contents: Default::default(),
                        recipe: None,
                    },
                    operator,
                )
                .await;
            match created {
                Ok(_) | Err(EnvironmentError::Conflict(_)) => {}
                Err(EnvironmentError::Invalid(reason)) => {
                    return Err(EnvironmentError::Invalid(format!(
                        "{name} has no computer to run on: {reason}"
                    )));
                }
                Err(error) => return Err(error),
            }
        }
        // Owner-bound, like every change to a computer.
        let record = self.owned_environment(&environment, operator).await?;
        self.require_live(&record).await?;
        self.await_computer(&environment, "run", |view| {
            view.status == ComputerStatus::Running
        })
        .await?;
        let label = resolved
            .evidence
            .as_ref()
            .map(|evidence| evidence.artifact_id.clone())
            .unwrap_or(bundle.bundle_id()?);
        let commit = self
            .import_source(
                &environment,
                operator,
                name,
                &archive,
                &format!("{name} {label}"),
            )
            .await?;
        // Desired state: the configuration, the repository at the imported
        // commit, the project, and the process that runs it.
        let taken = self.ports_in_use().await;
        let range = self.config.port_range;
        let short = commit[..12].to_owned();
        let view = self
            .change_environment(
                &environment,
                operator,
                format!("application {name} at {short}"),
                None,
                |value| {
                    value.config = config.clone();
                    let contents = value.contents.get_or_insert_with(Default::default);
                    let repository = RepositorySpec {
                        name: name.to_owned(),
                        url: imported_source(name),
                        revision: commit.clone(),
                        sync: 0,
                    };
                    upsert(&mut contents.repositories, repository, |item| &item.name);
                    let project = ProjectSpec {
                        name: name.to_owned(),
                        repository: name.to_owned(),
                        build: vec![],
                        test: vec![],
                        commands: Default::default(),
                        checks: vec![],
                    };
                    upsert(&mut contents.projects, project, |item| &item.name);
                    let current = contents.processes.iter().find(|item| item.name == name);
                    // The endpoint is stable: a process keeps its port.
                    let port = current.and_then(|process| process.port).or_else(|| {
                        (range.0..=range.1).find(|port| {
                            !taken.contains(port)
                                && !contents
                                    .processes
                                    .iter()
                                    .any(|other| other.port == Some(*port))
                        })
                    });
                    let Some(port) = port else {
                        return Err(EnvironmentError::Conflict(format!(
                            "no endpoint port is free in {}-{}",
                            range.0, range.1
                        )));
                    };
                    let process = ProcessSpec {
                        name: name.to_owned(),
                        kind: ProcessKind::Application,
                        runtime: Some(runtime_requirement(&bundle)),
                        command: command.clone(),
                        repository: Some(name.to_owned()),
                        env: process_env(&bundle),
                        desired: ProcessDesired::Running,
                        port: Some(port),
                        restart: current.map_or(0, |process| process.restart),
                        readiness: None,
                        restart_policy: Default::default(),
                        max_restarts: compute_core::DEFAULT_MAX_RESTARTS,
                    };
                    upsert(&mut contents.processes, process, |item| &item.name);
                    Ok(())
                },
            )
            .await?;
        let generation = view.desired.generation;
        self.await_computer(&environment, "hold the imported source", |view| {
            view.observed.converged_generation >= generation
                && view
                    .observed
                    .repositories
                    .get(name)
                    .is_some_and(|seen| seen.commit.as_deref() == Some(commit.as_str()))
        })
        .await?;
        let version = self
            .publish_version_from(
                name,
                operator,
                PublishRequest {
                    environment: environment.clone(),
                    version: None,
                },
                resolved.evidence,
            )
            .await?;
        let version = self.await_published(name, &version.version).await?;
        let rollout = self
            .deploy_version(
                name,
                operator,
                DeployVersionRequest {
                    environment,
                    version: version.version,
                    expected_generation: None,
                },
            )
            .await?;
        self.application_deployment(name, &rollout.rollout_id).await
    }

    /// Roll back to an earlier version: the canonical rollback of the
    /// project in the application's environment to that version.
    pub async fn rollback_application(
        self: &Arc<Self>,
        name: &str,
        operator: &str,
        request: ApplicationRollbackRequest,
    ) -> Result<ApplicationDeploymentView, EnvironmentError> {
        let deployments = self.application_deployments(name).await?;
        let target = find_version(&deployments, &request.target).ok_or_else(|| {
            EnvironmentError::NotFound(format!("{name} has no version {}", request.target))
        })?;
        if target.state == ApplicationDeploymentState::Failed {
            return Err(EnvironmentError::Invalid(format!(
                "{name} v{} never served; roll back to a version that did",
                target.version
            )));
        }
        let records = target.canonical.clone().expect("a rollout");
        let rollout = self
            .rollback_version(
                name,
                operator,
                RollbackRequest {
                    environment: records.environment,
                    version: Some(records.version),
                },
            )
            .await?;
        self.application_deployment(name, &rollout.rollout_id).await
    }

    /// Stop the application's process. Its computer, versions, endpoint,
    /// and evidence remain.
    pub async fn stop_application(
        self: &Arc<Self>,
        name: &str,
        operator: &str,
    ) -> Result<ApplicationView, EnvironmentError> {
        self.set_process(
            &application_environment(name),
            operator,
            name,
            ProcessDesired::Stopped,
        )
        .await?;
        self.application(name).await
    }

    /// The application's output: its process's log in the computer, read
    /// by a durable job on the target.
    pub async fn application_logs(
        self: &Arc<Self>,
        name: &str,
        operator: &str,
    ) -> Result<serde_json::Value, EnvironmentError> {
        let logs = self
            .computer_logs(
                &application_environment(name),
                operator,
                Some(name),
                LOG_LINES,
            )
            .await?;
        Ok(serde_json::json!({
            "stdout": logs["log"],
            "stderr": "",
            "process": name,
            "job_id": logs["evidence"]["job_id"],
            "execution_id": logs["evidence"]["execution_id"],
        }))
    }

    /// This node as a `compute.remote@1` provider: capabilities, health,
    /// runs, and jobs.
    pub fn remote_service(&self) -> Option<Arc<compute_provider::RemoteService>> {
        self.remote.clone()
    }

    // ---- Resolving an application to its canonical records ---------------

    async fn stored_environment(
        &self,
        environment: &str,
    ) -> Result<Option<compute_state::Stored<compute_state::EnvironmentRecord>>, EnvironmentError>
    {
        self.refresh_for_read().await?;
        Ok(self
            .inner
            .lock()
            .await
            .desired
            .environment(environment)
            .cloned())
    }

    /// The application's computer, when it is one this node hosts.
    async fn application_computer(&self, name: &str) -> Result<ComputerView, EnvironmentError> {
        ApplicationIdentity::new(name, None)?;
        let not_deployed = || {
            EnvironmentError::NotFound(format!("application {name} is not deployed on this node"))
        };
        let computer = match self.computer(&application_environment(name)).await {
            Ok(computer) => computer,
            Err(EnvironmentError::NotFound(_)) => return Err(not_deployed()),
            Err(error) => return Err(error),
        };
        if !computer
            .desired
            .projects
            .iter()
            .any(|project| project.name == name)
        {
            return Err(not_deployed());
        }
        Ok(computer)
    }

    fn application_endpoint(&self, name: &str, computer: &ComputerView) -> Option<String> {
        computer
            .endpoints
            .iter()
            .find(|endpoint| endpoint.process == name)
            .and_then(|endpoint| endpoint.url.clone())
    }

    async fn application_deployments_of(
        &self,
        name: &str,
        computer: &ComputerView,
    ) -> Result<Vec<ApplicationDeploymentView>, EnvironmentError> {
        // Newest first.
        let rollouts = self
            .rollouts(Some(&computer.environment), Some(name))
            .await?;
        let mut versions: BTreeMap<String, Option<VersionRecord>> = BTreeMap::new();
        for rollout in &rollouts {
            if !versions.contains_key(&rollout.version) {
                let version = self.version(name, &rollout.version).await.ok();
                versions.insert(rollout.version.clone(), version);
            }
        }
        let stopped = computer
            .desired
            .processes
            .iter()
            .any(|process| process.name == name && process.desired == ProcessDesired::Stopped);
        let endpoint = self.application_endpoint(name, computer);
        let count = rollouts.len() as u64;
        let number = |index: usize| count - index as u64;
        let mut views = vec![];
        for (index, rollout) in rollouts.iter().enumerate() {
            let state = match rollout.status {
                RolloutStatus::Applying => ApplicationDeploymentState::Deploying,
                RolloutStatus::Failed => ApplicationDeploymentState::Failed,
                RolloutStatus::Superseded => ApplicationDeploymentState::Superseded,
                RolloutStatus::Active if stopped => ApplicationDeploymentState::Stopped,
                RolloutStatus::Active => ApplicationDeploymentState::Active,
            };
            // The same version deployed again after another replaced it.
            let earlier = &rollouts[index + 1..];
            let rollback_of = earlier
                .first()
                .filter(|previous| {
                    previous.version_id != rollout.version_id
                        || rollout.kind == RolloutKind::Rollback
                })
                .and_then(|_| {
                    earlier
                        .iter()
                        .position(|older| {
                            older.version_id == rollout.version_id
                                && older.status != RolloutStatus::Failed
                        })
                        .map(|position| number(index + 1 + position))
                });
            let version = versions.get(&rollout.version).cloned().flatten();
            let artifact = version
                .as_ref()
                .and_then(|version| version.artifact.clone());
            let (job_id, execution_id, receipt) = restart_evidence(rollout);
            views.push(ApplicationDeploymentView {
                application: name.to_owned(),
                version: number(index),
                deployment_id: rollout.rollout_id.clone(),
                state,
                active: rollout.status == RolloutStatus::Active,
                rollback_of,
                endpoint: endpoint.clone(),
                runtime: artifact
                    .as_ref()
                    .and_then(|artifact| artifact.runtime.clone()),
                runtime_version: artifact
                    .as_ref()
                    .and_then(|artifact| artifact.runtime_version.clone()),
                placement: None,
                artifact,
                failure: rollout.failure.clone(),
                receipt: receipt.clone(),
                execution_receipts: receipt.into_iter().collect(),
                created_at: rollout.created_at,
                completed_at: rollout.completed_at,
                canonical: Some(ApplicationRecords {
                    environment: computer.environment.clone(),
                    environment_id: computer.environment_id.clone(),
                    computer_id: compute_state::ids::computer(&computer.environment_id),
                    project: name.to_owned(),
                    rollout_id: rollout.rollout_id.clone(),
                    rollout_kind: rollout.kind,
                    rollout_status: rollout.status,
                    version_id: rollout.version_id.clone(),
                    version: rollout.version.clone(),
                    commit: version.as_ref().and_then(|version| version.commit.clone()),
                    package_digest: version
                        .as_ref()
                        .and_then(|version| version.package_digest.clone()),
                    target: computer.target.clone(),
                    session_id: computer.session_id.clone(),
                    job_id,
                    execution_id,
                }),
            });
        }
        Ok(views)
    }

    // ---- Waiting on canonical operations -----------------------------------

    /// Wait until the application's computer is as `wanted`, failing as
    /// soon as it cannot get there: a computer that ended, is lost, or whose
    /// target is not answering says so.
    pub(crate) async fn await_computer(
        &self,
        environment: &str,
        what: &str,
        wanted: impl Fn(&ComputerView) -> bool,
    ) -> Result<ComputerView, EnvironmentError> {
        self.await_computer_within(environment, what, STEP_DEADLINE, wanted)
            .await
    }

    pub(crate) async fn await_computer_within(
        &self,
        environment: &str,
        what: &str,
        within: Duration,
        wanted: impl Fn(&ComputerView) -> bool,
    ) -> Result<ComputerView, EnvironmentError> {
        let deadline = tokio::time::Instant::now() + within;
        // An environment created a moment ago may not be visible to this read
        // yet: absent is tolerated briefly, then it is an error as before.
        let grace = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let view = match self.computer(environment).await {
                Ok(view) => view,
                Err(EnvironmentError::NotFound(_)) if tokio::time::Instant::now() < grace => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                Err(error) => return Err(error),
            };
            if wanted(&view) {
                return Ok(view);
            }
            let reason = || {
                view.failure
                    .as_ref()
                    .map(|failure| format!(": {} ({})", failure.message, failure.code))
                    .unwrap_or_default()
            };
            match view.status {
                status if status.is_terminal() => {
                    return Err(EnvironmentError::Conflict(format!(
                        "{environment}'s computer is {status}{}",
                        reason()
                    )));
                }
                ComputerStatus::Lost => {
                    return Err(EnvironmentError::Conflict(format!(
                        "{environment}'s computer is lost{}; replace or destroy it",
                        reason()
                    )));
                }
                ComputerStatus::Unreachable => {
                    return Err(EnvironmentError::RuntimeUnavailable(format!(
                        "{environment}'s computer is unreachable{}",
                        reason()
                    )));
                }
                _ => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(EnvironmentError::RuntimeUnavailable(format!(
                    "{environment}'s computer did not {what} in time ({}){}",
                    view.status,
                    reason()
                )));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    async fn await_published(
        &self,
        project: &str,
        label: &str,
    ) -> Result<VersionRecord, EnvironmentError> {
        let deadline = tokio::time::Instant::now() + STEP_DEADLINE;
        loop {
            let version = self.version(project, label).await?;
            match version.status {
                VersionStatus::Published => return Ok(version),
                VersionStatus::Failed => {
                    return Err(EnvironmentError::Invalid(format!(
                        "{project} {label} was not published: {}",
                        version.failure.unwrap_or_default()
                    )));
                }
                VersionStatus::Publishing if tokio::time::Instant::now() >= deadline => {
                    return Err(EnvironmentError::RuntimeUnavailable(format!(
                        "{project} {label} did not publish in time"
                    )));
                }
                VersionStatus::Publishing => tokio::time::sleep(POLL).await,
            }
        }
    }

    fn node_url(&self) -> String {
        self.config
            .public_url
            .clone()
            .unwrap_or_else(|| self.instance_id.clone())
    }
}

/// The job, execution, and receipt a rollout recorded for the process start
/// that made its version run.
fn restart_evidence(rollout: &RolloutRecord) -> (Option<String>, Option<String>, Option<String>) {
    rollout
        .steps
        .iter()
        .find(|step| step.name == "Restart applications")
        .map(|step| {
            (
                step.job_id.clone(),
                step.execution_id.clone(),
                step.receipt.clone(),
            )
        })
        .unwrap_or_default()
}

fn upsert<T>(items: &mut Vec<T>, item: T, name: impl Fn(&T) -> &String) {
    match items
        .iter_mut()
        .find(|existing| name(existing) == name(&item))
    {
        Some(existing) => *existing = item,
        None => items.push(item),
    }
}

fn find_version<'a>(
    deployments: &'a [ApplicationDeploymentView],
    target: &str,
) -> Option<&'a ApplicationDeploymentView> {
    deployments.iter().find(|deployment| {
        deployment.deployment_id == target || parse_version(target) == Some(deployment.version)
    })
}

fn parse_version(target: &str) -> Option<u64> {
    target.strip_prefix('v').unwrap_or(target).parse().ok()
}

/// What the application's computer must be: what its bundle asks of the
/// machine. Placement matches it against the pool's targets.
fn requirements(bundle: &WorkloadBundle) -> ComputerRequirements {
    let workload = &bundle.workload;
    ComputerRequirements {
        cpu_count: workload.resources.cpu_count,
        memory_bytes: workload
            .resources
            .memory_required_bytes
            .or(workload.resources.memory_bytes),
        architecture: workload.architecture.clone(),
        network: workload.network.clone(),
        runtimes: vec![runtime_requirement(bundle)],
        ..Default::default()
    }
}

fn runtime_requirement(bundle: &WorkloadBundle) -> ProviderRuntimeRequirement {
    let workload = &bundle.workload;
    ProviderRuntimeRequirement {
        runtime: workload.runtime,
        version: workload.runtime_version.clone(),
        // Architecture is a Computer placement constraint. The target, not
        // this controller, supplies the OS when it resolves the runtime.
        platform: None,
    }
}

/// The command a computer asks its target to start. Reconciliation replaces
/// its first word with the executable the same target resolved and prepared.
fn process_command(bundle: &WorkloadBundle) -> Result<Vec<String>, EnvironmentError> {
    let workload = &bundle.workload;
    if bundle.dependency_capsule.is_some() {
        return Err(EnvironmentError::Invalid(
            "this application carries a dependency capsule; a computer installs dependencies as packages, so deploy it without one".into(),
        ));
    }
    let entrypoint = workload.entrypoint.display().to_string();
    let mut command: Vec<String> = match workload.runtime {
        RuntimeKind::Python => vec!["python".into(), entrypoint],
        RuntimeKind::Node => vec!["node".into(), entrypoint],
        RuntimeKind::Bun => vec!["bun".into(), entrypoint],
        RuntimeKind::Deno => vec!["deno".into(), "run".into(), "-A".into(), entrypoint],
        RuntimeKind::Ruby => vec!["ruby".into(), entrypoint],
        RuntimeKind::Php => vec!["php".into(), entrypoint],
        RuntimeKind::Jvm => vec!["java".into(), "-jar".into(), entrypoint],
        RuntimeKind::Dotnet => vec!["dotnet".into(), entrypoint],
        RuntimeKind::Shell => vec!["sh".into(), entrypoint],
        RuntimeKind::Native => vec![format!("./{entrypoint}")],
        RuntimeKind::Wasm => {
            return Err(EnvironmentError::Invalid(format!(
                "a wasm application cannot run as a Computer process: the process model requires a persistent OS process; run it as a WASM workload instead"
            )));
        }
    };
    command.extend(workload.args.iter().cloned());
    Ok(command)
}

fn process_env(bundle: &WorkloadBundle) -> BTreeMap<String, String> {
    let mut env = bundle.workload.env.clone();
    if bundle.workload.runtime == RuntimeKind::Python {
        env.entry("PYTHONUNBUFFERED".into())
            .or_insert_with(|| "1".into());
    }
    env
}

/// The application's source as a deterministic tar archive: its entrypoint
/// and every input its bundle carries, at their paths.
fn source_archive(bundle: &WorkloadBundle) -> Result<Vec<u8>, EnvironmentError> {
    let mut files = BTreeMap::new();
    files.insert(
        bundle.entrypoint.path.clone(),
        bundle.entrypoint.data.clone(),
    );
    let bundled = bundle
        .inputs
        .iter()
        .map(|input| (input.path.clone(), input.data.clone()))
        .collect::<BTreeMap<_, _>>();
    for input in &bundle.workload.inputs {
        let data = match &input.source {
            InputSource::Inline { data } => data.clone(),
            InputSource::File { .. } => bundled.get(&input.path).cloned().ok_or_else(|| {
                EnvironmentError::Invalid(format!(
                    "the bundle does not carry its input {}",
                    input.path.display()
                ))
            })?,
        };
        files.insert(input.path.clone(), data);
    }
    let executable =
        (bundle.workload.runtime == RuntimeKind::Native).then(|| bundle.entrypoint.path.clone());
    let mut archive = tar::Builder::new(Vec::new());
    archive.mode(tar::HeaderMode::Deterministic);
    for (path, data) in files {
        if path.is_absolute()
            || path
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return Err(EnvironmentError::Invalid(format!(
                "the bundle names a path outside its source: {}",
                path.display()
            )));
        }
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(if executable.as_deref() == Some(Path::new(&path)) {
            0o755
        } else {
            0o644
        });
        header.set_mtime(0);
        header.set_cksum();
        archive
            .append_data(&mut header, &path, data.as_slice())
            .map_err(|error| EnvironmentError::Invalid(error.to_string()))?;
    }
    let mut bytes = archive
        .into_inner()
        .map_err(|error| EnvironmentError::Invalid(error.to_string()))?;
    bytes.flush().ok();
    Ok(bytes)
}

/// What a deploy request releases: the bundle, and, from an artifact, the
/// artifact's evidence and environment contract.
struct ResolvedApplication {
    bundle: Vec<u8>,
    evidence: Option<compute_state::ApplicationArtifactEvidence>,
    required_env: std::collections::BTreeSet<String>,
}

/// Resolve a deploy request to the application it releases. An artifact
/// by reference is fetched here and must have the digest the caller
/// pinned; a manifest must name the application being deployed.
async fn resolve_application(
    name: &str,
    request: &ApplicationDeployRequest,
) -> Result<ResolvedApplication, EnvironmentError> {
    let Some(source) = &request.artifact else {
        let port = request.port.filter(|port| *port > 0).ok_or_else(|| {
            EnvironmentError::Invalid("an application needs the port it listens on".into())
        })?;
        if request.bundle.is_empty() {
            return Err(EnvironmentError::Invalid(
                "a deployment needs an application artifact or a bundle".into(),
            ));
        }
        ApplicationIdentity::new(name, Some(port))?;
        return Ok(ResolvedApplication {
            bundle: request.bundle.clone(),
            evidence: None,
            required_env: Default::default(),
        });
    };
    if !request.bundle.is_empty() || request.port.is_some() {
        return Err(EnvironmentError::Invalid(
            "an application artifact carries its bundle and port; send one or the other".into(),
        ));
    }
    let (bytes, url) = match source {
        ApplicationArtifactSource::Inline { data } => (data.clone(), None),
        ApplicationArtifactSource::Reference(reference) => {
            let fetching = reference.clone();
            let bytes = tokio::task::spawn_blocking(move || fetching.fetch())
                .await
                .map_err(|error| EnvironmentError::Invalid(error.to_string()))??;
            (bytes, Some(reference.url.clone()))
        }
    };
    let artifact = compute_core::ApplicationArtifact::from_bytes(&bytes)?;
    let manifest = &artifact.manifest;
    if manifest.application.name != name {
        return Err(EnvironmentError::Invalid(format!(
            "the artifact is application {}, not {name}",
            manifest.application.name
        )));
    }
    Ok(ResolvedApplication {
        bundle: artifact.bundle_bytes().to_vec(),
        evidence: Some(compute_state::ApplicationArtifactEvidence {
            artifact_id: artifact.artifact_id()?,
            url,
            version: manifest.version.clone(),
            capabilities: manifest.capabilities.iter().cloned().collect(),
            runtime: Some(manifest.runtime.name.as_str().to_owned()),
            runtime_version: manifest.runtime.version.clone(),
        }),
        // Defaults built into the artifact satisfy a requirement too.
        required_env: manifest
            .env
            .required
            .iter()
            .filter(|name| !manifest.env.defaults.contains_key(*name))
            .cloned()
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle(runtime: RuntimeKind, entrypoint: &str) -> WorkloadBundle {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join(entrypoint), b"fixture").unwrap();
        let workload: compute_core::WorkloadSpec = serde_json::from_value(serde_json::json!({
            "version": "1",
            "runtime": runtime,
            "entrypoint": entrypoint,
        }))
        .unwrap();
        WorkloadBundle::create_from(workload, directory.path()).unwrap()
    }

    #[test]
    fn versions_parse() {
        assert_eq!(parse_version("v3"), Some(3));
        assert_eq!(parse_version("3"), Some(3));
        assert_eq!(parse_version("rol_abc"), None);
    }

    #[test]
    fn applications_express_jvm_and_dotnet_as_target_runtime_requirements() {
        let jvm = bundle(RuntimeKind::Jvm, "app.jar");
        assert_eq!(process_command(&jvm).unwrap(), ["java", "-jar", "app.jar"]);
        assert_eq!(runtime_requirement(&jvm).runtime, RuntimeKind::Jvm);

        let dotnet = bundle(RuntimeKind::Dotnet, "app.dll");
        assert_eq!(process_command(&dotnet).unwrap(), ["dotnet", "app.dll"]);
        assert_eq!(runtime_requirement(&dotnet).runtime, RuntimeKind::Dotnet);
    }

    #[test]
    fn wasm_is_not_misrepresented_as_a_persistent_os_process() {
        let wasm = bundle(RuntimeKind::Wasm, "app.wasm");
        let error = process_command(&wasm).unwrap_err().to_string();
        assert!(error.contains("persistent OS process"), "{error}");
    }
}
