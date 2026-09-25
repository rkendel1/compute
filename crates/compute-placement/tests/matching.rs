mod support;

use compute_core::{
    IsolationProfile, NetworkPolicy, PlatformIdentity, RuntimeDistribution, RuntimeKind,
    RuntimeLifecycleStatus,
};
use compute_placement::{
    DependencyRequirement, DistributionRequirement, ProviderDescriptor, ProviderKind, ReasonCode,
    SubmissionMode, match_provider,
};
use support::*;

fn codes(
    requirements: &compute_placement::PlacementRequirements,
    descriptor: &ProviderDescriptor,
) -> Vec<ReasonCode> {
    let matched = match_provider(requirements, descriptor);
    assert_eq!(matched.compatible, matched.reasons.is_empty());
    matched.codes()
}

fn python() -> ProviderDescriptor {
    Synthetic::new(ProviderKind::Remote, &[RuntimeKind::Python]).descriptor("python")
}

#[test]
fn exact_runtime_match_is_compatible() {
    let mut requirements = requirements(RuntimeKind::Python);
    requirements.runtime.version = Some("3.13".into());
    let matched = match_provider(&requirements, &python());
    assert!(matched.compatible, "{matched:?}");
    assert!(matched.reasons.is_empty());
}

#[test]
fn resource_availability_is_distinct_from_capacity_and_explained() {
    let mut provider = Synthetic::new(ProviderKind::Remote, &[RuntimeKind::Wasm]);
    provider.resources = compute_core::ProviderResourceInventory {
        capacity: compute_core::ResourceVector {
            cpu_count: 8,
            memory_bytes: 16 * 1024 * 1024 * 1024,
            disk_bytes: 20 * 1024 * 1024 * 1024,
        },
        available: compute_core::ResourceVector {
            cpu_count: 2,
            memory_bytes: 2 * 1024 * 1024 * 1024,
            disk_bytes: 3 * 1024 * 1024 * 1024,
        },
    };
    let mut required = wasm_requirements();
    required.resources.cpu_count = Some(4);
    required.resources.memory_bytes = Some(4 * 1024 * 1024 * 1024);
    required.resources.disk_bytes = Some(5 * 1024 * 1024 * 1024);

    let matched = match_provider(&required, &provider.descriptor("busy"));
    assert!(matched.compatible);
    assert!(matched.reasons.is_empty());
}

fn wasm_requirements() -> compute_placement::PlacementRequirements {
    let mut required = requirements(RuntimeKind::Wasm);
    required.isolation = IsolationProfile::Strict;
    required
}

#[test]
fn portable_architecture_aliases_match_and_mismatches_are_structured() {
    let mut provider = Synthetic::new(ProviderKind::Remote, &[RuntimeKind::Wasm]);
    provider.platform = "linux-aarch64".into();
    let descriptor = provider.descriptor("arm");
    let mut required = wasm_requirements();
    required.architecture = Some("arm64".into());
    assert!(match_provider(&required, &descriptor).compatible);

    required.architecture = Some("x86_64".into());
    assert_eq!(
        codes(&required, &descriptor),
        [ReasonCode::ArchitectureMismatch]
    );
}

#[test]
fn wrong_runtime_is_structured() {
    let matched = match_provider(&requirements(RuntimeKind::Node), &python());
    assert!(!matched.compatible);
    let reason = &matched.reasons[0];
    assert_eq!(reason.code, ReasonCode::RuntimeUnsupported);
    assert_eq!(reason.dimension, "runtime");
    assert_eq!(reason.required, serde_json::json!("node"));
    assert_eq!(reason.available, serde_json::json!(["python"]));
}

#[test]
fn installed_but_unavailable_runtime_is_distinct_from_unsupported() {
    let mut provider = Synthetic::new(ProviderKind::Remote, &[RuntimeKind::Python]);
    provider.unavailable = vec![RuntimeKind::Node];
    assert_eq!(
        codes(&requirements(RuntimeKind::Node), &provider.descriptor("p")),
        [ReasonCode::RuntimeUnavailable]
    );
}

