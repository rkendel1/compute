//! Certify Compute as a consumer of FeltDB 0.11.8, against a real
//! `feltdb-server`:
//!
//! ```sh
//! FELTDB_SERVER_BIN=/path/to/feltdb-server-0.11.8 \
//! FELTDB_PREVIOUS_SERVER_BIN=/path/to/feltdb-server-0.11.7 \
//!   cargo test -p compute-state-feltdb --test consumer -- --ignored --test-threads 1
//! ```
//!
//! Access plans are asserted from what FeltDB reports it executed (rows
//! scanned, index used), not from latency.

mod common;

use std::sync::Arc;

use common::*;
use compute_state::{
    Batch, Collection, ControlState, DeploymentRecord, EnvironmentRecord, ExecutionRecord, Query,
    SnapshotDefinition, SnapshotSource, StateError, StateStore,
};
use compute_state_feltdb::{
    BackupPlan, FeltDbConfig, FeltDbState, ModelRelation, ProvisionRequest, UpgradeRequest,
    provision, provision_manifest, upgrade_control_plane,
};
use serde_json::{Value, json};

struct Authority {
    server: Server,
    data: std::path::PathBuf,
    token: String,
    config: FeltDbConfig,
}

async fn authority(tenant: &str) -> Authority {
    authority_with(tenant, None).await
}

/// A server with the Compute model (or `manifest`) provisioned.
async fn authority_with(tenant: &str, manifest: Option<&str>) -> Authority {
    let data = tempfile_dir();
    let token = create_key(&data);
    let server = start(&data);
    let request = ProvisionRequest {
        url: server.url.clone(),
        token: token.clone(),
        application_id: None,
        tenant_id: None,
        tenant_name: tenant.into(),
        environment: "production".into(),
        ca_certificate: None,
    };
    let provisioned = match manifest {
        Some(manifest) => provision_manifest(request, manifest).await,
        None => provision(request).await,
    }
    .expect("provision");
    let config = FeltDbConfig {
        url: server.url.clone(),
        token: token.clone(),
        application_id: provisioned.application_id,
        environment: "production".into(),
        ca_certificate: None,
    };
    Authority {
        server,
        data,
        token,
        config,
    }
}

async fn connect(config: &FeltDbConfig) -> Arc<FeltDbState> {
    Arc::new(FeltDbState::connect(config.clone()).await.expect("connect"))
}

fn environment(name: &str) -> EnvironmentRecord {
    EnvironmentRecord {
        name: name.into(),
        desired_state: compute_state::DesiredState::Running,
        config: Default::default(),
        policy: None,
        provider: None,
        created_at: chrono::Utc::now(),
    }
}

fn execution(project: usize, index: usize) -> ExecutionRecord {
    serde_json::from_value(json!({
        "execution_id": format!("exe_{project}_{index}"),
        "environment_id": "env_production",
        "environment": "production",
        "project_id": format!("prj_{project}"),
        "project": format!("p{project}"),
        "workload_id": format!("wl_{project}"),
        "workload": "job",
        "kind": "task",
        "status": "succeeded",
        "started_at": chrono::Utc::now() + chrono::TimeDelta::milliseconds(index as i64),
    }))
    .expect("an execution")
}

/// The model before generation 2: no `record_id`, none of the view
/// indexes generation 2 added.
fn generation_one_manifest() -> String {
    let mut manifest: Value = serde_json::from_str(compute_state_feltdb::COMPUTE_MANIFEST).unwrap();
    for collection in manifest["collections"].as_array_mut().unwrap() {
        collection["fields"]
            .as_array_mut()
            .unwrap()
            .retain(|field| field["name"] != "record_id");
    }
    let added = [
        "Execution.project_id",
        "Receipt.project_id",
        "Event.environment",
        "Event.project",
        "Event.deployment_id",
    ];
    manifest["indexes"].as_array_mut().unwrap().retain(|index| {
        let name = index["name"].as_str().unwrap_or_default();
        !name.ends_with(".record_id") && !added.contains(&name)
    });
    manifest.to_string()
}

async fn seed_executions(state: &ControlState, projects: usize, each: usize) {
    for project in 0..projects {
        let mut batch = Batch::new();
        for index in 0..each {
            batch = batch.create(
                &compute_state::ids::execution(&format!("exe_{project}_{index}")),
                &execution(project, index),
            );
        }
        state.transaction(batch).await.expect("seed");
    }
}

