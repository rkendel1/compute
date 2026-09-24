use std::collections::BTreeMap;

use compute_core::{
    IsolationProfile, IsolationRequirement, NetworkPolicy, PlatformIdentity, ProviderIdentity,
    ResourceLimits, RuntimeKind, WORKLOAD_SPEC_VERSION, WorkloadBundle, WorkloadDependencies,
    WorkloadOutput, WorkloadSpec,
};
use compute_policy::*;

const DIST_A: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DIST_B: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const CAPSULE: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

fn spec(runtime: RuntimeKind, entrypoint: &str) -> WorkloadSpec {
    WorkloadSpec {
        version: WORKLOAD_SPEC_VERSION.into(),
        runtime,
        runtime_version: None,
        entrypoint: entrypoint.into(),
        args: vec![],
        env: BTreeMap::new(),
        inputs: vec![],
        outputs: vec![],
        resources: ResourceLimits::default(),
        network: NetworkPolicy::None,
        isolation: IsolationRequirement::default(),
        dependencies: None,
    }
}

fn contract_with(edit: impl FnOnce(&mut WorkloadSpec)) -> ExecutionContract {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("main.py"), "print('hello')\n").unwrap();
    let mut workload = spec(RuntimeKind::Python, "main.py");
    edit(&mut workload);
    let bundle = WorkloadBundle::create_from(workload, root.path()).unwrap();
    ExecutionContract::from_bundle(&bundle, None).unwrap()
}

fn contract() -> ExecutionContract {
    contract_with(|_| {})
}

fn provider() -> ProviderFacts {
    ProviderFacts {
        identity: ProviderIdentity::Local { id: "local".into() },
        distribution_id: Some(DIST_A.into()),
        platform: Some(PlatformIdentity {
            os: "linux".into(),
            architecture: "x86_64".into(),
            runtime_abi: None,
        }),
        runtime_version: Some("3.13.1".into()),
    }
}

fn policy(json: serde_json::Value) -> Policy {
    Policy::from_json(json.to_string().as_bytes()).unwrap()
}

fn codes(policy: &Policy, contract: &ExecutionContract) -> Vec<String> {
    let decision = admit(policy, contract, &provider(), &CapabilityStatus::Compatible);
    assert_eq!(decision.admitted, decision.reasons.is_empty());
    decision
        .reasons
        .iter()
        .map(|reason| reason.code.clone())
        .collect()
}

#[test]
fn baseline_admits_everything_compute_can_run() {
    let decision = admit(
        &Policy::baseline(),
        &contract(),
        &provider(),
        &CapabilityStatus::Compatible,
    );
    assert!(decision.admitted);
    assert_eq!(decision.status, AdmissionStatus::Admitted);
    assert!(decision.reproduce(&Policy::baseline()));
}

#[test]
fn runtime_allow_and_deny() {
    let allowed = policy(serde_json::json!({"version": 1, "allowed_runtimes": ["python", "wasm"]}));
    assert!(codes(&allowed, &contract()).is_empty());
    let denied = policy(serde_json::json!({"version": 1, "allowed_runtimes": ["wasm", "node"]}));
    let decision = admit(
        &denied,
        &contract(),
        &provider(),
        &CapabilityStatus::Compatible,
    );
    assert_eq!(decision.codes(), ["runtime_denied"]);
    assert_eq!(decision.reasons[0].kind, ReasonKind::Policy);
    assert_eq!(decision.reasons[0].requested, serde_json::json!("python"));
    assert_eq!(
        decision.reasons[0].allowed,
        serde_json::json!(["wasm", "node"])
    );
    assert_eq!(
        decision.reasons[0].message,
        "requested runtime \"python\" is not allowed by policy"
    );
}