#[test]
fn obtainable_runtime_is_compatible_before_it_is_installed() {
    let mut provider = Synthetic::new(ProviderKind::Remote, &[]);
    provider.unavailable = vec![RuntimeKind::Node];
    let mut capabilities = provider.capabilities("node-provider");
    let entry = capabilities
        .inventory
        .runtimes
        .iter_mut()
        .find(|entry| entry.id == RuntimeKind::Node)
        .unwrap();
    let mut distribution = RuntimeDistribution {
        id: String::new(),
        runtime: RuntimeKind::Node,
        version: entry.version.clone(),
        platform: PlatformIdentity {
            os: "linux".into(),
            architecture: "x86_64".into(),
            runtime_abi: None,
        },
        artifact: "https://example.invalid/node.tar.xz".into(),
        digest: compute_core::sha256_identity(b"node artifact"),
        source: "test catalog".into(),
        executable: entry.executable.clone(),
        capabilities: entry.capabilities.clone(),
    };
    distribution.id = distribution.canonical_id().unwrap();
    entry.lifecycle = Some(RuntimeLifecycleStatus::Available);
    entry.distribution = Some(distribution.clone());
    entry.compatible = true;
    let descriptor = ProviderDescriptor::from_capabilities(
        "node-provider",
        ProviderKind::Remote,
        &capabilities,
        availability(),
    )
    .unwrap();
    let matched = match_provider(&requirements(RuntimeKind::Node), &descriptor);
    assert!(matched.compatible, "{matched:?}");
    let offer = descriptor.runtime(RuntimeKind::Node).unwrap();
    assert_eq!(offer.lifecycle, RuntimeLifecycleStatus::Available);
    assert_eq!(offer.distribution.as_ref(), Some(&distribution));
}

#[test]
fn shell_does_not_claim_network_none_support() {
    let provider = Synthetic::new(ProviderKind::Remote, &[RuntimeKind::Shell]).descriptor("shell");
    let mut required = requirements(RuntimeKind::Shell);
    required.network = NetworkPolicy::None;
    let matched = match_provider(&required, &provider);
    assert!(!matched.compatible);
    assert_eq!(matched.codes(), [ReasonCode::NetworkUnsupported]);
    assert_eq!(matched.reasons[0].required, serde_json::json!("none"));
}

#[test]
fn wrong_runtime_version_is_never_substituted() {
    let mut requirements = requirements(RuntimeKind::Python);
    requirements.runtime.version = Some("3.12".into());
    let matched = match_provider(&requirements, &python());
    assert_eq!(matched.codes(), [ReasonCode::RuntimeVersionMismatch]);
    assert_eq!(matched.reasons[0].available, serde_json::json!("3.13.1"));
}

#[test]
fn runtime_artifact_identity_is_exact() {
    let mut provider = Synthetic::new(ProviderKind::Remote, &[RuntimeKind::Python]);
    provider
        .artifacts
        .insert(RuntimeKind::Python, ARTIFACT_P.into());
    let mut requirements = requirements(RuntimeKind::Python);
    requirements.runtime.artifact_id = Some(ARTIFACT_P.into());
    assert!(match_provider(&requirements, &provider.descriptor("p")).compatible);
    requirements.runtime.artifact_id = Some(CAPSULE_X.into());
    assert_eq!(
        codes(&requirements, &provider.descriptor("p")),
        [ReasonCode::RuntimeArtifactMismatch]
    );
}

#[test]
fn wrong_distribution_is_incompatible_even_with_the_same_runtime_version() {
    let mut requirements = requirements(RuntimeKind::Python);
    requirements.runtime.version = Some("3.13.1".into());
    requirements.distribution = Some(DistributionRequirement {
        id: DISTRIBUTION_B.into(),
    });
    let matched = match_provider(&requirements, &python());
    assert_eq!(matched.codes(), [ReasonCode::DistributionMismatch]);
    assert_eq!(
        matched.reasons[0].available,
        serde_json::json!(DISTRIBUTION_A)
    );

    requirements.distribution = Some(DistributionRequirement {
        id: DISTRIBUTION_A.into(),
    });
    assert!(match_provider(&requirements, &python()).compatible);

    let mut unknown = Synthetic::new(ProviderKind::Remote, &[RuntimeKind::Python]);
    unknown.distribution = None;
    assert_eq!(
        codes(&requirements, &unknown.descriptor("p")),
        [ReasonCode::DistributionMismatch]
    );
}

