//! Capture a checkpoint: durable, immutable, portable state.
//!
//! ```text
//! checkpoint ENVIRONMENT
//!   authorize: the environment's owner                       owned_environment
//!   export the workspace (a durable job; digest before and
//!     after; fails closed if it changed)                     workspace.rs
//!   build the canonical artifact from the validated archive  checkpoint.rs
//!   validate what was built (it must reproduce the digest)   checkpoint.rs
//!   store it (content-addressed) and read it back, verified  ArtifactStore
//!   PUBLISH: one fenced write creates the Checkpoint record
//!     and its event                                          FeltDB
//! ```
//!
//! **Nothing is published until everything is verified.** The record is the
//! last write and is created only by the transaction that also records the
//! event, so a capture that fails or is interrupted at any earlier point leaves
//! no record. An artifact stored before a failure is content-addressed bytes no
//! record names: unreachable, not authoritative, and identical to what a retry
//! stores. A failure is recorded as an event with its phase; the environment is
//! never marked, stopped, or otherwise changed, and the next capture is
//! unaffected.
//!
//! **A checkpoint is named by its content.** `ckp_` + the artifact digest, so
//! capturing state that has already been captured returns the existing record
//! unchanged (`existing`), and an existing checkpoint is never rewritten.
//!
//! The environment stays authoritative for declared state. The record keeps the
//! contents generation for provenance; the artifact holds no declaration.

use std::sync::Arc;

use compute_state::{CheckpointRecord, CheckpointStatus, events, ids};
use serde_json::json;

use super::{Change, Daemon, Scope};
use crate::EnvironmentError;
use crate::checkpoint::{self, CHECKPOINT_FORMAT, Provenance};
use crate::model::*;

/// Bound on the checkpoints listed for one environment.
const LIST_LIMIT: usize = 500;

impl Daemon {
    /// Capture a checkpoint of an environment's workspace; see the module
    /// documentation.
    pub async fn checkpoint_environment(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
        request: CheckpointRequest,
    ) -> Result<CheckpointReport, EnvironmentError> {
        let record = self.owned_environment(environment, operator).await?;
        self.require_live(&record).await?;
        if super::candidate::is_candidate(&record.value.name) {
            return Err(EnvironmentError::Invalid(format!(
                "{environment} is a candidate, not an environment to checkpoint"
            )));
        }
        if let Some(parent) = &request.parent {
            let parent = self
                .control()
                .get::<CheckpointRecord>(parent)
                .await?
                .ok_or_else(|| EnvironmentError::NotFound(format!("checkpoint {parent}")))?;
            if parent.value.environment_id != record.id {
                return Err(EnvironmentError::Invalid(format!(
                    "checkpoint {} is not a checkpoint of {environment}",
                    parent.value.checkpoint_id
                )));
            }
        }
        let spec =
            record.value.computer.clone().ok_or_else(|| {
                EnvironmentError::Invalid(format!("{environment} has no computer"))
            })?;
        let contents_generation = record
            .value
            .contents
            .as_ref()
            .map_or(0, |contents| contents.generation);
        let provenance = Provenance {
            environment_id: record.id.clone(),
            computer_generation: spec.generation,
            contents_generation,
            platform: String::new(),
            parent: request.parent.clone(),
        };

        let captured = self
            .capture(environment, operator, &record.id, provenance, &request)
            .await;
        match captured {
            Ok(report) => Ok(report),
            Err((phase, error)) => {
                // Recorded, best effort: the failure must be visible, and
                // recording it must not turn one failure into another.
                let change = self.event(
                    Change::new(),
                    events::CHECKPOINT_FAILED,
                    Scope::environment(environment),
                    format!("{operator}: checkpoint of {environment} failed while {phase}"),
                    json!({
                        "environment_id": record.id, "command": "checkpoint",
                        "outcome": "failed", "phase": phase, "published": false,
                        "error": error.to_string(),
                    }),
                );
                let _ = self.apply(change).await;
                Err(error)
            }
        }
    }

