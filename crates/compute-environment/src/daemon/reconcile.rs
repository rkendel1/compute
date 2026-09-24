//! The reconciler: durable desired state in, physical execution out, and
//! observed actual state back.
//!
//! ```text
//! desired state (control state)
//!        │ refresh
//!        ▼
//!    reconcile ── missing → start · wrong revision → replace
//!        │        should not run → stop · correct → no-op
//!        ▼
//!    observe ──── actual state, health, deployment progress → control state
//! ```
//!
//! A cycle that cannot read control state changes nothing: Compute fails
//! closed rather than acting on intent it cannot confirm.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use compute_core::ExecutionControl;
use compute_state::events;
use compute_state::{DeploymentStatus, WorkloadStatusRecord, ids};
use serde_json::json;

use super::{Change, Daemon, Key, Scope};
use crate::model::*;
use crate::status::*;

impl Daemon {
    /// One reconciliation cycle.
    pub async fn reconcile(self: &Arc<Self>) {
        let _cycle = self.reconciling.lock().await;
        if self.is_shutting_down() {
            return;
        }
        if self.refresh().await.is_err() {
            return;
        }
        let (to_start, to_stop) = {
            let inner = self.inner.lock().await;
            let desired = &inner.desired;
            let mut to_start = vec![];
            let mut to_stop = BTreeSet::new();
            for (key, workload) in &desired.workloads {
                if !desired.should_run(key) {
                    continue;
                }
                let runtime = inner.runtime.get(key);
                let active = runtime.is_some_and(|runtime| {
                    matches!(
                        runtime.state,
                        Some(ActualState::Starting | ActualState::Running)
                    )
                });
                let current = runtime.is_some_and(|runtime| {
                    runtime.deployment_id.as_deref() == Some(workload.value.deployment_id.as_str())
                });
                let held = runtime.is_some_and(|runtime| runtime.held);
                if active && !current {
                    // Wrong revision: replace.
                    to_stop.insert(key.clone());
                    to_start.push(key.clone());
                } else if !active && (!held || !current) {
                    to_start.push(key.clone());
                }
            }
            for (key, runtime) in &inner.runtime {
                let active = matches!(
                    runtime.state,
                    Some(ActualState::Starting | ActualState::Running)
                );
                if active && !desired.should_run(key) {
                    to_stop.insert(key.clone());
                }
            }
            (to_start, to_stop.into_iter().collect::<Vec<_>>())
        };
        self.stop_keys(&to_stop).await;
        {
            let mut inner = self.inner.lock().await;
            let desired_keys = inner
                .desired
                .workloads
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>();
            inner.runtime.retain(|key, runtime| {
                desired_keys.contains(key)
                    || matches!(
                        runtime.state,
                        Some(ActualState::Starting | ActualState::Running | ActualState::Stopping)
                    )
            });
        }
        for key in to_start {
            self.start_service(key).await;
        }
        // Read our own observations back, so every view reflects them.
        if self.observe().await {
            let _ = self.refresh().await;
        }
        self.inner.lock().await.last_reconciled_at = Some(Utc::now());
    }

    async fn start_service(self: &Arc<Self>, key: Key) {
        let control;
        let generation;
        {
            let mut inner = self.inner.lock().await;
            let Some(deployment_id) = inner
                .desired
                .workloads
                .get(&key)
                .map(|workload| workload.value.deployment_id.clone())
            else {
                return;
            };
            let runtime = inner.runtime.entry(key.clone()).or_default();
            runtime.generation += 1;
            generation = runtime.generation;
            let log_directory = self.logs_dir(&key).join(format!(
                "{}-{generation:04}",
                Utc::now().format("%Y%m%dT%H%M%S%.3fZ")
            ));
            control = ExecutionControl::new().with_log_directory(&log_directory);
            runtime.state = Some(ActualState::Starting);
            runtime.held = false;
            runtime.control = Some(control.clone());
            runtime.deployment_id = Some(deployment_id);
            runtime.started_at = Some(Utc::now());
            runtime.finished_at = None;
            runtime.exit_code = None;
            runtime.error = None;
            runtime.health = None;
            runtime.log_directory = Some(log_directory);
        }
        let daemon = self.clone();
        let task_key = key.clone();
        let handle = tokio::spawn(async move {
            let (outcome, deployment_id) =
                daemon.execute(&task_key, Some((generation, control))).await;
            daemon
                .finish(&task_key, generation, outcome, deployment_id, true)
                .await;
        });
        let mut inner = self.inner.lock().await;
        if let Some(runtime) = inner.runtime.get_mut(&key)
            && runtime.generation == generation
        {
            runtime.handle = Some(handle);
        }
    }

