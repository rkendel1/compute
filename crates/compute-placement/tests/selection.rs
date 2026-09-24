mod support;

use chrono::Duration;
use compute_core::{IsolationProfile, RuntimeKind, SelectionMode};
use compute_placement::{
    Availability, DiscoveryRecord, DiscoveryStatus, EvaluationStatus, Health, PlacementOutcome,
    PoolPolicy, ProviderDescriptor, ProviderKind, place,
};
use support::*;

fn wasm_strict() -> compute_placement::PlacementRequirements {
    let mut requirements = requirements(RuntimeKind::Wasm);
    requirements.isolation = IsolationProfile::Strict;
    requirements
}

/// A: python/process only (priority 100), B: wasm strict (priority 50),
/// C: node strict (priority 50).
fn matrix() -> (compute_placement::PoolConfig, Vec<DiscoveryRecord>) {
    let config = config(&[
        ("a", ProviderKind::Remote, 100),
        ("b", ProviderKind::Remote, 50),
        ("c", ProviderKind::Remote, 50),
    ]);
    let mut a = Synthetic::new(ProviderKind::Remote, &[RuntimeKind::Python]);
    a.isolation = vec![IsolationProfile::Process];
    let b = Synthetic::new(
        ProviderKind::Remote,
        &[RuntimeKind::Python, RuntimeKind::Wasm],
    );
    let c = Synthetic::new(
        ProviderKind::Remote,
        &[RuntimeKind::Node, RuntimeKind::Deno],
    );
    (config, vec![a.record("a"), b.record("b"), c.record("c")])
}

#[test]
fn only_compatible_providers_are_candidates_and_high_priority_incompatible_is_excluded() {
    let (config, records) = matrix();
    let report = place(
        &config.providers,
        &config.pool,
        &records,
        &wasm_strict(),
        &baseline(&wasm_strict()),
        None,
    );
    assert_eq!(report.outcome, PlacementOutcome::Placed);
    assert_eq!(report.compatible_providers, ["b"]);
    assert_eq!(report.incompatible_providers, ["a", "c"]);
    let selected = report.selected.as_ref().unwrap();
    assert_eq!(selected.provider_id, "b");
    assert_eq!(selected.selection_reason.selection_priority, 50);
    assert_eq!(selected.selection_reason.compatible_candidates, 1);
    assert_eq!(report.selection_mode, SelectionMode::Pool);
    let a = report
        .providers
        .iter()
        .find(|p| p.provider_id == "a")
        .unwrap();
    assert_eq!(a.status, EvaluationStatus::Incompatible);
    assert!(!a.reasons.is_empty());
}

#[test]
fn priority_orders_compatible_providers() {
    let (config, records) = matrix();
    let report = place(
        &config.providers,
        &config.pool,
        &records,
        &requirements(RuntimeKind::Python),
        &baseline(&requirements(RuntimeKind::Python)),
        None,
    );
    assert_eq!(report.compatible_providers, ["a", "b"]);
    assert_eq!(report.selected.unwrap().provider_id, "a");
}

#[test]
fn provider_id_breaks_priority_ties_deterministically() {
    let config = config(&[
        ("zeta", ProviderKind::Remote, 10),
        ("alpha", ProviderKind::Remote, 10),
        ("mid", ProviderKind::Remote, 10),
    ]);
    let provider = Synthetic::new(ProviderKind::Remote, &[RuntimeKind::Python]);
    let mut records = vec![
        provider.record("zeta"),
        provider.record("mid"),
        provider.record("alpha"),
    ];
    let first = place(
        &config.providers,
        &config.pool,
        &records,
        &requirements(RuntimeKind::Python),
        &baseline(&requirements(RuntimeKind::Python)),
        None,
    );
    records.reverse();
    let second = place(
        &config.providers,
        &config.pool,
        &records,
        &requirements(RuntimeKind::Python),
        &baseline(&requirements(RuntimeKind::Python)),
        None,
    );
    assert_eq!(first.compatible_providers, ["alpha", "mid", "zeta"]);
    assert_eq!(first.selected.as_ref().unwrap().provider_id, "alpha");
    assert_eq!(first, second, "record order must not affect placement");
}