#[test]
fn version_allow_and_deny() {
    let allowed =
        policy(serde_json::json!({"version": 1, "allowed_runtime_versions": {"python": ["3.13"]}}));
    assert!(codes(&allowed, &contract()).is_empty());
    let denied =
        policy(serde_json::json!({"version": 1, "allowed_runtime_versions": {"python": ["3.12"]}}));
    assert_eq!(codes(&denied, &contract()), ["runtime_version_denied"]);
    let mut unknown = provider();
    unknown.runtime_version = None;
    let decision = admit(
        &denied,
        &contract(),
        &unknown,
        &CapabilityStatus::Compatible,
    );
    assert_eq!(decision.codes(), ["runtime_version_unknown"]);
}

#[test]
fn distribution_allow_and_deny() {
    let allowed = policy(serde_json::json!({"version": 1, "allowed_distributions": [DIST_A]}));
    assert!(codes(&allowed, &contract()).is_empty());
    let denied = policy(serde_json::json!({"version": 1, "allowed_distributions": [DIST_B]}));
    assert_eq!(codes(&denied, &contract()), ["distribution_denied"]);
    let mut unknown = provider();
    unknown.distribution_id = None;
    let decision = admit(
        &denied,
        &contract(),
        &unknown,
        &CapabilityStatus::Compatible,
    );
    assert_eq!(decision.codes(), ["distribution_unknown"]);
}

#[test]
fn dependency_allow_and_deny() {
    let with_capsule = contract_with(|workload| {
        workload.dependencies = Some(WorkloadDependencies {
            capsule: CAPSULE.into(),
        })
    });
    let allowed = policy(serde_json::json!({"version": 1, "allowed_dependencies": [CAPSULE]}));
    assert!(codes(&allowed, &with_capsule).is_empty());
    assert!(
        codes(&allowed, &contract()).is_empty(),
        "no capsule is unaffected"
    );
    let denied = policy(serde_json::json!({"version": 1, "allowed_dependencies": [DIST_A]}));
    assert_eq!(codes(&denied, &with_capsule), ["dependency_denied"]);
}

#[test]
fn minimum_isolation() {
    let strict = policy(serde_json::json!({"version": 1, "minimum_isolation": "strict"}));
    assert_eq!(codes(&strict, &contract()), ["isolation_below_minimum"]);
    let isolated = contract_with(|workload| workload.isolation.profile = IsolationProfile::Strict);
    assert!(codes(&strict, &isolated).is_empty());
}

#[test]
fn network_restrictions() {
    let none_only = policy(serde_json::json!({"version": 1, "allowed_networks": ["none"]}));
    assert!(codes(&none_only, &contract()).is_empty());
    let networked = contract_with(|workload| workload.network = NetworkPolicy::Network);
    let decision = admit(
        &none_only,
        &networked,
        &provider(),
        &CapabilityStatus::Compatible,
    );
    assert_eq!(decision.codes(), ["network_denied"]);
    assert_eq!(decision.reasons[0].requested, serde_json::json!("network"));
    assert_eq!(decision.reasons[0].allowed, serde_json::json!(["none"]));
}

#[test]
fn memory_and_timeout_limits_treat_unset_as_unbounded() {
    let limited = policy(serde_json::json!({
        "version": 1, "limits": {"max_timeout_ms": 1000, "max_memory_bytes": 4096}
    }));
    assert_eq!(
        codes(&limited, &contract()),
        ["memory_unbounded", "timeout_unbounded"]
    );
    let within = contract_with(|workload| {
        workload.resources.wall_time = Some(std::time::Duration::from_millis(1000));
        workload.resources.memory_bytes = Some(4096);
    });
    assert!(codes(&limited, &within).is_empty());
    let over = contract_with(|workload| {
        workload.resources.wall_time = Some(std::time::Duration::from_millis(1001));
        workload.resources.memory_bytes = Some(4097);
    });
    assert_eq!(
        codes(&limited, &over),
        ["memory_exceeds_policy", "timeout_exceeds_policy"]
    );
}

