//! Lifecycle operations. Each writes desired state (and the events that
//! record it) to control state atomically, then reconciles. None of them
//! touches a process directly except to stop what a restart names.

use std::sync::Arc;

use chrono::Utc;
use compute_policy::Policy;
use compute_state::events;
use compute_state::{
    EnvironmentProjectRecord, EnvironmentRecord, ServiceRecord, WorkloadRecord,
    WorkloadStatusRecord, ids,
};
use serde_json::json;

use super::{Change, Daemon, Key, Scope};
use crate::EnvironmentError;
use crate::model::*;
use crate::status::*;

impl Daemon {
    // ---- Environments -----------------------------------------------------

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
        self.refresh().await?;
        if self
            .inner
            .lock()
            .await
            .desired
            .environments
            .contains_key(&definition.name)
        {
            return Err(EnvironmentError::Conflict(format!(
                "environment {} already exists",
                definition.name
            )));
        }
        let created_at = Utc::now();
        let id = format!(
            "env_{}",
            compute_state::short_digest(&[
                &definition.name,
                &created_at
                    .timestamp_nanos_opt()
                    .unwrap_or_default()
                    .to_string(),
                &self.instance_id,
            ])
        );
        let record = EnvironmentRecord {
            name: definition.name.clone(),
            desired_state: definition.desired_state,
            config: definition.env,
            policy: definition
                .policy
                .map(Policy::canonical)
                .map(|policy| serde_json::to_value(policy).expect("policies serialize")),
            provider: definition.provider,
            created_at,
        };
        let change = Change::new().with(|batch| batch.create(&id, &record));
        let change = self.event(
            change,
            events::ENVIRONMENT_CREATED,
            Scope::environment(&definition.name),
            format!("Environment {} created", definition.name),
            json!({ "environment_id": id }),
        );
        self.apply(change).await?;
        self.changed().await;
        self.environment(&definition.name).await
    }

    /// Stop and forget an environment: its desired state, memberships, and
    /// workloads. Deployments, executions, receipts, and events remain as
    /// history.
    pub async fn destroy_environment(self: &Arc<Self>, name: &str) -> Result<(), EnvironmentError> {
        // Changes are serialized with reconciliation: both write the
        // records a release touches.
        let cycle = self.reconciling.lock().await;
        self.refresh().await?;
        let (environment, memberships, workloads) = {
            let inner = self.inner.lock().await;
            let environment = inner
                .desired
                .environment(name)
                .cloned()
                .ok_or_else(|| EnvironmentError::NotFound(format!("environment {name}")))?;
            let env_name = environment.value.name.clone();
            let memberships = inner
                .desired
                .memberships
                .iter()
                .filter(|((environment, _), _)| *environment == env_name)
                .map(|(_, membership)| membership.clone())
                .collect::<Vec<_>>();
            let workloads = inner
                .desired
                .workloads
                .iter()
                .filter(|(key, _)| key.0 == env_name)
                .map(|(key, workload)| (key.clone(), workload.clone()))
                .collect::<Vec<_>>();
            (environment, memberships, workloads)
        };
        let env_name = environment.value.name.clone();
        let (instances, traffic) = {
            let inner = self.inner.lock().await;
            if let Some(domain) = inner
                .desired
                .domains
                .values()
                .find(|domain| domain.value.environment == env_name)
            {
                return Err(EnvironmentError::Conflict(format!(
                    "domain {} routes to {env_name}; remove it first",
                    domain.value.name
                )));
            }
            (
                inner
                    .desired
                    .instances
                    .values()
                    .filter(|instance| instance.value.environment == env_name)
                    .cloned()
                    .collect::<Vec<_>>(),
                inner
                    .desired
                    .traffic
                    .values()
                    .filter(|assignment| assignment.value.environment == env_name)
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        };
        let keys = workloads
            .iter()
            .map(|(key, _)| key.clone())
            .chain(
                instances
                    .iter()
                    .map(|instance| super::Desired::instance_key(&instance.value)),
            )
            .collect::<Vec<_>>();
        self.stop_keys(&keys).await;
        let mut change = Change::new().with(|batch| batch.delete(&environment));
        for membership in &memberships {
            change = change.with(|batch| batch.delete(membership));
        }
        for (_, workload) in &workloads {
            change = change.with(|batch| batch.delete(workload));
            change = self.delete_status(change, &workload.id).await?;
        }
        for instance in &instances {
            change = change.with(|batch| batch.delete(instance));
        }
        for assignment in &traffic {
            change = change.with(|batch| batch.delete(assignment));
        }
        let change = self.event(
            change,
            events::ENVIRONMENT_DESTROYED,
            Scope::environment(&env_name),
            format!("Environment {env_name} destroyed"),
            json!({ "environment_id": environment.id }),
        );
        self.apply(change).await?;
        self.inner
            .lock()
            .await
            .runtime
            .retain(|unit, _| unit.key.0 != env_name);
        drop(cycle);
        self.changed().await;
        Ok(())
    }

    pub async fn set_environment_state(
        self: &Arc<Self>,
        name: &str,
        desired: DesiredState,
        restart: bool,
    ) -> Result<EnvironmentView, EnvironmentError> {
        // Changes are serialized with reconciliation: both write the
        // records a release touches.
        let cycle = self.reconciling.lock().await;
        self.refresh().await?;
        let (environment, keys) = {
            let inner = self.inner.lock().await;
            let environment = inner
                .desired
                .environment(name)
                .cloned()
                .ok_or_else(|| EnvironmentError::NotFound(format!("environment {name}")))?;
            let keys = inner
                .desired
                .workloads
                .keys()
                .filter(|key| key.0 == environment.value.name)
                .cloned()
                .collect::<Vec<_>>();
            (environment, keys)
        };
        let env_name = environment.value.name.clone();
        let kind = match (desired, restart) {
            (_, true) => events::ENVIRONMENT_RESTARTED,
            (DesiredState::Running, false) => events::ENVIRONMENT_STARTED,
            (DesiredState::Stopped, false) => events::ENVIRONMENT_STOPPED,
        };
        let change = Change::new()
            .with(|batch| batch.update(&environment, json!({ "desired_state": desired })));
        let change = self.event(
            change,
            kind,
            Scope::environment(&env_name),
            format!("Environment {env_name} {}", verb(desired, restart)),
            json!({ "desired_state": desired }),
        );
        self.apply(change).await?;
        if restart {
            self.stop_keys(&keys).await;
        }
        if desired == DesiredState::Running {
            self.release_holds(&keys).await;
        }
        drop(cycle);
        self.changed().await;
        self.environment(&env_name).await
    }

    // ---- Projects in an environment --------------------------------------

    pub async fn set_project_state(
        self: &Arc<Self>,
        environment: &str,
        project: &str,
        desired: DesiredState,
        restart: bool,
    ) -> Result<ProjectView, EnvironmentError> {
        // Changes are serialized with reconciliation: both write the
        // records a release touches.
        let cycle = self.reconciling.lock().await;
        let (membership, keys) = self.membership(environment, project).await?;
        let env_name = membership.value.environment.clone();
        let kind = match (desired, restart) {
            (_, true) => events::PROJECT_RESTARTED,
            (DesiredState::Running, false) => events::PROJECT_STARTED,
            (DesiredState::Stopped, false) => events::PROJECT_STOPPED,
        };
        let change = Change::new().with(|batch| {
            batch.update(
                &membership,
                json!({ "desired_state": desired, "updated_at": Utc::now() }),
            )
        });
        let mut scope = Scope::project(&env_name, project);
        if let Some(deployment_id) = &membership.value.deployment_id {
            scope = scope.deployment(deployment_id);
        }
        let change = self.event(
            change,
            kind,
            scope,
            format!("{project} in {env_name} {}", verb(desired, restart)),
            json!({ "desired_state": desired }),
        );
        self.apply(change).await?;
        if restart {
            self.stop_keys(&keys).await;
        }
        if desired == DesiredState::Running {
            self.release_holds(&keys).await;
        }
        drop(cycle);
        self.changed().await;
        self.project(&env_name, project).await
    }

    /// Stop a project in one environment and remove it from there. The
    /// project, its revisions, and its other environments are unaffected.
    pub async fn remove_project(
        self: &Arc<Self>,
        environment: &str,
        project: &str,
    ) -> Result<(), EnvironmentError> {
        // Changes are serialized with reconciliation: both write the
        // records a release touches.
        let cycle = self.reconciling.lock().await;
        let (membership, keys) = self.membership(environment, project).await?;
        let env_name = membership.value.environment.clone();
        let (workloads, instances, traffic) = {
            let inner = self.inner.lock().await;
            let desired = &inner.desired;
            if let Some(domain) = desired.domains.values().find(|domain| {
                domain.value.environment == env_name && domain.value.project == project
            }) {
                return Err(EnvironmentError::Conflict(format!(
                    "domain {} routes to {project} in {env_name}; remove it first",
                    domain.value.name
                )));
            }
            (
                keys.iter()
                    .filter_map(|key| desired.workloads.get(key).cloned())
                    .collect::<Vec<_>>(),
                desired
                    .instances
                    .values()
                    .filter(|instance| {
                        instance.value.environment == env_name && instance.value.project == project
                    })
                    .cloned()
                    .collect::<Vec<_>>(),
                desired
                    .traffic
                    .values()
                    .filter(|assignment| {
                        assignment.value.environment == env_name
                            && assignment.value.project == project
                    })
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        };
        let stopping = keys
            .iter()
            .cloned()
            .chain(
                instances
                    .iter()
                    .map(|instance| super::Desired::instance_key(&instance.value)),
            )
            .collect::<Vec<_>>();
        self.stop_keys(&stopping).await;
        let mut change = Change::new().with(|batch| batch.delete(&membership));
        for workload in &workloads {
            change = change.with(|batch| batch.delete(workload));
            change = self.delete_status(change, &workload.id).await?;
        }
        for instance in &instances {
            change = change.with(|batch| batch.delete(instance));
        }
        for assignment in &traffic {
            change = change.with(|batch| batch.delete(assignment));
        }
        let change = self.event(
            change,
            events::PROJECT_REMOVED,
            Scope::project(&env_name, project),
            format!("{project} removed from {env_name}"),
            json!({ "project_id": membership.value.project_id }),
        );
        self.apply(change).await?;
        self.inner
            .lock()
            .await
            .runtime
            .retain(|unit, _| !(unit.key.0 == env_name && unit.key.1 == project));
        drop(cycle);
        self.changed().await;
        Ok(())
    }

    // ---- Workloads --------------------------------------------------------

    pub async fn set_workload_state(
        self: &Arc<Self>,
        environment: &str,
        project: &str,
        workload: &str,
        desired: DesiredState,
        restart: bool,
    ) -> Result<WorkloadView, EnvironmentError> {
        // Changes are serialized with reconciliation: both write the
        // records a release touches.
        let cycle = self.reconciling.lock().await;
        let (key, record) = self.workload_record(environment, project, workload).await?;
        if record.value.kind == WorkloadKind::Task {
            return Err(EnvironmentError::Invalid(format!(
                "{workload} is a task; run it instead of starting or stopping it"
            )));
        }
        let kind = if desired == DesiredState::Running {
            events::WORKLOAD_STARTED
        } else {
            events::WORKLOAD_STOPPED
        };
        let change =
            Change::new().with(|batch| batch.update(&record, json!({ "desired_state": desired })));
        let change = self.event(
            change,
            kind,
            Scope::workload(&key).deployment(&record.value.deployment_id),
            format!(
                "{} {} in {}/{}",
                workload,
                verb(desired, restart),
                key.0,
                key.1
            ),
            json!({ "desired_state": desired }),
        );
        self.apply(change).await?;
        if restart {
            self.stop_keys(std::slice::from_ref(&key)).await;
        }
        if desired == DesiredState::Running {
            self.release_holds(std::slice::from_ref(&key)).await;
        }
        drop(cycle);
        self.changed().await;
        self.workload(&key.0, &key.1, &key.2).await
    }

    // ---- Shared services --------------------------------------------------

    pub async fn register_service(
        self: &Arc<Self>,
        definition: ServiceDefinition,
    ) -> Result<ServiceRecord, EnvironmentError> {
        validate_name("service", &definition.name)?;
        if self.pool.member(&definition.provider).is_none() {
            return Err(EnvironmentError::Invalid(format!(
                "provider {} is not in the daemon's pool",
                definition.provider
            )));
        }
        let id = ids::service(&definition.name);
        let existing = self.control().get::<ServiceRecord>(&id).await?;
        let now = Utc::now();
        let record = ServiceRecord {
            name: definition.name.clone(),
            capabilities: definition.capabilities,
            provider: definition.provider,
            environment: definition.environment,
            project: definition.project,
            workload: definition.workload,
            endpoint: definition.endpoint,
            description: definition.description,
            created_at: existing
                .as_ref()
                .map_or(now, |existing| existing.value.created_at),
            updated_at: now,
        };
        let change = Change::new().with(|batch| match &existing {
            Some(existing) => batch.replace(existing, &record),
            None => batch.create(&id, &record),
        });
        let change = self.event(
            change,
            events::SHARED_SERVICE_REGISTERED,
            Scope::default(),
            format!("Shared service {} registered", definition.name),
            json!({ "service": definition.name, "capabilities": record.capabilities }),
        );
        self.apply(change).await?;
        Ok(record)
    }

    pub async fn remove_service(self: &Arc<Self>, name: &str) -> Result<(), EnvironmentError> {
        let existing = self
            .get_required::<ServiceRecord>(&ids::service(name))
            .await?;
        let change = Change::new().with(|batch| batch.delete(&existing));
        let change = self.event(
            change,
            events::SHARED_SERVICE_REMOVED,
            Scope::default(),
            format!("Shared service {name} removed"),
            json!({ "service": name }),
        );
        self.apply(change).await
    }

    pub async fn services(&self) -> Result<Vec<ServiceRecord>, EnvironmentError> {
        Ok(self
            .control()
            .query::<ServiceRecord>(
                compute_state::Query::all(compute_state::Collection::Service).ascending("name"),
            )
            .await?
            .into_iter()
            .map(|service| service.value)
            .collect())
    }

    pub async fn providers(&self) -> Result<Vec<compute_state::ProviderRecord>, EnvironmentError> {
        Ok(self
            .control()
            .query::<compute_state::ProviderRecord>(
                compute_state::Query::all(compute_state::Collection::Provider)
                    .ascending("provider_id"),
            )
            .await?
            .into_iter()
            .map(|provider| provider.value)
            .collect())
    }

    // ---- Helpers ----------------------------------------------------------

    /// A membership and its workload keys, read fresh.
    pub(crate) async fn membership(
        &self,
        environment: &str,
        project: &str,
    ) -> Result<(compute_state::Stored<EnvironmentProjectRecord>, Vec<Key>), EnvironmentError> {
        self.refresh().await?;
        let inner = self.inner.lock().await;
        let env_name = inner
            .desired
            .environment(environment)
            .map(|environment| environment.value.name.clone())
            .ok_or_else(|| EnvironmentError::NotFound(format!("environment {environment}")))?;
        let membership = inner
            .desired
            .memberships
            .get(&(env_name.clone(), project.to_string()))
            .cloned()
            .ok_or_else(|| {
                EnvironmentError::NotFound(format!("project {project} in {env_name}"))
            })?;
        let keys = inner
            .desired
            .workloads_of(&env_name, project)
            .map(|(key, _)| key.clone())
            .collect();
        Ok((membership, keys))
    }

    pub(crate) async fn workload_record(
        &self,
        environment: &str,
        project: &str,
        workload: &str,
    ) -> Result<(Key, compute_state::Stored<WorkloadRecord>), EnvironmentError> {
        self.refresh_for_read().await?;
        let inner = self.inner.lock().await;
        let env_name = inner
            .desired
            .environment(environment)
            .map(|environment| environment.value.name.clone())
            .ok_or_else(|| EnvironmentError::NotFound(format!("environment {environment}")))?;
        let key = (env_name, project.to_string(), workload.to_string());
        let record = inner.desired.workloads.get(&key).cloned().ok_or_else(|| {
            EnvironmentError::NotFound(format!("workload {workload} in {}/{project}", key.0))
        })?;
        Ok((key, record))
    }

    pub(crate) async fn release_holds(&self, keys: &[Key]) {
        let mut inner = self.inner.lock().await;
        for (unit, runtime) in inner.runtime.iter_mut() {
            if keys.contains(&unit.key) {
                runtime.held = false;
                runtime.consecutive_failures = 0;
            }
        }
    }

    pub(crate) async fn delete_status(
        &self,
        change: Change,
        workload_id: &str,
    ) -> Result<Change, EnvironmentError> {
        Ok(
            match self
                .control()
                .get::<WorkloadStatusRecord>(&ids::workload_status(workload_id))
                .await?
            {
                Some(status) => change.with(|batch| batch.delete(&status)),
                None => change,
            },
        )
    }
}

fn verb(desired: DesiredState, restart: bool) -> &'static str {
    match (desired, restart) {
        (_, true) => "restarted",
        (DesiredState::Running, false) => "started",
        (DesiredState::Stopped, false) => "stopped",
    }
}
