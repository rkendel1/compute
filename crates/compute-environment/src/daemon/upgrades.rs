//! Binary upgrades and rollbacks of this node's controller.

use std::sync::Arc;

use chrono::Utc;
use compute_state::events;
use serde_json::json;

use super::{Change, Daemon, Scope};
use crate::EnvironmentError;
use crate::auth::Principal;
use crate::upgrade::{
    UpgradeRecord, UpgradeRequest, current_build, inspect_artifact, keep_binary, read_record,
    write_record,
};

impl Daemon {
    /// The upgrade this node last ran, if any.
    pub fn upgrade_record(&self) -> Option<UpgradeRecord> {
        read_record(&self.config.state_dir)
    }

    /// Replace this controller with another build. The artifact is
    /// validated first; then this controller stops supervising (its
    /// workloads keep running on the supervisor) and hands over. The
    /// process that hosts it starts the new build and restores this one
    /// if the new one does not become ready.
    pub async fn request_upgrade(
        self: &Arc<Self>,
        principal: &Principal,
        request: UpgradeRequest,
    ) -> Result<UpgradeRecord, EnvironmentError> {
        let to = inspect_artifact(
            std::path::Path::new(&request.artifact),
            request.expect_sha256.as_deref(),
        )?;
        self.hand_over(principal, "upgrade", to, request.timeout_seconds)
            .await
    }

    /// Return to the build that ran before the last upgrade, with the same
    /// durable state and the same workloads.
    pub async fn request_rollback(
        self: &Arc<Self>,
        principal: &Principal,
        timeout_seconds: Option<u64>,
    ) -> Result<UpgradeRecord, EnvironmentError> {
        let last = self
            .upgrade_record()
            .ok_or_else(|| EnvironmentError::NotFound("no upgrade has run on this node".into()))?;
        let current = current_build();
        let previous = if last.to.build_id == current.build_id {
            last.from
        } else {
            return Err(EnvironmentError::Conflict(format!(
                "this controller ({}) is not the one the last upgrade installed ({}); nothing to roll back to",
                current.build_id, last.to.build_id
            )));
        };
        let to = inspect_artifact(
            std::path::Path::new(&previous.path),
            Some(&previous.build_id),
        )?;
        self.hand_over(principal, "rollback", to, timeout_seconds)
            .await
    }

    async fn hand_over(
        self: &Arc<Self>,
        principal: &Principal,
        kind: &str,
        to: crate::upgrade::BuildIdentity,
        timeout_seconds: Option<u64>,
    ) -> Result<UpgradeRecord, EnvironmentError> {
        if !self.data_plane().independent() {
            return Err(EnvironmentError::UpgradeFailed(
                "this controller runs its workloads in its own process; upgrading it would stop them. Start it with the supervisor data plane".into(),
            ));
        }
        if let Some(last) = self.upgrade_record()
            && !last.is_terminal()
        {
            return Err(EnvironmentError::Conflict(format!(
                "upgrade {} is {}",
                last.upgrade_id, last.status
            )));
        }
        let mut from = current_build();
        if from.build_id == to.build_id {
            return Err(EnvironmentError::Conflict(format!(
                "this controller already runs {}",
                to.build_id
            )));
        }
        // Both builds are kept, so either can run again.
        from.path = keep_binary(&self.config.state_dir, &from)?
            .display()
            .to_string();
        let mut to = to;
        to.path = keep_binary(&self.config.state_dir, &to)?
            .display()
            .to_string();
        let units = self
            .data_plane()
            .units()
            .await?
            .into_iter()
            .filter(|unit| unit.state == "running")
            .map(|unit| unit.manifest.unit_id)
            .collect::<Vec<_>>();
        let record = UpgradeRecord {
            upgrade_id: format!(
                "upg_{}",
                compute_state::short_digest(&[
                    &to.build_id,
                    &Utc::now()
                        .timestamp_nanos_opt()
                        .unwrap_or_default()
                        .to_string()
                ])
            ),
            kind: kind.into(),
            status: "started".into(),
            from,
            to,
            units,
            timeout_seconds: timeout_seconds.unwrap_or(60).clamp(5, 3600),
            requested_by: principal.operator_id.clone(),
            started_at: Utc::now(),
            finished_at: None,
            reason: None,
            controller: None,
        };
        write_record(&self.config.state_dir, &record)?;
        let change = self.event(
            Change::new(),
            events::UPGRADE_STARTED,
            Scope::default(),
            format!(
                "{} of the controller from {} ({}) to {} ({}) started; workloads keep running",
                record.kind,
                record.from.version,
                short(&record.from.build_id),
                record.to.version,
                short(&record.to.build_id)
            ),
            json!({ "upgrade": record }),
        );
        let _ = self.apply(change).await;
        *self.upgrade.lock().expect("upgrade") = Some(record.clone());
        self.detach().await?;
        Ok(record)
    }

