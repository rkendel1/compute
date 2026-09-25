//! Preparing, admitting, placing, and executing one workload invocation,
//! and recording its outcome as durable evidence.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::Utc;
use compute_core::{ExecutionControl, ExecutionStatus, ReceiptScope, WorkloadBundle};
use compute_placement::{
    AdmissionContext, DiscoveryMode, PlacementReport, PlacementRequirements, ProviderKind,
    RequirementOptions, SubmissionMode, place,
};
use compute_policy::{ExecutionContract, Policy, PolicySourceKind};
use compute_provider::{ComputeProvider, ProviderRequest};
use compute_state::events;
use compute_state::{
    DeploymentRecord, EnvironmentRecord, ExecutionRecord, ProjectRevisionRecord, ReceiptRecord,
    RevisionWorkload, Stored, ids,
};
use serde_json::json;

use super::{
    Change, Daemon, Outcome, RECENT_EXECUTIONS, RECENT_RECEIPTS, SERVICE_OUTPUT_BYTES, Scope, Unit,
    identity_label,
};
use crate::EnvironmentError;
use crate::model::*;
use crate::status::*;

pub(crate) struct Prepared {
    pub request: ProviderRequest,
    pub report: PlacementReport,
    pub scope: ReceiptScope,
}

/// What an invocation needs from desired state.
pub(crate) struct Target<'a> {
    pub environment: &'a Stored<EnvironmentRecord>,
    pub config: &'a BTreeMap<String, String>,
    pub project_id: &'a str,
    pub revision: &'a ProjectRevisionRecord,
    pub workload: &'a RevisionWorkload,
    pub workload_id: &'a str,
    pub ports: &'a [PortBinding],
}

impl Daemon {
    pub(crate) fn policy_sources(
        &self,
        environment: &EnvironmentRecord,
    ) -> Result<Vec<(PolicySourceKind, Policy)>, EnvironmentError> {
        let mut sources = vec![];
        if let Some(policy) = &self.config.policy {
            sources.push((PolicySourceKind::Local, policy.clone()));
        }
        if let Some(policy) = &environment.policy {
            let policy = Policy::from_json(&serde_json::to_vec(policy)?).map_err(|error| {
                EnvironmentError::Invalid(format!(
                    "environment {} has an invalid policy: {error}",
                    environment.name
                ))
            })?;
            sources.push((PolicySourceKind::Environment, policy));
        }
        Ok(sources)
    }