fn delta(before: &compute_state::AccessReport, after: &compute_state::AccessReport) -> Value {
    json!({
        "queries": after.queries - before.queries,
        "indexed": after.indexed_queries - before.indexed_queries,
        "scanned": after.scanned_queries - before.scanned_queries,
        "rows_scanned": after.rows_scanned - before.rows_scanned,
        "rows_returned": after.rows_returned - before.rows_returned,
    })
}

#[tokio::test]
#[ignore = "requires FELTDB_SERVER_BIN"]
async fn targeted_reads_are_indexed_and_bounded() {
    let authority = authority("compute-access").await;
    let store = connect(&authority.config).await;
    let state = ControlState::new(store.clone());
    let (projects, each) = (20, 25);
    seed_executions(&state, projects, each).await;

    // Identity: an index lookup of one row among 500.
    let before = store.access().unwrap();
    let id = compute_state::ids::execution("exe_7_3");
    let found = state.get::<ExecutionRecord>(&id).await.unwrap().unwrap();
    assert_eq!(
        found.value.execution_id, "exe_7_3",
        "the indexed lookup returns the record"
    );
    let after = store.access().unwrap();
    assert_eq!(
        delta(&before, &after),
        json!({"queries": 1, "indexed": 1, "scanned": 0, "rows_scanned": 1, "rows_returned": 1})
    );

    // Identity of a record that does not exist: still one index probe.
    let before = store.access().unwrap();
    assert!(
        state
            .get::<ExecutionRecord>("exe_absent")
            .await
            .unwrap()
            .is_none()
    );
    let after = store.access().unwrap();
    assert_eq!(after.rows_scanned - before.rows_scanned, 0);
    assert_eq!(after.indexed_queries - before.indexed_queries, 1);

    // A project's recent executions: its index, ordered and limited by
    // FeltDB. Only that project's rows are examined.
    let before = store.access().unwrap();
    let recent = state
        .query::<ExecutionRecord>(
            Query::all(Collection::Execution)
                .eq("environment_id", "env_production")
                .eq("project_id", "prj_4")
                .descending("started_at")
                .limit(5),
        )
        .await
        .unwrap();
    let after = store.access().unwrap();
    assert_eq!(recent.len(), 5, "a bounded list respects its limit");
    assert!(
        recent
            .iter()
            .all(|execution| execution.value.project_id == "prj_4")
    );
    assert_eq!(
        recent
            .iter()
            .map(|execution| execution.value.execution_id.clone())
            .collect::<Vec<_>>(),
        (20..25)
            .rev()
            .map(|index| format!("exe_4_{index}"))
            .collect::<Vec<_>>(),
        "newest first, ordered by FeltDB"
    );
    assert_eq!(after.indexed_queries - before.indexed_queries, 1);
    assert_eq!(
        after.rows_scanned - before.rows_scanned,
        each as u64,
        "a filtered query does not load unrelated records"
    );
    assert_eq!(after.rows_returned - before.rows_returned, 5);

    // Many identities at once: one index probe each, never a scan.
    let ids = (0..10)
        .map(|project| compute_state::ids::execution(&format!("exe_{project}_0")))
        .collect::<Vec<_>>();
    let before = store.access().unwrap();
    let many = state.get_many::<ExecutionRecord>(&ids).await.unwrap();
    let after = store.access().unwrap();
    assert_eq!(many.len(), 10);
    assert_eq!(after.scanned_queries - before.scanned_queries, 0);
    assert_eq!(after.rows_scanned - before.rows_scanned, 10);

    // The pattern this replaces, for the record: the whole collection,
    // filtered here.
    let before = store.access().unwrap();
    let everything = state.list::<ExecutionRecord>().await.unwrap();
    let after = store.access().unwrap();
    assert_eq!(everything.len(), projects * each);
    assert_eq!(
        after.rows_scanned - before.rows_scanned,
        (projects * each) as u64
    );
    let _ = std::fs::remove_dir_all(&authority.data);
}

