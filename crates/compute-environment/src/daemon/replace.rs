//! Replace an environment's computer without losing its workspace: a
//! composition of the workspace primitives and one atomic handoff.
//!
//! ```text
//! Environment E ── Computer A (current, untouched throughout)
//!   export A's workspace                               workspace.rs
//!   create a candidate environment for the new machine (an ordinary
//!     environment: same config and policy, the new requirements, no contents)
//!   seed the candidate, which verifies                 workspace.rs
//!   apply E's declared contents to the candidate; the ordinary controller
//!     starts them; wait until it holds them
//!   re-measure A: it must still be the workspace that was captured
//!   HANDOFF: one fenced transaction moves the candidate's machine into E's
//!     computer record, retires A's session, and removes the candidate
//! ```
//!
//! The Environment survives; the machine does not. Environment identity,
//! declared contents, configuration, policy, ownership, and lifecycle are
//! never touched; the Computer's machine binding (target, session, provider
//! resource, connection, observed contents) is replaced, and its generation
//! moves on. A's session becomes a retired session, torn down by the
//! controller exactly as any earlier session of a replaced computer is, so a
//! connection to A does not survive.
//!
//! **Why a candidate environment.** A computer record binds one machine, and
//! the controller reconciles the record's machine. To prepare a second machine
//! for the same environment *without* touching the first, the second needs a
//! record the controller will drive: an environment of its own. Its name ends
//! in [`CANDIDATE_SUFFIX`](super::candidate::CANDIDATE_SUFFIX), which no other environment may use. The handoff
//! transaction writes E's environment and computer records and deletes the
//! candidate's, so nothing about the candidate outlives a successful
//! replacement. After a failed one it stays, stopped, as evidence, until the
//! next replacement of E clears it.
//!
//! **Failure.** Before the handoff nothing of E's changes. The candidate is
//! stopped (Reality says so) and the failure is recorded with its phase.
//! After the handoff there is nothing to undo: the old machine is simply
//! retired. There is no rollback and no state kept anywhere but the records
//! that already exist.
//!
//! **Overlap.** While the candidate is prepared, A keeps running, and so
//! does the candidate's copy of the declared processes once they start. A
//! process that cannot tolerate two live instances (a fixed port on a target
//! whose machines share a network, single-writer state) will fail its
//! readiness on the candidate, and the replacement fails before the
//! handoff. Quiescing A is the caller's decision; if A's workspace changed
//! after it was captured the replacement is refused rather than losing that
//! change.

use std::sync::Arc;

use chrono::Utc;
use compute_core::{ComputerLifecycle, ComputerRequirements, ComputerStatus, RetiredSession};
use compute_state::events;
use serde_json::json;

use super::candidate::{candidate_name, is_candidate};
use super::{Change, Daemon, Scope};
use crate::EnvironmentError;
use crate::model::*;
use crate::status::ComputerView;

impl Daemon {
    /// Replace the environment's computer with one that meets `requirements`.
    /// A running computer is replaced with its workspace preserved (see the
    /// module documentation). A computer that cannot be exported from (lost,
    /// unreachable, failed, stopped) has no workspace to preserve and is
    /// replaced by the controller for the declared contents alone.
    pub async fn replace_computer(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
        requirements: ComputerRequirements,
    ) -> Result<ComputerView, EnvironmentError> {
        requirements.validate()?;
        let record = self.owned_environment(environment, operator).await?;
        self.require_live(&record).await?;
        if is_candidate(&record.value.name) {
            return Err(EnvironmentError::Invalid(format!(
                "{environment} is a replacement candidate, not an environment to replace"
            )));
        }
        let old = self.stored_computer(&record.id).await;
        if !old.as_ref().is_some_and(|old| {
            old.value.status == ComputerStatus::Running
                && record.value.desired_state == DesiredState::Running
        }) {
            return self
                .request_replacement(environment, operator, requirements)
                .await;
        }
        let old_session = old
            .as_ref()
            .and_then(|old| old.value.session_id.clone())
            .unwrap_or_default();
        let candidate = candidate_name(&record.value.name)?;
        let _claim = self.claim(&record.id, &format!("a replacement of {environment}"))?;
        self.discard_candidate(&candidate, operator).await?;

        // Capture first: nothing is created until A's workspace is a
        // verified archive.
        let export = self.export_workspace(environment, operator).await?;
        let spec =
            record.value.computer.clone().ok_or_else(|| {
                EnvironmentError::Invalid(format!("{environment} has no computer"))
            })?;
        let policy = record
            .value
            .policy
            .clone()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| {
                EnvironmentError::Invalid(format!("{environment}'s policy: {error}"))
            })?;
        let ttl_seconds = (spec.lifecycle == ComputerLifecycle::Ephemeral)
            .then(|| {
                spec.expires_at
                    .map(|at| (at - Utc::now()).num_seconds().max(1) as u64)
            })
            .flatten();
        self.create_computer_environment_inner(
            ComputerEnvironmentDefinition {
                name: candidate.clone(),
                desired_state: DesiredState::Running,
                env: record.value.config.clone(),
                policy,
                computer: ComputerRequest {
                    lifecycle: spec.lifecycle,
                    requirements: requirements.clone(),
                    target: None,
                    ttl_seconds,
                },
                contents: Default::default(),
                recipe: None,
            },
            operator,
        )
        .await?;

