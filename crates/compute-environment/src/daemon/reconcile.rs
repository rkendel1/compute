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
    /// One reconciliation cycle, measured. It begins with a full read of
    /// desired state, so changes made anywhere are acted on.
    pub async fn reconcile(self: &Arc<Self>) {
        self.reconcile_with(true).await;
    }

    /// A cycle right after this controller changed desired state, which
    /// it read in full just before: only what the change wrote is read
    /// again, and only what differs is acted on.
    pub(crate) async fn reconcile_targeted(self: &Arc<Self>) {
        self.reconcile_with(false).await;
    }

    async fn reconcile_with(self: &Arc<Self>, full: bool) {
        let _cycle = self.reconciling.lock().await;
        if self.is_shutting_down() {
            return;
        }
        let started_at = Utc::now();
        let started = std::time::Instant::now();
        let mut phases = std::collections::BTreeMap::new();
        self.inner.lock().await.cycle_changes = 0;
        let completed = self.reconcile_phases(&mut phases, full).await;
        let mut inner = self.inner.lock().await;
        let examined = inner.desired.workloads.len()
            + inner.desired.instances.len()
            + inner.desired.traffic.len()
            + inner.desired.in_flight().count();
        let errors = usize::from(!completed || inner.state_error.is_some());
        let duration = started.elapsed().as_secs_f64();
        let metrics = &mut inner.reconcile;
        metrics.cycles += 1;
        metrics.errors_total += errors as u64;
        metrics.duration_seconds_total += duration;
        let changed = inner.cycle_changes;
        let first = inner.reconcile.cycles == 1;
        let cycle = ReconcileCycle {
            started_at,
            duration_ms: duration * 1000.0,
            resources_examined: examined,
            resources_changed: changed,
            errors,
            phases_ms: phases,
        };
        inner.reconcile.last = Some(cycle.clone());
        let recordable = inner.state_error.is_none();
        drop(inner);
        // Full cycles that did something are recorded; idle ones are only
        // counted, so a quiet node writes nothing.
        if full && recordable && (first || changed > 0 || errors > 0) {
            let mut change = self.event(
                Change::new(),
                events::RECONCILE_STARTED,
                Scope::default(),
                format!("reconciliation started at {}", started_at.to_rfc3339()),
                json!({ "started_at": started_at }),
            );
            change = self.event(
                change,
                events::RECONCILE_FINISHED,
                Scope::default(),
                format!(
                    "reconciliation finished in {:.1} ms: {examined} resources examined, {changed} changed, {errors} errors",
                    cycle.duration_ms
                ),
                json!({ "cycle": cycle }),
            );
            let _ = self.apply(change).await;
        }
    }

    async fn reconcile_phases(
        self: &Arc<Self>,
        phases: &mut std::collections::BTreeMap<String, f64>,
        full: bool,
    ) -> bool {
        let mut clock = std::time::Instant::now();
        let mut lap = |phases: &mut std::collections::BTreeMap<String, f64>, name: &str| {
            *phases.entry(name.to_string()).or_default() += clock.elapsed().as_secs_f64() * 1000.0;
            clock = std::time::Instant::now();
        };
        // A supervisor that died is replaced first: the new one restores
        // the endpoints and reports what the old one left behind.
        if self.data_plane().independent()
            && let Ok(true) = self.data_plane().recover().await
        {
            let info = self.data_plane().info().await.ok();
            let change = self.event(
                Change::new(),
                events::DATA_PLANE_RESTARTED,
                Scope::default(),
                "the supervisor was unreachable; a new one was started".into(),
                json!({ "data_plane": info }),
            );
            let _ = self.apply(change).await;
            let _ = self.reattach().await;
        }
        lap(phases, "data_plane");
        let refreshed = if full {
            self.refresh().await
        } else {
            self.refresh_targeted().await
        };
        if refreshed.is_err() {
            self.announce_outage().await;
            lap(phases, "refresh");
            return false;
        }
        lap(phases, "refresh");
        self.record_recovery().await;
        self.flush_pending_evidence().await;
        self.flush_pending_audit().await;
        // Credentials revoked or created elsewhere reach this node's cache.
        if self
            .authority
            .loaded_at()
            .is_none_or(|at| Utc::now() - at > chrono::TimeDelta::seconds(30))
        {
            let _ = self.load_credentials().await;
        }
        lap(phases, "pending");
        let advanced = self.advance_releases().await;
        lap(phases, "releases");
        if advanced && self.refresh_targeted().await.is_err() {
            return false;
        }
        lap(phases, "refresh");
        self.converge().await;
        lap(phases, "converge");
        self.sync_endpoints().await;
        lap(phases, "endpoints");
        // Read our own observations back, so every view reflects them.
        if self.observe().await {
            let _ = self.refresh_targeted().await;
        }
        lap(phases, "observe");
        self.reconcile_network().await;
        lap(phases, "network");
        let mut inner = self.inner.lock().await;
        let releasing = inner.desired.in_flight().next().is_some();
        inner.releasing = releasing;
        inner.last_reconciled_at = Some(Utc::now());
        true
    }

    /// Durable state became unreachable: say so once. The event cannot be
    /// written durably now; it is published live and kept in the node's
    /// log, and the recovery event records the whole outage.
    async fn announce_outage(self: &Arc<Self>) {
        let (since, error) = {
            let mut inner = self.inner.lock().await;
            if inner.outage_announced {
                return;
            }
            inner.outage_announced = true;
            (inner.degraded_since, inner.state_error.clone())
        };
        for (kind, message) in [
            (
                events::FELTDB_UNAVAILABLE,
                format!(
                    "durable control state is unreachable: {}",
                    error.clone().unwrap_or_default()
                ),
            ),
            (
                events::CONTROLLER_DEGRADED,
                "control plane degraded: workloads keep running, reads are served from the last snapshot, changes are refused".to_string(),
            ),
        ] {
            let change = self.event(
                Change::new(),
                kind,
                Scope::default(),
                message,
                json!({ "since": since, "error": error }),
            );
            for event in change.events {
                let _ = self.events.send(event);
            }
        }
    }

    /// Durable state answers again: continue its event sequence, pick up
    /// credential changes, and record the outage.
    async fn record_recovery(self: &Arc<Self>) {
        let Some(since) = self.inner.lock().await.recovered_from.take() else {
            return;
        };
        if let Ok(last) = self
            .control()
            .query::<compute_state::EventRecord>(
                compute_state::Query::all(compute_state::Collection::Event)
                    .descending("sequence")
                    .limit(1),
            )
            .await
            && let Some(last) = last.first()
        {
            self.sequence
                .fetch_max(last.value.sequence, std::sync::atomic::Ordering::SeqCst);
        }
        let _ = self.load_credentials().await;
        let seconds = (Utc::now() - since).num_milliseconds() as f64 / 1000.0;
        let change = self.event(
            Change::new(),
            events::FELTDB_RECOVERED,
            Scope::default(),
            format!("durable control state is reachable again after {seconds:.1}s"),
            json!({ "since": since, "outage_seconds": seconds }),
        );
        let _ = self.apply(change).await;
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
        self.inner.lock().await.cycle_changes += to_start.len() + to_stop.len();
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
        let started_at = Utc::now();
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
            runtime.started_at = Some(started_at);
            runtime.running_since = None;
            runtime.finished_at = None;
            runtime.exit_code = None;
            runtime.error = None;
            runtime.health = None;
            runtime.log_directory = Some(log_directory);
        }
        let daemon = self.clone();
        let task_unit = unit.clone();
        let handle = tokio::spawn(async move {
            let (outcome, deployment_id, unit_id) = daemon
                .execute(&task_unit, Some((generation, control)))
                .await;
            let invocation = super::execute::Invocation {
                generation,
                started_at,
                unit_id,
            };
            let _ = daemon
                .finish(&task_unit, invocation, outcome, deployment_id, true)
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
        let current = self.routes_snapshot();
        for assignment in assignments {
            keep.insert(assignment.host_port);
            let route = Route {
                instance_id: assignment.instance_id.clone(),
                target_port: assignment.target_port,
            };
            if current.get(&assignment.host_port) == Some(&route) {
                continue;
            }
            match self
                .data_plane()
                .assign(assignment.host_port, route.clone())
                .await
            {
                Ok(()) => {
                    self.routes
                        .lock()
                        .expect("routes")
                        .insert(assignment.host_port, route);
                    self.inner.lock().await.cycle_changes += 1;
                }
                Err(error) => {
                    errors.insert(
                        assignment.host_port,
                        format!(
                            "endpoint {} cannot listen on port {}: {}",
                            assignment.endpoint,
                            assignment.host_port,
                            error.message()
                        ),
                    );
                }
            }
        }
        if current.keys().any(|port| !keep.contains(port))
            && self.data_plane().retain(keep.clone()).await.is_ok()
        {
            self.routes
                .lock()
                .expect("routes")
                .retain(|port, _| keep.contains(port));
        }
        let appeared = {
            let mut inner = self.inner.lock().await;
            let appeared = errors
                .iter()
                .filter(|(port, _)| !inner.endpoint_errors.contains_key(port))
                .map(|(port, error)| (*port, error.clone()))
                .collect::<Vec<_>>();
            inner.endpoint_errors = errors;
            appeared
        };
        for (port, error) in appeared {
            let change = self.event(
                Change::new(),
                events::ENDPOINT_UNAVAILABLE,
                Scope::default(),
                error,
                json!({ "failure": "endpoint_unavailable", "host_port": port }),
            );
            let _ = self.apply(change).await;
        }
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
        // Probe every service at once: a sequential probe of each port made
        // every cycle cost time in proportion to the number of services.
        let mut probes = tokio::task::JoinSet::new();
        for (index, (_, _, record, state, ports, ..)) in snapshot.iter().enumerate() {
            let (kind, state, ports) = (record.value.kind, *state, ports.clone());
            probes.spawn(async move { (index, workload_health(kind, state, &ports).await) });
        }
        let mut healths = vec![Health::Unknown; snapshot.len()];
        while let Some(Ok((index, health))) = probes.join_next().await {
            healths[index] = health;
        }
        for (
            index,
            (key, unit, record, state, _ports, previous_health, execution_id, restarts, error),
        ) in snapshot.into_iter().enumerate()
        {
            let health = healths[index];
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
        self.inner.lock().await.cycle_changes += change.batch.len();
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