#[tokio::test]
#[ignore = "requires FELTDB_SERVER_BIN"]
async fn snapshots_are_authorized_by_the_authority() {
    let authority = authority("compute-snapshot-auth").await;
    let state = ControlState::new(connect(&authority.config).await);
    state
        .transaction(Batch::new().create("env_one", &environment("one")))
        .await
        .unwrap();
    let definition = SnapshotDefinition::new(
        "authorized",
        vec![SnapshotSource::all(Collection::Environment)],
    );
    let (snapshot, _) = state
        .snapshot(definition.clone())
        .unwrap()
        .refresh(false)
        .await
        .unwrap();
    assert_eq!(snapshot.all(Collection::Environment).count(), 1);
    assert!(
        snapshot
            .identity
            .authority_context
            .contains(&authority.config.application_id),
        "the identity names the authority it read under"
    );
    assert!(
        !snapshot
            .identity
            .authority_context
            .contains(&authority.token)
    );

    // A key that may read the application but not its state cannot
    // materialize a snapshot of it.
    let reader = create_scoped_key(
        &authority.data,
        "no-state",
        "application:read,application:revision:read,application:environment:read",
    );
    // Keys are loaded at start: restart on the same port.
    let port = authority.server.port;
    drop(authority.server);
    let _server = start_with(&binary(), &authority.data, port);
    let limited = ControlState::new(Arc::new(
        FeltDbState::new(FeltDbConfig {
            token: reader,
            ..authority.config.clone()
        })
        .unwrap(),
    ));
    let refused = limited.snapshot(definition).unwrap().refresh(false).await;
    assert!(
        matches!(refused, Err(StateError::Unavailable(ref message)) if message.contains("refused Compute's credentials") && message.contains("state:read")),
        "{refused:?}"
    );
    let _ = std::fs::remove_dir_all(&authority.data);
}

#[tokio::test]
#[ignore = "requires FELTDB_SERVER_BIN"]
async fn an_outage_is_unavailability_and_recovery_is_automatic() {
    let authority = authority("compute-outage").await;
    let store = connect(&authority.config).await;
    let state = ControlState::new(store.clone());
    state
        .transaction(Batch::new().create("env_before", &environment("before")))
        .await
        .unwrap();
    let handle = state
        .snapshot(SnapshotDefinition::new(
            "outage",
            vec![SnapshotSource::all(Collection::Environment)],
        ))
        .unwrap();
    let (before, _) = handle.refresh(false).await.unwrap();

    let port = authority.server.port;
    drop(authority.server);
    // Reads and writes both fail as unavailable; nothing is fabricated.
    let read = state.get::<EnvironmentRecord>("env_before").await;
    assert!(matches!(read, Err(StateError::Unavailable(_))), "{read:?}");
    let write = state
        .transaction(Batch::new().create("env_during", &environment("during")))
        .await;
    assert!(
        matches!(write, Err(StateError::Unavailable(_))),
        "{write:?}"
    );
    assert!(matches!(
        handle.refresh(true).await,
        Err(StateError::Unavailable(_))
    ));
    assert_eq!(
        handle.current().unwrap().identity.id,
        before.identity.id,
        "a failed build leaves the published snapshot in place"
    );
    assert_eq!(store.access().unwrap().connection, "unreachable");

    // The same authority returns at the same URL.
    let _server = start_with(&binary(), &authority.data, port);
    handle.invalidate();
    let (after, _) = handle.refresh(false).await.expect("rebuilt after recovery");
    assert_eq!(after.all(Collection::Environment).count(), 1);
    assert_eq!(store.access().unwrap().connection, "connected");
    state
        .transaction(Batch::new().create("env_after", &environment("after")))
        .await
        .expect("mutations resume");
    assert!(
        state
            .get::<EnvironmentRecord>("env_during")
            .await
            .unwrap()
            .is_none(),
        "nothing written during the outage"
    );
    let _ = std::fs::remove_dir_all(&authority.data);
}

