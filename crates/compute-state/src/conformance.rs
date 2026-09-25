//! The semantics every `StateStore` must provide, as executable checks.
//!
//! Backends run `check` in their own tests. It uses only typed control
//! records, so it also runs against schema-enforcing backends such as
//! FeltDB, and it scopes everything it writes by a fresh run nonce, so it
//! can run repeatedly against durable state.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::Utc;
use serde_json::json;

use crate::control::{Batch, ControlState, Stored};
use crate::model::*;
use crate::store::{Collection, Query, StateError, StateStore};

fn nonce() -> String {
    format!(
        "{:x}",
        Utc::now().timestamp_nanos_opt().unwrap_or_default() as u64 ^ u64::from(std::process::id())
    )
}

fn event(nonce: &str, sequence: u64) -> EventRecord {
    EventRecord {
        sequence,
        kind: format!("conformance.{nonce}"),
        at: Utc::now(),
        environment: Some("conformance".into()),
        project: None,
        workload: None,
        deployment_id: None,
        execution_id: None,
        message: format!("event {sequence}"),
        data: json!({ "sequence": sequence }),
    }
}

fn environment(name: &str) -> EnvironmentRecord {
    EnvironmentRecord {
        name: name.into(),
        desired_state: DesiredState::Running,
        config: BTreeMap::from([("LOG_LEVEL".into(), "info".into())]),
        policy: Some(json!({ "version": 1, "minimum_isolation": "process" })),
        provider: Some("local".into()),
        created_at: Utc::now(),
    }
}