#[test]
fn wrong_architecture_and_os_are_reported_separately() {
    let mut requirements = requirements(RuntimeKind::Native);
    requirements.platform = Some(PlatformIdentity {
        os: "linux".into(),
        architecture: "aarch64".into(),
        runtime_abi: None,
    });
    let provider = Synthetic::new(ProviderKind::Remote, &[RuntimeKind::Native]).descriptor("p");
    assert_eq!(
        codes(&requirements, &provider),
        [ReasonCode::ArchitectureMismatch]
    );
    requirements.platform.as_mut().unwrap().os = "macos".into();
    assert_eq!(
        codes(&requirements, &provider),
        [
            ReasonCode::PlatformMismatch,
            ReasonCode::ArchitectureMismatch
        ]
    );
}

fn capsule(embedded: bool, id: &str) -> DependencyRequirement {
    DependencyRequirement {
        id: id.into(),
        format: compute_core::DEPENDENCY_CAPSULE_FORMAT.into(),
        embedded,
        runtime_version: Some("3.13.1".into()),
        platform: Some(PlatformIdentity {
            os: "linux".into(),
            architecture: "x86_64".into(),
            runtime_abi: None,
        }),
    }
}

#[test]
fn embedded_capsule_is_transferable_and_resident_capsule_is_present() {
    let mut requirements = requirements(RuntimeKind::Python);
    requirements.dependencies = Some(capsule(true, CAPSULE_X));
    assert!(match_provider(&requirements, &python()).compatible);

    let mut resident = Synthetic::new(ProviderKind::Remote, &[RuntimeKind::Python]);
    resident.resident = vec![CAPSULE_X.into()];
    requirements.dependencies = Some(capsule(false, CAPSULE_X));
    assert!(match_provider(&requirements, &resident.descriptor("p")).compatible);
}

#[test]
fn missing_dependency_capsule_has_no_host_fallback() {
    let mut requirements = requirements(RuntimeKind::Python);
    requirements.dependencies = Some(capsule(false, CAPSULE_X));
    assert_eq!(
        codes(&requirements, &python()),
        [ReasonCode::DependencyCapsuleMissing]
    );
}

#[test]
fn wrong_dependency_capsule_is_not_equivalent() {
    let mut provider = Synthetic::new(ProviderKind::Remote, &[RuntimeKind::Python]);
    provider.resident = vec![CAPSULE_Y.into()];
    let mut requirements = requirements(RuntimeKind::Python);
    requirements.dependencies = Some(capsule(false, CAPSULE_X));
    let matched = match_provider(&requirements, &provider.descriptor("p"));
    assert_eq!(matched.codes(), [ReasonCode::DependencyCapsuleMismatch]);
    assert_eq!(matched.reasons[0].available, serde_json::json!([CAPSULE_Y]));
}

#[test]
fn capsule_platform_and_runtime_version_must_match_the_provider() {
    let mut requirements = requirements(RuntimeKind::Python);
    let mut dependency = capsule(true, CAPSULE_X);
    dependency.runtime_version = Some("3.12.0".into());
    dependency.platform.as_mut().unwrap().architecture = "aarch64".into();
    requirements.dependencies = Some(dependency);
    assert_eq!(
        codes(&requirements, &python()),
        [
            ReasonCode::DependencyRuntimeMismatch,
            ReasonCode::DependencyPlatformMismatch
        ]
    );
}