#[tokio::test]
#[ignore = "requires FELTDB_SERVER_BIN"]
async fn the_upgrade_backs_up_migrates_and_verifies() {
    // State written by a generation-1 Compute.
    let authority = authority_with("compute-upgrade-flow", Some(&generation_one_manifest())).await;
    // This build refuses to run on the older model rather than write what
    // it cannot hold.
    let refused = FeltDbState::connect(authority.config.clone()).await;
    assert!(
        matches!(refused, Err(StateError::Invalid(ref message)) if message.contains("compute control-plane upgrade")),
        "{:?}",
        refused.err()
    );
    // State a generation-1 controller wrote: no record_id.
    legacy_write(
        &authority,
        "Environment",
        "env_legacy",
        serde_json::to_value(environment("legacy")).unwrap(),
    )
    .await;
    legacy_write(&authority, "Deployment", "dep_legacy", legacy_deployment()).await;

    // The backup needs cluster:write, which Compute's own key lacks.
    let backup_key = create_scoped_key(&authority.data, "backup", "cluster:write");
    let port = authority.server.port;
    drop(authority.server);
    let _server = start_with(&binary(), &authority.data, port);

    // A dry run inspects and changes nothing.
    let planned = upgrade_control_plane(UpgradeRequest {
        config: authority.config.clone(),
        backup: BackupPlan::Skip {
            reason: "dry run".into(),
        },
        dry_run: true,
    })
    .await;
    assert_eq!(planned.result, "planned", "{planned:#?}");
    assert_eq!(
        planned.before.as_ref().unwrap().comparison.relation,
        ModelRelation::Older
    );

    // Without a verifier the backup is unverified, and nothing changes.
    let archive = authority.data.join("backup-unverified");
    let unverified = upgrade_control_plane(UpgradeRequest {
        config: authority.config.clone(),
        backup: BackupPlan::Online {
            output: archive.display().to_string(),
            token: backup_key.clone(),
            verifier: None,
        },
        dry_run: false,
    })
    .await;
    assert_eq!(unverified.result, "failed");
    let verify = unverified
        .steps
        .iter()
        .find(|step| step.name == "backup_verify")
        .unwrap();
    assert_eq!(verify.status, "FAIL");
    assert!(
        unverified
            .steps
            .iter()
            .filter(|step| step.name == "apply_model")
            .all(|step| step.status == "NOT_RUN")
    );

    // FeltDB's online backup: FeltDB 0.11.8 cannot verify it for
    // application state, so the upgrade stops before changing anything.
    let online = upgrade_control_plane(UpgradeRequest {
        config: authority.config.clone(),
        backup: BackupPlan::Online {
            output: authority.data.join("online-backup").display().to_string(),
            token: backup_key,
            verifier: Some(binary()),
        },
        dry_run: false,
    })
    .await;
    let online_verify = online
        .steps
        .iter()
        .find(|step| step.name == "backup_verify")
        .cloned();
    if online.result != "upgraded" {
        assert_eq!(online.result, "failed", "{online:#?}");
        assert!(
            online
                .steps
                .iter()
                .filter(|step| step.name == "apply_model")
                .all(|step| step.status == "NOT_RUN")
        );
    }

    // FeltDB's offline backup of the stopped authority, verified again by
    // the upgrade before it changes anything.
    let port = _server.port;
    drop(_server);
    let archive = authority.data.join("backup");
    let created = std::process::Command::new(binary())
        .args(["backup", "create"])
        .arg(&archive)
        .arg("--data")
        .arg(authority.data.join("state.log"))
        .arg("--keys")
        .arg(authority.data.join("keys.json"))
        .arg("--audit")
        .arg(authority.data.join("audit.log"))
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let _server = start_with(&binary(), &authority.data, port);
    let report = upgrade_control_plane(UpgradeRequest {
        config: authority.config.clone(),
        backup: BackupPlan::Archive {
            archive: archive.display().to_string(),
            verifier: Some(binary()),
        },
        dry_run: false,
    })
    .await;
    println!("online backup verification: {online_verify:?}");
    assert_eq!(report.result, "upgraded", "{report:#?}");
    for step in &report.steps {
        assert_eq!(step.status, "PASS", "{step:?}");
    }
    assert_eq!(
        report.after.as_ref().unwrap().comparison.relation,
        ModelRelation::Current
    );
    std::fs::write(
        authority.data.join("upgrade-report.json"),
        serde_json::to_string_pretty(&report).unwrap(),
    )
    .unwrap();
    if let Ok(path) = std::env::var("COMPUTE_CERTIFICATION_OUT") {
        std::fs::write(
            std::path::Path::new(&path).join("upgrade-report.json"),
            serde_json::to_string_pretty(&report).unwrap(),
        )
        .unwrap();
    }

    // Old state reads, through the index, and decodes with safe defaults.
    let store = connect(&authority.config).await;
    let state = ControlState::new(store.clone());
    let before = store.access().unwrap();
    let legacy = state
        .get::<DeploymentRecord>("dep_legacy")
        .await
        .unwrap()
        .expect("legacy record found by identity");
    assert_eq!(legacy.value.project, "legacy");
    assert_eq!(
        store.access().unwrap().indexed_queries - before.indexed_queries,
        1
    );
    assert!(
        state
            .get::<EnvironmentRecord>("env_legacy")
            .await
            .unwrap()
            .is_some()
    );

    // Running it again is a no-op.
    let again = upgrade_control_plane(UpgradeRequest {
        config: authority.config.clone(),
        backup: BackupPlan::Skip {
            reason: "current".into(),
        },
        dry_run: false,
    })
    .await;
    assert_eq!(again.result, "current", "{again:#?}");
    let _ = std::fs::remove_dir_all(&authority.data);
}