#[test]
fn input_output_and_artifact_limits() {
    let tight = policy(serde_json::json!({
        "version": 1,
        "limits": {"max_input_bytes": 4, "max_output_bytes": 10, "max_artifact_bytes": 64}
    }));
    let bounded = contract_with(|workload| {
        workload.resources.stdout_bytes = Some(10);
        workload.resources.stderr_bytes = Some(1);
    });
    assert_eq!(
        codes(&tight, &bounded),
        [
            "artifact_exceeds_policy",
            "input_exceeds_policy",
            "output_exceeds_policy"
        ]
    );
    assert!(codes(&tight, &contract()).contains(&"output_unbounded".to_string()));
    let roomy = policy(serde_json::json!({
        "version": 1,
        "limits": {"max_input_bytes": 1024, "max_output_bytes": 11, "max_artifact_bytes": 1048576}
    }));
    assert!(codes(&roomy, &bounded).is_empty());
}

#[test]
fn output_classes() {
    let stdio = policy(serde_json::json!({"version": 1, "allowed_output_classes": ["stdio"]}));
    assert!(codes(&stdio, &contract()).is_empty());
    let files = contract_with(|workload| {
        workload.outputs = vec![WorkloadOutput {
            path: "result.txt".into(),
            required: true,
        }]
    });
    assert_eq!(codes(&stdio, &files), ["output_class_denied"]);
}

#[test]
fn platform_restrictions() {
    let arm = policy(serde_json::json!({"version": 1, "allowed_architectures": ["aarch64"]}));
    assert_eq!(codes(&arm, &contract()), ["architecture_denied"]);
    let mac = policy(serde_json::json!({"version": 1, "allowed_os": ["macos"]}));
    assert_eq!(codes(&mac, &contract()), ["platform_denied"]);
    let linux = policy(
        serde_json::json!({"version": 1, "allowed_os": ["linux"], "allowed_architectures": ["x86_64"]}),
    );
    assert!(codes(&linux, &contract()).is_empty());
    let mut unknown = provider();
    unknown.platform = None;
    let decision = admit(&linux, &contract(), &unknown, &CapabilityStatus::Compatible);
    assert_eq!(decision.codes(), ["platform_unknown", "platform_unknown"]);
}

#[test]
fn every_violation_is_reported() {
    let strict = policy(serde_json::json!({
        "version": 1,
        "allowed_runtimes": ["wasm"],
        "allowed_networks": ["none"],
        "minimum_isolation": "sandboxed"
    }));
    let networked = contract_with(|workload| workload.network = NetworkPolicy::Network);
    assert_eq!(
        codes(&strict, &networked),
        [
            "isolation_below_minimum",
            "network_denied",
            "runtime_denied"
        ]
    );
}

#[test]
fn capability_and_policy_are_reported_separately() {
    let denied = policy(serde_json::json!({"version": 1, "allowed_runtimes": ["wasm"]}));
    let decision = admit(
        &denied,
        &contract(),
        &provider(),
        &CapabilityStatus::Incompatible {
            codes: vec!["isolation_unsupported".into()],
        },
    );
    assert!(!decision.admitted);
    assert!(decision.has(ReasonKind::Capability));
    assert!(decision.has(ReasonKind::Policy));
    assert_eq!(decision.codes(), ["capability_mismatch", "runtime_denied"]);

    // Missing capability information fails closed even when policy allows.
    let unknown = admit(
        &Policy::baseline(),
        &contract(),
        &provider(),
        &CapabilityStatus::Unknown,
    );
    assert_eq!(unknown.codes(), ["capability_unknown"]);
}

