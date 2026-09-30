//! Bootstrap: where a computer stands in being brought to the configuration
//! its environment declares.
//!
//! Bootstrap is not a new engine. The declared contents (repositories,
//! packages, builds, processes) are applied by the controller's existing
//! reconciliation (`plan` / `apply_action`), each item a durable job on the
//! computer's target, idempotent by fingerprint and retried on request
//! (`reconcile`). This module only *derives* how far that has got from what
//! reconciliation already records: the observed contents, their evidence, and
//! the computer's failure. It stores nothing.
//!
//! Bootstrap is configuration; readiness is verification. A bootstrap that
//! finished is evidence, never a claim of readiness.

use compute_core::{
    ComputerSpec, ComputerStatus, EnvironmentContents, ObservedContents, OperationEvidence,
    ProcessDesired, ProcessState,
};
use compute_state::{ComputerRecord, EnvironmentRecord};

use crate::{BootstrapFailure, BootstrapState, BootstrapStep, EnvironmentBootstrap, FailureClass};

fn step(kind: &str, name: &str, evidence: Option<&OperationEvidence>) -> BootstrapStep {
    match evidence {
        None => BootstrapStep {
            kind: kind.into(),
            name: name.into(),
            outcome: "pending".into(),
            job_id: None,
            execution_id: None,
            at: None,
            error: None,
        },
        Some(evidence) => BootstrapStep {
            kind: kind.into(),
            name: name.into(),
            outcome: match evidence.outcome.as_str() {
                "succeeded" => "succeeded".into(),
                // Claimed and in flight: applying it is not over.
                "running" => "running".into(),
                _ => "failed".into(),
            },
            job_id: Some(evidence.job_id.clone()),
            execution_id: Some(evidence.execution_id.clone()),
            at: Some(evidence.at),
            error: evidence.error.clone(),
        },
    }
}

/// Whether a repository, package, or build failed to apply. These are the
/// deterministic configuration steps: while one has failed, the computer does
/// not hold what is declared, whatever else has been applied.
pub(crate) fn configuration_failed(observed: &ObservedContents) -> bool {
    observed
        .repositories
        .values()
        .map(|seen| &seen.evidence)
        .chain(observed.packages.values().map(|seen| &seen.evidence))
        .chain(observed.builds.values().map(|seen| &seen.evidence))
        .any(|evidence| !matches!(evidence.outcome.as_str(), "succeeded" | "running"))
}

/// Every declared item and how applying it went: the existing evidence,
/// read back. Declared items nothing has been recorded for are `pending`.
fn steps(contents: &EnvironmentContents, observed: &ObservedContents) -> Vec<BootstrapStep> {
    let mut steps = vec![];
    for repository in &contents.repositories {
        steps.push(step(
            "repository",
            &repository.name,
            observed
                .repositories
                .get(&repository.name)
                .map(|seen| &seen.evidence),
        ));
    }
    for package in &contents.packages {
        steps.push(step(
            "package",
            &package.name,
            observed
                .packages
                .get(&package.name)
                .map(|seen| &seen.evidence),
        ));
    }
    for project in contents
        .projects
        .iter()
        .filter(|project| !project.build.is_empty())
    {
        steps.push(step(
            "build",
            &project.name,
            observed
                .builds
                .get(&project.name)
                .map(|seen| &seen.evidence),
        ));
    }
    for process in contents
        .processes
        .iter()
        .filter(|process| process.desired == ProcessDesired::Running)
    {
        let seen = observed.processes.get(&process.name);
        let started_failed = seen.is_some_and(|seen| {
            seen.state == ProcessState::Failed
                && seen
                    .last_failure
                    .as_ref()
                    .is_some_and(|failure| failure.reason == "start_failed")
        });
        let mut process_step = match seen {
            Some(seen) if seen.state != ProcessState::Starting => {
                step("process", &process.name, Some(&seen.evidence))
            }
            _ => step("process", &process.name, None),
        };
        // A process that started and later exited was applied: what became of
        // it is runtime state, which readiness reports. Only a start that
        // failed is a failed step.
        if !started_failed && process_step.outcome == "failed" {
            process_step.outcome = "succeeded".into();
            process_step.error = None;
        }
        steps.push(process_step);
    }
    steps
}

fn failed_operation(kind: &str) -> (&'static str, FailureClass) {
    match kind {
        "repository" => ("repository sync", FailureClass::ConfigurationFailed),
        "package" => ("package install", FailureClass::ConfigurationFailed),
        _ => ("build", FailureClass::ConfigurationFailed),
    }
}