/// Write a record as a generation-1 controller did: through FeltDB's
/// transaction API, with no `record_id`.
async fn legacy_write(authority: &Authority, collection: &str, id: &str, value: Value) {
    let http = reqwest::Client::new();
    let revision = http
        .get(format!(
            "{}/v1/application?application_id={}&environment=production",
            authority.config.url, authority.config.application_id
        ))
        .bearer_auth(&authority.token)
        .header("FeltDB-Protocol", "1")
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["revision_id"]
        .as_str()
        .unwrap()
        .to_string();
    let response = http
        .post(format!("{}/v1/transactions", authority.config.url))
        .bearer_auth(&authority.token)
        .header("FeltDB-Protocol", "1")
        .json(&json!({
            "application_id": authority.config.application_id,
            "environment": "production",
            "revision_id": revision,
            "transaction": {
                "transaction_id": format!("legacy-{collection}-{id}"),
                "tenant_id": "", "application_id": "", "revision_id": "", "schema_version": 0,
                "authorization": { "subject": "", "tenant_id": "", "application_id": "", "revision_id": "", "capabilities": [] },
                "operations": [{ "kind": "insert", "collection": collection, "id": id, "value": value }],
            },
        }))
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "{}",
        response.text().await.unwrap()
    );
}

/// A deployment as a generation-1 controller wrote it: every optional
/// field this build added later is absent.
fn legacy_deployment() -> Value {
    json!({
        "environment_id": "env_production",
        "environment": "production",
        "project_id": "prj_legacy",
        "project": "legacy",
        "revision_id": "rev_legacy",
        "revision": "v1",
        "revision_digest": "sha256:legacy",
        "status": "complete",
        "workloads": [],
        "receipt_ids": [],
        "config": {},
        "created_at": "2026-09-01T00:00:00Z",
        "updated_at": "2026-09-01T00:00:00Z",
    })
}

#[tokio::test]
#[ignore = "requires FELTDB_SERVER_BIN"]
async fn a_newer_model_is_never_downgraded() {
    // A newer Compute installed a model with something this build lacks.
    let mut newer: Value = serde_json::from_str(compute_state_feltdb::COMPUTE_MANIFEST).unwrap();
    let collections = newer["collections"].as_array_mut().unwrap();
    let mut extra = collections[0].clone();
    extra["name"] = json!("FutureThing");
    collections.push(extra);
    let mut policy = newer["policies"][0].clone();
    policy["name"] = json!("FutureThing");
    policy["resource"] = json!("FutureThing");
    newer["policies"].as_array_mut().unwrap().push(policy);
    let authority = authority_with("compute-downgrade", Some(&newer.to_string())).await;

    let report = upgrade_control_plane(UpgradeRequest {
        config: authority.config.clone(),
        backup: BackupPlan::Skip {
            reason: "refusal test".into(),
        },
        dry_run: false,
    })
    .await;
    assert_eq!(report.result, "refused", "{report:#?}");
    assert_eq!(
        report.before.as_ref().unwrap().comparison.relation,
        ModelRelation::Newer
    );
    assert!(
        report
            .steps
            .iter()
            .any(|step| step.name == "refuse_unsafe_downgrade" && step.status == "FAIL")
    );

    // Provisioning directly refuses too.
    let direct = provision(ProvisionRequest {
        url: authority.config.url.clone(),
        token: authority.token.clone(),
        application_id: Some(authority.config.application_id.clone()),
        tenant_id: None,
        tenant_name: String::new(),
        environment: "production".into(),
        ca_certificate: None,
    })
    .await;
    assert!(
        matches!(direct, Err(StateError::Invalid(ref message)) if message.contains("downgrade")),
        "{direct:?}"
    );
    let _ = std::fs::remove_dir_all(&authority.data);
}

