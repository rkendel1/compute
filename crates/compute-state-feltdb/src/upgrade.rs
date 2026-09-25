//! `compute control-plane upgrade`: move the Compute model in FeltDB to
//! this build's, before a controller that needs it runs.
//!
//! 1. inspect the active model
//! 2. inspect the model this build requires
//! 3. refuse a downgrade: an active model this build does not fully know
//! 4. create a backup through FeltDB's own backup contract, and verify it
//!    with FeltDB's own verifier
//! 5. apply the model (draft, validate, commit, promote)
//! 6. verify the active model is now this build's, and give older records
//!    their indexed identity
//! 7. smoke-test: every collection reads and decodes, the revision reads,
//!    and a transaction commits
//! 8. report every step
//!
//! Compute never writes a backup format of its own: FeltDB creates and
//! verifies the backup; Compute only orchestrates and refuses to continue
//! without it.

use std::path::PathBuf;

use compute_state::{Collection, Query, StateError, StateStore, Write};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    FeltDbConfig, FeltDbState, ModelInspection, ModelRelation, ProvisionRequest, inspect_model,
    provision,
};

/// How the upgrade secures a backup first.
#[derive(Debug, Clone)]
pub enum BackupPlan {
    /// An online backup by FeltDB (`POST /admin/backups`, which needs a
    /// `cluster:write` key) to `output` on the FeltDB host, verified with
    /// `feltdb-server backup verify` from `verifier`.
    Online {
        output: String,
        token: String,
        verifier: Option<PathBuf>,
    },
    /// A backup the operator already took with FeltDB (`feltdb-server
    /// backup create` of the stopped authority), verified again here with
    /// `verifier` before anything changes.
    Archive {
        archive: String,
        verifier: Option<PathBuf>,
    },
    /// The operator declined a backup, and said why. Recorded as skipped,
    /// never as passed.
    Skip { reason: String },
}