#[test]
fn explicit_provider_is_validated_and_never_falls_back() {
    let (config, records) = matrix();
    let report = place(
        &config.providers,
        &config.pool,
        &records,
        &wasm_strict(),
        &baseline(&wasm_strict()),
        Some("a"),
    );
    assert_eq!(report.outcome, PlacementOutcome::PlacementFailed);
    assert_eq!(report.selection_mode, SelectionMode::Explicit);
    assert!(report.selected.is_none());
    assert!(report.compatible_providers.is_empty());
    assert_eq!(
        report.failure.as_ref().unwrap().code,
        "explicit_provider_incompatible"
    );
    assert_eq!(
        report.providers.len(),
        1,
        "only the named provider is evaluated"
    );
    assert!(report.receipt_binding().is_none());

    let explicit = place(
        &config.providers,
        &config.pool,
        &records,
        &wasm_strict(),
        &baseline(&wasm_strict()),
        Some("b"),
    );
    assert_eq!(explicit.outcome, PlacementOutcome::Placed);
    let binding = explicit.receipt_binding().unwrap();
    assert_eq!(binding.selection_mode, SelectionMode::Explicit);
    assert_eq!(binding.selection_reason.ordering, "explicit_provider");

    let unknown = place(
        &config.providers,
        &config.pool,
        &records,
        &wasm_strict(),
        &baseline(&wasm_strict()),
        Some("nope"),
    );
    assert_eq!(unknown.failure.unwrap().code, "provider_not_configured");
}

#[test]
fn empty_compatible_set_is_a_structured_failure() {
    let (config, records) = matrix();
    let mut requirements = requirements(RuntimeKind::Ruby);
    requirements.isolation = IsolationProfile::Strict;
    let report = place(
        &config.providers,
        &config.pool,
        &records,
        &requirements,
        &baseline(&requirements),
        None,
    );
    assert_eq!(report.outcome, PlacementOutcome::PlacementFailed);
    assert_eq!(
        report.failure.as_ref().unwrap().code,
        "no_compatible_provider"
    );
    assert_eq!(report.incompatible_providers, ["a", "b", "c"]);
    assert!(
        report
            .providers
            .iter()
            .all(|provider| !provider.reasons.is_empty())
    );
    assert!(
        report
            .explanation
            .selection
            .starts_with("placement_failed: no_compatible_provider")
    );
}

#[test]
fn placement_is_deterministic() {
    let (config, records) = matrix();
    let first = place(
        &config.providers,
        &config.pool,
        &records,
        &wasm_strict(),
        &baseline(&wasm_strict()),
        None,
    );
    for _ in 0..32 {
        let (config, records) = matrix();
        let again = place(
            &config.providers,
            &config.pool,
            &records,
            &wasm_strict(),
            &baseline(&wasm_strict()),
            None,
        );
        assert_eq!(again.placement_id, first.placement_id);
        assert_eq!(again.selected, first.selected);
        assert_eq!(again.explanation, first.explanation);
        assert_eq!(
            serde_json::to_vec(&again).unwrap(),
            serde_json::to_vec(&first).unwrap()
        );
    }
}

#[test]
fn placement_identity_ignores_observation_time_but_tracks_inputs() {
    let (config, mut records) = matrix();
    let first = place(
        &config.providers,
        &config.pool,
        &records,
        &wasm_strict(),
        &baseline(&wasm_strict()),
        None,
    );
    for record in &mut records {
        let descriptor = record.descriptor.as_mut().unwrap();
        descriptor.availability.fetched_at += Duration::seconds(10);
        descriptor.availability.expires_at += Duration::seconds(10);
        record.status = DiscoveryStatus::Cached;
    }
    let later = place(
        &config.providers,
        &config.pool,
        &records,
        &wasm_strict(),
        &baseline(&wasm_strict()),
        None,
    );
    assert_eq!(first.placement_id, later.placement_id);

    let mut reprioritized = config.clone();
    reprioritized.providers.get_mut("b").unwrap().priority = 51;
    let changed = place(
        &reprioritized.providers,
        &config.pool,
        &records,
        &wasm_strict(),
        &baseline(&wasm_strict()),
        None,
    );
    assert_ne!(first.placement_id, changed.placement_id);

    let mut other = wasm_strict();
    other.resources.timeout_ms = Some(1000);
    assert_ne!(
        first.placement_id,
        place(
            &config.providers,
            &config.pool,
            &records,
            &other,
            &baseline(&other),
            None,
        )
        .placement_id
    );
    let explicit = place(
        &config.providers,
        &config.pool,
        &records,
        &wasm_strict(),
        &baseline(&wasm_strict()),
        Some("b"),
    );
    assert_ne!(first.placement_id, explicit.placement_id);
}