/// Everything a query of every collection returns, keyed for comparison.
async fn everything(store: &FeltDbState) -> std::collections::BTreeMap<String, Value> {
    let mut all = std::collections::BTreeMap::new();
    for collection in Collection::ALL {
        for record in store.query(&Query::all(collection)).await.unwrap() {
            all.insert(
                format!("{}/{}", collection.name(), record.id),
                Value::Object(record.value),
            );
        }
    }
    all
}

#[tokio::test]
#[ignore = "requires FELTDB_SERVER_BIN"]
async fn a_feltdb_backup_restores_compute_state_exactly() {
    let authority = authority("compute-backup").await;
    let store = connect(&authority.config).await;
    let state = ControlState::new(store.clone());
    seed_executions(&state, 3, 10).await;
    let mut batch = Batch::new();
    for sequence in 1..=20u64 {
        batch = batch.create(
            &compute_state::ids::event(sequence),
            &compute_state::EventRecord {
                sequence,
                kind: "certification".into(),
                at: chrono::Utc::now(),
                environment: Some("production".into()),
                project: Some("p1".into()),
                workload: None,
                deployment_id: None,
                execution_id: None,
                message: format!("event {sequence}"),
                data: json!({}),
            },
        );
    }
    let audit = compute_state::AuditRecord {
        request_id: "req_1".into(),
        operator_id: "op_admin".into(),
        credential_id: None,
        operation: "environment.create".into(),
        resource: "environment".into(),
        resource_id: Some("production".into()),
        result: "allowed".into(),
        status: 200,
        error_kind: None,
        detail: serde_json::Map::new(),
        at: chrono::Utc::now(),
    };
    batch = batch.create("aud_1", &audit);
    state.transaction(batch).await.unwrap();
    let original = everything(&store).await;
    let original_revision = store.revision().await.unwrap().unwrap();

    // FeltDB's online backup, verified by FeltDB's own verifier. Recorded,
    // not asserted: 0.11.8 cannot verify an online backup of application
    // state (see the certification report).
    let backup_key = create_scoped_key(&authority.data, "backup", "cluster:write");
    let port = authority.server.port;
    drop(authority.server);
    let server = start_with(&binary(), &authority.data, port);
    let online = authority.data.join("online-backup");
    let response = reqwest::Client::new()
        .post(format!("{}/admin/backups", server.url))
        .bearer_auth(&backup_key)
        .json(&json!({ "output": online }))
        .send()
        .await
        .unwrap();
    let online_created = response.status().is_success();
    let online_verify = std::process::Command::new(binary())
        .args(["backup", "verify"])
        .arg(&online)
        .arg("--json")
        .output()
        .unwrap();
    let online_verified: Value =
        serde_json::from_slice(&online_verify.stdout).unwrap_or(Value::Null);
    drop(server);

    // FeltDB's offline backup of the stopped authority: created and
    // verified by FeltDB, restored by FeltDB.
    let archive = authority.data.join("backup");
    let create = std::process::Command::new(binary())
        .args(["backup", "create"])
        .arg(&archive)
        .arg("--data")
        .arg(authority.data.join("state.log"))
        .arg("--keys")
        .arg(authority.data.join("keys.json"))
        .arg("--audit")
        .arg(authority.data.join("audit.log"))
        .arg("--json")
        .output()
        .unwrap();
    let created: Value = serde_json::from_slice(&create.stdout).unwrap_or(Value::Null);
    assert_eq!(
        created["status"],
        "ok",
        "{created} {}",
        String::from_utf8_lossy(&create.stderr)
    );
    let verify = std::process::Command::new(binary())
        .args(["backup", "verify"])
        .arg(&archive)
        .arg("--json")
        .output()
        .unwrap();
    let verified: Value = serde_json::from_slice(&verify.stdout).unwrap();
    assert_eq!(verified["status"], "ok", "{verified}");
    assert_eq!(verified["verification"], "verified");

    let restored = authority.data.join("restored");
    let restore = std::process::Command::new(binary())
        .args(["backup", "restore", "--archive"])
        .arg(&archive)
        .arg("--output")
        .arg(&restored)
        .output()
        .unwrap();
    assert!(
        restore.status.success(),
        "{}",
        String::from_utf8_lossy(&restore.stderr)
    );
    // The restored files, served as an authority of their own: every
    // record, event, execution, and audit record, with indexes working.
    let files = restored.join("files");
    let keys = files.join("keys.json");
    if !keys.exists() {
        let source = if files.join("api-keys.json").exists() {
            files.join("api-keys.json")
        } else {
            authority.data.join("keys.json")
        };
        std::fs::copy(source, &keys).unwrap();
    }
    // FeltDB 0.11.8 restores its sidecars under their backup labels, while
    // a server started on `files/state.log` reads them as `state.*.json`:
    // the operator renames them (recorded in the certification report).
    let mut renamed = vec![];
    for (label, served) in [
        ("applications.json", "state.applications.json"),
        ("platform.json", "state.platform.json"),
    ] {
        if files.join(label).exists() && !files.join(served).exists() {
            std::fs::rename(files.join(label), files.join(served)).unwrap();
            renamed.push(format!("{label} -> {served}"));
        }
    }
    let restored_server = start_with(&binary(), &files, 0);
    let restored_store = connect(&FeltDbConfig {
        url: restored_server.url.clone(),
        ..authority.config.clone()
    })
    .await;
    let restored_records = everything(&restored_store).await;
    assert_eq!(restored_records, original, "record-by-record equivalence");
    let count = |prefix: &str| {
        restored_records
            .keys()
            .filter(|key| key.starts_with(prefix))
            .count()
    };
    assert_eq!(count("Execution/"), 30);
    assert_eq!(count("Event/"), 20);
    assert_eq!(count("Audit/"), 1);
    let before = restored_store.access().unwrap();
    let execution = ControlState::new(restored_store.clone())
        .get::<ExecutionRecord>(&compute_state::ids::execution("exe_2_9"))
        .await
        .unwrap();
    assert!(execution.is_some());
    let after = restored_store.access().unwrap();
    assert_eq!(
        after.indexed_queries - before.indexed_queries,
        1,
        "indexes work after restore"
    );
    assert_eq!(after.rows_scanned - before.rows_scanned, 1);
    let evidence = json!({
        "online_backup": {
            "created": online_created,
            "verification": online_verified,
        },
        "offline_backup": created,
        "backup": verified,
        "renamed_for_serving": renamed,
        "restored_files": std::fs::read_dir(&files).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect::<Vec<_>>(),
        "records_compared": original.len(),
        "executions": count("Execution/"),
        "events": count("Event/"),
        "audit": count("Audit/"),
        "revision_before_backup": original_revision.value,
    });
    println!("backup evidence: {evidence}");
    if let Ok(path) = std::env::var("COMPUTE_CERTIFICATION_OUT") {
        std::fs::write(
            std::path::Path::new(&path).join("backup-restore.json"),
            serde_json::to_string_pretty(&evidence).unwrap(),
        )
        .unwrap();
    }
    drop(restored_server);
    let _ = std::fs::remove_dir_all(&authority.data);
}