#[derive(Debug, Clone)]
pub struct UpgradeRequest {
    pub config: FeltDbConfig,
    pub backup: BackupPlan,
    /// Inspect and plan only.
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpgradeStep {
    pub name: String,
    /// `PASS`, `FAIL`, `SKIPPED`, or `NOT_RUN`.
    pub status: String,
    pub detail: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpgradeReport {
    /// `upgraded`, `current`, `planned`, `refused`, or `failed`.
    pub result: String,
    pub before: Option<ModelInspection>,
    pub after: Option<ModelInspection>,
    pub steps: Vec<UpgradeStep>,
}

impl UpgradeReport {
    pub fn succeeded(&self) -> bool {
        matches!(self.result.as_str(), "upgraded" | "current" | "planned")
    }

    fn step(&mut self, name: &str, status: &str, detail: Value) {
        self.steps.push(UpgradeStep {
            name: name.into(),
            status: status.into(),
            detail,
        });
    }

    fn finish(mut self, result: &str, remaining: &[&str]) -> Self {
        for name in remaining {
            self.step(name, "NOT_RUN", Value::Null);
        }
        self.result = result.into();
        self
    }
}

const STEPS: [&str; 9] = [
    "inspect_current_model",
    "inspect_required_model",
    "refuse_unsafe_downgrade",
    "backup_create",
    "backup_verify",
    "apply_model",
    "verify_model",
    "smoke_test",
    "report",
];

/// Run the upgrade workflow. Never panics on a failed step: the report
/// says which step failed and why, and nothing after it ran.
pub async fn upgrade_control_plane(request: UpgradeRequest) -> UpgradeReport {
    let mut report = UpgradeReport {
        result: String::new(),
        before: None,
        after: None,
        steps: vec![],
    };
    let rest = |from: usize| STEPS[from..].to_vec();

    // 1–2. What is active, and what this build requires.
    let before = match inspect_model(&request.config).await {
        Ok(inspection) => inspection,
        Err(error) => {
            report.step(STEPS[0], "FAIL", json!({ "error": error.to_string() }));
            return report.finish("failed", &rest(1));
        }
    };
    report.step(
        STEPS[0],
        "PASS",
        json!({
            "active_revision": before.active_revision,
            "active_schema_version": before.active_schema_version,
            "relation": before.comparison.relation,
        }),
    );
    report.step(
        STEPS[1],
        "PASS",
        json!({
            "model": before.required,
            "generation": before.required_generation,
            "additions": before.comparison.additions,
        }),
    );
    let relation = before.comparison.relation.clone();
    report.before = Some(before.clone());

    // 3. A model this build does not fully know is never replaced.
    if matches!(relation, ModelRelation::Newer | ModelRelation::Divergent) {
        report.step(
            STEPS[2],
            "FAIL",
            json!({
                "relation": relation,
                "unknown": before.comparison.unknown,
                "remedy": "run this upgrade with the Compute build that installed the active model, or a newer one",
            }),
        );
        return report.finish("refused", &rest(3));
    }
    report.step(STEPS[2], "PASS", json!({ "relation": relation }));
    let changes = relation != ModelRelation::Current;

    if request.dry_run {
        report.step(
            STEPS[3],
            "SKIPPED",
            json!({ "reason": "dry run", "would_change_model": changes }),
        );
        return report.finish("planned", &rest(4));
    }

    // 4–5. A verified backup before any change.
    if changes {
        match &request.backup {
            BackupPlan::Skip { reason } => {
                report.step(STEPS[3], "SKIPPED", json!({ "reason": reason }));
                report.step(STEPS[4], "SKIPPED", json!({ "reason": reason }));
            }
            BackupPlan::Archive { archive, verifier } => {
                let age = std::fs::read(std::path::Path::new(archive).join("manifest.json"))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                    .and_then(|manifest| manifest["created_ms"].as_i64())
                    .map(|created| (chrono::Utc::now().timestamp_millis() - created) / 1000);
                report.step(
                    STEPS[3],
                    "PASS",
                    json!({ "archive": archive, "taken_by": "operator", "age_seconds": age }),
                );
                match verify_backup(verifier.as_deref(), archive) {
                    Ok(verified) => report.step(STEPS[4], "PASS", verified),
                    Err(error) => {
                        report.step(STEPS[4], "FAIL", json!({ "error": error }));
                        return report.finish("failed", &rest(5));
                    }
                }
            }
            BackupPlan::Online {
                output,
                token,
                verifier,
            } => {
                let created = online_backup(&request.config, token, output).await;
                match created {
                    Ok(manifest) => report.step(
                        STEPS[3],
                        "PASS",
                        json!({
                            "output": output,
                            "format": manifest["format"],
                            "files": manifest["entries"].as_array().map(Vec::len),
                        }),
                    ),
                    Err(error) => {
                        report.step(STEPS[3], "FAIL", json!({ "error": error }));
                        return report.finish("failed", &rest(4));
                    }
                }
                match verify_backup(verifier.as_deref(), output) {
                    Ok(verified) => report.step(STEPS[4], "PASS", verified),
                    Err(error) => {
                        report.step(STEPS[4], "FAIL", json!({ "error": error }));
                        return report.finish("failed", &rest(5));
                    }
                }
            }
        }
    } else {
        report.step(
            STEPS[3],
            "SKIPPED",
            json!({ "reason": "the model is current" }),
        );
        report.step(
            STEPS[4],
            "SKIPPED",
            json!({ "reason": "the model is current" }),
        );
    }

    // 6. Apply.
    if changes {
        let applied = provision(ProvisionRequest {
            url: request.config.url.clone(),
            token: request.config.token.clone(),
            application_id: Some(request.config.application_id.clone()),
            tenant_id: None,
            tenant_name: String::new(),
            environment: request.config.environment.clone(),
            ca_certificate: request.config.ca_certificate.clone(),
        })
        .await;
        match applied {
            Ok(provisioned) => report.step(
                STEPS[5],
                "PASS",
                json!({ "revision_id": provisioned.revision_id, "changed": provisioned.changed }),
            ),
            Err(error) => {
                report.step(STEPS[5], "FAIL", json!({ "error": error.to_string() }));
                return report.finish("failed", &rest(6));
            }
        }
    } else {
        report.step(
            STEPS[5],
            "SKIPPED",
            json!({ "reason": "the model is current" }),
        );
    }

    // 7. The active model is now exactly this build's; older records get
    // their indexed identity.
    let after = match inspect_model(&request.config).await {
        Ok(after) if after.comparison.relation == ModelRelation::Current => after,
        Ok(after) => {
            report.step(
                STEPS[6],
                "FAIL",
                json!({ "relation": after.comparison.relation, "differences": after.comparison.additions }),
            );
            report.after = Some(after);
            return report.finish("failed", &rest(7));
        }
        Err(error) => {
            report.step(STEPS[6], "FAIL", json!({ "error": error.to_string() }));
            return report.finish("failed", &rest(7));
        }
    };
    let state = match FeltDbState::connect(request.config.clone()).await {
        Ok(state) => state,
        Err(error) => {
            report.step(STEPS[6], "FAIL", json!({ "error": error.to_string() }));
            return report.finish("failed", &rest(7));
        }
    };
    match state.upgrade_records().await {
        Ok(upgraded) => report.step(
            STEPS[6],
            "PASS",
            json!({ "active_revision": after.active_revision, "records_given_identity": upgraded }),
        ),
        Err(error) => {
            report.step(STEPS[6], "FAIL", json!({ "error": error.to_string() }));
            return report.finish("failed", &rest(7));
        }
    }
    report.after = Some(after);

    // 8. Smoke test.
    match smoke_test(&state).await {
        Ok(detail) => report.step(STEPS[7], "PASS", detail),
        Err(error) => {
            report.step(STEPS[7], "FAIL", json!({ "error": error.to_string() }));
            return report.finish("failed", &rest(8));
        }
    }
    report.step(STEPS[8], "PASS", Value::Null);
    let result = if changes { "upgraded" } else { "current" };
    report.finish(result, &[])
}

/// FeltDB's online backup: a consistent snapshot of the whole database,
/// bundled and manifested by FeltDB.
async fn online_backup(config: &FeltDbConfig, token: &str, output: &str) -> Result<Value, String> {
    let backup = FeltDbState::new(FeltDbConfig {
        token: token.into(),
        ..config.clone()
    })
    .map_err(|error| error.to_string())?;
    backup
        .send(
            reqwest::Method::POST,
            "/admin/backups",
            Some(&json!({ "output": output })),
        )
        .await
        .map_err(|refusal| match refusal {
            Ok(refusal) => format!(
                "FeltDB refused the backup ({} {}): {}; the backup key needs cluster:write",
                refusal.status, refusal.code, refusal.message
            ),
            Err(error) => error.to_string(),
        })
}

/// Verify a backup with FeltDB's own verifier, independently of the code
/// that wrote it. Without a verifier the backup is unverified, and the
/// upgrade does not continue.
fn verify_backup(verifier: Option<&std::path::Path>, archive: &str) -> Result<Value, String> {
    let verifier = verifier.ok_or_else(|| {
        "the backup cannot be verified: name a feltdb-server binary with --feltdb-server-bin (or FELTDB_SERVER_BIN) on a host that can read it".to_string()
    })?;
    let output = std::process::Command::new(verifier)
        .args(["backup", "verify", archive, "--json"])
        .output()
        .map_err(|error| format!("cannot run {}: {error}", verifier.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: Value = serde_json::from_str(stdout.trim()).map_err(|_| {
        format!(
            "feltdb-server backup verify did not report JSON (exit {}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
    })?;
    if !output.status.success() || report["status"] != "ok" || report["verification"] != "verified"
    {
        return Err(format!("the backup did not verify: {report}"));
    }
    Ok(json!({
        "backup_id": report["backup_id"],
        "format": report["format"],
        "files": report["files"],
        "bytes": report["bytes_written"],
        "verifier": verifier.display().to_string(),
    }))
}

/// Every collection reads and decodes as this build's records, the
/// revision reads, and a transaction commits and is undone.
async fn smoke_test(state: &FeltDbState) -> Result<Value, StateError> {
    let revision = StateStore::revision(state).await?;
    let mut decoded = serde_json::Map::new();
    for collection in Collection::ALL {
        let records = state.query(&Query::all(collection).limit(1)).await?;
        for record in &records {
            compute_state::decode_document(collection, &record.value)?;
        }
        decoded.insert(collection.name().into(), json!(records.len()));
    }
    let id = format!(
        "prov_upgrade_smoke_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default()
    );
    let value = serde_json::to_value(compute_state::ProviderRecord {
        provider_id: id.clone(),
        kind: "smoke".into(),
        endpoint: None,
        priority: 0,
        registered_by: "compute control-plane upgrade".into(),
        observed_at: chrono::Utc::now(),
    })
    .map_err(|error| StateError::Invalid(error.to_string()))?;
    let Value::Object(value) = value else {
        return Err(StateError::Invalid("a provider is an object".into()));
    };
    state
        .commit(vec![Write::Create {
            collection: Collection::Provider,
            id: id.clone(),
            value,
        }])
        .await?;
    let written = state.get(Collection::Provider, &id).await?.ok_or_else(|| {
        StateError::Invalid("the smoke-test record is not readable by its identity".into())
    })?;
    state
        .commit(vec![Write::Delete {
            collection: Collection::Provider,
            id,
            expected: Some(written.version),
        }])
        .await?;
    Ok(json!({
        "revision": revision.map(|revision| revision.value),
        "collections_decoded": decoded,
        "transaction": "committed and undone",
    }))
}