        let contents = record.value.contents.clone().unwrap_or_default();
        let prepared = self
            .prepare_candidate(&candidate, operator, &export, &contents, Some(environment))
            .await;
        let seed = match prepared {
            Ok(seed) => seed,
            Err((phase, error)) => {
                return Err(self
                    .abandon_composition(
                        "replace",
                        environment,
                        &candidate,
                        operator,
                        phase,
                        &error,
                    )
                    .await);
            }
        };
        let mut jobs = vec![export.job_id.clone()];
        jobs.extend(seed.jobs);
        match self
            .hand_off(
                &record.value,
                &candidate,
                operator,
                &old_session,
                &requirements,
                &export.digest,
                &jobs,
            )
            .await
        {
            Ok(view) => Ok(view),
            Err(error) => Err(self
                .abandon_composition(
                    "replace",
                    environment,
                    &candidate,
                    operator,
                    "handing off",
                    &error,
                )
                .await),
        }
    }

    /// The handoff: one fenced transaction. E's computer takes the
    /// candidate's machine and retires its own; E's requirements and
    /// generation move on; the candidate's records go. Refused, with
    /// everything as it was, if anything it depends on changed.
    #[allow(clippy::too_many_arguments)]
    async fn hand_off(
        self: &Arc<Self>,
        before: &compute_state::EnvironmentRecord,
        candidate: &str,
        operator: &str,
        old_session: &str,
        requirements: &ComputerRequirements,
        workspace: &str,
        jobs: &[String],
    ) -> Result<ComputerView, EnvironmentError> {
        let name = before.name.clone();
        for attempt in 0.. {
            let env = self.owned_environment(&name, operator).await?;
            let cand_env = self.owned_environment(candidate, operator).await?;
            let changed = env.value.contents != before.contents
                || env.value.config != before.config
                || env.value.policy != before.policy
                || env.value.computer != before.computer
                || env.value.desired_state != DesiredState::Running;
            if changed {
                return Err(EnvironmentError::Conflict(format!(
                    "{name} changed while its computer was being replaced"
                )));
            }
            let _ = self.refresh_targeted().await;
            let (Some(current), Some(cand)) = (
                self.stored_computer(&env.id).await,
                self.stored_computer(&cand_env.id).await,
            ) else {
                return Err(EnvironmentError::Conflict(
                    "a computer record is missing".into(),
                ));
            };
            if current.value.status != ComputerStatus::Running
                || current.value.session_id.as_deref() != Some(old_session)
            {
                return Err(EnvironmentError::Conflict(format!(
                    "{name}'s computer is no longer the one that was captured"
                )));
            }
            if cand.value.status != ComputerStatus::Running || cand.value.session_id.is_none() {
                return Err(EnvironmentError::Conflict(format!(
                    "the replacement for {name} is not running"
                )));
            }

            let mut new_env = env.value.clone();
            let spec = new_env.computer.as_mut().expect("computer environments");
            spec.requirements = requirements.clone();
            spec.generation += 1;
            let generation = spec.generation;

            let mut moved = current.value.clone();
            if let (Some(target), Some(session_id)) =
                (&current.value.target, &current.value.session_id)
            {
                moved.retired.push(RetiredSession {
                    target: target.clone(),
                    session_id: session_id.clone(),
                    spec_generation: current.value.spec_generation,
                });
            }
            moved.status = ComputerStatus::Running;
            moved.spec_generation = generation;
            moved.target = cand.value.target.clone();
            moved.placement_id = cand.value.placement_id.clone();
            moved.session_id = cand.value.session_id.clone();
            moved.reference = cand.value.reference.clone();
            moved.provider_kind = cand.value.provider_kind.clone();
            moved.provider_resource = cand.value.provider_resource.clone();
            moved.capabilities = cand.value.capabilities;
            moved.connection = cand.value.connection.clone();
            moved.observed = cand.value.observed.clone();
            moved.ready_at = cand.value.ready_at;
            moved.failure = None;
            moved.generation = current.value.generation + 1;
            moved.updated_at = Utc::now();

            let data = json!({
                "environment_id": env.id,
                "from_session": old_session,
                "to_session": moved.session_id,
                "from_target": current.value.target,
                "to_target": moved.target,
                "workspace": workspace,
                "workspace_verified": true,
                "spec_generation": generation,
                "candidate": candidate,
                "jobs": jobs,
            });
            let scope = Scope::environment(&name);
            let mut change = Change::new().with(|batch| {
                batch
                    .replace(&env, &new_env)
                    .replace(&current, &moved)
                    .delete(&cand)
                    .delete(&cand_env)
            });
            change = self.event(
                change,
                events::COMPUTER_REPLACING,
                scope.clone(),
                format!("{name}'s computer is replaced; the workspace moved with it"),
                data.clone(),
            );
            change = self.event(
                change,
                events::COMPUTER_REPLACED,
                scope,
                format!("{name}: handed off to a verified replacement"),
                data,
            );
            match self.apply(change).await {
                Ok(()) => break,
                Err(EnvironmentError::Conflict(_)) if attempt < 4 => continue,
                Err(error) => return Err(error),
            }
        }
        let id = self
            .owned_environment(&name, operator)
            .await
            .map(|env| env.id)
            .unwrap_or_default();
        self.computer_confirmed
            .lock()
            .expect("confirmations")
            .remove(&id);
        self.computer_wake.notify_waiters();
        self.changed().await;
        self.fresh_computer_view(&name).await
    }
}