#[test]
fn unsupported_isolation_lists_available_profiles() {
    let mut provider = Synthetic::new(ProviderKind::Remote, &[RuntimeKind::Wasm]);
    provider.isolation = vec![IsolationProfile::Process, IsolationProfile::Sandboxed];
    let mut requirements = requirements(RuntimeKind::Wasm);
    requirements.isolation = IsolationProfile::Strict;
    let matched = match_provider(&requirements, &provider.descriptor("p"));
    assert_eq!(matched.codes(), [ReasonCode::IsolationUnsupported]);
    assert_eq!(matched.reasons[0].required, serde_json::json!("strict"));
    assert_eq!(
        matched.reasons[0].available,
        serde_json::json!(["process", "sandboxed"])
    );
}

#[test]
fn runtime_that_cannot_enforce_strict_isolation_is_incompatible() {
    let mut requirements = requirements(RuntimeKind::Python);
    requirements.isolation = IsolationProfile::Strict;
    let matched = match_provider(&requirements, &python());
    assert_eq!(matched.codes(), [ReasonCode::IsolationUnsupported]);
    assert!(
        matched.reasons[0]
            .detail
            .as_deref()
            .unwrap()
            .starts_with("filesystem_isolation_unavailable")
    );
    assert_eq!(matched.reasons[0].available, serde_json::json!(["process"]));
}

#[test]
fn unsupported_network_is_never_relaxed() {
    let mut provider = Synthetic::new(ProviderKind::Remote, &[RuntimeKind::Wasm]);
    provider.network = vec![NetworkPolicy::Network];
    assert_eq!(
        codes(&requirements(RuntimeKind::Wasm), &provider.descriptor("p")),
        [ReasonCode::NetworkUnsupported]
    );
    let mut requirements = requirements(RuntimeKind::Python);
    requirements.network = NetworkPolicy::None;
    let matched = match_provider(&requirements, &python());
    assert_eq!(matched.codes(), [ReasonCode::NetworkUnsupported]);
    assert_eq!(matched.reasons[0].available, serde_json::json!(["network"]));
}

#[test]
fn insufficient_memory() {
    let mut provider = Synthetic::new(
        ProviderKind::Remote,
        &[RuntimeKind::Wasm, RuntimeKind::Python],
    );
    provider.max_memory_bytes = Some(1024);
    let descriptor = provider.descriptor("p");
    let mut requirements = requirements(RuntimeKind::Wasm);
    requirements.resources.memory_bytes = Some(512 * 1024 * 1024);
    requirements.resources.memory_limit_bytes = Some(512 * 1024 * 1024);
    assert_eq!(
        codes(&requirements, &descriptor),
        [ReasonCode::MemoryExceedsLimit]
    );
    requirements.resources.memory_bytes = Some(1024);
    requirements.resources.memory_limit_bytes = Some(1024);
    assert!(match_provider(&requirements, &descriptor).compatible);

    let mut python_requirements = crate::requirements(RuntimeKind::Python);
    python_requirements.resources.memory_bytes = Some(1024);
    python_requirements.resources.memory_limit_bytes = Some(1024);
    assert_eq!(
        codes(&python_requirements, &descriptor),
        [ReasonCode::MemoryUnenforceable]
    );
}

#[test]
fn insufficient_timeout() {
    let mut provider = Synthetic::new(ProviderKind::Remote, &[RuntimeKind::Python]);
    provider.max_timeout_ms = Some(10_000);
    let mut requirements = requirements(RuntimeKind::Python);
    requirements.resources.timeout_ms = Some(30_000);
    let matched = match_provider(&requirements, &provider.descriptor("p"));
    assert_eq!(matched.codes(), [ReasonCode::TimeoutExceedsLimit]);
    assert_eq!(matched.reasons[0].required, serde_json::json!(30_000));
    assert_eq!(matched.reasons[0].available, serde_json::json!(10_000));
}

