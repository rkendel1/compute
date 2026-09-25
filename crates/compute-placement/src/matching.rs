//! Capability matching: a provider is compatible with a set of requirements,
//! or it is incompatible for structured, specific reasons.

use compute_core::RuntimeKind;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::descriptor::{ProviderDescriptor, probe};
use crate::requirements::{PlacementRequirements, SubmissionMode};

/// Stable, machine-readable incompatibility codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    RuntimeUnsupported,
    RuntimeUnavailable,
    RuntimeVersionMismatch,
    RuntimeArtifactMismatch,
    DistributionMismatch,
    PlatformMismatch,
    ArchitectureMismatch,
    DependencyFormatUnsupported,
    DependencyCapsuleMissing,
    DependencyCapsuleMismatch,
    DependencyRuntimeMismatch,
    DependencyPlatformMismatch,
    IsolationUnsupported,
    NetworkUnsupported,
    TimeoutUnenforceable,
    TimeoutExceedsLimit,
    MemoryUnenforceable,
    MemoryExceedsLimit,
    CpuUnavailable,
    MemoryUnavailable,
    DiskUnavailable,
    CpuLimitUnenforceable,
    ProcessLimitUnenforceable,
    OutputLimitUnenforceable,
    ArtifactModeUnsupported,
    ArtifactTooLarge,
    OutputExceedsLimit,
    JobsUnsupported,
    DeploymentUnsupported,
    RunUnsupported,
}