    /// Stop services and wait until each has stopped.
    pub(crate) async fn stop_keys(self: &Arc<Self>, keys: &[Key]) {
        let mut handles = vec![];
        {
            let mut inner = self.inner.lock().await;
            for key in keys {
                if let Some(runtime) = inner.runtime.get_mut(key)
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
                if let Some(runtime) = inner.runtime.get_mut(&key) {
                    runtime.error = Some("service did not stop within 30s".into());
                }
            }
            let mut inner = self.inner.lock().await;
            if let Some(runtime) = inner.runtime.get_mut(&key)
                && runtime.state == Some(ActualState::Stopping)
            {
                runtime.state = Some(ActualState::Stopped);
            }
        }
    }

    /// Write observed actual state and deployment progress back to control
    /// state, with the events that mark transitions. Returns whether
    /// anything was written.
    pub(crate) async fn observe(self: &Arc<Self>) -> bool {
        let stored = match self.control().list::<WorkloadStatusRecord>().await {
            Ok(statuses) => statuses
                .into_iter()
                .map(|status| (status.id.clone(), status))
                .collect::<BTreeMap<_, _>>(),
            Err(error) => {
                self.inner.lock().await.state_error = Some(error.to_string());
                return false;
            }
        };
        // Observe every workload, probing listening services.
        let snapshot = {
            let inner = self.inner.lock().await;
            inner
                .desired
                .workloads
                .iter()
                .map(|(key, record)| {
                    let runtime = inner.runtime.get(key);
                    let state = runtime.and_then(|runtime| runtime.state).unwrap_or(
                        match record.value.kind {
                            WorkloadKind::Task => ActualState::Pending,
                            WorkloadKind::Service => ActualState::Stopped,
                        },
                    );
                    (
                        key.clone(),
                        record.clone(),
                        state,
                        runtime.and_then(|runtime| runtime.health),
                        runtime.and_then(|runtime| runtime.execution_id.clone()),
                        runtime.map_or(0, |runtime| runtime.restarts),
                        runtime.and_then(|runtime| runtime.error.clone()),
                        runtime.map_or(0, |runtime| runtime.consecutive_failures),
                        runtime.is_some_and(|runtime| runtime.held),
                    )
                })
                .collect::<Vec<_>>()
        };
        let mut change = Change::new();
        let mut observed = BTreeMap::new();
        for (key, record, state, previous_health, execution_id, restarts, error, failures, held) in
            snapshot
        {
            let health = workload_health(record.value.kind, state, &record.value.ports).await;
            observed.insert(key.clone(), (state, health, failures, held));
            if record.value.kind == WorkloadKind::Service && state == ActualState::Running {
                let scope = Scope::workload(&key).deployment(&record.value.deployment_id);
                match (previous_health, health) {
                    (None, _) => {
                        change = self.event(
                            change,
                            events::SERVICE_STARTED,
                            scope.clone(),
                            format!("{} started in {}/{}", key.2, key.0, key.1),
                            json!({ "ports": record.value.ports }),
                        );
                        if health == Health::Healthy {
                            change = self.event(
                                change,
                                events::SERVICE_HEALTHY,
                                scope,
                                format!("{} is healthy in {}/{}", key.2, key.0, key.1),
                                json!({}),
                            );
                        }
                    }
                    (Some(before), after) if before != after => {
                        let (kind, word) = if after == Health::Healthy {
                            (events::SERVICE_HEALTHY, "healthy")
                        } else {
                            (events::SERVICE_UNHEALTHY, "unhealthy")
                        };
                        change = self.event(
                            change,
                            kind,
                            scope,
                            format!("{} is {word} in {}/{}", key.2, key.0, key.1),
                            json!({}),
                        );
                    }
                    _ => {}
                }
            }
            {
                let mut inner = self.inner.lock().await;
                if let Some(runtime) = inner.runtime.get_mut(&key) {
                    runtime.health = (state == ActualState::Running).then_some(health);
                }
            }
            let status = WorkloadStatusRecord {
                workload_id: record.id.clone(),
                environment: key.0.clone(),
                project: key.1.clone(),
                workload: key.2.clone(),
                actual_state: state.as_str().into(),
                health: health.as_str().into(),
                deployment_id: Some(record.value.deployment_id.clone()),
                execution_id,
                restarts,
                error,
                observed_by: self.instance_id.clone(),
                observed_at: Utc::now(),
            };
            let id = ids::workload_status(&record.id);
            match stored.get(&id) {
                Some(existing) if same_observation(&existing.value, &status) => {}
                Some(existing) => change = change.with(|batch| batch.replace(existing, &status)),
                None => change = change.with(|batch| batch.create(&id, &status)),
            }
        }
        change = self.progress_deployments(change, &observed).await;
        if change.batch.is_empty() {
            return false;
        }
        match self.apply(change).await {
            Ok(()) => true,
            Err(error) => {
                self.inner.lock().await.state_error = Some(error.to_string());
                false
            }
        }
    }

