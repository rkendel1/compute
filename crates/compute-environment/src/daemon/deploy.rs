//! Revisions, deployments, and promotion.
//!
//! ```text
//! Project → Revision (immutable) → Deploy to environment
//!   queued → admitted → placed → [activated] starting → healthy
//!                 ╰──── failed (the previous deployment stays current)
//! ```
//!
//! A deployment is admitted and placed for every workload before anything
//! changes; only then does it become current, atomically. Promotion deploys
//! the exact revision current in another environment: what was validated
//! in preprod is what runs in production.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use chrono::Utc;
use compute_core::WorkloadBundle;
use compute_placement::ProviderKind;
use compute_state::events;
use compute_state::{
    Collection, DeploymentRecord, DeploymentWorkload, EnvironmentProjectRecord, ProjectRecord,
    ProjectRevisionRecord, Query, RevisionWorkload, Stored, WorkloadRecord, ids, short_digest,
};
use serde_json::json;

use super::execute::{Target, placement_failure};
use super::{Change, Daemon, Desired, Key, Scope};
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
            let bundle = WorkloadBundle::from_bytes(&workload.bundle)?;
            let canonical = bundle.to_bytes()?;
            workloads.push((workload, bundle, canonical));
        }
        let project_id = ids::project(project);
        let mut revision_workloads = workloads
            .iter()
            .map(|(workload, bundle, canonical)| {
                Ok(RevisionWorkload {
                    name: workload.name.clone(),
                    kind: workload.kind,
                    bundle_id: bundle.bundle_id()?,
                    artifact: compute_state::artifacts::digest(canonical),
                    workload_identity: bundle.workload_id()?,
                    runtime: bundle.workload.runtime.to_string(),
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

    /// Deploy a registered revision to an environment.
    pub async fn deploy(
        self: &Arc<Self>,
        request: DeployRequest,
    ) -> Result<DeploymentView, EnvironmentError> {
        self.deploy_with(request, None).await
    }

    async fn deploy_with(
        self: &Arc<Self>,
        request: DeployRequest,
        promoted_from: Option<String>,
    ) -> Result<DeploymentView, EnvironmentError> {
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
        let membership = desired
            .memberships
            .get(&(env_name.clone(), project.clone()))
            .cloned();
        let config = request
            .config
            .clone()
            .or_else(|| membership.as_ref().map(|m| m.value.config.clone()))
            .unwrap_or_default();

        // Queued: the intent is recorded before anything is evaluated.
        let now = Utc::now();
        let deployment_id = format!(
            "dep_{}",
            short_digest(&[
                &environment.id,
                &revision.id,
                &now.timestamp_nanos_opt().unwrap_or_default().to_string(),
                &self.instance_id,
            ])
        );
        let mut record = DeploymentRecord {
            environment_id: environment.id.clone(),
            environment: env_name.clone(),
            project_id: project_id.clone(),
            project: project.clone(),
            revision_id: revision.id.clone(),
            revision: revision.value.revision.clone(),
            revision_digest: revision.value.revision_digest.clone(),
            status: DeploymentStatus::Queued,
            promoted_from: promoted_from.clone(),
            previous: membership
                .as_ref()
                .and_then(|membership| membership.value.deployment_id.clone()),
            workloads: vec![],
            failure: None,
            receipt_ids: vec![],
            created_at: now,
            updated_at: now,
        };
        let scope = Scope::project(&env_name, project).deployment(&deployment_id);
        let change = Change::new().with(|batch| batch.create(&deployment_id, &record));
        let change = self.event(
            change,
            events::DEPLOYMENT_STARTED,
            scope.clone(),
            format!(
                "Deploying {project} {} to {env_name}",
                revision.value.revision
            ),
            json!({
                "revision": revision.value.revision,
                "revision_id": revision.id,
                "promoted_from": promoted_from,
            }),
        );
        self.apply(change).await?;

        // Admission and placement of every workload, before anything
        // changes. Ports are chosen now and bound at activation.
        let mut reserved = BTreeSet::new();
        let mut planned = vec![];
        let mut failure = None;
        for workload in &revision.value.workloads {
            let key = (env_name.clone(), project.clone(), workload.name.clone());
            let existing = desired.workloads.get(&key);
            let ports = match self.plan_ports(&desired, &key, workload, existing, &mut reserved) {
                Ok(ports) => ports,
                Err(error) => {
                    failure = Some(error.to_string());
                    break;
                }
            };
            let workload_id = ids::workload(&environment.id, &project_id, &workload.name);
            let prepared = match self
                .prepare(Target {
                    environment: &environment,
                    config: &config,
                    project_id: &project_id,
                    revision: &revision.value,
                    workload,
                    workload_id: &workload_id,
                    ports: &ports,
                })
                .await
            {
                Ok(prepared) => prepared,
                Err(error) => {
                    failure = Some(format!("{}: {error}", workload.name));
                    break;
                }
            };
            let report = &prepared.report;
            let mut evidence = DeploymentWorkload {
                name: workload.name.clone(),
                kind: workload.kind,
                bundle_id: workload.bundle_id.clone(),
                admitted: false,
                policy_id: Some(report.policy_id.clone()),
                admission_id: None,
                placement_id: Some(report.placement_id.clone()),
                provider: None,
                reasons: vec![],
            };
            match &report.selected {
                Some(selected)
                    if workload.kind == WorkloadKind::Service
                        && selected.provider_kind != ProviderKind::Local =>
                {
                    evidence.provider = Some(selected.provider_id.clone());
                    evidence.reasons.push(format!(
                        "services run on the daemon's own node; placement selected {}",
                        selected.provider_id
                    ));
                }
                Some(selected) => {
                    evidence.admitted = true;
                    evidence.policy_id = Some(selected.policy_id.clone());
                    evidence.admission_id = Some(selected.admission_id.clone());
                    evidence.provider = Some(selected.provider_id.clone());
                }
                None => {
                    let (message, decision) = placement_failure(report);
                    if let Some(decision) = decision {
                        evidence.policy_id = Some(decision.policy_id.clone());
                        evidence.admission_id = Some(decision.admission_id.clone());
                    }
                    evidence.reasons.push(message);
                }
            }
            if !evidence.admitted && failure.is_none() {
                failure = Some(format!(
                    "{}: {}",
                    workload.name,
                    evidence.reasons.join("; ")
                ));
            }
            record.workloads.push(evidence);
            planned.push((workload.clone(), workload_id, ports));
        }
        let stored = self
            .get_required::<DeploymentRecord>(&deployment_id)
            .await?;
        if let Some(failure) = failure {
            record.status = DeploymentStatus::Failed;
            record.failure = Some(failure.clone());
            record.updated_at = Utc::now();
            let change = Change::new().with(|batch| batch.replace(&stored, &record));
            let change = self.event(
                change,
                events::DEPLOYMENT_FAILED,
                scope,
                format!(
                    "{project} {} was not deployed to {env_name}: {failure}",
                    revision.value.revision
                ),
                json!({ "stage": "admission", "reason": failure }),
            );
            self.apply(change).await?;
            return self.deployment(&deployment_id).await;
        }
        record.status = DeploymentStatus::Placed;
        record.updated_at = Utc::now();
        let change = Change::new().with(|batch| batch.replace(&stored, &record));
        let change = self.event(
            change,
            events::DEPLOYMENT_ADMITTED,
            scope.clone(),
            format!("{project} {} admitted in {env_name}", revision.value.revision),
            json!({ "admissions": record.workloads.iter().map(|w| &w.admission_id).collect::<Vec<_>>() }),
        );
        let change = self.event(
            change,
            events::DEPLOYMENT_PLACED,
            scope.clone(),
            format!("{project} {} placed in {env_name}", revision.value.revision),
            json!({ "providers": record.workloads.iter().map(|w| &w.provider).collect::<Vec<_>>() }),
        );
        self.apply(change).await?;

        // Activation: one atomic change makes this deployment current.
        self.activate(
            &desired,
            &environment,
            membership,
            &revision,
            &deployment_id,
            record,
            &config,
            request.desired_state,
            planned,
            scope,
        )
        .await?;
        self.changed().await;
        self.deployment(&deployment_id).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn activate(
        self: &Arc<Self>,
        desired: &Desired,
        environment: &Stored<compute_state::EnvironmentRecord>,
        membership: Option<Stored<EnvironmentProjectRecord>>,
        revision: &Stored<ProjectRevisionRecord>,
        deployment_id: &str,
        mut record: DeploymentRecord,
        config: &BTreeMap<String, String>,
        desired_state: Option<DesiredState>,
        planned: Vec<(RevisionWorkload, String, Vec<PortBinding>)>,
        scope: Scope,
    ) -> Result<(), EnvironmentError> {
        let env_name = environment.value.name.clone();
        let project = record.project.clone();
        let now = Utc::now();
        let stored = self.get_required::<DeploymentRecord>(deployment_id).await?;
        record.status = DeploymentStatus::Starting;
        record.updated_at = now;
        let mut change = Change::new().with(|batch| batch.replace(&stored, &record));
        // The previous deployment is superseded.
        if let Some(previous) = &record.previous
            && let Some(previous) = self.control().get::<DeploymentRecord>(previous).await?
            && !previous.value.status.is_terminal()
        {
            change = change.with(|batch| {
                batch.update(
                    &previous,
                    json!({ "status": DeploymentStatus::Superseded, "updated_at": now }),
                )
            });
        }
        let membership_record = EnvironmentProjectRecord {
            environment_id: environment.id.clone(),
            environment: env_name.clone(),
            project_id: record.project_id.clone(),
            project: project.clone(),
            desired_state: desired_state
                .or_else(|| membership.as_ref().map(|m| m.value.desired_state))
                .unwrap_or_default(),
            config: config.clone(),
            revision_id: Some(revision.id.clone()),
            deployment_id: Some(deployment_id.into()),
            created_at: membership.as_ref().map_or(now, |m| m.value.created_at),
            updated_at: now,
        };
        change = match &membership {
            Some(existing) => change.with(|batch| batch.replace(existing, &membership_record)),
            None => {
                let change = change.with(|batch| {
                    batch.create(
                        &ids::membership(&environment.id, &record.project_id),
                        &membership_record,
                    )
                });
                self.event(
                    change,
                    events::PROJECT_ADDED,
                    Scope::project(&env_name, &project).deployment(deployment_id),
                    format!("{project} added to {env_name}"),
                    json!({ "revision": record.revision }),
                )
            }
        };
        // Workloads: kept names keep their desired state and ports.
        let mut kept = BTreeSet::new();
        for (workload, workload_id, ports) in planned {
            let key: Key = (env_name.clone(), project.clone(), workload.name.clone());
            kept.insert(key.clone());
            let existing = desired.workloads.get(&key);
            let value = WorkloadRecord {
                environment_id: environment.id.clone(),
                environment: env_name.clone(),
                project_id: record.project_id.clone(),
                project: project.clone(),
                name: workload.name.clone(),
                kind: workload.kind,
                desired_state: existing.map_or(workload.desired_state, |e| e.value.desired_state),
                restart: workload.restart,
                bundle_id: workload.bundle_id.clone(),
                workload_identity: workload.workload_identity.clone(),
                runtime: workload.runtime.clone(),
                ports,
                deployment_id: deployment_id.into(),
            };
            change = match existing {
                Some(existing) => change.with(|batch| batch.replace(existing, &value)),
                None => change.with(|batch| batch.create(&workload_id, &value)),
            };
        }
        for (key, existing) in desired.workloads_of(&env_name, &project) {
            if !kept.contains(key) {
                change = change.with(|batch| batch.delete(existing));
                change = self.delete_status(change, &existing.id).await?;
            }
        }
        let change = self.event(
            change,
            events::DEPLOYMENT_ACTIVATED,
            scope,
            format!("{project} {} is now current in {env_name}", record.revision),
            json!({ "revision": record.revision, "previous": record.previous }),
        );
        self.apply(change).await
    }

    /// Stable host ports: a workload keeps its bindings across revisions;
    /// new ports take the lowest free port in the daemon's range.
    fn plan_ports(
        &self,
        desired: &Desired,
        key: &Key,
        workload: &RevisionWorkload,
        existing: Option<&Stored<WorkloadRecord>>,
        reserved: &mut BTreeSet<u16>,
    ) -> Result<Vec<PortBinding>, EnvironmentError> {
        let used = desired
            .workloads
            .iter()
            .filter(|(other, _)| *other != key)
            .flat_map(|(_, record)| record.value.ports.iter().map(|port| port.host))
            .collect::<BTreeSet<_>>();
        let mut bindings = vec![];
        for port in &workload.ports {
            let kept = existing.and_then(|existing| {
                existing
                    .value
                    .ports
                    .iter()
                    .find(|binding| binding.name == port.name)
                    .map(|binding| binding.host)
            });
            let host = match kept {
                Some(host) => host,
                None => {
                    let (low, high) = self.config.port_range;
                    (low..=high)
                        .find(|candidate| {
                            !used.contains(candidate)
                                && !reserved.contains(candidate)
                                && existing.is_none_or(|existing| {
                                    !existing.value.ports.iter().any(|b| b.host == *candidate)
                                })
                                && std::net::TcpListener::bind(("127.0.0.1", *candidate)).is_ok()
                        })
                        .ok_or_else(|| {
                            EnvironmentError::Invalid(
                                "no free host port in the daemon's range".into(),
                            )
                        })?
                }
            };
            reserved.insert(host);
            bindings.push(PortBinding {
                name: port.name.clone(),
                logical: port.port,
                host,
            });
        }
        Ok(bindings)
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
        let mut revisions = self.control().query::<ProjectRevisionRecord>(query).await?;
        revisions.sort_by(|left, right| right.value.created_at.cmp(&left.value.created_at));
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
        if source.value.status != DeploymentStatus::Healthy && !request.allow_unhealthy {
            return Err(EnvironmentError::Conflict(format!(
                "{} {} is {} in {}, not healthy; promote a validated revision",
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
                    config: None,
                    desired_state: None,
                },
                Some(source_id.clone()),
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
            })
            .await?;
        if deployment.record.status == DeploymentStatus::Failed {
            return Err(EnvironmentError::Denied(format!(
                "{} was not deployed: {}",
                definition.name,
                deployment.record.failure.unwrap_or_default()
            )));
        }
        self.project(environment, &definition.name).await
    }
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