#[tokio::test]
#[ignore = "requires FELTDB_SERVER_BIN and FELTDB_PREVIOUS_SERVER_BIN"]
async fn state_written_by_the_previous_server_opens_on_this_one() {
    let Some(previous) = previous_binary() else {
        panic!("FELTDB_PREVIOUS_SERVER_BIN must name the previous feltdb-server");
    };
    let data = tempfile_dir();
    let token = create_key(&data);
    let old = start_with(&previous, &data, 0);
    let provisioned = provision(ProvisionRequest {
        url: old.url.clone(),
        token: token.clone(),
        application_id: None,
        tenant_id: None,
        tenant_name: "compute-previous".into(),
        environment: "production".into(),
        ca_certificate: None,
    })
    .await
    .expect("provision on the previous server");
    let config = FeltDbConfig {
        url: old.url.clone(),
        token,
        application_id: provisioned.application_id,
        environment: "production".into(),
        ca_certificate: None,
    };
    let state = ControlState::new(connect(&config).await);
    seed_executions(&state, 2, 5).await;
    let written = everything(&*connect(&config).await).await;
    let port = old.port;
    drop(old);

    // Forward: the certified server opens it unchanged.
    let new = start_with(&binary(), &data, port);
    let store = connect(&config).await;
    assert_eq!(everything(&store).await, written, "every record, unchanged");
    compute_state::conformance::check(store.clone(), None).await;
    let after_new = everything(&store).await;
    drop(new);

    // Back: the previous server opens what the certified one wrote.
    let old_again = start_with(&previous, &data, port);
    let store = connect(&config).await;
    let rolled_back = everything(&store).await;
    assert_eq!(
        rolled_back, after_new,
        "the previous server reads the certified server's state"
    );
    drop(old_again);
    let _ = std::fs::remove_dir_all(data);
}

