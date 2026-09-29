//! Clone an environment: a composition of the workspace primitives.
//!
//! ```text
//! clone SOURCE NAME
//!   export SOURCE's workspace          (workspace.rs)
//!   create NAME: same requirements and policy, no contents
//!   seed NAME, which verifies          (workspace.rs)
//!   apply SOURCE's declared contents; the reconciler starts them
//!   report the commits on both sides
//! ```
//!
//! Nothing here knows how a workspace is read, written, or identified. The
//! workspace is exported and seeded whole; declared repositories are *not*
//! part of it (`repos/` is excluded by the workspace contract) and are
//! re-derived from the declared contents at the same revision.
//!
//! A failed clone never presents an unverified environment as runnable: the
//! new environment is created with no contents (nothing runs before the seed
//! is verified), and if any phase after creation fails it is stopped and the
//! failure is recorded. Reality then says stopped; there is no rollback.

use std::sync::Arc;

use compute_core::ComputerStatus;
use compute_state::events;
use serde_json::json;

use super::{Change, Daemon, Scope};
use crate::EnvironmentError;
use crate::model::*;
use crate::status::ComputerView;

impl Daemon {
    /// Clone an environment; see the module documentation.
    pub async fn clone_environment(
        self: &Arc<Self>,
        source: &str,
        operator: &str,
        request: CloneRequest,
    ) -> Result<CloneReport, EnvironmentError> {
        let name = request.name.clone();
        let record = self.owned_environment(source, operator).await?;
        self.require_live(&record).await?;
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

        // Nothing exists until the source has been captured.
        let export = self.export_workspace(source, operator).await?;
        self.create_computer_environment(
            ComputerEnvironmentDefinition {
                name: name.clone(),
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
            .seed_and_start(&name, operator, &export, &contents)
            .await;
        let (seed, computer) = match composed {
            Ok(done) => done,
            Err((phase, error)) => {
                return Err(self
                    .abandon_composition("clone", source, &name, operator, phase, &error)
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
        let mut jobs = vec![export.job_id.clone()];
        jobs.extend(seed.jobs);
        let cloned = self.owned_environment(&name, operator).await?;
        let change = self.event(
            Change::new(),
            events::ENVIRONMENT_COMMAND,
            Scope::environment(&name),
            format!("{operator} cloned {source} into {name}"),
            json!({
                "environment_id": cloned.id, "command": "clone", "source": source,
                "workspace": export.digest, "archive": export.archive_digest, "jobs": jobs,
            }),
        );
        self.apply(change).await?;
        Ok(CloneReport {
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

    /// Everything after the new environment exists, each step tagged for the
    /// failure record.
    async fn seed_and_start(
        self: &Arc<Self>,
        name: &str,
        operator: &str,
        export: &WorkspaceExport,
        contents: &compute_core::EnvironmentContents,
    ) -> Result<(WorkspaceSeed, ComputerView), (&'static str, EnvironmentError)> {
        self.await_computer(name, "run", |view| view.status == ComputerStatus::Running)
            .await
            .map_err(|error| ("provisioning", error))?;
        let seed = self
            .seed_workspace(
                name,
                operator,
                WorkspaceSeedRequest {
                    archive: export.archive.clone(),
                    digest: Some(export.digest.clone()),
                },
            )
            .await
            .map_err(|error| ("seeding", error))?;
        self.change_environment(name, operator, "cloned".into(), None, |value| {
            value.contents = Some(contents.clone());
            Ok(())
        })
        .await
        .map_err(|error| ("applying contents", error))?;
        let computer = self
            .await_computer(name, "converge", |view| view.converged)
            .await
            .map_err(|error| ("starting", error))?;
        Ok((seed, computer))
    }
}
