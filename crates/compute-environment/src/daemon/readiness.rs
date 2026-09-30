//! Environment readiness: whether an environment can run workloads now.
//!
//! Requirements are desired state (`ComputerRequirements`, unchanged).
//! Readiness is observed: it is evaluated, never stored, from the
//! environment's own computer and its own target. The requirements verdict
//! is placement's own evaluation (`place_with_policy`) restricted to the
//! computer's target, so the reasons are placement's reasons; capacity is
//! left out because the machine already holds its capacity.
//!
//! Evaluating readiness is read-only: it may re-discover the target's
//! capabilities (a cache refresh, at most once per `read_cache`), and it
//! creates no computer, starts no process, and changes no record.

use chrono::Utc;
use compute_core::{ComputerSpec, ComputerStatus};
use compute_placement::{
    DiscoveryMode, EvaluationStatus, IncompatibilityReason, PlacementPolicy, place_with_policy,
};
use compute_state::{ComputerRecord, EnvironmentRecord};

use super::Daemon;
use crate::{
    ComputerReality, EnvironmentError, EnvironmentReadiness, ReadinessCondition, ReadinessState,
};

/// What the computer's own target says about its requirements right now.
enum Verdict {
    Satisfied,
    /// It does not: placement's reasons (empty when policy denies it).
    Unsatisfied(Vec<IncompatibilityReason>, String),
    /// The target could not be asked, so nothing is verified.
    Unknown(String),
}

impl Daemon {
    /// Evaluate readiness. `fresh` forces the target to be asked now,
    /// which admission does; a view accepts an answer up to `read_cache`
    /// old.
    pub(crate) async fn evaluate_readiness(
        &self,
        environment: &EnvironmentRecord,
        spec: &ComputerSpec,
        computer: &ComputerRecord,
        converged: bool,
        reality: &ComputerReality,
        fresh: bool,
    ) -> EnvironmentReadiness {
        let name = &environment.name;
        let target = computer.target.as_deref().unwrap_or("its target");
        let failure = computer
            .failure
            .as_ref()
            .map(|failure| format!("{} ({})", failure.message, failure.code))
            .unwrap_or_default();
        let done = |state, conditions, unsatisfied, explanation: String| EnvironmentReadiness {
            state,
            conditions,
            unsatisfied,
            explanation,
            evaluated_at: Utc::now(),
        };
        let machine = |satisfied: bool, detail: String| ReadinessCondition {
            name: "machine".into(),
            satisfied,
            detail,
        };
        match computer.status {
            ComputerStatus::Pending => {
                return done(
                    ReadinessState::Created,
                    vec![machine(false, "no machine yet".into())],
                    vec![],
                    format!("{name} exists; its machine has not been provisioned yet."),
                );
            }
            ComputerStatus::Provisioning | ComputerStatus::Resuming => {
                return done(
                    ReadinessState::Starting,
                    vec![machine(false, format!("{} on {target}", computer.status))],
                    vec![],
                    format!("{name}'s machine is {} on {target}.", computer.status),
                );
            }
            ComputerStatus::Failed => {
                return done(
                    ReadinessState::Failed,
                    vec![machine(false, failure.clone())],
                    vec![],
                    format!("Establishing {name} failed: {failure}."),
                );
            }
            ComputerStatus::Running => {}
            status => {
                let detail = match status {
                    ComputerStatus::Stopped => "stopped: start it to make it ready".to_owned(),
                    ComputerStatus::Unreachable | ComputerStatus::Lost => failure.clone(),
                    other => other.to_string(),
                };
                return done(
                    ReadinessState::Unavailable,
                    vec![machine(false, detail.clone())],
                    vec![],
                    format!("{name} cannot run workloads: its computer is {status}. {detail}"),
                );
            }
        }

        // Running by its record. The requirements are re-verified against
        // the target as it is now.
        let verdict = self
            .target_verdict(environment, spec, computer, fresh)
            .await;
        let confirmed = reality.observed != "unverified";
        let mut conditions = vec![machine(
            confirmed,
            if confirmed {
                format!("running on {target}, confirmed by the target")
            } else {
                format!("{target} has not confirmed the machine recently")
            },
        )];
        let mut unsatisfied = vec![];
        let requirements = match &verdict {
            Verdict::Satisfied => ReadinessCondition {
                name: "requirements".into(),
                satisfied: true,
                detail: format!("{target} satisfies the requirements now"),
            },
            Verdict::Unsatisfied(reasons, detail) => {
                unsatisfied = reasons.clone();
                ReadinessCondition {
                    name: "requirements".into(),
                    satisfied: false,
                    detail: detail.clone(),
                }
            }
            Verdict::Unknown(detail) => ReadinessCondition {
                name: "requirements".into(),
                satisfied: false,
                detail: detail.clone(),
            },
        };
        conditions.push(requirements);
        conditions.push(ReadinessCondition {
            name: "contents".into(),
            satisfied: converged,
            detail: if converged {
                "holds what the environment declares".into()
            } else {
                "still being brought to what the environment declares".into()
            },
        });
        let impaired: Vec<String> = reality
            .processes
            .iter()
            .filter(|(_, process)| {
                process.desired == "running"
                    && matches!(
                        process.process.as_str(),
                        "failed" | "exited" | "unready" | "stopped"
                    )
            })
            .map(|(process, reality)| format!("{process} is {}", reality.process))
            .collect();
        conditions.push(ReadinessCondition {
            name: "processes".into(),
            satisfied: impaired.is_empty(),
            detail: if impaired.is_empty() {
                "every declared process that should run is running".into()
            } else {
                impaired.join(", ")
            },
        });

        match verdict {
            Verdict::Unsatisfied(_, detail) => done(
                ReadinessState::Unavailable,
                conditions,
                unsatisfied,
                format!(
                    "{name} runs on {target}, but {target} no longer satisfies its requirements: {detail}. No workload is admitted; replace the computer or change its requirements."
                ),
            ),
            Verdict::Unknown(detail) => done(
                ReadinessState::Degraded,
                conditions,
                unsatisfied,
                format!(
                    "{name}'s requirements could not be re-verified: {detail}. It is not reported ready."
                ),
            ),
            Verdict::Satisfied if !confirmed => done(
                ReadinessState::Degraded,
                conditions,
                unsatisfied,
                format!(
                    "{name} is running by its record, but {target} has not confirmed the machine recently; Compute is checking."
                ),
            ),
            Verdict::Satisfied if !impaired.is_empty() => done(
                ReadinessState::Degraded,
                conditions,
                unsatisfied,
                format!("{name} is running but impaired: {}.", impaired.join(", ")),
            ),
            Verdict::Satisfied if !converged => done(
                ReadinessState::Starting,
                conditions,
                unsatisfied,
                format!("{name} is running; Compute is bringing it to what it declares."),
            ),
            Verdict::Satisfied => done(
                ReadinessState::Ready,
                conditions,
                unsatisfied,
                format!("{name} is ready: verified against {target}."),
            ),
        }
    }