#[test]
fn health_is_observational_unless_explicitly_required() {
    let (config, mut records) = matrix();
    records[0].descriptor.as_mut().unwrap().availability.health = Health::Unhealthy;
    let report = place(
        &config.providers,
        &config.pool,
        &records,
        &requirements(RuntimeKind::Python),
        &baseline(&requirements(RuntimeKind::Python)),
        None,
    );
    assert_eq!(
        report.selected.unwrap().provider_id,
        "a",
        "health does not override selection by default"
    );

    let mut strict = config.clone();
    strict.pool.require_healthy = true;
    let report = place(
        &strict.providers,
        &strict.pool,
        &records,
        &requirements(RuntimeKind::Python),
        &baseline(&requirements(RuntimeKind::Python)),
        None,
    );
    assert_eq!(report.selected.as_ref().unwrap().provider_id, "b");
    let a = report
        .providers
        .iter()
        .find(|p| p.provider_id == "a")
        .unwrap();
    assert_eq!(a.status, EvaluationStatus::ExcludedUnhealthy);

    // Healthy never makes an incompatible provider compatible.
    let report = place(
        &strict.providers,
        &strict.pool,
        &records,
        &wasm_strict(),
        &baseline(&wasm_strict()),
        None,
    );
    assert_eq!(report.selected.unwrap().provider_id, "b");
}

#[test]
fn stale_capabilities_are_unknown_not_valid() {
    let (config, mut records) = matrix();
    records[1].status = DiscoveryStatus::Stale;
    records[1].descriptor.as_mut().unwrap().availability.health = Health::Unknown;
    let report = place(
        &config.providers,
        &config.pool,
        &records,
        &wasm_strict(),
        &baseline(&wasm_strict()),
        None,
    );
    assert_eq!(report.outcome, PlacementOutcome::PlacementFailed);
    let b = report
        .providers
        .iter()
        .find(|p| p.provider_id == "b")
        .unwrap();
    assert_eq!(b.status, EvaluationStatus::CapabilitiesUnknown);
    assert_eq!(
        b.error.as_ref().unwrap().code,
        "provider_capabilities_stale"
    );

    let mut permissive = config.clone();
    permissive.pool.allow_stale_capabilities = true;
    let report = place(
        &permissive.providers,
        &permissive.pool,
        &records,
        &wasm_strict(),
        &baseline(&wasm_strict()),
        None,
    );
    assert_eq!(report.selected.unwrap().provider_id, "b");
    assert!(report.selection_policy.allow_stale_capabilities);
}

#[test]
fn unavailable_and_invalid_providers_are_excluded_with_their_own_status() {
    let (config, mut records) = matrix();
    records[0] = DiscoveryRecord {
        provider_id: "a".into(),
        status: DiscoveryStatus::Unavailable,
        descriptor: None,
        error: Some(compute_placement::DiscoveryError {
            code: "provider_unavailable".into(),
            message: "connection refused".into(),
        }),
    };
    records[2] = DiscoveryRecord {
        provider_id: "c".into(),
        status: DiscoveryStatus::Invalid,
        descriptor: None,
        error: Some(compute_placement::DiscoveryError {
            code: "provider_capabilities_invalid".into(),
            message: "protocol".into(),
        }),
    };
    let report = place(
        &config.providers,
        &config.pool,
        &records,
        &requirements(RuntimeKind::Python),
        &baseline(&requirements(RuntimeKind::Python)),
        None,
    );
    assert_eq!(report.selected.unwrap().provider_id, "b");
    let status = |id: &str| {
        report
            .providers
            .iter()
            .find(|p| p.provider_id == id)
            .unwrap()
            .status
    };
    assert_eq!(report.excluded_providers, ["a", "c"]);
    assert!(report.incompatible_providers.is_empty());
    assert_eq!(status("a"), EvaluationStatus::ProviderUnavailable);
    assert_eq!(status("c"), EvaluationStatus::CapabilitiesInvalid);
}

