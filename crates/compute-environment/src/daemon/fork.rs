//! Fork an environment: a new environment from portable workspace state.
//!
//! ```text
//! fork SOURCE NAME
//!   export SOURCE's workspace                       workspace.rs
//!   prepare a candidate machine beside nothing      candidate.rs
//!     (seed + verify, apply SOURCE's declared contents, reconcile)
//!   HANDOFF: one fenced transaction creates NAME, its own environment
//!     and computer records, owned by the operator, on the candidate's
//!     machine, and deletes the candidate's records
//! ```
//!
//! **Replace keeps the environment and changes the computer; fork creates a
//! new environment and a new computer.** Nothing of the source's authority or
//! machine identity is inherited. The new environment gets a new id, the
//! operator as owner, and a new machine and session. What is *inherited* is
//! only what the environment model calls state: the declared contents
//! (re-derived by the new controller, so processes start there because it
//! reconciled them, not because one was copied), the policy, the requirements
//! and lifecycle kind, and the workspace files, moved through the generic
//! export/seed/verify path.
//!
//! What is deliberately not inherited: configuration values (where credentials
//! live; `copy_config` opts in, and the names left behind are reported),
//! sessions and connections, provider and target choice (placement decides,
//! unless the caller names a target), endpoints, running processes,
//! `repos/` (re-derived from declared revisions), controller state, and the
//! source's events and receipts.
//!
//! A failed fork never touches the requested name: everything is prepared in
//! a candidate, and NAME exists only once the workspace is verified and the
//! contents reconciled. A failure stops the candidate, records the phase on the
//! candidate and the source, and the next fork clears it. The source is not
//! written to at any point.

use std::sync::Arc;

use chrono::Utc;
use compute_state::{ComputerRecord, EnvironmentRecord, Stored, events, ids};
use serde_json::json;

use super::candidate::{Claim, candidate_name, is_candidate};
use super::{Change, Daemon, Scope};
use crate::EnvironmentError;
use crate::model::*;
use crate::status::ComputerView;

/// What a new environment is derived from and how: the shared composition of
/// fork (from another environment's exported workspace) and restore (from a
/// checkpoint's). The declared state is the source environment's; the
/// workspace archive is whatever the caller resolved.
pub(crate) struct Derivation<'a> {
    /// `fork` or `restore`, in evidence and messages.
    pub composition: &'a str,
    /// What it derives from, for messages and the failure record: an
    /// environment name (a failure is recorded on it too) or a checkpoint id.
    pub label: &'a str,
    /// The environment whose declared contents, policy, requirements, and
    /// lifecycle the new one inherits.
    pub source: &'a Stored<EnvironmentRecord>,
    pub name: &'a str,
    pub operator: &'a str,
    pub target: Option<String>,
    pub copy_config: bool,
    /// Extra fields for the creation event: how this environment came to be.
    pub provenance: serde_json::Value,
}

/// How often a handoff that lost a version race is retried from a fresh read.
const HANDOFF_RETRIES: usize = 5;

impl Daemon {
    /// Fork an environment; see the module documentation.
    pub async fn fork_environment(
        self: &Arc<Self>,
        source: &str,
        operator: &str,
        request: ForkRequest,
    ) -> Result<ForkReport, EnvironmentError> {
        let name = request.name.clone();
        validate_name("environment", &name)?;
        if is_candidate(&name) {
            return Err(EnvironmentError::Invalid(format!(
                "names ending {:?} are reserved for candidates",
                super::candidate::CANDIDATE_SUFFIX
            )));
        }
        let record = self.owned_environment(source, operator).await?;
        self.require_live(&record).await?;
        if is_candidate(&record.value.name) {
            return Err(EnvironmentError::Invalid(format!(
                "{source} is a candidate, not an environment to fork"
            )));
        }
        let derivation = Derivation {
            composition: "fork",
            label: source,
            source: &record,
            name: &name,
            operator,
            target: request.target.clone(),
            copy_config: request.copy_config,
            provenance: json!({ "source": source }),
        };
        // A duplicate is refused before anything is exported or created.
        let claim = self.begin_derivation(&derivation).await?;
        // Nothing exists until the source has been captured.
        let export = self.export_workspace(source, operator).await?;
        let (seed, computer, jobs) = self
            .derive_environment(&claim, &derivation, &export, vec![export.job_id.clone()])
            .await?;
        let contents = record.value.contents.clone().unwrap_or_default();
        let original = self.computer(source).await?;
        let repositories = contents
            .repositories
            .iter()
            .map(|repository| {
                let commit = |view: &ComputerView| {
                    view.observed
                        .repositories
                        .get(&repository.name)
                        .and_then(|observed| observed.commit.clone())
                };
                (
                    repository.name.clone(),
                    (commit(&original), commit(&computer)),
                )
            })
            .collect();
        Ok(ForkReport {
            source: source.to_owned(),
            environment: name,
            workspace: export.digest,
            archive: export.archive_digest,
            files: export.files,
            directories: export.directories,
            bytes: export.bytes,
            workspace_verified: seed.verified,
            repositories,
            omitted_config: if request.copy_config {
                vec![]
            } else {
                record.value.config.keys().cloned().collect()
            },
            jobs,
            computer,
        })
    }