#[test]
fn fail_closed_on_malformed_or_unknown_policies() {
    for (document, expect_version) in [
        (r#"{"version": 2}"#, true),
        (r#"{"version": "1"}"#, true),
        (r#"{}"#, false),
        (r#"not json"#, false),
        (r#"{"version": 1, "allowed_runtimes": ["cobol"]}"#, false),
        (r#"{"version": 1, "minimum_isolation": "vm"}"#, false),
        (r#"{"version": 1, "allowed_networks": ["internet"]}"#, false),
        (r#"{"version": 1, "limits": {"max_timeout_ms": -1}}"#, false),
        (
            r#"{"version": 1, "limits": {"max_memory_bytes": 0}}"#,
            false,
        ),
        (
            r#"{"version": 1, "limits": {"max_input_bytes": 10, "max_artifact_bytes": 5}}"#,
            false,
        ),
        (
            r#"{"version": 1, "allowed_distributions": ["sha256:nope"]}"#,
            false,
        ),
        (
            r#"{"version": 1, "allowed_dependencies": ["capsule"]}"#,
            false,
        ),
        (
            r#"{"version": 1, "allowed_runtimes": ["python", "python"]}"#,
            false,
        ),
        (r#"{"version": 1, "allowed_runtimes": []}"#, false),
        (
            r#"{"version": 1, "allowed_runtimes": ["wasm"], "allowed_runtime_versions": {"python": ["3.13"]}}"#,
            false,
        ),
        (
            r#"{"version": 1, "defaults": {"network": "network"}, "allowed_networks": ["none"]}"#,
            false,
        ),
        (
            r#"{"version": 1, "defaults": {"isolation": "process"}, "minimum_isolation": "strict"}"#,
            false,
        ),
        (r#"{"version": 1, "allowed_os": ["Linux"]}"#, false),
        (r#"{"version": 1, "rules": []}"#, false),
    ] {
        let error = Policy::from_json(document.as_bytes()).expect_err(document);
        assert_eq!(
            matches!(error, PolicyError::UnsupportedVersion(_)),
            expect_version,
            "{document}: {error}"
        );
    }
}

#[test]
fn impossible_and_conflicting_requests_are_rejected() {
    // WorkloadSpec validation already rejects this; admission rejects it too
    // for contracts that arrive by any other route.
    let mut zero = contract();
    zero.resources.memory_bytes = Some(0);
    let decision = admit(
        &Policy::baseline(),
        &zero,
        &provider(),
        &CapabilityStatus::Compatible,
    );
    assert_eq!(decision.codes(), ["resource_request_invalid"]);
    assert_eq!(decision.reasons[0].kind, ReasonKind::Contract);

    let mut conflicting = contract();
    conflicting.platform = Some(PlatformIdentity {
        os: "linux".into(),
        architecture: "aarch64".into(),
        runtime_abi: None,
    });
    let decision = admit(
        &Policy::baseline(),
        &conflicting,
        &provider(),
        &CapabilityStatus::Compatible,
    );
    assert_eq!(decision.codes(), ["platform_conflict"]);

    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("main.py"), "").unwrap();
    let mut strict = spec(RuntimeKind::Python, "main.py");
    strict.isolation.profile = IsolationProfile::Strict;
    let bundle = WorkloadBundle::create_from(strict, root.path()).unwrap();
    assert!(matches!(
        ExecutionContract::from_bundle(&bundle, Some(IsolationProfile::Process)),
        Err(PolicyError::Contract(_))
    ));
}

#[test]
fn policy_identity_is_canonical_and_tracks_every_change() {
    let first = policy(serde_json::json!({"version": 1, "allowed_runtimes": ["wasm", "python"]}));
    let reordered =
        policy(serde_json::json!({"allowed_runtimes": ["python", "wasm"], "version": 1}));
    assert_eq!(first.policy_id(), reordered.policy_id());
    assert_eq!(first.canonical_bytes(), reordered.canonical_bytes());
    let changed = policy(serde_json::json!({"version": 1, "allowed_runtimes": ["python"]}));
    assert_ne!(first.policy_id(), changed.policy_id());
    let named = policy(
        serde_json::json!({"version": 1, "name": "production-policy", "allowed_runtimes": ["python", "wasm"]}),
    );
    assert_ne!(first.policy_id(), named.policy_id());
    assert_eq!(named.label(), "production-policy@1");
    assert!(first.policy_id().starts_with("sha256:"));
}

#[test]
fn admission_identity_is_deterministic_and_excludes_nothing_relevant() {
    let policy = Policy::baseline();
    let first = admit(
        &policy,
        &contract(),
        &provider(),
        &CapabilityStatus::Compatible,
    );
    for _ in 0..16 {
        let again = admit(
            &policy,
            &contract(),
            &provider(),
            &CapabilityStatus::Compatible,
        );
        assert_eq!(again, first);
    }
    let mut other_provider = provider();
    other_provider.identity = ProviderIdentity::Remote {
        id: "https://compute.example".into(),
        endpoint: "https://compute.example".into(),
    };
    let moved = admit(
        &policy,
        &contract(),
        &other_provider,
        &CapabilityStatus::Compatible,
    );
    assert_ne!(moved.admission_id, first.admission_id);
    let stricter = policy.intersect(&Policy {
        allowed_networks: Some(vec![NetworkPolicy::None]),
        ..Policy::unrestricted()
    });
    let restricted = admit(
        &stricter,
        &contract(),
        &provider(),
        &CapabilityStatus::Compatible,
    );
    assert!(restricted.admitted);
    assert_ne!(restricted.admission_id, first.admission_id);
    assert_ne!(restricted.policy_id, first.policy_id);
    // A decision cannot be reproduced under a different policy.
    assert!(!first.reproduce(&stricter));
}

#[test]
fn composition_is_an_intersection_and_never_widens() {
    let server = policy(serde_json::json!({
        "version": 1, "name": "server",
        "allowed_runtimes": ["python", "wasm", "node"],
        "allowed_networks": ["none", "localhost"],
        "limits": {"max_timeout_ms": 60000},
        "defaults": {"network": "localhost"}
    }));
    let explicit = policy(serde_json::json!({
        "version": 1,
        "allowed_runtimes": ["python", "ruby"],
        "minimum_isolation": "sandboxed",
        "limits": {"max_timeout_ms": 30000, "max_memory_bytes": 1024},
        "defaults": {"network": "none"}
    }));
    let effective = EffectivePolicy::compose(&[
        (PolicySourceKind::Explicit, explicit.clone()),
        (PolicySourceKind::Server, server.clone()),
    ]);
    let reversed = EffectivePolicy::compose(&[
        (PolicySourceKind::Server, server.clone()),
        (PolicySourceKind::Explicit, explicit.clone()),
    ]);
    assert_eq!(effective, reversed, "source order does not matter");
    let composed = &effective.policy;
    assert_eq!(composed.allowed_runtimes, Some(vec![RuntimeKind::Python]));
    assert_eq!(
        composed.allowed_networks,
        Some(vec![NetworkPolicy::Localhost, NetworkPolicy::None])
    );
    assert_eq!(
        composed.minimum_isolation,
        Some(IsolationProfile::Sandboxed)
    );
    assert_eq!(composed.limits.max_timeout_ms, Some(30000));
    assert_eq!(composed.limits.max_memory_bytes, Some(1024));
    assert_eq!(composed.defaults.network, Some(NetworkPolicy::None));
    assert_eq!(effective.sources.len(), 3);
    assert_eq!(effective.sources[0].kind, PolicySourceKind::Baseline);

    // Anything the effective policy admits, every source admits.
    let candidate = contract_with(|workload| {
        workload.isolation.profile = IsolationProfile::Sandboxed;
        workload.resources.wall_time = Some(std::time::Duration::from_millis(100));
        workload.resources.memory_bytes = Some(512);
    });
    for source in [composed, &server, &explicit, &Policy::baseline()] {
        assert!(codes(source, &candidate).is_empty());
    }
    // And anything a source denies, the effective policy denies.
    let networked = contract_with(|workload| workload.network = NetworkPolicy::Network);
    assert!(!codes(&server, &networked).is_empty());
    assert!(codes(composed, &networked).contains(&"network_denied".to_string()));

    // With no other source, the effective policy is the (unnamed) baseline.
    assert_eq!(
        EffectivePolicy::compose(&[]).policy,
        Policy {
            name: None,
            ..Policy::baseline()
        }
    );
}

#[test]
fn evaluation_contract_is_explicit() {
    let contract = contract();
    assert_eq!(contract.network, NetworkPolicy::None);
    assert_eq!(contract.isolation, IsolationProfile::Process);
    assert_eq!(contract.output_classes, [OutputClass::Stdio]);
    let json = serde_json::to_value(&contract).unwrap();
    assert_eq!(json["network"], "none");
    assert_eq!(json["isolation"], "process");
}