    async fn capture(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
        environment_id: &str,
        provenance: Provenance,
        request: &CheckpointRequest,
    ) -> Result<CheckpointReport, (&'static str, EnvironmentError)> {
        let export = self
            .export_workspace(environment, operator)
            .await
            .map_err(|error| ("capturing the workspace", error))?;
        let provenance = Provenance {
            platform: export.platform.clone(),
            ..provenance
        };
        let workspace = super::read_workspace(&export.archive)
            .map_err(|error| ("reading the workspace", error))?;
        let bytes = checkpoint::build(&workspace, &provenance)
            .map_err(|error| ("building the artifact", error))?;
        let built =
            checkpoint::validate(&bytes).map_err(|error| ("validating the artifact", error))?;
        if built.manifest.tree_digest != export.digest {
            return Err((
                "validating the artifact",
                EnvironmentError::Conflict(format!(
                    "the artifact holds workspace {}, not the {} that was captured",
                    built.manifest.tree_digest, export.digest
                )),
            ));
        }
        let artifacts = &self.config.artifacts;
        let stored = artifacts
            .put("checkpoint", &bytes)
            .await
            .map_err(|error| ("storing the artifact", error.into()))?;
        if stored != built.artifact_digest {
            return Err((
                "storing the artifact",
                EnvironmentError::Conflict(format!(
                    "the store named the artifact {stored}, not {}",
                    built.artifact_digest
                )),
            ));
        }
        // What is published is what can be read back and validated.
        let read = artifacts
            .get(&stored)
            .await
            .map_err(|error| ("verifying the stored artifact", error.into()))?
            .ok_or_else(|| {
                (
                    "verifying the stored artifact",
                    EnvironmentError::Conflict(format!(
                        "artifact {stored} was not found after it was stored"
                    )),
                )
            })?;
        let again = checkpoint::validate(&read)
            .map_err(|error| ("verifying the stored artifact", error))?;
        if again.artifact_digest != built.artifact_digest {
            return Err((
                "verifying the stored artifact",
                EnvironmentError::Conflict(
                    "the stored artifact is not the one that was built".into(),
                ),
            ));
        }

        let id = ids::checkpoint(&built.artifact_digest);
        let existing = self
            .control()
            .get::<CheckpointRecord>(&id)
            .await
            .map_err(|error| ("publishing", error.into()))?;
        let (published, existed) = match existing {
            Some(existing) => (existing.value, true),
            None => {
                let checkpoint = CheckpointRecord {
                    checkpoint_id: id.clone(),
                    environment_id: environment_id.to_owned(),
                    environment: environment.to_owned(),
                    owner: operator.to_owned(),
                    status: CheckpointStatus::Ready,
                    format: CHECKPOINT_FORMAT.into(),
                    computer_generation: provenance.computer_generation,
                    contents_generation: provenance.contents_generation,
                    artifact_id: built.artifact_digest.clone(),
                    workspace_digest: export.digest.clone(),
                    parent_checkpoint_id: request.parent.clone(),
                    size: built.size,
                    files: built.files as u64,
                    directories: built.directories as u64,
                    capture_job_id: export.job_id.clone(),
                    created_at: chrono::Utc::now(),
                };
                let change = Change::new().with(|batch| batch.create(&id, &checkpoint));
                let change = self.event(
                    change,
                    events::CHECKPOINT_CAPTURED,
                    Scope::environment(environment),
                    format!("{operator} captured checkpoint {id} of {environment}"),
                    json!({
                        "environment_id": environment_id, "command": "checkpoint",
                        "outcome": "captured", "checkpoint_id": id,
                        "workspace": export.digest, "artifact": built.artifact_digest,
                        "parent": request.parent, "job_id": export.job_id,
                        "computer_generation": provenance.computer_generation,
                        "contents_generation": provenance.contents_generation,
                    }),
                );
                self.apply(change)
                    .await
                    .map_err(|error| ("publishing", error))?;
                (checkpoint, false)
            }
        };
        Ok(CheckpointReport {
            environment: environment.to_owned(),
            checkpoint_id: published.checkpoint_id,
            format: published.format,
            workspace: published.workspace_digest,
            artifact: published.artifact_id,
            size: published.size,
            files: published.files as usize,
            directories: published.directories as usize,
            computer_generation: published.computer_generation,
            contents_generation: published.contents_generation,
            platform: provenance.platform,
            parent: published.parent_checkpoint_id,
            existing: existed,
            verified: true,
            job_id: export.job_id,
        })
    }

    /// The checkpoints of an environment, newest first, as records. Their
    /// artifacts are not read.
    pub async fn checkpoints(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
    ) -> Result<Vec<CheckpointView>, EnvironmentError> {
        let record = self.owned_environment(environment, operator).await?;
        Ok(self
            .control()
            .query::<CheckpointRecord>(
                compute_state::Query::all(compute_state::Collection::Checkpoint)
                    .eq("environment_id", record.id.clone())
                    .descending("created_at")
                    .limit(LIST_LIMIT),
            )
            .await?
            .into_iter()
            .map(|stored| CheckpointView {
                checkpoint: stored.value,
                valid: None,
                invalid_reason: None,
            })
            .collect())
    }

    /// One checkpoint, with its artifact read from the store and validated:
    /// a record never vouches for bytes that no longer validate.
    pub async fn checkpoint(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
        checkpoint_id: &str,
    ) -> Result<CheckpointView, EnvironmentError> {
        let record = self.owned_environment(environment, operator).await?;
        let stored = self
            .control()
            .get::<CheckpointRecord>(checkpoint_id)
            .await?
            .filter(|stored| stored.value.environment_id == record.id)
            .ok_or_else(|| EnvironmentError::NotFound(format!("checkpoint {checkpoint_id}")))?;
        let checkpoint = stored.value;
        let verdict = async {
            let bytes = self
                .config
                .artifacts
                .get(&checkpoint.artifact_id)
                .await?
                .ok_or_else(|| {
                    EnvironmentError::NotFound(format!("artifact {}", checkpoint.artifact_id))
                })?;
            let valid = checkpoint::validate(&bytes)?;
            if valid.artifact_digest != checkpoint.artifact_id
                || valid.manifest.tree_digest != checkpoint.workspace_digest
            {
                return Err(EnvironmentError::Conflict(
                    "the artifact is not the one the record names".into(),
                ));
            }
            Ok::<(), EnvironmentError>(())
        }
        .await;
        Ok(CheckpointView {
            valid: Some(verdict.is_ok()),
            invalid_reason: verdict.err().map(|error| error.to_string()),
            checkpoint,
        })
    }
}
