//! The reconciler: durable desired state in, physical execution out, and
//! observed actual state back.
//!
//! ```text
//! desired state (control state)
//!        │ refresh
//!        ▼
//!    releases ─── advance each release in flight by what it can prove
//!        ▼
//!    converge ─── instance missing → start · should not run → stop
//!        │        correct → no-op
//!        ▼
//!    data plane ─ endpoints follow traffic assignments; ingress follows
//!        │        domains
//!        ▼
//!    observe ──── actual state and health → control state
//! ```
//!
//! A cycle that cannot read control state changes nothing: Compute fails
//! closed rather than acting on intent it cannot confirm.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use compute_core::ExecutionControl;
use compute_network::Route;
use compute_state::events;
use compute_state::{WorkloadStatusRecord, ids};
use serde_json::json;

use super::{Change, Daemon, Key, Scope, Unit, Want};
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
        if self.advance_releases().await && self.refresh().await.is_err() {
            return;
        }
        self.converge().await;
        self.sync_endpoints().await;
        // Read our own observations back, so every view reflects them.
        if self.observe().await {
            let _ = self.refresh().await;
        }
        self.reconcile_network().await;
        let mut inner = self.inner.lock().await;
        let releasing = inner.desired.in_flight().next().is_some();
        inner.releasing = releasing;
        inner.last_reconciled_at = Some(Utc::now());
    }

    /// Start every instance that should run and does not; stop every one
    /// that runs and should not.
    async fn converge(self: &Arc<Self>) {
        let (to_start, to_stop) = {
            let inner = self.inner.lock().await;
            let desired = &inner.desired;
            let mut to_start = vec![];
            let mut wanted = BTreeMap::new();
            for instance in desired.instances.values() {
                let unit = Unit::new(
                    &super::Desired::instance_key(&instance.value),
                    &instance.value.deployment_id,
                );
                let want = desired.want(&instance.value);
                wanted.insert(unit.clone(), want);
                if want != Want::Run {
                    continue;
                }
                let runtime = inner.runtime.get(&unit);
                let active = runtime.is_some_and(|runtime| {
                    matches!(
                        runtime.state,
                        Some(ActualState::Starting | ActualState::Running)
                    )
                });
                let held = runtime.is_some_and(|runtime| runtime.held);
                if !active && !held {
                    to_start.push(unit);
                }
            }
            let to_stop = inner
                .runtime
                .iter()
                .filter(|(unit, runtime)| {
                    runtime.service
                        && matches!(
                            runtime.state,
                            Some(ActualState::Starting | ActualState::Running)
                        )
                        && wanted.get(*unit).copied().unwrap_or(Want::Stop) == Want::Stop
                })
                .map(|(unit, _)| unit.clone())
                .collect::<Vec<_>>();
            (to_start, to_stop)
        };
        self.stop_units(&to_stop).await;
        {
            let mut inner = self.inner.lock().await;
            let desired = &inner.desired;
            let instances = desired
                .instances
                .values()
                .map(|instance| {
                    Unit::new(
                        &super::Desired::instance_key(&instance.value),
                        &instance.value.deployment_id,
                    )
                })
                .collect::<BTreeSet<_>>();
            let tasks = desired
                .workloads
                .iter()
                .filter(|(_, record)| record.value.kind == WorkloadKind::Task)
                .map(|(key, record)| Unit::new(key, &record.value.deployment_id))
                .collect::<BTreeSet<_>>();
            inner.runtime.retain(|unit, runtime| {
                instances.contains(unit)
                    || tasks.contains(unit)
                    || matches!(
                        runtime.state,
                        Some(ActualState::Starting | ActualState::Running | ActualState::Stopping)
                    )
            });
        }
        for unit in to_start {
            self.start_service(unit).await;
        }
    }

    async fn start_service(self: &Arc<Self>, unit: Unit) {
        let control;
        let generation;
        {
            let mut inner = self.inner.lock().await;
            let log_directory = self.logs_dir(&unit.key);
            let runtime = inner.runtime.entry(unit.clone()).or_default();
            runtime.service = true;
            runtime.generation += 1;
            generation = runtime.generation;
            let log_directory = log_directory.join(format!(
                "{}-{generation:04}",
                Utc::now().format("%Y%m%dT%H%M%S%.3fZ")
            ));
            control = ExecutionControl::new().with_log_directory(&log_directory);
            runtime.state = Some(ActualState::Starting);
            runtime.held = false;
            runtime.control = Some(control.clone());
            runtime.deployment_id = Some(unit.deployment_id.clone());
            runtime.started_at = Some(Utc::now());
            runtime.running_since = None;
            runtime.finished_at = None;
            runtime.exit_code = None;
            runtime.error = None;
            runtime.health = None;
            runtime.log_directory = Some(log_directory);
        }
        // Record the process group on this node once it exists.
        {
            let control = control.clone();
            let state_dir = self.config.state_dir.clone();
            let unit = unit.clone();
            tokio::spawn(async move {
                for _ in 0..600 {
                    if let Some(pid) = control.process_id() {
                        super::processes::record(&state_dir, &unit, pid);
                        return;
                    }
                    if control.is_cancelled() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            });
        }
        let daemon = self.clone();
        let task_unit = unit.clone();
        let handle = tokio::spawn(async move {
            let (outcome, deployment_id) = daemon
                .execute(&task_unit, Some((generation, control)))
                .await;
            daemon
                .finish(&task_unit, generation, outcome, deployment_id, true)
                .await;
        });
        let mut inner = self.inner.lock().await;
        if let Some(runtime) = inner.runtime.get_mut(&unit)
            && runtime.generation == generation
        {
            runtime.handle = Some(handle);
        }
    }

    /// Stop every running instance of these workloads and wait until each
    /// has stopped.
    pub(crate) async fn stop_keys(self: &Arc<Self>, keys: &[Key]) {
        let units = {
            let inner = self.inner.lock().await;
            inner
                .runtime
                .keys()
                .filter(|unit| keys.contains(&unit.key))
                .cloned()
                .collect::<Vec<_>>()
        };
        self.stop_units(&units).await;
    }

    /// Stop units and wait until each has stopped.
    pub(crate) async fn stop_units(self: &Arc<Self>, units: &[Unit]) {
        let mut handles = vec![];
        {
            let mut inner = self.inner.lock().await;
            for unit in units {
                if let Some(runtime) = inner.runtime.get_mut(unit)
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
                        handles.push((unit.clone(), handle));
                    }
                }
            }
        }
        for (unit, handle) in handles {
            if tokio::time::timeout(Duration::from_secs(30), handle)
                .await
                .is_err()
            {
                let mut inner = self.inner.lock().await;
                if let Some(runtime) = inner.runtime.get_mut(&unit) {
                    runtime.error = Some("service did not stop within 30s".into());
                }
            }
            let mut inner = self.inner.lock().await;
            if let Some(runtime) = inner.runtime.get_mut(&unit)
                && runtime.state == Some(ActualState::Stopping)
            {
                runtime.state = Some(ActualState::Stopped);
            }
        }
    }

    /// Point every endpoint at the instance its traffic assignment names.
    /// Endpoints nothing is assigned to stop listening.
    pub(crate) async fn sync_endpoints(&self) {
        let assignments = self
            .inner
            .lock()
            .await
            .desired
            .traffic
            .values()
            .map(|assignment| assignment.value.clone())
            .collect::<Vec<_>>();
        let mut errors = BTreeMap::new();
        let mut keep = BTreeSet::new();
        for assignment in assignments {
            keep.insert(assignment.host_port);
            if let Err(error) = self
                .endpoints()
                .assign(
                    assignment.host_port,
                    Route {
                        instance_id: assignment.instance_id.clone(),
                        target_port: assignment.target_port,
                    },
                )
                .await
            {
                errors.insert(
                    assignment.host_port,
                    format!(
                        "endpoint {} cannot listen on port {}: {error}",
                        assignment.endpoint, assignment.host_port
                    ),
                );
            }
        }
        self.endpoints().retain(&keep);
        self.inner.lock().await.endpoint_errors = errors;
    }

    /// Write observed actual state back to control state, with the events
    /// that mark transitions. Returns whether anything was written.
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
        // Observe each workload through the unit that serves it.
        let snapshot = {
            let inner = self.inner.lock().await;
            inner
                .desired
                .workloads
                .iter()
                .map(|(key, record)| {
                    let unit = Unit::new(key, &record.value.deployment_id);
                    let runtime = inner.runtime.get(&unit);
                    let state = runtime.and_then(|runtime| runtime.state).unwrap_or(
                        match record.value.kind {
                            WorkloadKind::Task => ActualState::Pending,
                            WorkloadKind::Service => ActualState::Stopped,
                        },
                    );
                    let ports = inner
                        .desired
                        .instance(key, &record.value.deployment_id)
                        .map(|instance| instance.value.ports.clone())
                        .unwrap_or_default();
                    (
                        key.clone(),
                        unit,
                        record.clone(),
                        state,
                        ports,
                        runtime.and_then(|runtime| runtime.health),
                        runtime.and_then(|runtime| runtime.execution_id.clone()),
                        runtime.map_or(0, |runtime| runtime.restarts),
                        runtime.and_then(|runtime| runtime.error.clone()),
                    )
                })
                .collect::<Vec<_>>()
        };
        let mut change = Change::new();
        for (key, unit, record, state, ports, previous_health, execution_id, restarts, error) in
            snapshot
        {
            let health = workload_health(record.value.kind, state, &ports).await;
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
                if let Some(runtime) = inner.runtime.get_mut(&unit) {
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

/// Whether a local port accepts connections.
pub(crate) async fn accepts(port: u16) -> bool {
    tokio::time::timeout(
        Duration::from_millis(300),
        tokio::net::TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    .is_ok_and(|result| result.is_ok())
}

/// Health of a workload: a running service is healthy when every port of
/// the instance that serves it accepts connections.
pub(crate) async fn workload_health(
    kind: WorkloadKind,
    state: ActualState,
    ports: &[PortBinding],
) -> Health {
    match (kind, state) {
        (WorkloadKind::Service, ActualState::Running) => {
            for binding in ports {
                if !accepts(binding.host).await {
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
