//! Revisions, deployments, promotion, and rollback.
//!
//! ```text
//! Project → Revision (immutable) → Release to an environment
//! ```
//!
//! A release is recorded as `pending` and then taken through its states by
//! the controller in `release.rs`. Promotion releases the exact revision
//! current in another environment: what was validated in preprod is what
//! runs in production.

use std::collections::BTreeSet;
use std::sync::Arc;

use chrono::Utc;
use compute_core::WorkloadBundle;
use compute_state::events;
use compute_state::{
    Collection, DeploymentRecord, EnvironmentProjectRecord, ProjectRecord, ProjectRevisionRecord,
    Query, RevisionWorkload, Stored, ids, short_digest,
};
use serde_json::json;

use super::{Change, Daemon, Scope};
use crate::EnvironmentError;
use crate::model::*;
use crate::status::*;

impl Daemon {
    /// Register immutable project content. Registering the same content
    /// again returns the existing revision; reusing a label for different
    /// content is refused.
    pub async fn register_revision(
        self: &Arc<Self>,
        project: &str,
        definition: RevisionDefinition,
    ) -> Result<RevisionView, EnvironmentError> {
        validate_name("project", project)?;
        validate_revision_label(&definition.revision)?;
        if definition.workloads.is_empty() {
            return Err(EnvironmentError::Invalid(
                "a project needs at least one workload".into(),
            ));
        }
        let mut names = BTreeSet::new();
        let mut workloads = vec![];
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
            validate_readiness(workload, &definition.workloads)?;
            let bundle = WorkloadBundle::from_bytes(&workload.bundle)?;
            let canonical = bundle.to_bytes()?;
            workloads.push((workload, bundle, canonical));
        }
        let project_id = ids::project(project);
        let mut revision_workloads = workloads
            .iter()
            .map(|(workload, bundle, canonical)| {
                let distribution = compute_placement::PlacementRequirements::from_bundle(
                    bundle,
                    canonical.len() as u64,
                    compute_placement::SubmissionMode::Synchronous,
                    &compute_placement::RequirementOptions::default(),
                )
                .ok()
                .and_then(|requirements| requirements.distribution)
                .map(|distribution| distribution.id);
                Ok(RevisionWorkload {
                    name: workload.name.clone(),
                    kind: workload.kind,
                    bundle_id: bundle.bundle_id()?,
                    artifact: compute_state::artifacts::digest(canonical),
                    workload_identity: bundle.workload_id()?,
                    runtime: bundle.workload.runtime.to_string(),
                    runtime_version: bundle.workload.runtime_version.clone(),
                    dependency: bundle
                        .dependency_capsule
                        .as_ref()
                        .and_then(|capsule| capsule.capsule_id().ok()),
                    distribution,
                    readiness: workload.readiness.clone(),
                    ports: workload.ports.clone(),
                    restart: workload.restart,
                    desired_state: workload.desired_state,
                })
            })
            .collect::<Result<Vec<_>, compute_core::ComputeError>>()?;
        revision_workloads.sort_by(|left, right| left.name.cmp(&right.name));
        let revision_digest =
            compute_core::sha256_identity(serde_json::to_vec(&revision_workloads)?.as_slice());
        // A revision is a label and its content: two labels may carry the
        // same content, but a label never changes content.
        let revision_id = ids::revision(
            &project_id,
            &format!("{}\n{revision_digest}", definition.revision),
        );
        let same_label = self
            .control()
            .query::<ProjectRevisionRecord>(
                Query::all(Collection::ProjectRevision)
                    .eq("project_id", project_id.clone())
                    .eq("revision", definition.revision.clone()),
            )
            .await?;
        if let Some(existing) = same_label.first() {
            if existing.id == revision_id {
                return Ok(revision_view(existing.id.clone(), existing.value.clone()));
            }
            return Err(EnvironmentError::Conflict(format!(
                "revision {} of {project} already names different content ({}); revisions are immutable",
                definition.revision, existing.value.revision_digest
            )));
        }
        // Artifacts first: a revision never references missing bytes.
        for (_, _, canonical) in &workloads {
            self.config.artifacts.put("bundle", canonical).await?;
        }
        let now = Utc::now();
        let record = ProjectRevisionRecord {
            project_id: project_id.clone(),
            project: project.into(),
            revision: definition.revision.clone(),
            revision_digest: revision_digest.clone(),
            source: definition.source.clone(),
            workloads: revision_workloads,
            created_at: now,
        };
        let mut change = Change::new();
        if self
            .control()
            .get::<ProjectRecord>(&project_id)
            .await?
            .is_none()
        {
            let project_record = ProjectRecord {
                name: project.into(),
                source: definition.source.clone(),
                created_at: now,
            };
            change = change.with(|batch| batch.create(&project_id, &project_record));
            change = self.event(
                change,
                events::PROJECT_REGISTERED,
                Scope {
                    project: Some(project.into()),
                    ..Scope::default()
                },
                format!("Project {project} registered"),
                json!({ "project_id": project_id }),
            );
        }
        change = change.with(|batch| batch.create(&revision_id, &record));
        change = self.event(
            change,
            events::PROJECT_REVISION_CREATED,
            Scope {
                project: Some(project.into()),
                ..Scope::default()
            },
            format!("{project} revision {} registered", definition.revision),
            json!({ "revision_id": revision_id, "revision_digest": revision_digest }),
        );
        self.apply(change).await?;
        Ok(revision_view(revision_id, record))
    }

    /// Deploy a registered revision to an environment: record the release
    /// and let the controller take it through its states. Returns the
    /// release as it stands after the first reconciliation.
    pub async fn deploy(
        self: &Arc<Self>,
        request: DeployRequest,
    ) -> Result<DeploymentView, EnvironmentError> {
        self.deploy_with(request, None, json!({})).await
    }

    async fn deploy_with(
        self: &Arc<Self>,
        request: DeployRequest,
        promoted_from: Option<String>,
        context: serde_json::Value,
    ) -> Result<DeploymentView, EnvironmentError> {
        // Changes are serialized with reconciliation: both write the
        // records a release touches.
        let cycle = self.reconciling.lock().await;
        if let Some(config) = &request.config {
            validate_env("project", config)?;
        }
        self.refresh().await?;
        let desired = self.inner.lock().await.desired.clone();
        let environment = desired
            .environment(&request.environment)
            .cloned()
            .ok_or_else(|| {
                EnvironmentError::NotFound(format!("environment {}", request.environment))
            })?;
        let env_name = environment.value.name.clone();
        let project = &request.project;
        let project_id = ids::project(project);
        let revision = self
            .resolve_revision(&project_id, project, request.revision.as_deref())
            .await?;
        if let Some(in_flight) = desired.in_flight().find(|release| {
            release.value.environment == env_name && release.value.project == *project
        }) {
            return Err(EnvironmentError::Conflict(format!(
                "{project} is already being released to {env_name} ({}, {}); wait for it or roll it back",
                in_flight.id,
                in_flight.value.status.as_str()
            )));
        }
        let membership = desired
            .memberships
            .get(&(env_name.clone(), project.clone()))
            .cloned();
        let config = request
            .config
            .clone()
            .or_else(|| membership.as_ref().map(|m| m.value.config.clone()))
            .unwrap_or_default();
        let missing = request
            .required_config
            .iter()
            .filter(|name| {
                !config.contains_key(*name) && !environment.value.config.contains_key(*name)
            })
            .cloned()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(EnvironmentError::Invalid(format!(
                "{project} requires configuration it was not given: {}",
                missing.join(", ")
            )));
        }
        let previous = membership
            .as_ref()
            .and_then(|membership| membership.value.deployment_id.clone());
        let old_revision = previous
            .as_ref()
            .and_then(|id| desired.deployments.get(id))
            .map(|deployment| deployment.value.revision.clone());
        let config_digest = compute_core::sha256_identity(
            serde_json::to_vec(&json!({
                "environment": environment.value.config,
                "project": config,
            }))?
            .as_slice(),
        );

        let now = Utc::now();
        let latest = self
            .control()
            .query::<DeploymentRecord>(
                Query::all(Collection::Deployment)
                    .eq("project_id", project_id.clone())
                    .descending("created_at")
                    .limit(1),
            )
            .await?;
        let version = latest.first().map_or(1, |deployment| {
            deployment.value.version.saturating_add(1).max(1)
        });
        let deployment_id = format!("dep_{}", short_digest(&[&project_id, &version.to_string()]));
        let record = DeploymentRecord {
            environment_id: environment.id.clone(),
            environment: env_name.clone(),
            project_id: project_id.clone(),
            project: project.clone(),
            version,
            revision_id: revision.id.clone(),
            revision: revision.value.revision.clone(),
            revision_digest: revision.value.revision_digest.clone(),
            status: DeploymentStatus::Pending,
            promoted_from: promoted_from.clone(),
            previous: previous.clone(),
            // The caller's pool placement and the artifact are evidence
            // from the start; the release adds this node's own admission
            // and placement to it.
            workloads: (request.placement.is_some() || request.artifact.is_some())
                .then(|| {
                    revision
                        .value
                        .workloads
                        .iter()
                        .map(|workload| DeploymentWorkload {
                            name: workload.name.clone(),
                            kind: workload.kind,
                            bundle_id: workload.bundle_id.clone(),
                            artifact: workload.artifact.clone(),
                            runtime: workload.runtime.clone(),
                            runtime_version: workload.runtime_version.clone(),
                            resolved_runtime_version: None,
                            distribution: workload.distribution.clone(),
                            admitted: false,
                            policy_id: None,
                            admission_id: None,
                            placement_id: None,
                            provider: None,
                            reasons: vec![],
                            endpoints: vec![],
                            pool_placement: request.placement.clone(),
                            application_artifact: request.artifact.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            failure: None,
            receipt_ids: vec![],
            old_revision: old_revision.clone(),
            config_digest: Some(config_digest),
            config: config.clone(),
            readiness_result: None,
            network_result: None,
            traffic_switch_result: None,
            rollback_reason: None,
            receipt: None,
            status_since: Some(now),
            completed_at: None,
            created_at: now,
            updated_at: now,
        };
        let scope = Scope::project(&env_name, project).deployment(&deployment_id);
        let mut change = Change::new().with(|batch| batch.create(&deployment_id, &record));
        // A project's membership exists from its first release; it has no
        // current deployment until that release switches traffic.
        match &membership {
            Some(existing) => {
                if let Some(desired_state) = request.desired_state
                    && desired_state != existing.value.desired_state
                {
                    change = change.with(|batch| {
                        batch.update(
                            existing,
                            json!({ "desired_state": desired_state, "updated_at": now }),
                        )
                    });
                }
            }
            None => {
                let membership_record = EnvironmentProjectRecord {
                    environment_id: environment.id.clone(),
                    environment: env_name.clone(),
                    project_id: project_id.clone(),
                    project: project.clone(),
                    desired_state: request.desired_state.unwrap_or_default(),
                    config: config.clone(),
                    revision_id: None,
                    deployment_id: None,
                    created_at: now,
                    updated_at: now,
                };
                change = change.with(|batch| {
                    batch.create(
                        &ids::membership(&environment.id, &project_id),
                        &membership_record,
                    )
                });
                change = self.event(
                    change,
                    events::PROJECT_ADDED,
                    scope.clone(),
                    format!("{project} added to {env_name}"),
                    json!({ "revision": revision.value.revision }),
                );
            }
        }
        let mut data = json!({
            "revision": revision.value.revision,
            "revision_id": revision.id,
            "old_revision": old_revision,
            "promoted_from": promoted_from,
        });
        if let (Some(data), Some(context)) = (data.as_object_mut(), context.as_object()) {
            data.extend(context.clone());
        }
        let change = self.event(
            change,
            events::DEPLOYMENT_STARTED,
            scope,
            format!(
                "Releasing {project} {} to {env_name}",
                revision.value.revision
            ),
            data,
        );
        self.apply(change).await?;
        drop(cycle);
        self.changed().await;
        self.deployment(&deployment_id).await
    }

    /// Wait until a release ends, up to `timeout`.
    pub async fn await_release(
        self: &Arc<Self>,
        deployment_id: &str,
        timeout: std::time::Duration,
    ) -> Result<DeploymentView, EnvironmentError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let deployment = self.deployment(deployment_id).await?;
            let status = deployment.record.status;
            if status.is_terminal() || tokio::time::Instant::now() >= deadline {
                return Ok(deployment);
            }
            self.wake();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// Roll a release back. Before its traffic moved, the release is
    /// abandoned; after, traffic returns to what it replaced. A completed
    /// release is rolled back by releasing the revision it replaced again.
    pub async fn rollback(
        self: &Arc<Self>,
        deployment_id: &str,
    ) -> Result<DeploymentView, EnvironmentError> {
        let _cycle = self.reconciling.lock().await;
        self.refresh().await?;
        let release = self.get_required::<DeploymentRecord>(deployment_id).await?;
        let desired = self.inner.lock().await.desired.clone();
        match release.value.status {
            DeploymentStatus::Pending
            | DeploymentStatus::Starting
            | DeploymentStatus::Ready
            | DeploymentStatus::NetworkReady => {
                self.fail(
                    &release,
                    "rollback",
                    "rolled back by an operator before traffic moved".into(),
                )
                .await?;
            }
            DeploymentStatus::Switching | DeploymentStatus::Active | DeploymentStatus::Draining => {
                self.roll_back(&release, &desired, "rolled back by an operator".into())
                    .await?;
            }
            DeploymentStatus::Complete => {
                let current = desired
                    .memberships
                    .get(&(
                        release.value.environment.clone(),
                        release.value.project.clone(),
                    ))
                    .and_then(|membership| membership.value.deployment_id.clone());
                if current.as_deref() != Some(deployment_id) {
                    return Err(EnvironmentError::Conflict(format!(
                        "{deployment_id} is not the current deployment of {} in {}",
                        release.value.project, release.value.environment
                    )));
                }
                let previous = release.value.previous.clone().ok_or_else(|| {
                    EnvironmentError::Conflict(format!(
                        "{deployment_id} replaced nothing; there is nothing to roll back to"
                    ))
                })?;
                let previous = self.get_required::<DeploymentRecord>(&previous).await?;
                drop(_cycle);
                return self
                    .deploy_with(
                        DeployRequest {
                            project: release.value.project.clone(),
                            environment: release.value.environment.clone(),
                            revision: Some(previous.value.revision_id.clone()),
                            config: Some(super::execute::run_config(&previous.value, None)),
                            desired_state: None,
                            placement: None,
                            artifact: previous
                                .value
                                .workloads
                                .iter()
                                .find_map(|workload| workload.application_artifact.clone()),
                            required_config: Default::default(),
                        },
                        None,
                        json!({ "rollback_of": deployment_id }),
                    )
                    .await;
            }
            DeploymentStatus::Failed | DeploymentStatus::RolledBack => {
                return Err(EnvironmentError::Conflict(format!(
                    "{deployment_id} is {}; there is nothing to roll back",
                    release.value.status.as_str()
                )));
            }
        }
        drop(_cycle);
        self.changed().await;
        self.deployment(deployment_id).await
    }

    async fn resolve_revision(
        &self,
        project_id: &str,
        project: &str,
        selector: Option<&str>,
    ) -> Result<Stored<ProjectRevisionRecord>, EnvironmentError> {
        if let Some(selector) = selector
            && selector.starts_with("rev_")
        {
            let revision = self.get_required::<ProjectRevisionRecord>(selector).await?;
            if revision.value.project_id != project_id {
                return Err(EnvironmentError::Invalid(format!(
                    "{selector} is a revision of {}, not {project}",
                    revision.value.project
                )));
            }
            return Ok(revision);
        }
        let mut query =
            Query::all(Collection::ProjectRevision).eq("project_id", project_id.to_string());
        if let Some(label) = selector {
            query = query.eq("revision", label.to_string());
        }
        // The newest, chosen by FeltDB within the project's index.
        let revisions = self
            .control()
            .query::<ProjectRevisionRecord>(query.descending("created_at").limit(1))
            .await?;
        revisions.into_iter().next().ok_or_else(|| {
            EnvironmentError::NotFound(match selector {
                Some(label) => format!("revision {label} of {project}"),
                None => format!("any revision of {project}; register one first"),
            })
        })
    }

    /// Deploy the revision current in `from` to `to`. The target runs the
    /// exact revision — the same content digest — validated in the source.
    pub async fn promote(
        self: &Arc<Self>,
        request: PromoteRequest,
    ) -> Result<DeploymentView, EnvironmentError> {
        let (membership, _) = self.membership(&request.from, &request.project).await?;
        let source_id = membership.value.deployment_id.clone().ok_or_else(|| {
            EnvironmentError::Conflict(format!(
                "{} has no current deployment in {}",
                request.project, request.from
            ))
        })?;
        let source = self.get_required::<DeploymentRecord>(&source_id).await?;
        let released = matches!(
            source.value.status,
            DeploymentStatus::Active | DeploymentStatus::Draining | DeploymentStatus::Complete
        );
        if !released && !request.allow_unhealthy {
            return Err(EnvironmentError::Conflict(format!(
                "{} {} is {} in {}, not released; promote a validated revision",
                request.project,
                source.value.revision,
                source.value.status.as_str(),
                request.from
            )));
        }
        let deployment = self
            .deploy_with(
                DeployRequest {
                    project: request.project.clone(),
                    environment: request.to.clone(),
                    revision: Some(source.value.revision_id.clone()),
                    config: request.config.clone(),
                    desired_state: None,
                    placement: None,
                    ..DeployRequest::default()
                },
                Some(source_id.clone()),
                json!({}),
            )
            .await?;
        assert_eq!(
            deployment.record.revision_digest, source.value.revision_digest,
            "promotion deploys the exact source revision"
        );
        if deployment.record.status != DeploymentStatus::Failed {
            let change = self.event(
                Change::new(),
                events::DEPLOYMENT_PROMOTED,
                Scope::project(&deployment.record.environment, &request.project)
                    .deployment(&deployment.deployment_id),
                format!(
                    "{} {} promoted from {} to {}",
                    request.project,
                    source.value.revision,
                    source.value.environment,
                    deployment.record.environment
                ),
                json!({
                    "from_deployment": source_id,
                    "revision_digest": source.value.revision_digest,
                }),
            );
            self.apply(change).await?;
        }
        Ok(deployment)
    }

    /// Register a revision and deploy it: `compute project add`.
    pub async fn add_project(
        self: &Arc<Self>,
        environment: &str,
        definition: ProjectDefinition,
    ) -> Result<ProjectView, EnvironmentError> {
        validate_name("project", &definition.name)?;
        validate_env("project", &definition.env)?;
        let revision = self
            .register_revision(&definition.name, definition.revision_definition())
            .await?;
        let deployment = self
            .deploy(DeployRequest {
                project: definition.name.clone(),
                environment: environment.into(),
                revision: Some(revision.revision_id),
                config: Some(definition.env.clone()),
                desired_state: Some(definition.desired_state),
                placement: None,
                ..DeployRequest::default()
            })
            .await?;
        let deployment = self
            .await_release(
                &deployment.deployment_id,
                std::time::Duration::from_secs(900),
            )
            .await?;
        if matches!(
            deployment.record.status,
            DeploymentStatus::Failed | DeploymentStatus::RolledBack
        ) {
            return Err(EnvironmentError::Denied(format!(
                "{} was not deployed: {}",
                definition.name,
                deployment
                    .record
                    .failure
                    .or(deployment.record.rollback_reason)
                    .unwrap_or_default()
            )));
        }
        self.project(environment, &definition.name).await
    }
}

/// A readiness check names only what the workload declares.
fn validate_readiness(
    workload: &WorkloadDefinition,
    siblings: &[WorkloadDefinition],
) -> Result<(), EnvironmentError> {
    let Some(readiness) = &workload.readiness else {
        return Ok(());
    };
    let invalid = |message: String| {
        Err(EnvironmentError::Invalid(format!(
            "{}: {message}",
            workload.name
        )))
    };
    if workload.kind != WorkloadKind::Service {
        return invalid("only services have readiness checks".into());
    }
    if let Some(port) = &readiness.port
        && !workload.ports.iter().any(|declared| declared.name == *port)
    {
        return invalid(format!(
            "readiness names port {port}, which it does not declare"
        ));
    }
    match readiness.check {
        compute_state::ReadinessCheck::Port | compute_state::ReadinessCheck::Http
            if workload.ports.is_empty() =>
        {
            return invalid("a port or http readiness check needs a declared port".into());
        }
        compute_state::ReadinessCheck::Http
            if !readiness.path.as_deref().unwrap_or("/").starts_with('/') =>
        {
            return invalid("an http readiness path starts with /".into());
        }
        compute_state::ReadinessCheck::Task => {
            let task = readiness.task.as_deref().unwrap_or_default();
            if !siblings
                .iter()
                .any(|sibling| sibling.name == task && sibling.kind == WorkloadKind::Task)
            {
                return invalid(format!(
                    "readiness task {task:?} is not a task of this revision"
                ));
            }
        }
        _ => {}
    }
    if readiness.timeout_ms == 0 || readiness.interval_ms == 0 {
        return invalid("readiness timeout and interval must be positive".into());
    }
    Ok(())
}

pub(crate) fn revision_view(revision_id: String, record: ProjectRevisionRecord) -> RevisionView {
    RevisionView {
        revision_id,
        project: record.project,
        revision: record.revision,
        revision_digest: record.revision_digest,
        source: record.source,
        workloads: record.workloads,
        created_at: record.created_at,
    }
}
