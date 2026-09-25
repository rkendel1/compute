//! Read models: desired state joined with live observations.

use std::path::Path;

use compute_core::WorkloadBundle;
use compute_policy::EffectivePolicy;
use compute_state::{
    Collection, DeploymentRecord, EventRecord, ExecutionRecord, ProjectRecord,
    ProjectRevisionRecord, Query, ReceiptRecord, Stored, ids,
};

use super::deploy::revision_view;
use super::reconcile::workload_health;
use super::{Daemon, Key, Unit};
use crate::EnvironmentError;
use crate::model::*;
use crate::status::*;

/// Filters for the event log.
#[derive(Debug, Clone, Default)]
pub struct EventFilter {
    pub after: Option<u64>,
    pub limit: Option<usize>,
    pub environment: Option<String>,
    pub project: Option<String>,
    pub deployment_id: Option<String>,
}

impl Daemon {
    pub async fn environments(&self) -> Result<Vec<EnvironmentSummary>, EnvironmentError> {
        self.refresh_for_read().await?;
        let names = self
            .inner
            .lock()
            .await
            .desired
            .environments
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let mut summaries = vec![];
        for name in names {
            let view = self.environment_view(&name).await?;
            summaries.push(EnvironmentSummary {
                environment_id: view.environment_id,
                name: view.name,
                desired_state: view.desired_state,
                actual_state: view.actual_state,
                health: view.health,
                project_count: view.project_count,
                workload_count: view.workload_count,
                service_count: view.service_count,
                provider: view.provider.unwrap_or_else(|| "local".into()),
            });
        }
        Ok(summaries)
    }

    pub async fn environment(&self, name: &str) -> Result<EnvironmentView, EnvironmentError> {
        self.refresh_for_read().await?;
        self.environment_view(name).await
    }

    async fn environment_view(&self, name: &str) -> Result<EnvironmentView, EnvironmentError> {
        let (record, projects) = {
            let inner = self.inner.lock().await;
            let record = inner
                .desired
                .environment(name)
                .cloned()
                .ok_or_else(|| EnvironmentError::NotFound(format!("environment {name}")))?;
            let projects = inner
                .desired
                .memberships
                .keys()
                .filter(|(environment, _)| *environment == record.value.name)
                .map(|(_, project)| project.clone())
                .collect::<Vec<_>>();
            (record, projects)
        };
        let mut views = vec![];
        for project in &projects {
            views.push(self.project_view(&record.value.name, project).await?);
        }
        let actual_state = aggregate(
            record.value.desired_state,
            views
                .iter()
                .map(|project| (project.desired_state, project.actual_state)),
        );
        let health = combine_health(views.iter().map(|project| project.health));
        Ok(EnvironmentView {
            version: ENVIRONMENT_VERSION.into(),
            environment_id: record.id.clone(),
            name: record.value.name.clone(),
            desired_state: record.value.desired_state,
            actual_state,
            health,
            created_at: record.value.created_at,
            policy_id: EffectivePolicy::compose(&self.policy_sources(&record.value)?).policy_id,
            provider: record.value.provider.clone(),
            config: record.value.config.clone(),
            project_count: views.len(),
            workload_count: views.iter().map(|project| project.workload_count).sum(),
            service_count: views.iter().map(|project| project.service_count).sum(),
            disk_bytes: directory_size(
                &self.config.state_dir.join("logs").join(&record.value.name),
            ),
            projects: views,
        })
    }

    pub async fn project(
        &self,
        environment: &str,
        project: &str,
    ) -> Result<ProjectView, EnvironmentError> {
        self.refresh_for_read().await?;
        let name = self
            .inner
            .lock()
            .await
            .desired
            .environment(environment)
            .map(|record| record.value.name.clone())
            .ok_or_else(|| EnvironmentError::NotFound(format!("environment {environment}")))?;
        self.project_view(&name, project).await
    }