    /// Ask the computer's own target whether it satisfies the requirements:
    /// placement's evaluation of that one provider, capabilities and policy.
    async fn target_verdict(
        &self,
        environment: &EnvironmentRecord,
        spec: &ComputerSpec,
        computer: &ComputerRecord,
        fresh: bool,
    ) -> Verdict {
        let Some(target) = computer.target.as_deref() else {
            return Verdict::Unknown("the computer has no target".into());
        };
        let (requirements, _create, context) = match self.placement_inputs(environment, spec) {
            Ok(inputs) => inputs,
            Err(error) => return Verdict::Unknown(error.to_string()),
        };
        let recent = self
            .readiness_probed
            .lock()
            .expect("readiness probes")
            .get(target)
            .is_some_and(|at| at.elapsed() < self.config.read_cache);
        let mode = if fresh || !recent {
            DiscoveryMode::Refresh
        } else {
            DiscoveryMode::PreferCache
        };
        let records = {
            let mut cache = self.cache.lock().await;
            self.pool
                .capabilities(&mut cache, mode, Some(target), Utc::now())
                .await
        };
        if mode == DiscoveryMode::Refresh {
            self.readiness_probed
                .lock()
                .expect("readiness probes")
                .insert(target.to_owned(), std::time::Instant::now());
        }
        let report = place_with_policy(
            &self.pool.configs(),
            self.pool.policy(),
            &records,
            &requirements,
            &context,
            PlacementPolicy::Provider(target.to_owned()),
        );
        let Some(evaluation) = report
            .providers
            .iter()
            .find(|provider| provider.provider_id == target)
        else {
            return Verdict::Unknown(format!("target {target} is not in the daemon's pool"));
        };
        match evaluation.status {
            EvaluationStatus::Compatible | EvaluationStatus::ExcludedUnhealthy => {
                Verdict::Satisfied
            }
            EvaluationStatus::Incompatible => {
                let detail = evaluation
                    .reasons
                    .iter()
                    .map(|reason| {
                        format!(
                            "{}{}",
                            reason.code.as_str(),
                            reason
                                .detail
                                .as_deref()
                                .map(|detail| format!(" ({detail})"))
                                .unwrap_or_default()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                Verdict::Unsatisfied(evaluation.reasons.clone(), detail)
            }
            EvaluationStatus::PolicyDenied => Verdict::Unsatisfied(
                vec![],
                "the effective execution policy no longer admits it on this target".into(),
            ),
            EvaluationStatus::CapabilitiesUnknown
            | EvaluationStatus::CapabilitiesInvalid
            | EvaluationStatus::ProviderUnavailable => Verdict::Unknown(format!(
                "{target}'s capabilities could not be discovered ({:?})",
                evaluation.status
            )),
        }
    }

    /// Admit a user's workload only when the environment can run it: the
    /// boundary Compute owns, so no consumer needs its own check. Setup work
    /// Compute does itself (importing, installing) does not come through
    /// here.
    pub(crate) async fn require_ready(
        &self,
        record: &compute_state::Stored<EnvironmentRecord>,
    ) -> Result<(), EnvironmentError> {
        let Some(view) = self.computer_view_fresh(record, true).await else {
            return Err(EnvironmentError::Conflict(format!(
                "environment {} has no computer",
                record.value.name
            )));
        };
        let readiness = &view.readiness;
        if readiness.state.admits_workloads() {
            return Ok(());
        }
        // An unreachable or lost machine keeps its established error, which
        // already says what to do.
        if matches!(
            view.status,
            ComputerStatus::Unreachable | ComputerStatus::Lost
        ) {
            self.running(record).await?;
        }
        Err(EnvironmentError::Conflict(format!(
            "environment {} is {}, not ready for workloads: {}",
            record.value.name,
            readiness.state.as_str(),
            readiness.explanation
        )))
    }
}
