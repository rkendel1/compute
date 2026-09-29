//! Restore a checkpoint into a new environment.
//!
//! ```text
//! restore CHECKPOINT NAME
//!   authorize: the checkpoint's owner, who must also own the environment
//!     its declarations come from
//!   resolve and VALIDATE the checkpoint: the record, then its artifact:
//!     canonical form, artifact digest, checkpoint id, workspace digest    checkpoint.rs
//!   only then begin: refuse a name in use, clear an earlier candidate
//!   derive the new environment                                            fork.rs
//!     candidate machine, seed (verified inside it), declared contents
//!     applied and reconciled, one fenced handoff that creates NAME
//! ```
//!
//! **A checkpoint is input state, not an environment.** Restore recreates
//! portable workspace state and nothing of the source's identity or authority:
//! the new environment has a new id, the operator as owner, a new computer and
//! session. No process, memory, socket, session, credential, connection,
//! provider, or endpoint is restored. Processes exist in the restored
//! environment because *it* reconciled its declared contents, under a new pid.
//!
//! **The checkpoint is never trusted and never touched.** The record and the
//! artifact are validated before anything is created, so a corrupted, truncated,
//! or mismatched checkpoint fails before it can produce an environment. Restore
//! reads the checkpoint and writes nothing to it or to the source environment
//! (a failure is recorded on the candidate only). Any number of restores of one
//! checkpoint are independent environments.
//!
//! **Declared state is not in a checkpoint, on purpose.** A checkpoint records
//! the contents generation it was captured under, as provenance. Declarations
//! stay the environment's: restore applies the source environment's *current*
//! declared contents, policy, requirements and lifecycle through the ordinary
//! declaration path, exactly as fork does, and says so: the report and the
//! event state the generation captured and the generation applied, and whether
//! they match. Nothing is serialized into the artifact to be treated as a second
//! authority. Configuration values are never restored.
//!
//! Provenance is recorded as the `restore` event on the new environment (the
//! checkpoint, its artifact, workspace digest, and the source), the repository's
//! existing convention for how an environment came to be (see `fork.rs`); the
//! checkpoint is provenance, never a parent authority.
//!
//! Restore reuses the candidate composition (`candidate.rs`, `fork.rs`); it has
//! no candidate machinery of its own.

use std::sync::Arc;

use compute_state::CheckpointRecord;
use serde_json::json;

use super::Daemon;
use super::fork::Derivation;
use crate::EnvironmentError;
use crate::checkpoint::{self, CHECKPOINT_FORMAT};
use crate::model::*;