    async fn project_view(
        &self,
        environment: &str,
        project: &str,
    ) -> Result<ProjectView, EnvironmentError> {
        let (record, membership, revision, deployment, keys) = {
            let inner = self.inner.lock().await;
            let desired = &inner.desired;
            let record = desired
                .environments
                .get(environment)
                .cloned()
                .ok_or_else(|| EnvironmentError::NotFound(format!("environment {environment}")))?;
            let membership = desired
                .memberships
                .get(&(environment.to_string(), project.to_string()))
                .cloned()
                .ok_or_else(|| {
                    EnvironmentError::NotFound(format!("project {project} in {environment}"))
                })?;
            let revision = membership
                .value
                .revision_id
                .as_ref()
                .and_then(|id| desired.revisions.get(id).cloned());
            let deployment = membership
                .value
                .deployment_id
                .as_ref()
                .and_then(|id| desired.deployments.get(id).cloned());
            let keys = desired
                .workloads_of(environment, project)
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            (record, membership, revision, deployment, keys)
        };
        let mut workloads = vec![];
        for key in &keys {
            workloads.push(self.workload_view(key).await?);
        }
        let services = workloads
            .iter()
            .filter(|workload| workload.kind == WorkloadKind::Service)
            .collect::<Vec<_>>();
        let effective = if record.value.desired_state == DesiredState::Stopped {
            DesiredState::Stopped
        } else {
            membership.value.desired_state
        };
        let actual_state = aggregate(
            effective,
            services
                .iter()
                .map(|workload| (workload.desired_state, workload.actual_state)),
        );
        let health = combine_health(services.iter().map(|workload| workload.health));
        Ok(ProjectView {
            project_id: membership.value.project_id.clone(),
            name: project.into(),
            environment: environment.into(),
            environment_id: record.id.clone(),
            revision: revision
                .as_ref()
                .map(|revision| revision.revision.clone())
                .unwrap_or_default(),
            revision_id: membership.value.revision_id.clone().unwrap_or_default(),
            revision_digest: revision
                .as_ref()
                .map(|revision| revision.revision_digest.clone())
                .unwrap_or_default(),
            source: revision
                .as_ref()
                .and_then(|revision| revision.source.clone()),
            desired_state: membership.value.desired_state,
            actual_state,
            health,
            deployed_at: deployment
                .as_ref()
                .map_or(membership.value.updated_at, |deployment| {
                    deployment.value.created_at
                }),
            deployment: deployment.map(summary),
            config: membership.value.config.clone(),
            workload_count: workloads.len(),
            service_count: services.len(),
            provider: record
                .value
                .provider
                .clone()
                .unwrap_or_else(|| "local".into()),
            disk_bytes: directory_size(
                &self
                    .config
                    .state_dir
                    .join("logs")
                    .join(environment)
                    .join(project),
            ),
            workloads,
        })
    }

    pub async fn workload(
        &self,
        environment: &str,
        project: &str,
        workload: &str,
    ) -> Result<WorkloadView, EnvironmentError> {
        let (key, _) = self.workload_record(environment, project, workload).await?;
        self.workload_view(&key).await
    }

    async fn workload_view(&self, key: &Key) -> Result<WorkloadView, EnvironmentError> {
        let (record, artifact, runtime_view, serving_ports) = {
            let inner = self.inner.lock().await;
            let record = inner.desired.workloads.get(key).cloned().ok_or_else(|| {
                EnvironmentError::NotFound(format!("workload {} in {}/{}", key.2, key.0, key.1))
            })?;
            let artifact = inner
                .desired
                .memberships
                .get(&(key.0.clone(), key.1.clone()))
                .and_then(|membership| membership.value.revision_id.as_ref())
                .and_then(|id| inner.desired.revisions.get(id))
                .and_then(|revision| {
                    revision
                        .workloads
                        .iter()
                        .find(|workload| workload.name == key.2)
                        .map(|workload| workload.artifact.clone())
                });
            let unit = Unit::new(key, &record.value.deployment_id);
            let serving_ports = inner
                .desired
                .instance(key, &record.value.deployment_id)
                .map(|instance| instance.value.ports.clone())
                .unwrap_or_default();
            let runtime = inner.runtime.get(&unit).map(|runtime| {
                (
                    runtime.state,
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
                    runtime.health,
                )
            });
            (record, artifact, runtime, serving_ports)
        };
        let (
            state,
            execution_id,
            restarts,
            started_at,
            finished_at,
            exit_code,
            error,
            placement,
            evidence,
            log_directory,
            observed_health,
        ) = runtime_view.unwrap_or_default();
        let state = state.unwrap_or(match record.value.kind {
            WorkloadKind::Task => ActualState::Pending,
            WorkloadKind::Service => ActualState::Stopped,
        });
        // The health the reconciler last observed; probed here only when
        // it has not observed this run yet.
        let health = match (record.value.kind, state, observed_health) {
            (WorkloadKind::Service, ActualState::Running, Some(health)) => health,
            _ => workload_health(record.value.kind, state, &serving_ports).await,
        };
        // What the bundle declares, read from it once per bundle identity.
        let declared = match &artifact {
            Some(digest) => self.declared_resources(digest),
            None => None,
        };
        Ok(WorkloadView {
            workload_id: record.id.clone(),
            name: record.value.name.clone(),
            kind: record.value.kind,
            desired_state: record.value.desired_state,
            actual_state: state,
            health,
            restart: record.value.restart,
            runtime: record.value.runtime.clone(),
            bundle_id: record.value.bundle_id.clone(),
            deployment_id: record.value.deployment_id.clone(),
            execution_id,
            ports: record.value.ports.clone(),
            restarts,
            started_at,
            finished_at,
            exit_code,
            error,
            placement,
            evidence,
            resources: ResourceView {
                cpu: "not_measured".into(),
                memory_limit_bytes: declared.as_ref().and_then(|declared| declared.0),
                timeout_ms: declared.as_ref().and_then(|declared| declared.1),
                disk_bytes: directory_size(&self.logs_dir(key)),
                network: declared
                    .as_ref()
                    .map(|declared| declared.2.clone())
                    .unwrap_or_default(),
            },
            log_directory,
        })
    }