pub(crate) fn derive_bootstrap(
    environment: &EnvironmentRecord,
    spec: &ComputerSpec,
    computer: &ComputerRecord,
    converged: bool,
) -> EnvironmentBootstrap {
    let empty = EnvironmentContents::default();
    let contents = environment.contents.as_ref().unwrap_or(&empty);
    let steps = steps(contents, &computer.observed);
    let generation = contents.generation;
    let held = computer.observed.converged_generation == generation
        && !configuration_failed(&computer.observed);
    let completed_at = steps
        .iter()
        .filter_map(|step| step.at)
        .max()
        .or(computer.ready_at)
        .filter(|_| held);
    // Only configuration steps fail a bootstrap. A declared process that
    // will not start is a runtime impairment: the environment was
    // configured, and readiness reports the process (its restart policy is
    // responsible for recovering it).
    let failed_step = steps
        .iter()
        .find(|step| step.outcome == "failed" && step.kind != "process");
    let recorded = computer.failure.as_ref();
    let wants_stop = environment.desired_state == compute_state::DesiredState::Stopped
        || spec.destroy_requested_at.is_some();

    let mut failure = None;
    let state = if let Some(failed) = failed_step {
        let (operation, class) = failed_operation(&failed.kind);
        failure = Some(BootstrapFailure {
            class,
            operation: operation.into(),
            message: format!(
                "{} {}: {}",
                failed.kind,
                failed.name,
                failed.error.clone().unwrap_or_else(|| "failed".into())
            ),
            retryable: true,
            job_id: failed.job_id.clone(),
            execution_id: failed.execution_id.clone(),
            at: failed.at,
        });
        BootstrapState::Failed
    } else if let Some(recorded) = recorded.filter(|recorded| {
        computer.status == ComputerStatus::Failed || recorded.code == "destruction_failed"
    }) {
        let (operation, class) = if recorded.code == "destruction_failed" {
            ("destroy", FailureClass::DestructionFailed)
        } else {
            (
                match recorded.phase.as_str() {
                    "placement" => "placement",
                    "resuming" => "resume",
                    _ => "provisioning",
                },
                FailureClass::ProviderFailed,
            )
        };
        failure = Some(BootstrapFailure {
            class,
            operation: operation.into(),
            message: format!("{} ({})", recorded.message, recorded.code),
            retryable: recorded.retryable,
            job_id: None,
            execution_id: None,
            at: Some(recorded.at),
        });
        BootstrapState::Failed
    } else {
        match computer.status {
            ComputerStatus::Pending | ComputerStatus::Provisioning => BootstrapState::NotStarted,
            ComputerStatus::Running if held && converged => BootstrapState::Succeeded,
            // Held once, and only a scheduled restart of a process remains:
            // the configuration is complete.
            ComputerStatus::Running if held => BootstrapState::Succeeded,
            ComputerStatus::Running => BootstrapState::Running,
            _ if held => BootstrapState::Succeeded,
            _ if wants_stop || computer.status.is_terminal() => {
                failure = Some(BootstrapFailure {
                    class: FailureClass::BootstrapCancelled,
                    operation: "bootstrap".into(),
                    message: format!(
                        "the environment is {} before its configuration completed",
                        computer.status
                    ),
                    retryable: !computer.status.is_terminal(),
                    job_id: None,
                    execution_id: None,
                    at: Some(computer.updated_at),
                });
                BootstrapState::Failed
            }
            _ => BootstrapState::NotStarted,
        }
    };
    EnvironmentBootstrap {
        state,
        contents_generation: generation,
        converged_generation: computer.observed.converged_generation,
        steps,
        completed_at,
        failure,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use compute_core::{ObservedPackage, OperationEvidence};

    fn evidence(outcome: &str) -> OperationEvidence {
        OperationEvidence {
            job_id: "job_1".into(),
            execution_id: "exec_1".into(),
            outcome: outcome.into(),
            at: Utc::now(),
            error: (outcome != "succeeded").then(|| "boom".to_owned()),
        }
    }

    #[test]
    fn a_failed_package_is_a_configuration_failure_and_a_pending_one_is_not() {
        let mut observed = ObservedContents::default();
        assert!(!configuration_failed(&observed));
        observed.packages.insert(
            "deps".into(),
            ObservedPackage {
                fingerprint: "f".into(),
                evidence: evidence("failed"),
            },
        );
        assert!(configuration_failed(&observed));
        observed.packages.get_mut("deps").unwrap().evidence = evidence("succeeded");
        assert!(!configuration_failed(&observed));
    }
}
