//! Recovering the data plane after a controller restart, and leaving it
//! running when the controller stops.
//!
//! On start, the controller asks the data plane what it runs. A unit still
//! running is reattached: the controller supervises it again without
//! restarting it. A unit that ended while no controller was running has
//! its outcome recorded as evidence. A unit a dead supervisor left behind
//! is reported orphaned. Reconciliation then decides, from durable desired
//! state, what else should run — never the registry.

use std::sync::Arc;

use chrono::Utc;
use compute_core::ExecutionControl;
use compute_state::events;
use serde_json::json;

use super::execute::Invocation;
use super::{Change, Daemon, Scope, Unit};
use crate::EnvironmentError;
use crate::dataplane::UnitOutcome;
use crate::model::*;
use crate::status::*;

/// What a controller found when it started.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Recovery {
    /// Units found running and supervised again, untouched.
    pub reattached: Vec<String>,
    /// Units that ended while no controller ran; their evidence was
    /// recorded.
    pub collected: Vec<String>,
    /// Units a stopped supervisor left behind.
    pub orphaned: Vec<String>,
}

impl Daemon {
    pub(crate) async fn reattach(self: &Arc<Self>) -> Result<Recovery, EnvironmentError> {
        let mut recovery = Recovery::default();
        let units = self.data_plane().units().await?;
        // Units this controller already supervises: their waiters collect
        // them.
        let supervised = self
            .inner
            .lock()
            .await
            .runtime
            .values()
            .filter(|runtime| runtime.handle.is_some())
            .filter_map(|runtime| runtime.unit_id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let mut change = Change::new();
        for status in units {
            if supervised.contains(&status.manifest.unit_id) {
                continue;
            }
            let manifest = status.manifest.clone();
            let key = (
                manifest.environment.clone(),
                manifest.project.clone(),
                manifest.workload.clone(),
            );
            let unit = Unit::new(&key, &manifest.deployment_id);
            let label = format!("{}/{}/{}", key.0, key.1, key.2);
            let scope = Scope::workload(&key).deployment(&manifest.deployment_id);
            let placement = PlacementView {
                placement_id: manifest.placement_id.clone(),
                provider: manifest.provider.clone(),
                node: None,
            };
            change = self.event(
                change,
                events::WORKLOAD_DISCOVERED,
                scope.clone(),
                format!("{label} found {} on the data plane", status.state),
                json!({ "unit_id": manifest.unit_id, "pid": status.process.as_ref().map(|process| process.pid) }),
            );
            match status.outcome {
                None => {
                    let mut control = ExecutionControl::new();
                    if let Some(directory) = &manifest.log_directory {
                        control = control.with_log_directory(directory);
                    }
                    let generation = {
                        let mut inner = self.inner.lock().await;
                        let runtime = inner.runtime.entry(unit.clone()).or_default();
                        runtime.service = true;
                        runtime.generation += 1;
                        runtime.state = Some(ActualState::Running);
                        runtime.held = false;
                        runtime.control = Some(control.clone());
                        runtime.deployment_id = Some(manifest.deployment_id.clone());
                        runtime.started_at = Some(manifest.started_at);
                        runtime.running_since = Some(Utc::now());
                        runtime.finished_at = None;
                        runtime.exit_code = None;
                        runtime.error = None;
                        runtime.log_directory = manifest
                            .log_directory
                            .as_ref()
                            .map(std::path::PathBuf::from);
                        runtime.unit_id = Some(manifest.unit_id.clone());
                        runtime.pid = status.process.as_ref().map(|process| process.pid);
                        runtime.placement = placement.clone();
                        runtime.generation
                    };
                    change = self.event(
                        change,
                        events::WORKLOAD_REATTACHED,
                        scope,
                        format!(
                            "{label} reattached without a restart (pid {})",
                            status
                                .process
                                .as_ref()
                                .map(|process| process.pid.to_string())
                                .unwrap_or_else(|| "unknown".into())
                        ),
                        json!({ "unit_id": manifest.unit_id, "pid": status.process.as_ref().map(|process| process.pid) }),
                    );
                    let daemon = self.clone();
                    let waited = unit.clone();
                    let unit_id = manifest.unit_id.clone();
                    let started_at = manifest.started_at;
                    let deployment_id = manifest.deployment_id.clone();
                    let handle = tokio::spawn(async move {
                        let outcome = daemon.wait_unit(&unit_id, &control).await;
                        let outcome = daemon.unit_outcome(outcome, None, placement);
                        let _ = daemon
                            .finish(
                                &waited,
                                Invocation {
                                    generation,
                                    started_at,
                                    unit_id: Some(unit_id),
                                },
                                outcome,
                                Some(deployment_id),
                                true,
                            )
                            .await;
                    });
                    if let Some(runtime) = self.inner.lock().await.runtime.get_mut(&unit)
                        && runtime.generation == generation
                    {
                        runtime.handle = Some(handle);
                    }
                    recovery.reattached.push(label);
                }
                Some(outcome) => {
                    if let UnitOutcome::Orphaned { message } = &outcome {
                        change = self.event(
                            change,
                            events::WORKLOAD_ORPHANED,
                            scope,
                            format!("{label} was orphaned: {message}"),
                            json!({ "unit_id": manifest.unit_id }),
                        );
                        recovery.orphaned.push(label.clone());
                    } else {
                        recovery.collected.push(label.clone());
                    }
                    let outcome = self.unit_outcome(Ok(outcome), None, placement);
                    // Evidence only: a newer run of the unit, if any, keeps
                    // its runtime view.
                    let _ = self
                        .finish(
                            &unit,
                            Invocation {
                                generation: u64::MAX,
                                started_at: manifest.started_at,
                                unit_id: Some(manifest.unit_id.clone()),
                            },
                            outcome,
                            Some(manifest.deployment_id.clone()),
                            true,
                        )
                        .await;
                }
            }
        }
        let _ = self.apply(change).await;
        Ok(recovery)
    }

    /// Stop the controller and leave every workload running on the data
    /// plane, for a restart or an upgrade. Refused when the data plane
    /// lives in this process: its workloads cannot outlive it.
    pub async fn detach(self: &Arc<Self>) -> Result<(), EnvironmentError> {
        if !self.data_plane().independent() {
            return Err(EnvironmentError::Invalid(
                "this controller runs its workloads in its own process; they cannot outlive it"
                    .into(),
            ));
        }
        let _ = self.shutdown.send(true);
        let _guard = self.reconciling.lock().await;
        // Stop supervising; the data plane keeps running and keeps every
        // outcome until a controller acknowledges it.
        let handles = self
            .inner
            .lock()
            .await
            .runtime
            .values_mut()
            .filter_map(|runtime| runtime.handle.take())
            .collect::<Vec<_>>();
        for handle in handles {
            handle.abort();
        }
        let change = self.event(
            Change::new(),
            events::CONTROLLER_STOPPED,
            Scope::default(),
            format!(
                "Compute controller {} stopped; its workloads keep running",
                self.instance_id
            ),
            json!({ "workloads": "kept" }),
        );
        let _ = self.apply(change).await;
        let _ = self.stopped.send(true);
        Ok(())
    }
}