#[test]
fn explanation_answers_the_four_questions() {
    let (config, records) = matrix();
    let report = place(
        &config.providers,
        &config.pool,
        &records,
        &wasm_strict(),
        &baseline(&wasm_strict()),
        None,
    );
    let explanation = &report.explanation;
    assert!(
        explanation
            .requires
            .iter()
            .any(|line| line == "runtime wasm (any version)")
    );
    assert!(
        explanation
            .requires
            .iter()
            .any(|line| line == "isolation strict")
    );
    assert!(
        explanation
            .requires
            .iter()
            .any(|line| line == "network none")
    );
    assert_eq!(explanation.considered.len(), 3);
    assert!(explanation.considered[0].starts_with(
        "a (remote, priority 100, health healthy): incompatible (policy would admit): runtime_unsupported"
    ));
    assert!(
        explanation
            .considered
            .iter()
            .any(|line| line.starts_with("b ") && line.ends_with(": compatible"))
    );
    assert!(explanation.selection.starts_with(
        "selected provider b: compatible and admitted by policy, with selection priority 50"
    ));
    let text = serde_json::to_string(&report).unwrap();
    for subjective in ["best", "optimal"] {
        assert!(
            !text.contains(subjective),
            "placement must not use {subjective:?}"
        );
    }
}

fn valid() -> compute_provider::ProviderCapabilities {
    Synthetic::new(
        ProviderKind::Remote,
        &[RuntimeKind::Python, RuntimeKind::Wasm],
    )
    .capabilities("r")
}

fn reject(mut mutate: impl FnMut(&mut compute_provider::ProviderCapabilities), field: &str) {
    let mut capabilities = valid();
    mutate(&mut capabilities);
    let error = ProviderDescriptor::from_capabilities(
        "r",
        ProviderKind::Remote,
        &capabilities,
        availability(),
    )
    .expect_err(field);
    assert_eq!(error.code, "provider_capabilities_invalid");
    assert!(error.field.starts_with(field), "{field}: {error}");
}

#[test]
fn malformed_or_contradictory_capabilities_are_rejected() {
    reject(|c| c.protocol = "compute.remote@2".into(), "protocol");
    reject(
        |c| c.provider = compute_core::ProviderIdentity::Local { id: "local".into() },
        "provider",
    );
    reject(
        |c| {
            c.provider = compute_core::ProviderIdentity::Remote {
                id: "x".into(),
                endpoint: "ftp://x".into(),
            }
        },
        "provider",
    );
    reject(
        |c| c.distribution_id = Some("sha256:nothex".into()),
        "distribution_id",
    );
    reject(
        |c| c.inventory.platform = "linux".into(),
        "inventory.platform",
    );
    reject(|c| c.isolation_profiles.clear(), "isolation_profiles");
    reject(
        |c| c.isolation_profiles.push(IsolationProfile::Process),
        "isolation_profiles",
    );
    reject(|c| c.network_policies.clear(), "network_policies");
    reject(|c| c.max_request_bytes = 0, "artifact_limits");
    reject(|c| c.max_memory_bytes = Some(0), "resource_capabilities");
    reject(
        |c| c.artifact_modes = vec!["inline".into()],
        "artifact_modes",
    );
    reject(
        |c| c.dependency_capsules = vec!["capsule".into()],
        "dependency_capsules",
    );
    reject(
        |c| {
            let duplicate = c.inventory.runtimes[0].clone();
            c.inventory.runtimes.push(duplicate);
        },
        "inventory.runtimes",
    );
    reject(
        |c| c.inventory.runtimes[0].version = "\u{7}".into(),
        "inventory.runtimes",
    );
    // A process runtime claiming a filesystem boundary it cannot provide.
    reject(
        |c| {
            c.inventory.runtimes[0]
                .capabilities
                .isolation
                .filesystem_boundary = true
        },
        "inventory.runtimes",
    );
    // A runtime claiming memory enforcement without the memory capability.
    reject(
        |c| {
            c.inventory.runtimes[0]
                .capabilities
                .isolation
                .memory_enforcement = true
        },
        "inventory.runtimes",
    );
    reject(
        |c| {
            c.runtime_artifacts
                .insert(RuntimeKind::Ruby, ARTIFACT_P.into());
        },
        "runtime_artifacts",
    );
}