    /// Build the request and evaluate placement and admission. Nothing
    /// executes.
    pub(crate) async fn prepare(&self, target: Target<'_>) -> Result<Prepared, EnvironmentError> {
        let stored = self.artifact(&target.workload.artifact).await?;
        let mut bundle = WorkloadBundle::from_bytes(&stored)?;
        // Configuration layering: workload < environment < project <
        // Compute-owned port bindings.
        for (name, value) in target
            .environment
            .value
            .config
            .iter()
            .chain(target.config.iter())
        {
            bundle.workload.env.insert(name.clone(), value.clone());
        }
        for binding in target.ports {
            bundle.workload.env.insert(
                format!(
                    "COMPUTE_PORT_{}",
                    binding.name.to_ascii_uppercase().replace('-', "_")
                ),
                binding.host.to_string(),
            );
        }
        if target.ports.len() == 1 {
            bundle
                .workload
                .env
                .insert("PORT".into(), target.ports[0].host.to_string());
        }
        if target.workload.kind == WorkloadKind::Service {
            let resources = &mut bundle.workload.resources;
            resources.stdout_bytes.get_or_insert(SERVICE_OUTPUT_BYTES);
            resources.stderr_bytes.get_or_insert(SERVICE_OUTPUT_BYTES);
        }
        bundle.validate()?;
        let mut request = ProviderRequest::bundle(bundle.to_bytes()?);
        request.expected.workload_id = Some(bundle.workload_id()?);
        request.expected.bundle_id = Some(bundle.bundle_id()?);
        let request_bytes = serde_json::to_vec(&request)?.len() as u64;
        let requirements = PlacementRequirements::from_bundle(
            &bundle,
            request_bytes,
            SubmissionMode::Synchronous,
            &RequirementOptions::default(),
        )
        .map_err(|error| EnvironmentError::Invalid(error.to_string()))?;
        let contract = ExecutionContract::from_bundle(&bundle, Some(requirements.isolation))
            .map_err(|error| EnvironmentError::Invalid(error.to_string()))?;
        let context =
            AdmissionContext::new(&self.policy_sources(&target.environment.value)?, contract);
        let explicit = target.environment.value.provider.as_deref();
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
                environment_id: target.environment.id.clone(),
                environment: target.environment.value.name.clone(),
                project_id: target.project_id.into(),
                project: target.revision.project.clone(),
                revision: target.revision.revision.clone(),
                workload_id: target.workload_id.into(),
                workload: target.workload.name.clone(),
                workload_kind: target.workload.kind.as_str().into(),
            },
        })
    }

    /// Prepare a unit: a workload at one deployment's revision, with that
    /// deployment's configuration and, for a service, its instance's ports.
    async fn prepare_unit(&self, unit: &Unit) -> Result<Prepared, EnvironmentError> {
        let key = &unit.key;
        let (environment, deployment, revision, ports, config) = {
            let inner = self.inner.lock().await;
            let desired = &inner.desired;
            let environment = desired
                .environments
                .get(&key.0)
                .cloned()
                .ok_or_else(|| EnvironmentError::NotFound(format!("environment {}", key.0)))?;
            let deployment = desired
                .deployments
                .get(&unit.deployment_id)
                .cloned()
                .ok_or_else(|| {
                    EnvironmentError::NotFound(format!("deployment {}", unit.deployment_id))
                })?;
            let revision = desired
                .revisions
                .get(&deployment.value.revision_id)
                .cloned()
                .ok_or_else(|| {
                    EnvironmentError::NotFound(format!("the revision of {}", unit.deployment_id))
                })?;
            let ports = desired
                .instance(key, &unit.deployment_id)
                .map(|instance| instance.value.ports.clone())
                .unwrap_or_default();
            let config = run_config(
                &deployment.value,
                desired.memberships.get(&(key.0.clone(), key.1.clone())),
            );
            (environment, deployment, revision, ports, config)
        };
        let workload = revision
            .workloads
            .iter()
            .find(|workload| workload.name == key.2)
            .cloned()
            .ok_or_else(|| {
                EnvironmentError::NotFound(format!(
                    "workload {} in revision {}",
                    key.2, revision.revision
                ))
            })?;
        let workload_id = ids::workload(&environment.id, &deployment.value.project_id, &key.2);
        self.prepare(Target {
            environment: &environment,
            config: &config,
            project_id: &deployment.value.project_id,
            revision: &revision,
            workload: &workload,
            workload_id: &workload_id,
            ports: &ports,
        })
        .await
    }

    /// Admission, placement, and execution of one workload invocation.
    pub(crate) async fn execute(
        self: &Arc<Self>,
        unit: &Unit,
        service: Option<(u64, ExecutionControl)>,
    ) -> (Outcome, Option<String>) {
        let deployment = Some(unit.deployment_id.clone());
        let prepared = match self.prepare_unit(unit).await {
            Ok(prepared) => prepared,
            Err(error) => return (Outcome::Failed(error), deployment),
        };
        let Prepared {
            request,
            report,
            scope,
        } = prepared;
        let Some(selected) = report.selected.clone() else {
            let (message, decision) = placement_failure(&report);
            return (Outcome::Denied(message, decision.map(Box::new)), deployment);
        };
        let placement = PlacementView {
            placement_id: Some(report.placement_id.clone()),
            provider: Some(selected.provider_id.clone()),
            node: Some(identity_label(&selected.provider_identity)),
        };
        let mut request = request;
        request.execution.scope = Some(scope);
        let outcome = match service {
            Some((generation, control)) => {
                if selected.provider_kind != ProviderKind::Local {
                    return (
                        Outcome::Failed(EnvironmentError::Invalid(format!(
                            "services run on the daemon's own node; placement selected {}",
                            selected.provider_id
                        ))),
                        deployment,
                    );
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
                    Err(error) => {
                        return (
                            Outcome::Failed(EnvironmentError::RuntimeUnavailable(
                                error.to_string(),
                            )),
                            deployment,
                        );
                    }
                };
                if !admission.decision.admitted {
                    let reasons = admission
                        .decision
                        .reasons
                        .iter()
                        .map(|reason| reason.message.clone())
                        .collect::<Vec<_>>()
                        .join("; ");
                    return (
                        Outcome::Denied(reasons, Some(Box::new(admission.decision))),
                        deployment,
                    );
                }
                {
                    let mut inner = self.inner.lock().await;
                    if let Some(runtime) = inner.runtime.get_mut(unit)
                        && runtime.generation == generation
                    {
                        if control.is_cancelled() {
                            return (
                                Outcome::Failed(EnvironmentError::Cancelled(
                                    "stopped before starting".into(),
                                )),
                                deployment,
                            );
                        }
                        runtime.state = Some(ActualState::Running);
                        runtime.running_since = Some(Utc::now());
                        runtime.evidence.policy_id = Some(admission.decision.policy_id.clone());
                        runtime.evidence.admission_id =
                            Some(admission.decision.admission_id.clone());
                        runtime.placement = placement.clone();
                    }
                }
                self.wake();
                match self
                    .config
                    .provider
                    .execute_controlled(request, admission, &control)
                    .await
                {
                    Ok(response) => {
                        let warning = match response
                            .result
                            .receipt
                            .as_ref()
                            .map(|receipt| report.verify_receipt(receipt))
                        {
                            Some(Err(error)) => Some(error),
                            None => Some("execution returned no receipt".into()),
                            Some(Ok(())) => None,
                        };
                        Outcome::Executed(Box::new(response.result), warning, placement)
                    }
                    Err(error) if error.admission.is_some() => {
                        Outcome::Denied(error.message.clone(), error.admission)
                    }
                    Err(error) => {
                        Outcome::Failed(EnvironmentError::RuntimeUnavailable(error.to_string()))
                    }
                }
            }
            None => {
                match compute_placement::dispatch::execute(&self.pool, &report, request).await {
                    Ok(response) => Outcome::Executed(Box::new(response.result), None, placement),
                    Err(error) if error.admission.is_some() => {
                        Outcome::Denied(error.message.clone(), error.admission)
                    }
                    Err(error) => {
                        Outcome::Failed(EnvironmentError::RuntimeUnavailable(error.to_string()))
                    }
                }
            }
        };
        (outcome, deployment)
    }

    /// Terminalize one invocation. Its evidence (the execution record, its
    /// receipt, and the event) belongs to the execution and is always
    /// persisted, even when a newer invocation of the same unit started in
    /// the meantime; only the unit's current runtime view is left to the
    /// newest invocation. Terminalizing an execution twice records it once.
    pub(crate) async fn finish(
        self: &Arc<Self>,
        unit: &Unit,
        invocation: Invocation,
        outcome: Outcome,
        deployment_id: Option<String>,
        service: bool,
    ) -> Result<ExecutionRecord, EnvironmentError> {
        let Invocation {
            generation,
            started_at,
        } = invocation;
        let key = &unit.key;
        // Where the execution belongs, from desired state or, when this
        // daemon no longer holds it in memory, from durable state.
        let located = {
            let inner = self.inner.lock().await;
            inner
                .desired
                .deployments
                .get(&unit.deployment_id)
                .map(|deployment| {
                    (
                        inner
                            .desired
                            .revision_workload(&unit.deployment_id, &key.2)
                            .map(|workload| workload.restart),
                        deployment.value.clone(),
                    )
                })
        };
        let (restart_policy, deployment) = match located {
            Some(located) => (located.0, Some(located.1)),
            None => (
                None,
                self.control()
                    .get::<DeploymentRecord>(&unit.deployment_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|stored| stored.value),
            ),
        };
        let (workload_id, environment_id, project_id) = deployment
            .as_ref()
            .map(|deployment| {
                (
                    ids::workload(&deployment.environment_id, &deployment.project_id, &key.2),
                    deployment.environment_id.clone(),
                    deployment.project_id.clone(),
                )
            })
            .unwrap_or_default();
        let mut restart_after = None;
        let mut change = Change::new();
        let mut receipt = None;
        let execution;
        {
            let mut inner = self.inner.lock().await;
            if let Outcome::Executed(result, ..) = &outcome
                && let Some(record) = inner.terminal.get(&result.execution_id)
            {
                // Already terminalized: finishing again changes nothing.
                return Ok(record.clone());
            }
            let restart_delay = self.config.restart_delay;
            let shutting_down = self.is_shutting_down();
            let now = Utc::now();
            let scope = Scope::workload(key);
            let scope = match &deployment_id {
                Some(id) => scope.deployment(id),
                None => scope,
            };
            // The unit's runtime view, when this invocation is still the
            // newest one of the unit.
            let mut current = inner
                .runtime
                .get_mut(unit)
                .filter(|runtime| runtime.generation == generation);
            if service && current.is_some() {
                super::processes::forget(&self.config.state_dir, unit);
            }
            let stopping = shutting_down
                || current.as_ref().is_some_and(|runtime| {
                    runtime
                        .control
                        .as_ref()
                        .is_some_and(ExecutionControl::is_cancelled)
                });
            let ran_for = now - started_at;
            if let Some(runtime) = current.as_deref_mut() {
                runtime.finished_at = Some(now);
                runtime.running_since = None;
                runtime.control = None;
                runtime.handle = None;
            }
            match outcome {
                Outcome::Denied(message, decision) => {
                    if let Some(runtime) = current.as_deref_mut() {
                        runtime.state = Some(ActualState::Denied);
                        runtime.held = true;
                        runtime.error = Some(message.clone());
                        if let Some(decision) = &decision {
                            runtime.evidence.policy_id = Some(decision.policy_id.clone());
                            runtime.evidence.admission_id = Some(decision.admission_id.clone());
                        }
                    }
                    change = self.event(
                        change,
                        if service {
                            events::SERVICE_DENIED
                        } else {
                            events::TASK_DENIED
                        },
                        scope,
                        format!("{} in {}/{} was denied: {message}", key.2, key.0, key.1),
                        json!({ "admission_id": decision.as_ref().map(|d| d.admission_id.clone()) }),
                    );
                    execution = Err(EnvironmentError::Denied(message));
                }
                Outcome::Failed(failure) => {
                    let message = failure.message();
                    if let Some(runtime) = current.as_deref_mut() {
                        runtime.state = Some(if stopping {
                            ActualState::Stopped
                        } else {
                            ActualState::Failed
                        });
                        runtime.error = Some(message.clone());
                        if !stopping {
                            runtime.held = true;
                            if service && restart_policy == Some(RestartPolicy::OnFailure) {
                                restart_after = Some(backoff(runtime, restart_delay, ran_for));
                            }
                        }
                    }
                    if !stopping {
                        change = self.event(
                            change,
                            if service {
                                events::SERVICE_FAILED
                            } else {
                                events::TASK_FAILED
                            },
                            scope,
                            format!("{} in {}/{} failed: {message}", key.2, key.0, key.1),
                            json!({ "kind": failure.kind() }),
                        );
                    }
                    execution = Err(failure);
                }
                Outcome::Executed(result, warning, placement) => {
                    let succeeded = result.status == ExecutionStatus::Completed
                        && result.exit_code.is_none_or(|code| code == 0);
                    let error = warning.clone().or_else(|| {
                        (!succeeded && !stopping).then(|| {
                            result
                                .error
                                .as_ref()
                                .map(|error| error.message.clone())
                                .unwrap_or_else(|| match result.exit_code {
                                    Some(code) => format!("exited with status {code}"),
                                    None => format!("ended as {}", status_name(&result.status)),
                                })
                        })
                    });
                    let receipt_id = result
                        .receipt
                        .as_ref()
                        .map(|receipt| receipt.receipt_hash.0.clone());
                    if let Some(runtime) = current.as_deref_mut() {
                        runtime.state = Some(match (service, stopping, succeeded) {
                            (_, true, _) => ActualState::Stopped,
                            (false, _, true) => ActualState::Completed,
                            (true, _, true) => ActualState::Stopped,
                            (_, _, false) => ActualState::Failed,
                        });
                        runtime.held = service && !stopping;
                        runtime.exit_code = result.exit_code;
                        runtime.error = error.clone();
                        runtime.execution_id = Some(result.execution_id.clone());
                        runtime.placement = placement.clone();
                        if let Some(admission) = &result.admission {
                            runtime.evidence.policy_id = Some(admission.policy_id.clone());
                            runtime.evidence.admission_id = Some(admission.admission_id.clone());
                        }
                        if let Some(receipt_id) = &receipt_id {
                            runtime.evidence.receipt_ids.push(receipt_id.clone());
                            if runtime.evidence.receipt_ids.len() > RECENT_RECEIPTS {
                                runtime.evidence.receipt_ids.remove(0);
                            }
                        }
                        if service
                            && !stopping
                            && !succeeded
                            && restart_policy == Some(RestartPolicy::OnFailure)
                        {
                            restart_after = Some(backoff(runtime, restart_delay, ran_for));
                        }
                    }
                    let record = ExecutionRecord {
                        execution_id: result.execution_id.clone(),
                        environment_id: environment_id.clone(),
                        environment: key.0.clone(),
                        project_id: project_id.clone(),
                        project: key.1.clone(),
                        workload_id: workload_id.clone(),
                        workload: key.2.clone(),
                        kind: if service {
                            WorkloadKind::Service
                        } else {
                            WorkloadKind::Task
                        },
                        deployment_id: deployment_id.clone(),
                        status: status_name(&result.status),
                        exit_code: result.exit_code,
                        started_at,
                        finished_at: Some(now),
                        receipt_id: receipt_id.clone(),
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
                        error: error.clone(),
                    };
                    let kind = match (service, stopping, succeeded) {
                        (true, true, _) => events::SERVICE_STOPPED,
                        (true, false, true) => events::SERVICE_STOPPED,
                        (true, false, false) => events::SERVICE_FAILED,
                        (false, _, true) => events::TASK_COMPLETED,
                        (false, _, false) => events::TASK_FAILED,
                    };
                    let message = match kind {
                        events::TASK_COMPLETED => {
                            format!("{} in {}/{} completed", key.2, key.0, key.1)
                        }
                        events::SERVICE_STOPPED => {
                            format!("{} in {}/{} stopped", key.2, key.0, key.1)
                        }
                        _ => format!(
                            "{} in {}/{} failed: {}",
                            key.2,
                            key.0,
                            key.1,
                            error.clone().unwrap_or_default()
                        ),
                    };
                    change = self.event(
                        change,
                        kind,
                        scope.execution(&result.execution_id),
                        message,
                        json!({ "exit_code": result.exit_code, "receipt_id": receipt_id }),
                    );
                    inner.outputs.insert(
                        result.execution_id.clone(),
                        (result.stdout.text.clone(), result.stderr.text.clone()),
                    );
                    while inner.outputs.len() > RECENT_EXECUTIONS {
                        let oldest = inner.outputs.keys().next().cloned().expect("non-empty");
                        inner.outputs.remove(&oldest);
                    }
                    inner.terminal.insert(record.clone());
                    receipt = result.receipt.clone().zip(receipt_id);
                    execution = Ok(record);
                }
            }
        }
        let mut evidence = PendingEvidence {
            record: execution.as_ref().ok().cloned(),
            receipt: None,
            events: std::mem::take(&mut change.events),
        };
        if let (Some(record), Some((receipt, receipt_id))) = (&evidence.record, receipt) {
            let artifact = match receipt.encoded_bytes() {
                Ok(bytes) => self.config.artifacts.put("receipt", &bytes).await.ok(),
                Err(_) => None,
            };
            evidence.receipt = Some(ReceiptRecord {
                receipt_id,
                execution_id: record.execution_id.clone(),
                environment_id: record.environment_id.clone(),
                project_id: record.project_id.clone(),
                workload_id: record.workload_id.clone(),
                deployment_id: record.deployment_id.clone(),
                policy_id: record.policy_id.clone(),
                admission_id: record.admission_id.clone(),
                artifact_digest: artifact,
                created_at: Utc::now(),
            });
        }
        if let Err(error) = self.persist_evidence(&evidence).await {
            // Control state is not reachable. The evidence stays pending on
            // this node and is written once it is; it is never dropped.
            let mut inner = self.inner.lock().await;
            inner.state_error = Some(error.to_string());
            inner.pending_evidence.push(evidence);
        }
        if let Some(delay) = restart_after {
            self.restart_later(unit.clone(), generation, delay);
        }
        self.wake();
        execution
    }

    /// Commit an execution's evidence: its record, its receipt reference,
    /// and its events. Records are keyed by execution and receipt identity;
    /// one that already exists was written by an earlier attempt and is not
    /// written again. The deployment's receipt list is updated on its own,
    /// retrying on a version conflict, because concurrent executions of one
    /// deployment each append to it.
    pub(crate) async fn persist_evidence(
        &self,
        evidence: &PendingEvidence,
    ) -> Result<(), EnvironmentError> {
        let control = self.control();
        let mut change = Change::new();
        if let Some(record) = &evidence.record
            && control
                .get::<ExecutionRecord>(&ids::execution(&record.execution_id))
                .await?
                .is_none()
        {
            change =
                change.with(|batch| batch.create(&ids::execution(&record.execution_id), record));
        }
        if let Some(receipt) = &evidence.receipt
            && control
                .get::<ReceiptRecord>(&ids::receipt(&receipt.receipt_id))
                .await?
                .is_none()
        {
            change = change.with(|batch| batch.create(&ids::receipt(&receipt.receipt_id), receipt));
        }
        for event in &evidence.events {
            change = change.with(|batch| batch.create(&ids::event(event.sequence), event));
            change.events.push(event.clone());
        }
        match self.apply(change).await {
            // Another attempt of this terminalization landed first.
            Err(EnvironmentError::Conflict(_)) if evidence.record.is_some() => {}
            other => other?,
        }
        if let (Some(record), Some(receipt)) = (&evidence.record, &evidence.receipt)
            && let Some(deployment_id) = &record.deployment_id
        {
            for _ in 0..16 {
                let Some(deployment) = control.get::<DeploymentRecord>(deployment_id).await? else {
                    break;
                };
                if deployment.value.receipt_ids.contains(&receipt.receipt_id) {
                    break;
                }
                let mut receipts = deployment.value.receipt_ids.clone();
                receipts.push(receipt.receipt_id.clone());
                let excess = receipts.len().saturating_sub(32);
                receipts.drain(..excess);
                let change = Change::new()
                    .with(|batch| batch.update(&deployment, json!({ "receipt_ids": receipts })));
                match self.apply(change).await {
                    Err(EnvironmentError::Conflict(_)) => continue,
                    other => {
                        other?;
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// Write evidence that was held while control state was unreachable.
    pub(crate) async fn flush_pending_evidence(&self) {
        let pending = std::mem::take(&mut self.inner.lock().await.pending_evidence);
        let mut failed = vec![];
        for evidence in pending {
            if self.persist_evidence(&evidence).await.is_err() {
                failed.push(evidence);
            }
        }
        if !failed.is_empty() {
            let mut inner = self.inner.lock().await;
            failed.append(&mut inner.pending_evidence);
            inner.pending_evidence = failed;
        }
    }

    /// Release a failed service's hold after `delay`, if nothing newer
    /// happened to it, and reconcile.
    fn restart_later(self: &Arc<Self>, unit: Unit, generation: u64, delay: std::time::Duration) {
        let daemon = Arc::downgrade(self);
        let future: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                let Some(daemon) = daemon.upgrade() else {
                    return;
                };
                {
                    let mut inner = daemon.inner.lock().await;
                    match inner.runtime.get_mut(&unit) {
                        Some(runtime) if runtime.generation == generation => {
                            runtime.held = false;
                            runtime.restarts += 1;
                        }
                        _ => return,
                    }
                }
                daemon.reconcile().await;
            });
        tokio::spawn(future);
    }

    /// Run a task to completion. The result, receipt, and admission are
    /// recorded; the task's failure never affects any other workload.
    pub async fn run_task(
        self: &Arc<Self>,
        environment: &str,
        project: &str,
        workload: &str,
    ) -> Result<ExecutionView, EnvironmentError> {
        let (key, record) = self.workload_record(environment, project, workload).await?;
        if record.value.kind != WorkloadKind::Task {
            return Err(EnvironmentError::Invalid(format!(
                "{workload} is a service; start it instead of running it"
            )));
        }
        self.run_unit_task(Unit::new(&key, &record.value.deployment_id))
            .await
    }

    /// Run a task at one deployment's revision: a current task, or a
    /// candidate revision's readiness task. Concurrent runs of one task are
    /// independent executions, each with its own record and receipt.
    pub(crate) async fn run_unit_task(
        self: &Arc<Self>,
        unit: Unit,
    ) -> Result<ExecutionView, EnvironmentError> {
        let invocation = {
            let mut inner = self.inner.lock().await;
            let runtime = inner.runtime.entry(unit.clone()).or_default();
            runtime.generation += 1;
            runtime.state = Some(ActualState::Running);
            runtime.started_at = Some(Utc::now());
            runtime.finished_at = None;
            Invocation {
                generation: runtime.generation,
                started_at: runtime.started_at.expect("set"),
            }
        };
        let (outcome, deployment_id) = self.execute(&unit, None).await;
        let record = self
            .finish(&unit, invocation, outcome, deployment_id, false)
            .await?;
        let (stdout, stderr) = self
            .inner
            .lock()
            .await
            .outputs
            .get(&record.execution_id)
            .cloned()
            .unwrap_or_default();
        Ok(ExecutionView {
            record,
            stdout,
            stderr,
        })
    }
}

/// One invocation of a unit: the unit's generation when it started, which
/// decides whether it may still update the unit's runtime view, and when
/// it started, which belongs to the execution.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Invocation {
    pub generation: u64,
    pub started_at: chrono::DateTime<Utc>,
}

/// An execution's evidence, as it is written to control state.
#[derive(Debug, Clone)]
pub(crate) struct PendingEvidence {
    pub record: Option<ExecutionRecord>,
    pub receipt: Option<ReceiptRecord>,
    pub events: Vec<compute_state::EventRecord>,
}

fn status_name(status: &ExecutionStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The project configuration a deployment runs with. Deployments made
/// before releases recorded their configuration run with the membership's.
pub(crate) fn run_config(
    deployment: &DeploymentRecord,
    membership: Option<&Stored<compute_state::EnvironmentProjectRecord>>,
) -> BTreeMap<String, String> {
    if !deployment.config.is_empty() || deployment.config_digest.is_some() {
        return deployment.config.clone();
    }
    membership
        .map(|membership| membership.value.config.clone())
        .unwrap_or_default()
}

/// Exponential backoff for a failing service: the delay doubles with each
/// consecutive failure, up to a minute, and resets once a run lasts.
fn backoff(
    runtime: &mut super::WorkloadRuntime,
    base: std::time::Duration,
    ran_for: chrono::TimeDelta,
) -> std::time::Duration {
    if ran_for > chrono::TimeDelta::seconds(60) {
        runtime.consecutive_failures = 0;
    }
    let exponent = runtime.consecutive_failures.min(6);
    runtime.consecutive_failures += 1;
    (base * 2u32.pow(exponent)).min(std::time::Duration::from_secs(60))
}

/// Why placement selected nothing, and the admission decision behind it.
pub(crate) fn placement_failure(
    report: &PlacementReport,
) -> (String, Option<compute_policy::AdmissionDecision>) {
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
    (message, decision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DaemonConfig;

    fn record(execution_id: &str) -> ExecutionRecord {
        ExecutionRecord {
            execution_id: execution_id.into(),
            environment_id: "env".into(),
            environment: "prod".into(),
            project_id: "prj".into(),
            project: "jobs".into(),
            workload_id: "wkl".into(),
            workload: "work".into(),
            kind: WorkloadKind::Task,
            deployment_id: None,
            status: "completed".into(),
            exit_code: Some(0),
            started_at: Utc::now(),
            finished_at: Some(Utc::now()),
            receipt_id: Some(format!("receipt-{execution_id}")),
            policy_id: None,
            admission_id: None,
            placement_id: None,
            provider: None,
            error: None,
        }
    }

    fn receipt(record: &ExecutionRecord) -> ReceiptRecord {
        ReceiptRecord {
            receipt_id: record.receipt_id.clone().unwrap(),
            execution_id: record.execution_id.clone(),
            environment_id: record.environment_id.clone(),
            project_id: record.project_id.clone(),
            workload_id: record.workload_id.clone(),
            deployment_id: None,
            policy_id: None,
            admission_id: None,
            artifact_digest: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn the_terminal_log_remembers_each_execution_once_and_is_bounded() {
        let mut log = super::super::TerminalLog::default();
        log.insert(record("a"));
        log.insert(record("a"));
        assert!(log.get("a").is_some());
        for index in 0..5000 {
            log.insert(record(&format!("x{index}")));
        }
        assert!(log.get("a").is_none(), "the oldest are forgotten");
        assert!(log.get("x4999").is_some());
    }

    #[tokio::test]
    async fn persisting_the_same_evidence_twice_records_it_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(compute_state_memory::MemoryState::new());
        let artifacts = std::sync::Arc::new(compute_state::StateArtifacts::new(
            compute_state::ControlState::new(store.clone()),
        ));
        let daemon = Daemon::start(DaemonConfig::new(dir.path(), store, artifacts))
            .await
            .unwrap();
        let record = record("e1");
        let evidence = PendingEvidence {
            receipt: Some(receipt(&record)),
            record: Some(record.clone()),
            events: vec![],
        };
        daemon.persist_evidence(&evidence).await.unwrap();
        // A duplicate or late terminalization of the same execution.
        daemon.persist_evidence(&evidence).await.unwrap();
        let executions = daemon.control().list::<ExecutionRecord>().await.unwrap();
        assert_eq!(executions.len(), 1);
        assert_eq!(executions[0].value, record);
        let receipts = daemon.control().list::<ReceiptRecord>().await.unwrap();
        assert_eq!(receipts.len(), 1);
        daemon.shutdown().await;
    }
}