/// FeltDB's own cost per request, by the size of the state it holds:
/// `/health` (no state), `/v1/state/version` (a revision read), and an
/// indexed identity lookup, timed from the client, beside the execution
/// time FeltDB reports for the query itself. FeltDB-only: Compute does
/// nothing but send the request. Measures; asserts nothing about time.
#[tokio::test]
#[ignore = "requires FELTDB_SERVER_BIN; measures"]
async fn feltdb_request_cost_by_state_size() {
    let authority = authority("compute-request-cost").await;
    let store = connect(&authority.config).await;
    let state = ControlState::new(store.clone());
    let http = reqwest::Client::new();
    let revision = http
        .get(format!(
            "{}/v1/application?application_id={}&environment=production",
            authority.config.url, authority.config.application_id
        ))
        .bearer_auth(&authority.token)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["revision_id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut rows = vec![];
    let mut written = 0usize;
    for size in [0usize, 500, 1000, 2000, 4000] {
        while written < size {
            let mut batch = Batch::new();
            for index in written..(written + 200).min(size) {
                batch = batch.create(
                    &compute_state::ids::execution(&format!("cost_{index}")),
                    &execution(index % 20, index),
                );
            }
            written = (written + 200).min(size);
            state.transaction(batch).await.unwrap();
        }
        let time = |started: std::time::Instant| started.elapsed().as_secs_f64() * 1000.0;
        let median = |mut values: Vec<f64>| {
            values.sort_by(|a, b| a.partial_cmp(b).unwrap());
            (values[values.len() / 2] * 100.0).round() / 100.0
        };
        let (mut health, mut version, mut lookup, mut server) = (vec![], vec![], vec![], vec![]);
        for _ in 0..15 {
            let started = std::time::Instant::now();
            http.get(format!("{}/health", authority.config.url))
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
            health.push(time(started));
            let started = std::time::Instant::now();
            http.get(format!(
                "{}/v1/state/version?application_id={}&environment=production&revision_id={}",
                authority.config.url, authority.config.application_id, revision
            ))
            .bearer_auth(&authority.token)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
            version.push(time(started));
            let started = std::time::Instant::now();
            let response = http
                .post(format!("{}/v1/query", authority.config.url))
                .bearer_auth(&authority.token)
                .json(&json!({
                    "application_id": authority.config.application_id,
                    "environment": "production",
                    "revision_id": revision,
                    "query": { "collection": "Execution", "limit": 1,
                               "filter": { "operator": "eq", "field": "record_id", "value": "exe_absent" } },
                }))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap();
            lookup.push(time(started));
            server.push(
                response["plan"]["execution_timing"]["total_ms"]
                    .as_f64()
                    .or_else(|| response["plan"]["timing"]["total_ms"].as_f64())
                    .unwrap_or(-1.0),
            );
        }
        let row = json!({
            "executions": size,
            "revision": StateStore::revision(&*store).await.unwrap().unwrap().value,
            "health_p50_ms": median(health),
            "state_version_p50_ms": median(version),
            "indexed_lookup_p50_ms": median(lookup),
            "indexed_lookup_server_reported_p50_ms": median(server),
        });
        println!("{row}");
        rows.push(row);
    }
    if let Ok(path) = std::env::var("COMPUTE_CERTIFICATION_OUT") {
        std::fs::write(
            std::path::Path::new(&path).join("feltdb-request-cost.json"),
            serde_json::to_string_pretty(&json!({
                "format": "compute.feltdb-request-cost@1",
                "scope": "feltdb-only",
                "samples_per_size": 15,
                "rows": rows,
            }))
            .unwrap(),
        )
        .unwrap();
    }
    let _ = std::fs::remove_dir_all(&authority.data);
}