    /// Advance current deployments from what was observed.
    async fn progress_deployments(
        &self,
        mut change: Change,
        observed: &BTreeMap<Key, (ActualState, Health, u32, bool)>,
    ) -> Change {
        let desired = self.inner.lock().await.desired.clone();
        for ((environment, project), membership) in &desired.memberships {
            let Some(deployment_id) = &membership.value.deployment_id else {
                continue;
            };
            let Some(deployment) = desired.deployments.get(deployment_id) else {
                continue;
            };
            let status = deployment.value.status;
            if matches!(
                status,
                DeploymentStatus::Queued
                    | DeploymentStatus::Admitted
                    | DeploymentStatus::Placed
                    | DeploymentStatus::Failed
                    | DeploymentStatus::Superseded
            ) {
                continue;
            }
            let project_runs = membership.value.desired_state == DesiredState::Running
                && desired
                    .environments
                    .get(environment)
                    .is_some_and(|record| record.value.desired_state == DesiredState::Running);
            let services = desired
                .workloads_of(environment, project)
                .filter(|(_, record)| {
                    record.value.kind == WorkloadKind::Service
                        && record.value.desired_state == DesiredState::Running
                        && record.value.deployment_id == *deployment_id
                })
                .map(|(key, _)| observed.get(key).copied())
                .collect::<Vec<_>>();
            let failure = services
                .iter()
                .flatten()
                .find_map(|(state, _, failures, held)| match state {
                    ActualState::Denied => Some("a service was denied by admission"),
                    ActualState::Failed if *held && *failures >= 3 => {
                        Some("a service keeps failing")
                    }
                    ActualState::Failed if *held && *failures == 0 => {
                        Some("a service failed and has no restart policy")
                    }
                    _ => None,
                });
            let all_healthy = services.iter().all(|observation| {
                matches!(
                    observation,
                    Some((ActualState::Running, Health::Healthy, _, _))
                )
            });
            let next = if !project_runs {
                DeploymentStatus::Stopped
            } else if all_healthy {
                DeploymentStatus::Healthy
            } else if let Some(failure) = failure
                && status == DeploymentStatus::Starting
            {
                change =
                    change.with(|batch| batch.update(deployment, json!({ "failure": failure })));
                DeploymentStatus::Failed
            } else if status == DeploymentStatus::Stopped {
                DeploymentStatus::Starting
            } else {
                status
            };
            if next == status {
                continue;
            }
            change = change.with(|batch| {
                batch.update(
                    deployment,
                    json!({ "status": next, "updated_at": Utc::now() }),
                )
            });
            let scope = Scope::project(environment, project).deployment(deployment_id);
            let revision = &deployment.value.revision;
            change = match next {
                DeploymentStatus::Healthy if status == DeploymentStatus::Starting => self.event(
                    change,
                    events::DEPLOYMENT_COMPLETED,
                    scope,
                    format!("{project} {revision} is healthy in {environment}"),
                    json!({ "revision": revision }),
                ),
                DeploymentStatus::Failed => self.event(
                    change,
                    events::DEPLOYMENT_FAILED,
                    scope,
                    format!("{project} {revision} failed to start in {environment}"),
                    json!({ "revision": revision, "stage": "startup" }),
                ),
                _ => change,
            };
        }
        change
    }
}

fn same_observation(left: &WorkloadStatusRecord, right: &WorkloadStatusRecord) -> bool {
    left.actual_state == right.actual_state
        && left.health == right.health
        && left.deployment_id == right.deployment_id
        && left.execution_id == right.execution_id
        && left.restarts == right.restarts
        && left.error == right.error
        && left.observed_by == right.observed_by
}

/// Health of a workload: a running service is healthy when every declared
/// port accepts connections.
pub(crate) async fn workload_health(
    kind: WorkloadKind,
    state: ActualState,
    ports: &[PortBinding],
) -> Health {
    match (kind, state) {
        (WorkloadKind::Service, ActualState::Running) => {
            for binding in ports {
                let reachable = tokio::time::timeout(
                    Duration::from_millis(300),
                    tokio::net::TcpStream::connect(("127.0.0.1", binding.host)),
                )
                .await
                .is_ok_and(|result| result.is_ok());
                if !reachable {
                    return Health::Unhealthy;
                }
            }
            Health::Healthy
        }
        (_, ActualState::Failed | ActualState::Denied) => Health::Unhealthy,
        (WorkloadKind::Task, _) => Health::Healthy,
        _ => Health::Unknown,
    }
}