impl ReasonCode {
    pub const fn dimension(self) -> &'static str {
        use ReasonCode::*;
        match self {
            RuntimeUnsupported
            | RuntimeUnavailable
            | RuntimeVersionMismatch
            | RuntimeArtifactMismatch => "runtime",
            DistributionMismatch => "distribution",
            PlatformMismatch | ArchitectureMismatch => "platform",
            DependencyFormatUnsupported
            | DependencyCapsuleMissing
            | DependencyCapsuleMismatch
            | DependencyRuntimeMismatch
            | DependencyPlatformMismatch => "dependencies",
            IsolationUnsupported => "isolation",
            NetworkUnsupported => "network",
            TimeoutUnenforceable
            | TimeoutExceedsLimit
            | MemoryUnenforceable
            | MemoryExceedsLimit
            | CpuUnavailable
            | MemoryUnavailable
            | DiskUnavailable
            | CpuLimitUnenforceable
            | ProcessLimitUnenforceable
            | OutputLimitUnenforceable => "resources",
            ArtifactModeUnsupported | ArtifactTooLarge | OutputExceedsLimit => "artifact",
            DeploymentUnsupported | RunUnsupported | JobsUnsupported => "execution",
        }
    }

    pub fn as_str(self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IncompatibilityReason {
    pub code: ReasonCode,
    pub dimension: String,
    pub required: Value,
    pub available: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityMatch {
    pub compatible: bool,
    pub reasons: Vec<IncompatibilityReason>,
}

impl CapabilityMatch {
    pub fn codes(&self) -> Vec<ReasonCode> {
        self.reasons.iter().map(|reason| reason.code).collect()
    }
}

struct Reasons(Vec<IncompatibilityReason>);

impl Reasons {
    fn push(
        &mut self,
        code: ReasonCode,
        required: Value,
        available: Value,
        detail: Option<String>,
    ) {
        self.0.push(IncompatibilityReason {
            code,
            dimension: code.dimension().into(),
            required,
            available,
            detail,
        });
    }
}

/// Evaluate every matching dimension and report every incompatibility found.
pub fn match_provider(
    requirements: &PlacementRequirements,
    descriptor: &ProviderDescriptor,
) -> CapabilityMatch {
    let mut reasons = Reasons(vec![]);
    let runtime_kind = requirements.runtime.kind;
    let offer = descriptor.runtime(runtime_kind);

    // Runtime.
    match offer {
        None if descriptor.unavailable_runtimes.contains(&runtime_kind) => reasons.push(
            ReasonCode::RuntimeUnavailable,
            json!(runtime_kind),
            json!(offered_runtimes(descriptor)),
            Some(format!(
                "{runtime_kind} is installed but not currently executable"
            )),
        ),
        None => reasons.push(
            ReasonCode::RuntimeUnsupported,
            json!(runtime_kind),
            json!(offered_runtimes(descriptor)),
            None,
        ),
        Some(offer) => {
            if let Some(version) = &requirements.runtime.version
                && !runtime_version_matches(runtime_kind, version, offer.effective_version())
            {
                reasons.push(
                    ReasonCode::RuntimeVersionMismatch,
                    json!(version),
                    json!(offer.effective_version()),
                    None,
                );
            }
            if let Some(artifact) = &requirements.runtime.artifact_id
                && offer.artifact_id.as_ref() != Some(artifact)
            {
                reasons.push(
                    ReasonCode::RuntimeArtifactMismatch,
                    json!(artifact),
                    json!(offer.artifact_id),
                    None,
                );
            }
        }
    }

    // Distribution: identity is authoritative; runtime versions are not a
    // substitute for it.
    if let Some(distribution) = &requirements.distribution
        && descriptor.distribution.id.as_ref() != Some(&distribution.id)
    {
        reasons.push(
            ReasonCode::DistributionMismatch,
            json!(distribution.id),
            json!(descriptor.distribution.id),
            None,
        );
    }

    // Platform.
    let provider_platform = &descriptor.distribution.platform;
    if let Some(platform) = &requirements.platform {
        if platform.os != provider_platform.os {
            reasons.push(
                ReasonCode::PlatformMismatch,
                json!(platform.os),
                json!(provider_platform.os),
                None,
            );
        }
        if platform.architecture != provider_platform.architecture {
            reasons.push(
                ReasonCode::ArchitectureMismatch,
                json!(platform.architecture),
                json!(provider_platform.architecture),
                None,
            );
        }
    }
    if let Some(architecture) = &requirements.architecture
        && !architecture_matches(architecture, &provider_platform.architecture)
    {
        reasons.push(
            ReasonCode::ArchitectureMismatch,
            json!(architecture),
            json!(provider_platform.architecture),
            None,
        );
    }

    // Dependencies: capsule identity is authoritative.
    if let Some(dependencies) = &requirements.dependencies {
        let support = &descriptor.dependency_capsules;
        let resident = support.resident.contains(&dependencies.id);
        if !support.formats.contains(&dependencies.format) {
            reasons.push(
                ReasonCode::DependencyFormatUnsupported,
                json!(dependencies.format),
                json!(support.formats),
                None,
            );
        } else if !(resident || dependencies.embedded && support.transfer) {
            let code = if support.resident.is_empty() {
                ReasonCode::DependencyCapsuleMissing
            } else {
                ReasonCode::DependencyCapsuleMismatch
            };
            reasons.push(
                code,
                json!(dependencies.id),
                json!(support.resident),
                Some(
                    "the capsule is neither embedded for transfer nor resident at the provider"
                        .into(),
                ),
            );
        }
        if let Some(platform) = &dependencies.platform
            && (platform.os != provider_platform.os
                || platform.architecture != provider_platform.architecture)
        {
            reasons.push(
                ReasonCode::DependencyPlatformMismatch,
                json!(platform.label()),
                json!(provider_platform.label()),
                None,
            );
        }
        if let (Some(version), Some(offer)) = (&dependencies.runtime_version, offer)
            && !compute_core::same_runtime_version(version, offer.effective_version())
        {
            reasons.push(
                ReasonCode::DependencyRuntimeMismatch,
                json!(format!("{runtime_kind}@{version}")),
                json!(format!("{runtime_kind}@{}", offer.effective_version())),
                None,
            );
        }
    }

    // Isolation.
    if !descriptor
        .isolation_profiles
        .contains(&requirements.isolation)
    {
        reasons.push(
            ReasonCode::IsolationUnsupported,
            json!(requirements.isolation),
            json!(descriptor.isolation_profiles),
            Some("the provider does not offer this isolation profile".into()),
        );
    }

    // Host isolation: a process runtime's operating-system boundary.
    if !requirements.host.is_trusted()
        && offer.is_some_and(|offer| {
            !offer
                .capabilities
                .host_profiles
                .contains(&requirements.host)
        })
    {
        reasons.push(
            ReasonCode::IsolationUnsupported,
            json!(requirements.host),
            json!(offer.map(|offer| offer.capabilities.host_profiles.clone())),
            Some(format!(
                "{runtime_kind} on this provider cannot run under host isolation {}",
                requirements.host
            )),
        );
    }

    // Network.
    if !descriptor
        .network_capabilities
        .contains(&requirements.network)
    {
        reasons.push(
            ReasonCode::NetworkUnsupported,
            json!(requirements.network),
            json!(descriptor.network_capabilities),
            Some("the provider does not offer this network policy".into()),
        );
    } else if let Some(offer) = offer
        && !offer.network_policies.contains(&requirements.network)
        && !host_offered(Some(offer), requirements)
    {
        reasons.push(
            ReasonCode::NetworkUnsupported,
            json!(requirements.network),
            json!(offer.network_policies),
            Some(format!("{runtime_kind} cannot enforce this network policy")),
        );
    }

    // Resources.
    let resources = &requirements.resources;
    let limits = &descriptor.resource_capabilities;
    let inventory = &descriptor.resources;
    if let Some(cpu) = resources.cpu_count
        && u64::from(cpu) > inventory.capacity.cpu_count
    {
        reasons.push(
            ReasonCode::CpuUnavailable,
            json!(cpu),
            json!({"capacity": inventory.capacity.cpu_count}),
            Some(format!(
                "cpu requires {cpu}, provider capacity is {}",
                inventory.capacity.cpu_count
            )),
        );
    }
    if let Some(memory) = resources.memory_bytes
        && memory > inventory.capacity.memory_bytes
    {
        reasons.push(
            ReasonCode::MemoryUnavailable,
            json!(memory),
            json!({"capacity": inventory.capacity.memory_bytes}),
            Some(format!(
                "memory requires {memory} bytes, provider capacity is {} bytes",
                inventory.capacity.memory_bytes
            )),
        );
    }
    if let Some(disk) = resources.disk_bytes
        && disk > inventory.capacity.disk_bytes
    {
        reasons.push(
            ReasonCode::DiskUnavailable,
            json!(disk),
            json!({"capacity": inventory.capacity.disk_bytes}),
            Some(format!(
                "disk requires {disk} bytes, provider capacity is {} bytes",
                inventory.capacity.disk_bytes
            )),
        );
    }
    if let Some(timeout) = resources.timeout_ms {
        if let Some(maximum) = limits.max_timeout_ms
            && timeout > maximum
        {
            reasons.push(
                ReasonCode::TimeoutExceedsLimit,
                json!(timeout),
                json!(maximum),
                None,
            );
        }
        if offer.is_some_and(|offer| !offer.capabilities.timeout.supported) {
            reasons.push(
                ReasonCode::TimeoutUnenforceable,
                json!(timeout),
                json!(null),
                None,
            );
        }
    }
    if let Some(memory) = resources.memory_limit_bytes {
        if let Some(maximum) = limits.max_memory_bytes
            && memory > maximum
        {
            reasons.push(
                ReasonCode::MemoryExceedsLimit,
                json!(memory),
                json!(maximum),
                None,
            );
        }
        if offer.is_some_and(|offer| {
            !offer.capabilities.memory_limit.supported && !host_offered(Some(offer), requirements)
        }) {
            reasons.push(
                ReasonCode::MemoryUnenforceable,
                json!(memory),
                json!(null),
                Some(format!("{runtime_kind} cannot enforce a memory limit")),
            );
        }
    }
    if let Some(offer) = offer {
        let capabilities = &offer.capabilities;
        if let Some(value) = resources.cpu_time_ms
            && !capabilities.cpu_limit.supported
            && !host_offered(Some(offer), requirements)
        {
            reasons.push(
                ReasonCode::CpuLimitUnenforceable,
                json!(value),
                json!(null),
                None,
            );
        }
        if let Some(value) = resources.process_count
            && !capabilities.process_limit.supported
            && !host_offered(Some(offer), requirements)
        {
            reasons.push(
                ReasonCode::ProcessLimitUnenforceable,
                json!(value),
                json!(null),
                None,
            );
        }
        if (resources.stdout_bytes.is_some() && !capabilities.stdout_limit.supported)
            || (resources.stderr_bytes.is_some() && !capabilities.stderr_limit.supported)
        {
            reasons.push(
                ReasonCode::OutputLimitUnenforceable,
                json!({"stdout_bytes": resources.stdout_bytes, "stderr_bytes": resources.stderr_bytes}),
                json!(null),
                None,
            );
        }
        // Runtime-level isolation, evaluated on its own so it is reported
        // even when network or resource requirements also fail.
        if descriptor
            .isolation_profiles
            .contains(&requirements.isolation)
            && !offer.isolation_profiles.contains(&requirements.isolation)
        {
            let boundary = offer
                .network_policies
                .first()
                .and_then(|network| {
                    capabilities
                        .resolve_isolation(
                            runtime_kind,
                            &probe(
                                runtime_kind,
                                requirements.isolation,
                                network.clone(),
                                Default::default(),
                            ),
                        )
                        .err()
                })
                .map(|rejection| format!("{}: {}", rejection.code, rejection.message));
            reasons.push(
                ReasonCode::IsolationUnsupported,
                json!(requirements.isolation),
                json!(offer.isolation_profiles),
                boundary,
            );
        }
        // Safety net: the complete request, resolved by the function
        // execution uses, so placement never admits what execution rejects.
        if reasons.0.is_empty()
            && let Err(rejection) = capabilities.resolve_isolation(
                runtime_kind,
                &crate::descriptor::probe_on_host(
                    runtime_kind,
                    requirements.isolation,
                    requirements.host,
                    requirements.network.clone(),
                    resources.to_limits(),
                ),
            )
        {
            reasons.push(
                ReasonCode::IsolationUnsupported,
                json!(requirements.isolation),
                json!(offer.isolation_profiles),
                Some(format!("{}: {}", rejection.code, rejection.message)),
            );
        }
    }

    // Artifact transport.
    let artifact = &requirements.artifact;
    let transport = &descriptor.artifact_limits;
    if !transport.modes.contains(&artifact.mode) {
        reasons.push(
            ReasonCode::ArtifactModeUnsupported,
            json!(artifact.mode),
            json!(transport.modes),
            None,
        );
    }
    if artifact.request_bytes > transport.max_request_bytes {
        reasons.push(
            ReasonCode::ArtifactTooLarge,
            json!(artifact.request_bytes),
            json!(transport.max_request_bytes),
            None,
        );
    }
    if let Some(output) = artifact.output_bytes
        && output > transport.max_output_bytes
    {
        reasons.push(
            ReasonCode::OutputExceedsLimit,
            json!(output),
            json!(transport.max_output_bytes),
            None,
        );
    }
    // Execution mode: each submission needs exactly the mode that serves it.
    let offered = [
        (transport.run, "run"),
        (transport.jobs, "jobs"),
        (transport.deployments, "deployments"),
    ]
    .into_iter()
    .filter_map(|(offered, mode)| offered.then_some(mode))
    .collect::<Vec<_>>();
    let missing = match artifact.submission {
        SubmissionMode::Synchronous if !transport.run => Some((
            ReasonCode::RunUnsupported,
            "run",
            "the provider does not run workloads on request",
        )),
        SubmissionMode::Job if !transport.jobs => Some((
            ReasonCode::JobsUnsupported,
            "jobs",
            "the provider does not accept durable asynchronous jobs",
        )),
        SubmissionMode::Deployment if !transport.deployments => Some((
            ReasonCode::DeploymentUnsupported,
            "deployments",
            "the provider does not host application deployments",
        )),
        _ => None,
    };
    if let Some((code, required, detail)) = missing {
        reasons.push(code, json!(required), json!(offered), Some(detail.into()));
    }

    let mut reasons = reasons.0;
    reasons.sort_by(|left, right| left.code.cmp(&right.code));
    reasons.dedup();
    CapabilityMatch {
        compatible: reasons.is_empty(),
        reasons,
    }
}

/// Version matching mirrors the runtime adapters: WASM accepts only `wasi`,
/// process runtimes require the observed version to contain the request.
/// Whether the runtime offers the host profile the requirements ask for,
/// in which case the operating system enforces the network and limits the
/// runtime cannot; the full request is resolved as execution resolves it.
fn host_offered(offer: Option<&crate::RuntimeOffer>, requirements: &PlacementRequirements) -> bool {
    !requirements.host.is_trusted()
        && offer.is_some_and(|offer| {
            offer
                .capabilities
                .host_profiles
                .contains(&requirements.host)
        })
}

pub fn runtime_version_matches(kind: RuntimeKind, requested: &str, offered: &str) -> bool {
    compute_core::runtime_version_matches(kind, requested, offered)
}

fn offered_runtimes(descriptor: &ProviderDescriptor) -> Vec<RuntimeKind> {
    descriptor
        .runtimes
        .iter()
        .map(|runtime| runtime.kind)
        .collect()
}

fn architecture_matches(required: &str, available: &str) -> bool {
    fn canonical(value: &str) -> String {
        match value.to_ascii_lowercase().as_str() {
            "arm64" | "aarch64" => "arm64".into(),
            "x86_64" | "amd64" => "x86_64".into(),
            other => other.into(),
        }
    }
    canonical(required) == canonical(available)
}