impl Daemon {
    /// Restore a checkpoint into a new environment; see the module
    /// documentation.
    pub async fn restore_checkpoint(
        self: &Arc<Self>,
        checkpoint_id: &str,
        operator: &str,
        request: RestoreRequest,
    ) -> Result<RestoreReport, EnvironmentError> {
        let name = request.name.clone();
        validate_name("environment", &name)?;
        if super::candidate::is_candidate(&name) {
            return Err(EnvironmentError::Invalid(format!(
                "names ending {:?} are reserved for candidates",
                super::candidate::CANDIDATE_SUFFIX
            )));
        }
        // A checkpoint another operator owns is not revealed to exist.
        let record = self
            .control()
            .get::<CheckpointRecord>(checkpoint_id)
            .await?
            .map(|stored| stored.value)
            .filter(|record| record.owner == operator)
            .ok_or_else(|| EnvironmentError::NotFound(format!("checkpoint {checkpoint_id}")))?;

        // The declared state is the source environment's, and must still be.
        let source = self
            .owned_environment(&record.environment, operator)
            .await?;
        if source.id != record.environment_id {
            return Err(EnvironmentError::Conflict(format!(
                "checkpoint {} was captured from environment {}, which no longer exists under the name {}; its declared state is gone",
                record.checkpoint_id, record.environment_id, record.environment
            )));
        }
        let current_generation = source
            .value
            .contents
            .as_ref()
            .map_or(0, |contents| contents.generation);

        // Validate before anything is created.
        let (bytes, archive, valid) = self.resolve_checkpoint(&record).await?;
        let export = WorkspaceExport {
            identity: crate::daemon::WORKSPACE_IDENTITY.into(),
            digest: valid.manifest.tree_digest.clone(),
            archive_digest: compute_core::sha256_identity(&archive),
            files: valid.files,
            directories: valid.directories,
            bytes: valid.bytes,
            platform: valid.manifest.source.platform.clone(),
            job_id: String::new(),
            archive,
        };
        drop(bytes);

        let declared = if current_generation == record.contents_generation {
            "matches the state at capture"
        } else {
            "changed since capture"
        };
        let derivation = Derivation {
            composition: "restore",
            label: &record.checkpoint_id,
            source: &source,
            name: &name,
            operator,
            target: request.target.clone(),
            copy_config: false,
            provenance: json!({
                "checkpoint_id": record.checkpoint_id,
                "artifact": record.artifact_id,
                "format": CHECKPOINT_FORMAT,
                "source_environment": record.environment,
                "source_environment_id": record.environment_id,
                "captured_contents_generation": record.contents_generation,
                "applied_contents_generation": current_generation,
                "declared_state": declared,
            }),
        };
        let claim = self.begin_derivation(&derivation).await?;
        let (seed, computer, jobs) = self
            .derive_environment(&claim, &derivation, &export, vec![])
            .await?;
        Ok(RestoreReport {
            checkpoint_id: record.checkpoint_id,
            artifact: record.artifact_id,
            source: record.environment,
            environment: name,
            environment_id: computer.environment_id.clone(),
            computer_id: compute_state::ids::computer(&computer.environment_id),
            workspace: export.digest,
            files: export.files,
            directories: export.directories,
            bytes: export.bytes,
            workspace_verified: seed.verified,
            captured_contents_generation: record.contents_generation,
            applied_contents_generation: current_generation,
            declared_state: declared.into(),
            omitted_config: source.value.config.keys().cloned().collect(),
            configuration_required: valid
                .manifest
                .configuration
                .iter()
                .flat_map(|configuration| &configuration.variables)
                .map(|variable| crate::configuration::ConfigurationRequirement {
                    name: variable.name.clone(),
                    sensitive: variable.sensitive,
                })
                .collect(),
            jobs,
            computer,
        })
    }

    /// The checkpoint's artifact, read and validated against its record, and
    /// the workspace archive it holds. Fails before anything is created.
    async fn resolve_checkpoint(
        &self,
        record: &CheckpointRecord,
    ) -> Result<(Vec<u8>, Vec<u8>, checkpoint::Checkpoint), EnvironmentError> {
        let refused = |why: String| {
            EnvironmentError::Conflict(format!(
                "checkpoint {} cannot be restored: {why}",
                record.checkpoint_id
            ))
        };
        if record.format != CHECKPOINT_FORMAT {
            return Err(refused(format!(
                "its format is {:?}, not {CHECKPOINT_FORMAT}",
                record.format
            )));
        }
        let bytes = self
            .config
            .artifacts
            .get(&record.artifact_id)
            .await?
            .ok_or_else(|| refused(format!("its artifact {} is missing", record.artifact_id)))?;
        let (valid, archive) = checkpoint::restorable(&bytes)
            .map_err(|error| refused(format!("its artifact is not valid: {error}")))?;
        if valid.artifact_digest != record.artifact_id {
            return Err(refused(format!(
                "its artifact is {}, not the {} it records",
                valid.artifact_digest, record.artifact_id
            )));
        }
        if valid.checkpoint_id != record.checkpoint_id {
            return Err(refused(format!(
                "its artifact is checkpoint {}, not {}",
                valid.checkpoint_id, record.checkpoint_id
            )));
        }
        if valid.manifest.tree_digest != record.workspace_digest {
            return Err(refused(format!(
                "its artifact holds workspace {}, not the {} it records",
                valid.manifest.tree_digest, record.workspace_digest
            )));
        }
        if valid.manifest.source.environment_id != record.environment_id {
            return Err(refused(
                "its artifact was captured from a different environment than it records".into(),
            ));
        }
        Ok((bytes, archive, valid))
    }
}