    /// A bundle's declared memory limit, wall-time limit, and network
    /// policy. Bundles are immutable, so each is parsed once.
    fn declared_resources(&self, digest: &str) -> Option<(Option<u64>, Option<u64>, String)> {
        if let Some(declared) = self.declared.lock().expect("declared").get(digest) {
            return Some(declared.clone());
        }
        let bundle = std::fs::read(
            self.config
                .state_dir
                .join("cache")
                .join(digest.trim_start_matches("sha256:")),
        )
        .ok()
        .and_then(|bytes| WorkloadBundle::from_bytes(&bytes).ok())?;
        let declared = (
            bundle.workload.resources.memory_bytes,
            bundle
                .workload
                .resources
                .wall_time
                .map(|value| u64::try_from(value.as_millis()).unwrap_or(u64::MAX)),
            bundle.workload.network.to_string(),
        );
        self.declared
            .lock()
            .expect("declared")
            .insert(digest.to_string(), declared.clone());
        Some(declared)
    }

    /// The most recent log output of a workload on this node.
    pub async fn logs(
        &self,
        environment: &str,
        project: &str,
        workload: &str,
    ) -> Result<(String, String), EnvironmentError> {
        let view = self.workload(environment, project, workload).await?;
        if let Some(directory) = view.log_directory {
            let read = |name: &str| {
                std::fs::read(Path::new(&directory).join(name))
                    .map(|bytes| {
                        let start = bytes.len().saturating_sub(64 * 1024);
                        String::from_utf8_lossy(&bytes[start..]).into_owned()
                    })
                    .unwrap_or_default()
            };
            return Ok((read("stdout.log"), read("stderr.log")));
        }
        // A task's output is kept with its latest execution.
        if let Some(execution_id) = view.execution_id
            && let Some(output) = self.inner.lock().await.outputs.get(&execution_id)
        {
            return Ok(output.clone());
        }
        Ok((String::new(), String::new()))
    }

    // ---- Projects across environments ---------------------------------

    pub async fn projects(&self) -> Result<Vec<ProjectSummary>, EnvironmentError> {
        self.refresh_for_read().await?;
        let names = self
            .inner
            .lock()
            .await
            .desired
            .projects
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let mut summaries = vec![];
        for name in names {
            summaries.push(self.project_summary(&name).await?);
        }
        Ok(summaries)
    }

    async fn project_summary(&self, name: &str) -> Result<ProjectSummary, EnvironmentError> {
        let (record, environments) = {
            let inner = self.inner.lock().await;
            let record: Stored<ProjectRecord> = inner
                .desired
                .projects
                .get(name)
                .cloned()
                .ok_or_else(|| EnvironmentError::NotFound(format!("project {name}")))?;
            let environments = inner
                .desired
                .memberships
                .keys()
                .filter(|(_, project)| project == name)
                .map(|(environment, _)| environment.clone())
                .collect::<Vec<_>>();
            (record, environments)
        };
        let revisions = self.revisions(name).await?;
        let mut placements = vec![];
        for environment in environments {
            let view = self.project_view(&environment, name).await?;
            placements.push(ProjectPlacement {
                environment,
                revision: view.revision,
                revision_id: view.revision_id,
                desired_state: view.desired_state,
                actual_state: view.actual_state,
                health: view.health,
                deployment: view.deployment,
            });
        }
        Ok(ProjectSummary {
            project_id: record.id.clone(),
            name: name.into(),
            source: record.value.source.clone(),
            created_at: record.value.created_at,
            revision_count: revisions.len(),
            latest_revision: revisions.first().map(|revision| revision.revision.clone()),
            environments: placements,
        })
    }