#[test]
fn descriptor_is_canonical_and_inspectable() {
    let first = Synthetic::new(
        ProviderKind::Remote,
        &[RuntimeKind::Wasm, RuntimeKind::Python],
    )
    .descriptor("r");
    let mut shuffled = Synthetic::new(
        ProviderKind::Remote,
        &[RuntimeKind::Python, RuntimeKind::Wasm],
    );
    shuffled.isolation.reverse();
    shuffled.network.reverse();
    let second = ProviderDescriptor::from_capabilities(
        "r",
        ProviderKind::Remote,
        &shuffled.capabilities("r"),
        Availability {
            health: Health::Unhealthy,
            ..availability()
        },
    )
    .unwrap();
    assert_eq!(first.capability_version, second.capability_version);
    assert_eq!(first.runtimes, second.runtimes);
    let json = serde_json::to_value(&first).unwrap();
    for field in [
        "provider_id",
        "provider_kind",
        "protocol_version",
        "runtimes",
        "distribution",
        "isolation_profiles",
        "network_capabilities",
        "resource_capabilities",
        "dependency_capsules",
        "artifact_limits",
        "availability",
    ] {
        assert!(json.get(field).is_some(), "descriptor field {field}");
    }
    let parsed: ProviderDescriptor = serde_json::from_value(json).unwrap();
    assert_eq!(parsed, first);
    assert_eq!(
        parsed.compute_capability_version(),
        parsed.capability_version
    );
}

#[test]
fn pool_config_is_caller_owned_and_validated() {
    let config = compute_placement::PoolConfig::parse(
        r#"
[pool]
require_healthy = true

[providers.local]
kind = "local"
priority = 100

[providers.dev]
kind = "remote"
endpoint = "http://compute-dev:8080"
priority = 50

[providers.production]
kind = "remote"
endpoint = "https://compute.example"
token_env = "COMPUTE_PRODUCTION_TOKEN"
"#,
    )
    .unwrap();
    assert!(config.pool.require_healthy);
    assert_eq!(config.providers.len(), 3);
    assert_eq!(config.providers["production"].priority, 0);
    for invalid in [
        "[providers.x]\nkind = \"remote\"\n",
        "[providers.x]\nkind = \"local\"\nendpoint = \"http://a\"\n",
        "[providers.x]\nkind = \"remote\"\nendpoint = \"http://user:pw@a\"\n",
        "[providers.\"bad id\"]\nkind = \"local\"\n",
        "[providers.x]\nkind = \"local\"\nweight = 3\n",
        "[pool]\ncapability_ttl_seconds = 0\n",
    ] {
        assert!(
            compute_placement::PoolConfig::parse(invalid).is_err(),
            "{invalid}"
        );
    }
    let _ = PoolPolicy::default();
}

fn policy(json: serde_json::Value) -> compute_policy::Policy {
    compute_policy::Policy::from_json(json.to_string().as_bytes()).unwrap()
}