/// Run every check. `reopen`, when given, opens the same state again as a
/// new process would, to prove durability.
pub async fn check(
    store: Arc<dyn StateStore>,
    reopen: Option<&(dyn Fn() -> Arc<dyn StateStore> + Send + Sync)>,
) {
    let state = ControlState::new(store);
    let run = nonce();
    let env_id = format!("env_conf{run}");

    // Create, read, and refuse to create twice.
    let original = environment(&format!("conf-{run}"));
    state
        .transaction(Batch::new().create(&env_id, &original))
        .await
        .expect("create");
    let stored = state
        .get::<EnvironmentRecord>(&env_id)
        .await
        .expect("get")
        .expect("created document is readable");
    assert_eq!(stored.value, original, "documents round-trip exactly");
    assert!(stored.version > 0);
    let duplicate = state
        .transaction(Batch::new().create(&env_id, &original))
        .await;
    assert!(
        matches!(duplicate, Err(StateError::Conflict { .. })),
        "a second create is a conflict: {duplicate:?}"
    );

    // Replace clears optional fields and advances the version; a stale
    // version is refused.
    let mut changed = original.clone();
    changed.desired_state = DesiredState::Stopped;
    changed.policy = None;
    changed.provider = None;
    changed.config.clear();
    state
        .transaction(Batch::new().replace(&stored, &changed))
        .await
        .expect("replace");
    let replaced = state
        .get::<EnvironmentRecord>(&env_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replaced.value, changed, "replace is total, not a merge");
    assert!(replaced.version > stored.version, "versions increase");
    let stale = state
        .transaction(Batch::new().replace(&stored, &original))
        .await;
    assert!(
        matches!(stale, Err(StateError::Precondition { .. })),
        "a stale replace is refused: {stale:?}"
    );
    // Update merges: given fields change, others keep their values.
    state
        .transaction(Batch::new().update(&replaced, json!({ "provider": "edge" })))
        .await
        .expect("update");
    let updated = state
        .get::<EnvironmentRecord>(&env_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.value.provider.as_deref(), Some("edge"));
    assert_eq!(
        updated.value.desired_state,
        DesiredState::Stopped,
        "unnamed fields keep their values"
    );
    assert!(updated.version > replaced.version);
    let stale_update = state
        .transaction(Batch::new().update(&replaced, json!({ "provider": "x" })))
        .await;
    assert!(
        matches!(stale_update, Err(StateError::Precondition { .. })),
        "a stale update is refused: {stale_update:?}"
    );
    let missing = Stored {
        id: format!("env_missing{run}"),
        version: 1,
        value: original.clone(),
    };
    let absent = state
        .transaction(Batch::new().replace(&missing, &original))
        .await;
    assert!(absent.is_err(), "replacing a missing document fails");

    // A batch is atomic: one failing write commits nothing.
    let other_id = format!("env_other{run}");
    let failed = state
        .transaction(
            Batch::new()
                .create(&other_id, &environment(&format!("other-{run}")))
                .create(&env_id, &original),
        )
        .await;
    assert!(failed.is_err(), "the batch fails");
    assert!(
        state
            .get::<EnvironmentRecord>(&other_id)
            .await
            .unwrap()
            .is_none(),
        "no write of a failed batch is visible"
    );

    // Queries: equality, range, ordering, limit.
    let mut batch = Batch::new();
    for sequence in 1..=5 {
        batch = batch.create(&format!("evt_conf{run}_{sequence}"), &event(&run, sequence));
    }
    state.transaction(batch).await.expect("create events");
    let kind = format!("conformance.{run}");
    let after_two = state
        .query::<EventRecord>(
            Query::all(Collection::Event)
                .eq("kind", kind.clone())
                .gt("sequence", 2)
                .ascending("sequence"),
        )
        .await
        .unwrap();
    assert_eq!(
        after_two
            .iter()
            .map(|event| event.value.sequence)
            .collect::<Vec<_>>(),
        vec![3, 4, 5]
    );
    let newest = state
        .query::<EventRecord>(
            Query::all(Collection::Event)
                .eq("kind", kind.clone())
                .descending("sequence")
                .limit(2),
        )
        .await
        .unwrap();
    assert_eq!(
        newest
            .iter()
            .map(|event| event.value.sequence)
            .collect::<Vec<_>>(),
        vec![5, 4]
    );

    // Collections are independent namespaces.
    let project = ProjectRecord {
        name: format!("conf-{run}"),
        source: None,
        created_at: Utc::now(),
    };
    state
        .transaction(Batch::new().create(&env_id, &project))
        .await
        .expect("the same ID in another collection is independent");

    // Delete with a fence.
    let current = state
        .get::<EnvironmentRecord>(&env_id)
        .await
        .unwrap()
        .unwrap();
    let stale_delete = state.transaction(Batch::new().delete(&stored)).await;
    assert!(stale_delete.is_err(), "a stale delete is refused");
    state
        .transaction(Batch::new().delete(&current))
        .await
        .expect("delete");
    assert!(
        state
            .get::<EnvironmentRecord>(&env_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        state.get::<ProjectRecord>(&env_id).await.unwrap().is_some(),
        "deleting in one collection leaves the other"
    );

    // Every record type round-trips.
    round_trip_every_record(&state, &run).await;

    // Artifacts larger than one chunk round-trip, verified, idempotently.
    let artifacts = crate::artifacts::StateArtifacts::new(state.clone());
    let bytes = (0..(crate::artifacts::StateArtifacts::CHUNK_BYTES * 2 + 1000))
        .map(|index| (index as u64 ^ run.len() as u64).wrapping_mul(2654435761) as u8)
        .chain(run.bytes())
        .collect::<Vec<_>>();
    use crate::artifacts::ArtifactStore as _;
    let digest = artifacts.put("bundle", &bytes).await.expect("put artifact");
    assert_eq!(digest, crate::artifacts::digest(&bytes));
    assert_eq!(
        artifacts.put("bundle", &bytes).await.unwrap(),
        digest,
        "put is idempotent"
    );
    assert_eq!(
        artifacts.get(&digest).await.unwrap().as_deref(),
        Some(&bytes[..])
    );
    assert!(
        artifacts
            .get(&crate::artifacts::digest(b"absent"))
            .await
            .unwrap()
            .is_none()
    );

    // Durability.
    if let Some(reopen) = reopen {
        let reopened = ControlState::new(reopen());
        let events = reopened
            .query::<EventRecord>(Query::all(Collection::Event).eq("kind", kind))
            .await
            .unwrap();
        assert_eq!(events.len(), 5, "state survives reopening");
        assert!(
            reopened
                .get::<ProjectRecord>(&env_id)
                .await
                .unwrap()
                .is_some()
        );
    }
}

async fn round_trip<T: Document + PartialEq + std::fmt::Debug>(
    state: &ControlState,
    id: &str,
    value: T,
) {
    state
        .transaction(Batch::new().create(id, &value))
        .await
        .unwrap_or_else(|error| panic!("create {}: {error}", T::COLLECTION.name()));
    let stored = state
        .get::<T>(id)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("{} is readable", T::COLLECTION.name()));
    assert_eq!(stored.value, value, "{} round-trips", T::COLLECTION.name());
}