    pub async fn project_detail(&self, name: &str) -> Result<ProjectDetail, EnvironmentError> {
        self.refresh_for_read().await?;
        let summary = self.project_summary(name).await?;
        let revisions = self.revisions(name).await?;
        let deployments = self
            .deployments(None, Some(name.to_string()), Some(50))
            .await?;
        Ok(ProjectDetail {
            summary,
            revisions,
            deployments,
        })
    }

    /// Newest first.
    pub async fn revisions(&self, project: &str) -> Result<Vec<RevisionView>, EnvironmentError> {
        let revisions = self
            .control()
            .query::<ProjectRevisionRecord>(
                Query::all(Collection::ProjectRevision)
                    .eq("project_id", ids::project(project))
                    .descending("created_at"),
            )
            .await?;
        Ok(revisions
            .into_iter()
            .map(|revision| revision_view(revision.id, revision.value))
            .collect())
    }

    /// Newest first.
    pub async fn deployments(
        &self,
        environment: Option<String>,
        project: Option<String>,
        limit: Option<usize>,
    ) -> Result<Vec<DeploymentView>, EnvironmentError> {
        let mut query = Query::all(Collection::Deployment);
        if let Some(environment) = environment {
            let id = self
                .inner
                .lock()
                .await
                .desired
                .environment(&environment)
                .map(|record| record.id.clone())
                .unwrap_or(environment);
            query = query.eq("environment_id", id);
        }
        if let Some(project) = project {
            query = query.eq("project_id", ids::project(&project));
        }
        // Ordered and limited by FeltDB, within the environment's or the
        // project's index when one is named.
        query = query.descending("created_at");
        if let Some(limit) = limit {
            query = query.limit(limit);
        }
        let deployments = self.control().query::<DeploymentRecord>(query).await?;
        Ok(deployments
            .into_iter()
            .map(|deployment| DeploymentView {
                deployment_id: deployment.id,
                record: deployment.value,
                instances: vec![],
            })
            .collect())
    }

    pub async fn deployment(&self, id: &str) -> Result<DeploymentView, EnvironmentError> {
        let deployment = self.get_required::<DeploymentRecord>(id).await?;
        let instances = self
            .control()
            .query::<compute_state::WorkloadInstanceRecord>(
                Query::all(Collection::WorkloadInstance).eq("deployment_id", id.to_string()),
            )
            .await?;
        let mut connections = std::collections::BTreeMap::new();
        for instance in &instances {
            connections.insert(
                instance.id.clone(),
                self.data_plane()
                    .connections(&instance.id)
                    .await
                    .map(|(open, _)| open)
                    .unwrap_or(0),
            );
        }
        let inner = self.inner.lock().await;
        let instances = instances
            .into_iter()
            .map(|instance| {
                let unit = Unit::new(
                    &super::Desired::instance_key(&instance.value),
                    &instance.value.deployment_id,
                );
                InstanceView {
                    actual_state: inner.runtime.get(&unit).and_then(|runtime| runtime.state),
                    open_connections: connections.get(&instance.id).copied().unwrap_or(0),
                    instance_id: instance.id,
                    record: instance.value,
                }
            })
            .collect();
        Ok(DeploymentView {
            deployment_id: deployment.id,
            record: deployment.value,
            instances,
        })
    }

    pub async fn execution(&self, execution_id: &str) -> Result<ExecutionView, EnvironmentError> {
        // Durable state first; then evidence this controller holds but has
        // not written yet (control state was down when it ended).
        let record = match self
            .get_required::<ExecutionRecord>(&ids::execution(execution_id))
            .await
        {
            Ok(record) => record.value,
            Err(failure) => {
                let inner = self.inner.lock().await;
                // This controller's own evidence, not confirmed by durable
                // state: the response says so.
                if let Some(loaded) = inner.loaded {
                    crate::auth::set_freshness("stale", loaded.as_of);
                }
                inner
                    .terminal
                    .get(execution_id)
                    .cloned()
                    .or_else(|| {
                        inner
                            .pending_evidence
                            .iter()
                            .filter_map(|pending| pending.record.as_ref())
                            .find(|record| record.execution_id == execution_id)
                            .cloned()
                    })
                    .ok_or(failure)?
            }
        };
        let (stdout, stderr) = self
            .inner
            .lock()
            .await
            .outputs
            .get(execution_id)
            .cloned()
            .unwrap_or_default();
        Ok(ExecutionView {
            record,
            stdout,
            stderr,
        })
    }