/// Capability and admission are independent: all four combinations are
/// distinguishable, and only capable + admitted providers are candidates.
#[test]
fn capability_and_admission_matrix() {
    let config = config(&[
        ("capable_admitted", ProviderKind::Remote, 10),
        ("capable_denied", ProviderKind::Remote, 40),
        ("incapable_admitted", ProviderKind::Remote, 30),
        ("incapable_denied", ProviderKind::Remote, 20),
    ]);
    // The workload needs sandboxed WASM. "incapable" providers offer only
    // process isolation; "denying" providers run on aarch64, which the
    // caller's policy does not allow.
    let wasm = |isolation: Vec<IsolationProfile>, platform: &str| {
        let mut synthetic = Synthetic::new(ProviderKind::Remote, &[RuntimeKind::Wasm]);
        synthetic.isolation = isolation;
        synthetic.platform = platform.into();
        synthetic
    };
    let records = vec![
        wasm(IsolationProfile::ALL.to_vec(), "linux-x86_64").record("capable_admitted"),
        wasm(IsolationProfile::ALL.to_vec(), "linux-aarch64").record("capable_denied"),
        wasm(vec![IsolationProfile::Process], "linux-x86_64").record("incapable_admitted"),
        wasm(vec![IsolationProfile::Process], "linux-aarch64").record("incapable_denied"),
    ];
    let mut sandboxed = requirements(RuntimeKind::Wasm);
    sandboxed.isolation = IsolationProfile::Sandboxed;
    let caller = policy(serde_json::json!({"version": 1, "allowed_architectures": ["x86_64"]}));
    let context = with_policy(&sandboxed, caller);
    let report = place(
        &config.providers,
        &config.pool,
        &records,
        &sandboxed,
        &context,
        None,
    );

    let status = |id: &str| {
        report
            .providers
            .iter()
            .find(|p| p.provider_id == id)
            .unwrap()
    };
    assert_eq!(
        status("capable_admitted").status,
        EvaluationStatus::Compatible
    );
    assert_eq!(
        status("capable_denied").status,
        EvaluationStatus::PolicyDenied
    );
    assert_eq!(
        status("incapable_admitted").status,
        EvaluationStatus::Incompatible
    );
    assert_eq!(
        status("incapable_denied").status,
        EvaluationStatus::Incompatible
    );

    // Only the capable, admitted provider is a candidate, even though it
    // has the lowest priority.
    assert_eq!(report.compatible_providers, ["capable_admitted"]);
    assert_eq!(
        report.selected.as_ref().unwrap().provider_id,
        "capable_admitted"
    );

    // Policy denial is not reported as a capability problem.
    let denied = status("capable_denied");
    assert!(denied.reasons.is_empty());
    let decision = denied.admission.as_ref().unwrap();
    assert_eq!(decision.codes(), ["architecture_denied"]);

    // Capability mismatch is not reported as a policy problem.
    let incapable_only = status("incapable_admitted");
    assert!(!incapable_only.reasons.is_empty());
    let decision = incapable_only.admission.as_ref().unwrap();
    assert!(decision.has(compute_policy::ReasonKind::Capability));
    assert!(!decision.has(compute_policy::ReasonKind::Policy));

    // Both facts are preserved when both fail.
    let both = status("incapable_denied");
    assert!(!both.reasons.is_empty());
    let decision = both.admission.as_ref().unwrap();
    assert!(decision.has(compute_policy::ReasonKind::Capability));
    assert_eq!(
        decision
            .reasons
            .iter()
            .filter(|reason| reason.kind == compute_policy::ReasonKind::Policy)
            .map(|reason| reason.code.as_str())
            .collect::<Vec<_>>(),
        ["architecture_denied"]
    );
    assert!(
        report
            .explanation
            .considered
            .iter()
            .any(|line| line.starts_with("capable_denied")
                && line.contains("capable, but policy denied"))
    );
    assert!(report.explanation.considered.iter().any(
        |line| line.starts_with("incapable_denied") && line.contains("policy would also deny")
    ));
    assert!(
        report
            .explanation
            .considered
            .iter()
            .any(|line| line.starts_with("incapable_admitted")
                && line.contains("policy would admit"))
    );

    // Explicit selection never bypasses policy.
    let explicit = place(
        &config.providers,
        &config.pool,
        &records,
        &sandboxed,
        &context,
        Some("capable_denied"),
    );
    assert_eq!(
        explicit.failure.as_ref().unwrap().code,
        "explicit_provider_denied"
    );
    assert!(explicit.receipt_binding().is_none());
    let explicit = place(
        &config.providers,
        &config.pool,
        &records,
        &sandboxed,
        &context,
        Some("capable_admitted"),
    );
    assert_eq!(explicit.outcome, PlacementOutcome::Placed);
}

#[test]
fn provider_advertised_policy_is_intersected_and_changes_identity() {
    let config = config(&[("a", ProviderKind::Remote, 10)]);
    let open = Synthetic::new(ProviderKind::Remote, &[RuntimeKind::Python]);
    let mut restricted = Synthetic::new(ProviderKind::Remote, &[RuntimeKind::Python]);
    restricted.policy = Some(policy(
        serde_json::json!({"version": 1, "allowed_networks": ["none"]}),
    ));
    let requirements = requirements(RuntimeKind::Python);
    let context = baseline(&requirements);
    let admitted = place(
        &config.providers,
        &config.pool,
        &[open.record("a")],
        &requirements,
        &context,
        None,
    );
    assert_eq!(admitted.outcome, PlacementOutcome::Placed);
    let denied = place(
        &config.providers,
        &config.pool,
        &[restricted.record("a")],
        &requirements,
        &context,
        None,
    );
    assert_eq!(denied.providers[0].status, EvaluationStatus::PolicyDenied);
    assert_eq!(
        denied.providers[0].admission.as_ref().unwrap().codes(),
        ["network_denied"]
    );
    assert_ne!(admitted.placement_id, denied.placement_id);

    // Changing only the caller policy changes placement and admission identity.
    let stricter = with_policy(
        &requirements,
        policy(serde_json::json!({"version": 1, "allowed_runtimes": ["python"]})),
    );
    let again = place(
        &config.providers,
        &config.pool,
        &[open.record("a")],
        &requirements,
        &stricter,
        None,
    );
    assert_eq!(again.outcome, PlacementOutcome::Placed);
    assert_ne!(again.placement_id, admitted.placement_id);
    assert_ne!(
        again.selected.unwrap().admission_id,
        admitted.selected.unwrap().admission_id
    );
}
