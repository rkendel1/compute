//! The candidate: a temporary composition of ordinary Environment and
//! Computer machinery, used to make a change atomic from an environment's
//! point of view.
//!
//! **A candidate is not a Compute primitive.** It is an environment like any
//! other, reconciled by the ordinary controller, with the ordinary lifecycle
//! and Reality. What sets it apart is only that its name ends in
//! [`CANDIDATE_SUFFIX`], which no operator can use, and that a composition
//! ([`replace`](super::replace), [`fork`](super::fork)) prepares a machine in
//! it (workspace seeded and verified, declared contents reconciled) while the
//! environment it is for is untouched. The composition then finishes in one
//! fenced transaction over the records involved, deleting the candidate's:
//! nothing of a candidate outlives success, and nothing but an inert, stopped
//! candidate outlives failure, cleared by the next attempt at the same name.
//!
//! A machine cannot be prepared before the environment that will hold it
//! exists, and the controller reconciles only what a record binds. Rather than
//! generalize the controller to drive two machines for one record, or to bind a
//! machine to a record that does not exist yet, the second machine is prepared
//! in a record of its own and handed over.

use std::collections::BTreeSet;
use std::time::Duration;

use compute_core::{ComputerStatus, EnvironmentContents};

use super::{Change, Daemon};
use crate::EnvironmentError;
use crate::model::*;

/// Reserved: only a composition over the workspace primitives creates an
/// environment whose name ends this way.
pub(crate) const CANDIDATE_SUFFIX: &str = "--candidate";

pub(crate) fn is_candidate(name: &str) -> bool {
    name.ends_with(CANDIDATE_SUFFIX)
}

/// The candidate's name for an environment name, refusing one too long for it.
pub(crate) fn candidate_name(name: &str) -> Result<String, EnvironmentError> {
    let candidate = format!("{name}{CANDIDATE_SUFFIX}");
    if candidate.len() > 63 {
        return Err(EnvironmentError::Invalid(format!(
            "{name} is too long to have a candidate (its name would exceed 63 characters)"
        )));
    }
    Ok(candidate)
}

/// Releases the in-progress mark of a composition when it ends.
pub(crate) struct Claim<'a> {
    claims: &'a std::sync::Mutex<BTreeSet<String>>,
    key: String,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.claims.lock().expect("claims").remove(&self.key);
    }
}

impl Daemon {
    /// Mark a composition as in progress on this daemon. A guard against two
    /// at once, never state: what a composition creates is recorded durably.
    pub(crate) fn claim(&self, key: &str, what: &str) -> Result<Claim<'_>, EnvironmentError> {
        if !self.claims.lock().expect("claims").insert(key.to_owned()) {
            return Err(EnvironmentError::Conflict(format!(
                "{what} is already in progress"
            )));
        }
        Ok(Claim {
            claims: &self.claims,
            key: key.to_owned(),
        })
    }

    /// Everything before a candidate is handed over, each step tagged for the
    /// failure record: the candidate runs, its workspace is seeded and
    /// verified, the declared contents are applied and reconciled. With
    /// `recheck` (the name of the source environment) the source is
    /// re-measured last and must still be the workspace that was captured, for
    /// a composition that would otherwise lose what the source did since.
    pub(crate) async fn prepare_candidate(
        self: &std::sync::Arc<Self>,
        candidate: &str,
        operator: &str,
        export: &WorkspaceExport,
        contents: &EnvironmentContents,
        recheck: Option<&str>,
    ) -> Result<WorkspaceSeed, (&'static str, EnvironmentError)> {
        let within = self.config.replacement_deadline;
        self.await_computer_within(candidate, "run", within, |view| {
            view.status == ComputerStatus::Running
        })
        .await
        .map_err(|error| ("provisioning", error))?;
        let seed = self
            .seed_workspace(
                candidate,
                operator,
                WorkspaceSeedRequest {
                    archive: export.archive.clone(),
                    digest: Some(export.digest.clone()),
                },
            )
            .await
            .map_err(|error| ("seeding", error))?;
        self.change_environment(candidate, operator, "prepared".into(), None, |value| {
            value.contents = Some(contents.clone());
            Ok(())
        })
        .await
        .map_err(|error| ("applying contents", error))?;
        self.await_computer_within(candidate, "converge", within, |view| view.converged)
            .await
            .map_err(|error| ("reconciling", error))?;
        if let Some(environment) = recheck {
            let source = self
                .verify_workspace(
                    environment,
                    operator,
                    WorkspaceVerifyRequest {
                        digest: Some(export.digest.clone()),
                    },
                )
                .await
                .map_err(|error| ("verifying the source", error))?;
            if !source.verified {
                return Err((
                    "verifying the source",
                    EnvironmentError::Conflict(format!(
                        "the workspace of {environment} is {}, no longer the {} that was captured; quiesce it and try again",
                        source.digest, export.digest
                    )),
                ));
            }
        }
        Ok(seed)
    }

    /// Clear a candidate an earlier, failed composition left: destroy its
    /// machine, wait until it is gone, and delete its records.
    pub(crate) async fn discard_candidate(
        self: &std::sync::Arc<Self>,
        candidate: &str,
        operator: &str,
    ) -> Result<(), EnvironmentError> {
        let env = match self.owned_environment(candidate, operator).await {
            Ok(env) => env,
            Err(EnvironmentError::NotFound(_)) => return Ok(()),
            Err(error) => return Err(error),
        };
        self.destroy_computer(candidate, operator).await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            self.refresh().await?;
            match self.stored_computer(&env.id).await {
                Some(computer)
                    if computer.value.status.is_terminal() && computer.value.retired.is_empty() =>
                {
                    let env = self.owned_environment(candidate, operator).await?;
                    let change = Change::new().with(|batch| batch.delete(&computer).delete(&env));
                    return self.apply(change).await;
                }
                None => {
                    let change = Change::new().with(|batch| batch.delete(&env));
                    return self.apply(change).await;
                }
                Some(_) => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(EnvironmentError::Conflict(format!(
                    "an earlier attempt's candidate {candidate} could not be cleared yet"
                )));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}