    /// The hand-over this controller agreed to, for the process hosting it
    /// to carry out once it has stopped.
    pub fn take_upgrade(&self) -> Option<UpgradeRecord> {
        self.upgrade.lock().expect("upgrade").take()
    }

    /// Release the node (its lock) so another controller can take it.
    pub fn release_node(&self) {
        self.lock.lock().expect("lock").take();
    }

    /// Called as a controller starts, after reattaching: finish the
    /// hand-over it is part of. A new build that cannot account for every
    /// workload that was running refuses to take over; the previous build
    /// is then restored.
    pub(crate) async fn resume_upgrade(self: &Arc<Self>) -> Result<(), EnvironmentError> {
        let Some(mut record) = self.upgrade_record() else {
            return Ok(());
        };
        let own = current_build();
        match record.status.as_str() {
            "started" if record.to.build_id == own.build_id => {
                let supervised = self
                    .inner
                    .lock()
                    .await
                    .runtime
                    .values()
                    .filter_map(|runtime| runtime.unit_id.clone())
                    .collect::<std::collections::BTreeSet<_>>();
                let running = self
                    .data_plane()
                    .units()
                    .await?
                    .into_iter()
                    .filter(|unit| unit.state == "running")
                    .map(|unit| unit.manifest.unit_id)
                    .collect::<std::collections::BTreeSet<_>>();
                let missing = record
                    .units
                    .iter()
                    .filter(|unit| running.contains(*unit) && !supervised.contains(*unit))
                    .cloned()
                    .collect::<Vec<_>>();
                if !missing.is_empty() {
                    record.status = "refused".into();
                    record.reason = Some(format!(
                        "the new controller could not reattach {}",
                        missing.join(", ")
                    ));
                    write_record(&self.config.state_dir, &record)?;
                    return Err(EnvironmentError::UpgradeFailed(
                        record.reason.clone().unwrap_or_default(),
                    ));
                }
                record.status = "completed".into();
                record.finished_at = Some(Utc::now());
                record.controller = Some(self.instance_id.clone());
                write_record(&self.config.state_dir, &record)?;
                let seconds = (Utc::now() - record.started_at).num_milliseconds() as f64 / 1000.0;
                for (kind, message) in [
                    (
                        events::UPGRADE_READY,
                        format!(
                            "controller {} ({}) reattached {} workloads",
                            record.to.version,
                            short(&record.to.build_id),
                            record.units.len()
                        ),
                    ),
                    (
                        events::UPGRADE_COMPLETED,
                        format!(
                            "{} to {} completed in {seconds:.1}s without restarting a workload",
                            record.kind,
                            short(&record.to.build_id)
                        ),
                    ),
                ] {
                    let change = self.event(
                        Change::new(),
                        kind,
                        Scope::default(),
                        message,
                        json!({ "upgrade": record }),
                    );
                    let _ = self.apply(change).await;
                }
                Ok(())
            }
            "rolling_back" if record.from.build_id == own.build_id => {
                record.status = "rolled_back".into();
                record.finished_at = Some(Utc::now());
                record.controller = Some(self.instance_id.clone());
                write_record(&self.config.state_dir, &record)?;
                for (kind, message) in [
                    (
                        events::UPGRADE_FAILED,
                        format!(
                            "{} to {} failed: {}",
                            record.kind,
                            short(&record.to.build_id),
                            record.reason.clone().unwrap_or_default()
                        ),
                    ),
                    (
                        events::UPGRADE_ROLLED_BACK,
                        format!(
                            "controller {} ({}) restored; workloads kept running",
                            record.from.version,
                            short(&record.from.build_id)
                        ),
                    ),
                ] {
                    let change = self.event(
                        Change::new(),
                        kind,
                        Scope::default(),
                        message,
                        json!({ "upgrade": record }),
                    );
                    let _ = self.apply(change).await;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

fn short(build_id: &str) -> &str {
    let digest = build_id.trim_start_matches("sha256:");
    &digest[..digest.len().min(12)]
}