    /// Recent executions of a project in an environment, newest first.
    pub async fn executions(
        &self,
        environment: &str,
        project: &str,
        limit: usize,
    ) -> Result<Vec<ExecutionRecord>, EnvironmentError> {
        let (membership, _) = self.membership(environment, project).await?;
        let executions = self
            .control()
            .query::<ExecutionRecord>(
                Query::all(Collection::Execution)
                    .eq("project_id", membership.value.project_id.clone())
                    .eq("environment_id", membership.value.environment_id.clone())
                    .descending("started_at")
                    .limit(limit),
            )
            .await?;
        Ok(executions.into_iter().map(|record| record.value).collect())
    }

    /// Receipt references of a project in an environment, newest first.
    pub async fn receipts(
        &self,
        environment: &str,
        project: &str,
        limit: usize,
    ) -> Result<Vec<ReceiptRecord>, EnvironmentError> {
        let (membership, _) = self.membership(environment, project).await?;
        let receipts = self
            .control()
            .query::<ReceiptRecord>(
                Query::all(Collection::Receipt)
                    .eq("project_id", membership.value.project_id.clone())
                    .eq("environment_id", membership.value.environment_id.clone())
                    .descending("created_at")
                    .limit(limit),
            )
            .await?;
        Ok(receipts.into_iter().map(|record| record.value).collect())
    }

    /// The receipt document itself, from durable artifacts.
    pub async fn receipt(&self, receipt_id: &str) -> Result<serde_json::Value, EnvironmentError> {
        let reference = self
            .get_required::<ReceiptRecord>(&ids::receipt(receipt_id))
            .await?;
        let digest = reference.value.artifact_digest.ok_or_else(|| {
            EnvironmentError::NotFound(format!("the receipt document of {receipt_id}"))
        })?;
        let bytes = self
            .config
            .artifacts
            .get(&digest)
            .await?
            .ok_or_else(|| EnvironmentError::NotFound(format!("receipt artifact {digest}")))?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// A deployment's receipt document.
    pub async fn deployment_receipt_document(
        &self,
        deployment_id: &str,
    ) -> Result<serde_json::Value, EnvironmentError> {
        let deployment = self.get_required::<DeploymentRecord>(deployment_id).await?;
        let digest = deployment.value.receipt.ok_or_else(|| {
            EnvironmentError::NotFound(format!(
                "a receipt for {deployment_id}; one is written when the release ends"
            ))
        })?;
        let bytes = self
            .config
            .artifacts
            .get(&digest)
            .await?
            .ok_or_else(|| EnvironmentError::NotFound(format!("receipt artifact {digest}")))?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Lifecycle events, oldest first.
    pub async fn events(&self, filter: EventFilter) -> Result<Vec<EventRecord>, EnvironmentError> {
        let limit = filter.limit.unwrap_or(200).min(1000);
        let mut query = Query::all(Collection::Event);
        if let Some(after) = filter.after {
            query = query.gt("sequence", after);
        }
        if let Some(environment) = &filter.environment {
            query = query.eq("environment", environment.clone());
        }
        if let Some(project) = &filter.project {
            query = query.eq("project", project.clone());
        }
        if let Some(deployment) = &filter.deployment_id {
            query = query.eq("deployment_id", deployment.clone());
        }
        // Without `after`, the most recent events; with it, the next ones.
        let events = if filter.after.is_some() {
            self.control()
                .query::<EventRecord>(query.ascending("sequence").limit(limit))
                .await?
        } else {
            let mut events = self
                .control()
                .query::<EventRecord>(query.descending("sequence").limit(limit))
                .await?;
            events.reverse();
            events
        };
        Ok(events.into_iter().map(|event| event.value).collect())
    }
}

fn summary(deployment: Stored<DeploymentRecord>) -> DeploymentSummary {
    DeploymentSummary {
        deployment_id: deployment.id,
        version: deployment.value.version,
        status: deployment.value.status,
        revision: deployment.value.revision,
        created_at: deployment.value.created_at,
        updated_at: deployment.value.updated_at,
        promoted_from: deployment.value.promoted_from,
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