    /// The candidate's definition: the source's requirements, lifecycle,
    /// policy and (with `copy_config`) configuration, on the derivation's
    /// target, holding `contents`.
    async fn candidate_definition(
        &self,
        derivation: &Derivation<'_>,
        candidate: &str,
        contents: compute_core::EnvironmentContents,
    ) -> Result<ComputerEnvironmentDefinition, EnvironmentError> {
        let Derivation { label, source, .. } = derivation;
        let spec = source.value.computer.clone().ok_or_else(|| {
            EnvironmentError::Invalid(format!("{label} has no computer to derive from"))
        })?;
        let policy = source
            .value
            .policy
            .clone()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| EnvironmentError::Invalid(format!("{label}'s policy: {error}")))?;
        let config = if derivation.copy_config {
            source.value.config.clone()
        } else {
            Default::default()
        };
        // Same configuration, so the same provenance: kept only while the
        // source's recipe version still resolves to what is inherited.
        let computer = ComputerRequest {
            lifecycle: spec.lifecycle,
            requirements: spec.requirements,
            target: derivation.target.clone(),
            ttl_seconds: None,
        };
        let recipe = self
            .surviving_recipe(source.value.recipe.as_ref(), &computer, policy.as_ref())
            .await;
        Ok(ComputerEnvironmentDefinition {
            name: candidate.to_owned(),
            desired_state: DesiredState::Running,
            env: config,
            policy,
            computer,
            contents,
            recipe,
        })
    }

    /// Fork an environment's running machine, memory and disk, into a new
    /// environment on the same target. Refused, with nothing created, where
    /// the computer cannot fork its machine: a workspace fork is a different
    /// operation, asked for by name, never substituted.
    pub async fn fork_machine(
        self: &Arc<Self>,
        source: &str,
        operator: &str,
        request: ForkRequest,
    ) -> Result<MachineForkReport, EnvironmentError> {
        let name = request.name.clone();
        validate_name("environment", &name)?;
        if is_candidate(&name) {
            return Err(EnvironmentError::Invalid(format!(
                "names ending {:?} are reserved for candidates",
                super::candidate::CANDIDATE_SUFFIX
            )));
        }
        let record = self.owned_environment(source, operator).await?;
        self.require_live(&record).await?;
        if is_candidate(&record.value.name) {
            return Err(EnvironmentError::Invalid(format!(
                "{source} is a candidate, not an environment to fork"
            )));
        }
        let computer = self
            .fresh_computer(&record.id)
            .await
            .map(|stored| stored.value)
            .filter(|computer| computer.status == compute_core::ComputerStatus::Running)
            .ok_or_else(|| {
                EnvironmentError::Conflict(format!(
                    "{source}'s computer is not running; only a running machine is forked"
                ))
            })?;
        let (Some(target), Some(session_id)) =
            (computer.target.clone(), computer.session_id.clone())
        else {
            return Err(EnvironmentError::Conflict(format!(
                "{source}'s computer has no machine to fork"
            )));
        };
        if !computer
            .capabilities
            .is_some_and(|capabilities| capabilities.fork)
        {
            return Err(EnvironmentError::Invalid(format!(
                "{source}'s computer ({} on {target}) cannot fork its running machine; \
                 a workspace fork (state: workspace) copies its files into a new one",
                computer.provider_kind.as_deref().unwrap_or("its provider"),
            )));
        }
        if let Some(asked) = &request.target
            && asked != &target
        {
            return Err(EnvironmentError::Invalid(format!(
                "a machine fork runs where its source's memory is, on {target}, not {asked}"
            )));
        }
        let derivation = Derivation {
            composition: "fork",
            label: source,
            source: &record,
            name: &name,
            operator,
            target: Some(target.clone()),
            copy_config: request.copy_config,
            provenance: json!({
                "source": source,
                "state": ForkState::Machine,
                "source_session": session_id,
            }),
        };
        let _claim = self.begin_derivation(&derivation).await?;
        let candidate = candidate_name(&name)?;
        // The fork holds what its source holds: the same declared contents,
        // which the controller then finds already there.
        let contents = record.value.contents.clone().unwrap_or_default();
        let definition = self
            .candidate_definition(&derivation, &candidate, contents)
            .await?;
        let fork = compute_core::MachineFork {
            source_environment_id: record.id.clone(),
            target: target.clone(),
            session_id: session_id.clone(),
            generation: 1,
        };
        self.create_computer_environment_with(definition, operator, Some(fork))
            .await?;
        let within = self.config.replacement_deadline;
        for (phase, what) in [("forking", "run"), ("reconciling", "converge")] {
            let reached = self
                .await_computer_within(&candidate, what, within, |view| match what {
                    "run" => view.status == compute_core::ComputerStatus::Running,
                    _ => view.converged,
                })
                .await;
            if let Err(error) = reached {
                return Err(self
                    .abandon_composition("fork", source, &candidate, operator, phase, &error)
                    .await);
            }
        }
        let computer = self
            .hand_off_candidate(
                &derivation,
                &candidate,
                json!({ "state": ForkState::Machine }),
            )
            .await?;
        let session = computer.session_id.clone().unwrap_or_default();
        let checkpoint = match self.target_client(&target) {
            Ok(client) => client
                .session(&session)
                .await
                .ok()
                .and_then(|child| child.lineage)
                .map(|lineage| compute_core::MachineCheckpoint {
                    id: lineage.checkpoint,
                    evidence: lineage.evidence,
                }),
            Err(_) => None,
        };
        Ok(MachineForkReport {
            source: source.to_owned(),
            environment: name,
            state: ForkState::Machine,
            target,
            source_session: session_id,
            session,
            checkpoint,
            omitted_config: if request.copy_config {
                vec![]
            } else {
                record.value.config.keys().cloned().collect()
            },
            computer,
        })
    }

    /// Refuse a name in use, mark the derivation in progress, and clear the
    /// candidate an earlier failed attempt left. Nothing is created.
    pub(crate) async fn begin_derivation(
        self: &Arc<Self>,
        derivation: &Derivation<'_>,
    ) -> Result<Claim<'_>, EnvironmentError> {
        let name = derivation.name;
        self.refresh().await?;
        if self
            .inner
            .lock()
            .await
            .desired
            .environments
            .contains_key(name)
        {
            return Err(EnvironmentError::Conflict(format!(
                "environment {name} already exists"
            )));
        }
        let candidate = candidate_name(name)?;
        let claim = self.claim(
            &format!("derive:{name}"),
            &format!("a {} into {name}", derivation.composition),
        )?;
        self.discard_candidate(&candidate, derivation.operator)
            .await?;
        Ok(claim)
    }

    /// Prepare a candidate from `export` and the source's declared state, and
    /// hand it over as NAME. Returns the seed, the new computer, and every job
    /// that did the work. On any failure the candidate is stopped and the
    /// failure recorded; NAME is never created.
    pub(crate) async fn derive_environment(
        self: &Arc<Self>,
        _claim: &Claim<'_>,
        derivation: &Derivation<'_>,
        export: &WorkspaceExport,
        prior_jobs: Vec<String>,
    ) -> Result<(WorkspaceSeed, ComputerView, Vec<String>), EnvironmentError> {
        let Derivation {
            composition,
            label,
            source,
            name,
            operator,
            ..
        } = derivation;
        let candidate = candidate_name(name)?;
        let definition = self
            .candidate_definition(derivation, &candidate, Default::default())
            .await?;
        self.create_computer_environment_inner(definition, operator)
            .await?;
        let contents = source.value.contents.clone().unwrap_or_default();
        let composed = self
            .prepare_candidate(&candidate, operator, export, &contents, None)
            .await;
        let seed = match composed {
            Ok(seed) => seed,
            Err((phase, error)) => {
                return Err(self
                    .abandon_composition(composition, label, &candidate, operator, phase, &error)
                    .await);
            }
        };
        let mut jobs = prior_jobs;
        jobs.extend(seed.jobs.iter().cloned());
        let computer = self
            .hand_off_candidate(
                derivation,
                &candidate,
                json!({
                    "workspace": export.digest,
                    "workspace_verified": true,
                    "jobs": jobs,
                }),
            )
            .await?;
        Ok((seed, computer, jobs))
    }

    /// Hand a prepared candidate over as NAME. The handoff is fenced on the
    /// candidate's records: a write that lost to another change is retried
    /// from a fresh read, which checks again that the candidate is still the
    /// running machine. A handoff that cannot happen abandons the candidate.
    async fn hand_off_candidate(
        self: &Arc<Self>,
        derivation: &Derivation<'_>,
        candidate: &str,
        detail: serde_json::Value,
    ) -> Result<ComputerView, EnvironmentError> {
        let mut adopted = self.adopt_candidate(derivation, candidate, &detail).await;
        for _ in 0..HANDOFF_RETRIES {
            match &adopted {
                Err(EnvironmentError::Conflict(message))
                    if message.contains("changed: expected version") =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    adopted = self.adopt_candidate(derivation, candidate, &detail).await;
                }
                _ => break,
            }
        }
        match adopted {
            Ok(computer) => Ok(computer),
            Err(error) => Err(self
                .abandon_composition(
                    derivation.composition,
                    derivation.label,
                    candidate,
                    derivation.operator,
                    "handing off",
                    &error,
                )
                .await),
        }
    }

    /// The handoff: one fenced transaction. NAME's environment and computer
    /// records are created from the candidate's, machine and all, and the
    /// candidate's are deleted. Refused, with nothing changed, if NAME has
    /// appeared or the candidate is no longer the running machine.
    async fn adopt_candidate(
        self: &Arc<Self>,
        derivation: &Derivation<'_>,
        candidate: &str,
        detail: &serde_json::Value,
    ) -> Result<ComputerView, EnvironmentError> {
        let Derivation {
            composition,
            label,
            name,
            operator,
            ..
        } = derivation;
        let cand_env = self.owned_environment(candidate, operator).await?;
        let _ = self.refresh_targeted().await;
        let cand = self
            .stored_computer(&cand_env.id)
            .await
            .filter(|cand| {
                cand.value.status == compute_core::ComputerStatus::Running
                    && cand.value.session_id.is_some()
            })
            .ok_or_else(|| {
                EnvironmentError::Conflict(format!("the candidate for {name} is not running"))
            })?;
        let created_at = Utc::now();
        let id = format!(
            "env_{}",
            compute_state::short_digest(&[
                name,
                &created_at
                    .timestamp_nanos_opt()
                    .unwrap_or_default()
                    .to_string(),
                &self.instance_id,
            ])
        );
        let environment = EnvironmentRecord {
            name: (*name).to_owned(),
            created_at,
            ..cand_env.value.clone()
        };
        let computer = ComputerRecord {
            environment_id: id.clone(),
            environment: (*name).to_owned(),
            generation: 1,
            created_at,
            updated_at: Utc::now(),
            ..cand.value.clone()
        };
        let mut data = json!({
            "environment_id": id, "command": composition, "source": label,
            "candidate": candidate, "session": computer.session_id,
        });
        if let Some(data) = data.as_object_mut() {
            for extra in [detail, &derivation.provenance] {
                if let Some(extra) = extra.as_object() {
                    data.extend(extra.clone());
                }
            }
        }
        let scope = Scope::environment(name);
        let mut change = Change::new().with(|batch| {
            batch
                .create(&id, &environment)
                .create(&ids::computer(&id), &computer)
                .delete(&cand)
                .delete(&cand_env)
        });
        change = self.event(
            change,
            events::ENVIRONMENT_CREATED,
            scope.clone(),
            format!("Environment {name} created"),
            json!({ "environment_id": id }),
        );
        change = self.event(
            change,
            events::ENVIRONMENT_COMMAND,
            scope,
            format!("{operator}: {composition} {label} into {name}"),
            data,
        );
        self.apply(change).await?;
        self.computer_wake.notify_waiters();
        self.changed().await;
        self.fresh_computer_view(name).await
    }
}