#[test]
fn artifact_size_and_output_limits() {
    let mut provider = Synthetic::new(ProviderKind::Remote, &[RuntimeKind::Python]);
    provider.max_request_bytes = 1000;
    provider.max_output_bytes = 100;
    let mut requirements = requirements(RuntimeKind::Python);
    requirements.artifact.request_bytes = 1001;
    requirements.artifact.output_bytes = Some(101);
    assert_eq!(
        codes(&requirements, &provider.descriptor("p")),
        [ReasonCode::ArtifactTooLarge, ReasonCode::OutputExceedsLimit]
    );
}

#[test]
fn deployment_requires_a_provider_that_hosts_deployments() {
    // A job-capable provider that cannot host a durable application is
    // rejected for deployment; one that hosts deployments is not.
    let jobs_only = Synthetic::new(ProviderKind::Remote, &[RuntimeKind::Python]).descriptor("jobs");
    let mut hosting = Synthetic::new(ProviderKind::Remote, &[RuntimeKind::Python]);
    hosting.deployments = true;
    let hosting = hosting.descriptor("daemon");
    let mut requirements = requirements(RuntimeKind::Python);
    requirements.artifact.submission = SubmissionMode::Deployment;
    assert_eq!(
        codes(&requirements, &jobs_only),
        [ReasonCode::DeploymentUnsupported]
    );
    assert_eq!(ReasonCode::DeploymentUnsupported.dimension(), "deployment");
    assert!(match_provider(&requirements, &hosting).compatible);
    // Hosting deployments changes nothing for other submissions.
    requirements.artifact.submission = SubmissionMode::Synchronous;
    assert!(match_provider(&requirements, &jobs_only).compatible);
    assert!(match_provider(&requirements, &hosting).compatible);
}

#[test]
fn job_submission_requires_a_job_capable_provider() {
    let local = Synthetic::new(ProviderKind::Local, &[RuntimeKind::Python]).descriptor("local");
    let mut requirements = requirements(RuntimeKind::Python);
    assert!(match_provider(&requirements, &local).compatible);
    requirements.artifact.submission = SubmissionMode::Job;
    assert_eq!(codes(&requirements, &local), [ReasonCode::JobsUnsupported]);
}

#[test]
fn every_incompatibility_is_reported_not_just_the_first() {
    let mut provider = Synthetic::new(ProviderKind::Remote, &[RuntimeKind::Python]);
    provider.isolation = vec![IsolationProfile::Process];
    provider.max_timeout_ms = Some(1);
    let mut requirements = requirements(RuntimeKind::Python);
    requirements.isolation = IsolationProfile::Strict;
    requirements.resources.timeout_ms = Some(5);
    requirements.distribution = Some(DistributionRequirement {
        id: DISTRIBUTION_B.into(),
    });
    let codes = codes(&requirements, &provider.descriptor("p"));
    assert!(codes.contains(&ReasonCode::DistributionMismatch));
    assert!(codes.contains(&ReasonCode::IsolationUnsupported));
    assert!(codes.contains(&ReasonCode::TimeoutExceedsLimit));
}

#[test]
fn placement_is_runtime_neutral() {
    for kind in RuntimeKind::ALL {
        let provider = Synthetic::new(ProviderKind::Remote, &RuntimeKind::ALL).descriptor("all");
        let requirements = requirements(kind);
        let matched = match_provider(&requirements, &provider);
        assert!(matched.compatible, "{kind}: {matched:?}");
        let other = RuntimeKind::ALL
            .into_iter()
            .find(|other| *other != kind)
            .unwrap();
        let single = Synthetic::new(ProviderKind::Remote, &[other]).descriptor("single");
        assert_eq!(
            match_provider(&requirements, &single).codes(),
            [ReasonCode::RuntimeUnsupported],
            "{kind}"
        );
    }
}

#[test]
fn isolation_is_reported_even_when_network_also_fails() {
    let mut requirements = requirements(RuntimeKind::Python);
    requirements.isolation = IsolationProfile::Strict;
    requirements.network = NetworkPolicy::None;
    assert_eq!(
        codes(&requirements, &python()),
        [
            ReasonCode::IsolationUnsupported,
            ReasonCode::NetworkUnsupported
        ]
    );
}
