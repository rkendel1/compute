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
use compute_state::{ComputerRecord, EnvironmentRecord, events, ids};
use serde_json::json;

use super::candidate::{candidate_name, is_candidate};
use super::{Change, Daemon, Scope};
use crate::EnvironmentError;
use crate::model::*;
use crate::status::ComputerView;

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
        // A duplicate is refused before anything is exported or created.
        self.refresh().await?;
        if self
            .inner
            .lock()
            .await
            .desired
            .environments
            .contains_key(&name)
        {
            return Err(EnvironmentError::Conflict(format!(
                "environment {name} already exists"
            )));
        }
        let candidate = candidate_name(&name)?;
        let _claim = self.claim(&format!("fork:{name}"), &format!("a fork into {name}"))?;
        let spec = record
            .value
            .computer
            .clone()
            .ok_or_else(|| EnvironmentError::Invalid(format!("{source} has no computer")))?;
        let contents = record.value.contents.clone().unwrap_or_default();
        let policy = record
            .value
            .policy
            .clone()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| EnvironmentError::Invalid(format!("{source}'s policy: {error}")))?;
        let config = if request.copy_config {
            record.value.config.clone()
        } else {
            Default::default()
        };
        self.discard_candidate(&candidate, operator).await?;

        // Nothing exists until the source has been captured.
        let export = self.export_workspace(source, operator).await?;
        self.create_computer_environment_inner(
            ComputerEnvironmentDefinition {
                name: candidate.clone(),
                desired_state: DesiredState::Running,
                env: config,
                policy,
                computer: ComputerRequest {
                    lifecycle: spec.lifecycle,
                    requirements: spec.requirements,
                    target: request.target,
                    ttl_seconds: None,
                },
                contents: Default::default(),
            },
            operator,
        )
        .await?;
        let composed = self
            .prepare_candidate(&candidate, operator, &export, &contents, None)
            .await;
        let seed = match composed {
            Ok(seed) => seed,
            Err((phase, error)) => {
                return Err(self
                    .abandon_composition("fork", source, &candidate, operator, phase, &error)
                    .await);
            }
        };
        let mut jobs = vec![export.job_id.clone()];
        jobs.extend(seed.jobs);
        let computer = match self
            .adopt_candidate(source, &name, &candidate, operator, &export.digest, &jobs)
            .await
        {
            Ok(computer) => computer,
            Err(error) => {
                return Err(self
                    .abandon_composition(
                        "fork",
                        source,
                        &candidate,
                        operator,
                        "handing off",
                        &error,
                    )
                    .await);
            }
        };

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

    /// The handoff: one fenced transaction. NAME's environment and computer
    /// records are created from the candidate's, machine and all, and the
    /// candidate's are deleted. Refused, with nothing changed, if NAME has
    /// appeared or the candidate is no longer the verified, running machine.
    async fn adopt_candidate(
        self: &Arc<Self>,
        source: &str,
        name: &str,
        candidate: &str,
        operator: &str,
        workspace: &str,
        jobs: &[String],
    ) -> Result<ComputerView, EnvironmentError> {
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
            name: name.to_owned(),
            created_at,
            ..cand_env.value.clone()
        };
        let computer = ComputerRecord {
            environment_id: id.clone(),
            environment: name.to_owned(),
            generation: 1,
            created_at,
            updated_at: Utc::now(),
            ..cand.value.clone()
        };
        let data = json!({
            "environment_id": id, "command": "fork", "source": source,
            "candidate": candidate, "workspace": workspace, "workspace_verified": true,
            "session": computer.session_id, "jobs": jobs,
        });
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
            format!("{operator} forked {source} into {name}"),
            data,
        );
        self.apply(change).await?;
        self.computer_wake.notify_waiters();
        self.changed().await;
        self.fresh_computer_view(name).await
    }
}