async fn round_trip_every_record(state: &ControlState, run: &str) {
    let now = Utc::now();
    let workload = RevisionWorkload {
        name: "api".into(),
        kind: WorkloadKind::Service,
        bundle_id: "sha256:0".into(),
        artifact: "sha256:3".into(),
        workload_identity: "sha256:1".into(),
        runtime: "python".into(),
        runtime_version: Some("3.13.15".into()),
        dependency: Some("sha256:capsule".into()),
        distribution: None,
        readiness: Some(Readiness {
            check: ReadinessCheck::Http,
            port: Some("http".into()),
            path: Some("/healthz".into()),
            task: None,
            timeout_ms: 30_000,
            interval_ms: 250,
        }),
        ports: vec![PortSpec {
            name: "http".into(),
            port: 8000,
        }],
        restart: RestartPolicy::OnFailure,
        desired_state: DesiredState::Running,
    };
    round_trip(
        state,
        &format!("rev_conf{run}"),
        ProjectRevisionRecord {
            project_id: "prj_conf".into(),
            project: "conf".into(),
            revision: "abc123".into(),
            revision_digest: "sha256:2".into(),
            source: Some("/src".into()),
            workloads: vec![workload],
            created_at: now,
        },
    )
    .await;
    round_trip(
        state,
        &format!("ep_conf{run}"),
        EnvironmentProjectRecord {
            environment_id: "env_conf".into(),
            environment: "conf".into(),
            project_id: "prj_conf".into(),
            project: "conf".into(),
            desired_state: DesiredState::Running,
            config: BTreeMap::from([("A".into(), "1".into())]),
            revision_id: Some("rev_conf".into()),
            deployment_id: Some("dep_conf".into()),
            created_at: now,
            updated_at: now,
        },
    )
    .await;
    round_trip(
        state,
        &format!("dep_conf{run}"),
        DeploymentRecord {
            environment_id: "env_conf".into(),
            environment: "conf".into(),
            project_id: "prj_conf".into(),
            project: "conf".into(),
            revision_id: "rev_conf".into(),
            revision: "abc123".into(),
            revision_digest: "sha256:2".into(),
            status: DeploymentStatus::Complete,
            promoted_from: Some("dep_prev".into()),
            previous: None,
            workloads: vec![DeploymentWorkload {
                name: "api".into(),
                kind: WorkloadKind::Service,
                bundle_id: "sha256:0".into(),
                admitted: true,
                policy_id: Some("sha256:p".into()),
                admission_id: Some("sha256:a".into()),
                placement_id: Some("sha256:l".into()),
                provider: Some("local".into()),
                reasons: vec![],
                endpoints: vec![PortBinding {
                    name: "http".into(),
                    logical: 8000,
                    host: 20001,
                }],
            }],
            failure: None,
            receipt_ids: vec!["sha256:r".into()],
            old_revision: Some("abc122".into()),
            config_digest: Some("sha256:c".into()),
            config: BTreeMap::from([("A".into(), "1".into())]),
            readiness_result: Some(json!({ "api": { "ready": true, "check": "http" } })),
            network_result: Some(json!({ "endpoint": "ok" })),
            traffic_switch_result: Some(json!([{ "endpoint": "conf/conf/api/http" }])),
            rollback_reason: None,
            receipt: Some("sha256:dr".into()),
            status_since: Some(now),
            completed_at: Some(now),
            created_at: now,
            updated_at: now,
        },
    )
    .await;
    round_trip(
        state,
        &format!("wl_conf{run}"),
        WorkloadRecord {
            environment_id: "env_conf".into(),
            environment: "conf".into(),
            project_id: "prj_conf".into(),
            project: "conf".into(),
            name: "api".into(),
            kind: WorkloadKind::Service,
            desired_state: DesiredState::Running,
            restart: RestartPolicy::Never,
            bundle_id: "sha256:0".into(),
            workload_identity: "sha256:1".into(),
            runtime: "python".into(),
            ports: vec![PortBinding {
                name: "http".into(),
                logical: 8000,
                host: 20000,
            }],
            deployment_id: "dep_conf".into(),
        },
    )
    .await;
    round_trip(
        state,
        &format!("exec_conf{run}"),
        ExecutionRecord {
            execution_id: format!("exec_conf{run}"),
            environment_id: "env_conf".into(),
            environment: "conf".into(),
            project_id: "prj_conf".into(),
            project: "conf".into(),
            workload_id: "wl_conf".into(),
            workload: "migrate".into(),
            kind: WorkloadKind::Task,
            deployment_id: Some("dep_conf".into()),
            status: "completed".into(),
            exit_code: Some(0),
            started_at: now,
            finished_at: Some(now),
            receipt_id: Some("sha256:r".into()),
            policy_id: Some("sha256:p".into()),
            admission_id: Some("sha256:a".into()),
            placement_id: Some("sha256:l".into()),
            provider: Some("local".into()),
            error: None,
        },
    )
    .await;
    round_trip(
        state,
        &format!("rcpt_conf{run}"),
        ReceiptRecord {
            receipt_id: "sha256:r".into(),
            execution_id: format!("exec_conf{run}"),
            environment_id: "env_conf".into(),
            project_id: "prj_conf".into(),
            workload_id: "wl_conf".into(),
            deployment_id: None,
            policy_id: Some("sha256:p".into()),
            admission_id: Some("sha256:a".into()),
            artifact_digest: Some("sha256:d".into()),
            created_at: now,
        },
    )
    .await;
    round_trip(
        state,
        &format!("svc_conf{run}"),
        ServiceRecord {
            name: format!("laya-{run}"),
            capabilities: vec!["llm.generate@1".into()],
            provider: "local".into(),
            environment: Some("prod".into()),
            project: Some("laya".into()),
            workload: Some("api".into()),
            endpoint: Some("http://127.0.0.1:20000".into()),
            description: Some("LLM gateway".into()),
            created_at: now,
            updated_at: now,
        },
    )
    .await;
    round_trip(
        state,
        &format!("ws_conf{run}"),
        WorkloadStatusRecord {
            workload_id: "wl_conf".into(),
            environment: "conf".into(),
            project: "conf".into(),
            workload: "api".into(),
            actual_state: "running".into(),
            health: "healthy".into(),
            deployment_id: Some("dep_conf".into()),
            execution_id: None,
            restarts: 2,
            error: None,
            observed_by: "daemon_conf".into(),
            observed_at: now,
        },
    )
    .await;
    round_trip(
        state,
        &format!("wi_conf{run}"),
        WorkloadInstanceRecord {
            environment_id: "env_conf".into(),
            environment: "conf".into(),
            project_id: "prj_conf".into(),
            project: "conf".into(),
            workload: "api".into(),
            workload_id: "wl_conf".into(),
            deployment_id: "dep_conf".into(),
            revision: "abc123".into(),
            state: InstanceState::Serving,
            ports: vec![PortBinding {
                name: "http".into(),
                logical: 8000,
                host: 21000,
            }],
            readiness: Some("http 200".into()),
            started_at: Some(now),
            ready_at: Some(now),
            stopped_at: None,
            error: None,
            updated_at: now,
        },
    )
    .await;
    round_trip(
        state,
        &format!("ta_conf{run}"),
        TrafficAssignmentRecord {
            endpoint: "conf/conf/api/http".into(),
            environment: "conf".into(),
            project: "conf".into(),
            workload: "api".into(),
            port: "http".into(),
            host_port: 20000,
            domains: vec!["conf.example.com".into()],
            deployment_id: "dep_conf".into(),
            revision: "abc123".into(),
            instance_id: "wi_conf".into(),
            target_port: 21000,
            status: "active".into(),
            previous_deployment_id: Some("dep_prev".into()),
            previous_instance_id: Some("wi_prev".into()),
            switched_at: now,
        },
    )
    .await;
    let reconciliation = Reconciliation {
        status: "healthy".into(),
        desired: Some("A 203.0.113.10".into()),
        actual: Some("A 203.0.113.10".into()),
        last_error: None,
        last_reconciled_at: Some(now),
    };
    round_trip(
        state,
        &format!("dom_conf{run}"),
        DomainRecord {
            name: format!("conf{run}.example.com"),
            environment_id: "env_conf".into(),
            environment: "conf".into(),
            project_id: "prj_conf".into(),
            project: "conf".into(),
            workload: "api".into(),
            port: "http".into(),
            dns_provider: "hetzner".into(),
            certificate_id: Some("cert_conf".into()),
            status: "healthy".into(),
            dns: reconciliation.clone(),
            tls: Reconciliation::default(),
            routing: reconciliation.clone(),
            created_at: now,
        },
    )
    .await;
    round_trip(
        state,
        &format!("dns_conf{run}"),
        DnsRecordRecord {
            domain: "conf.example.com".into(),
            provider: "hetzner".into(),
            zone: "example.com".into(),
            name: "conf".into(),
            record_type: "A".into(),
            value: "203.0.113.10".into(),
            ttl: 300,
            provider_record_id: Some("rec_1".into()),
            state: reconciliation,
        },
    )
    .await;
    round_trip(
        state,
        &format!("cert_conf{run}"),
        CertificateRecord {
            domain: "conf.example.com".into(),
            issuer: "https://acme.example/directory".into(),
            status: "valid".into(),
            renewal_status: "not_due".into(),
            not_before: Some(now),
            expires_at: Some(now),
            fingerprint: Some("sha256:f".into()),
            secret_reference: Some("node:certificates/cert_conf".into()),
            held_by: Some("daemon_conf".into()),
            last_error: None,
            last_reconciled_at: Some(now),
        },
    )
    .await;
    round_trip(
        state,
        &format!("pvd_conf{run}"),
        ProviderRecord {
            provider_id: "local".into(),
            kind: "local".into(),
            endpoint: None,
            priority: 0,
            registered_by: "daemon_conf".into(),
            observed_at: now,
        },
    )
    .await;
}
