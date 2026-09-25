//! Canonical, validated provider descriptors.
//!
//! A descriptor is derived from the existing provider capability API
//! (`ProviderCapabilities`). Capability responses are untrusted input: every
//! field that can influence compatibility is validated, and contradictory
//! data is rejected rather than interpreted charitably.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use compute_core::{
    IsolationProfile, NetworkPolicy, PlatformIdentity, ProviderIdentity, ProviderResourceInventory,
    ResourceLimits, RuntimeCapabilities, RuntimeKind,
};
use compute_provider::{ProviderCapabilities, REMOTE_PROTOCOL};
use serde::{Deserialize, Serialize};

use crate::{LOCAL_PROTOCOL, canonical_identity};

pub const DESCRIPTOR_VERSION: &str = "compute.provider.descriptor@1";

/// Transport family of a configured provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Local,
    Remote,
}

impl ProviderKind {
    pub const fn protocol(self) -> &'static str {
        match self {
            Self::Local => LOCAL_PROTOCOL,
            Self::Remote => REMOTE_PROTOCOL,
        }
    }
}

impl std::fmt::Display for ProviderKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Local => "local",
            Self::Remote => "remote",
        })
    }
}

/// Observed provider health. Health is descriptive: it never makes an
/// incompatible provider compatible, and it only affects selection when the
/// pool is explicitly configured with `require_healthy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    Healthy,
    Unhealthy,
    Unknown,
}

impl std::fmt::Display for Health {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Healthy => "healthy",
            Self::Unhealthy => "unhealthy",
            Self::Unknown => "unknown",
        })
    }
}

/// A runtime the provider can execute right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeOffer {
    pub kind: RuntimeKind,
    /// Pinned version from the provider's runtime lock.
    pub version: String,
    /// Version the provider observed from the runtime itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detected_version: Option<String>,
    /// Content identity of the runtime artifact, as recorded in receipts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
    /// Whether this runtime is immediately executable or can be prepared.
    #[serde(default = "ready_lifecycle")]
    pub lifecycle: compute_core::RuntimeLifecycleStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distribution: Option<compute_core::RuntimeDistribution>,
    /// Isolation profiles this runtime can satisfy (without resource limits).
    pub isolation_profiles: Vec<IsolationProfile>,
    pub network_policies: Vec<NetworkPolicy>,
    /// The runtime adapter's declared capabilities, used verbatim for
    /// matching so that placement and execution apply the same rules.
    pub capabilities: RuntimeCapabilities,
}

fn ready_lifecycle() -> compute_core::RuntimeLifecycleStatus {
    compute_core::RuntimeLifecycleStatus::Ready
}

impl RuntimeOffer {
    /// The version string execution compares requested versions against.
    pub fn effective_version(&self) -> &str {
        self.detected_version.as_deref().unwrap_or(&self.version)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DistributionOffer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub platform: PlatformIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_memory_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyCapsuleSupport {
    pub formats: Vec<String>,
    /// Whether an embedded capsule can be transferred with the workload and
    /// verified by the provider.
    pub transfer: bool,
    /// Capsules already resident at the provider.
    pub resident: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactLimits {
    pub modes: Vec<String>,
    pub max_request_bytes: u64,
    pub max_output_bytes: u64,
    /// Whether the provider accepts durable asynchronous jobs.
    pub jobs: bool,
    /// Whether the provider hosts durable application deployments.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deployments: bool,
}

/// When the capability data was observed and until when it is valid.
/// Availability is excluded from `capability_version` and from placement
/// identity: it describes the observation, not the capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Availability {
    pub health: Health,
    pub fetched_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderDescriptor {
    pub descriptor_version: String,
    /// Caller-configured pool identifier.
    pub provider_id: String,
    pub provider_kind: ProviderKind,
    /// Identity the provider reports and binds into receipts.
    pub provider_identity: ProviderIdentity,
    pub protocol_version: String,
    /// Digest of stable capability fields. Health, observation timestamps,
    /// and currently available resources are excluded.
    pub capability_version: String,
    pub runtimes: Vec<RuntimeOffer>,
    /// Runtimes the provider knows but cannot execute right now.
    pub unavailable_runtimes: Vec<RuntimeKind>,
    pub distribution: DistributionOffer,
    pub isolation_profiles: Vec<IsolationProfile>,
    pub network_capabilities: Vec<NetworkPolicy>,
    pub resource_capabilities: ResourceCapabilities,
    pub resources: ProviderResourceInventory,
    pub dependency_capsules: DependencyCapsuleSupport,
    pub artifact_limits: ArtifactLimits,
    /// The provider's advertised execution policy, validated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<compute_policy::Policy>,
    pub availability: Availability,
}

/// Why a capability response could not be turned into a descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DescriptorError {
    /// Always `provider_capabilities_invalid`.
    pub code: String,
    pub field: String,
    pub message: String,
}

impl DescriptorError {
    fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: "provider_capabilities_invalid".into(),
            field: field.into(),
            message: message.into(),
        }
    }
}

fn invalid(field: impl Into<String>, message: impl Into<String>) -> DescriptorError {
    DescriptorError::new(field, message)
}

impl std::fmt::Display for DescriptorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}: {}", self.code, self.field, self.message)
    }
}

impl std::error::Error for DescriptorError {}

impl ProviderDescriptor {
    /// Validate a capability response and derive the canonical descriptor.
    pub fn from_capabilities(
        provider_id: &str,
        kind: ProviderKind,
        capabilities: &ProviderCapabilities,
        availability: Availability,
    ) -> Result<Self, DescriptorError> {
        if capabilities.protocol != kind.protocol() {
            return Err(invalid(
                "protocol",
                format!(
                    "{kind} provider must speak {}, reported {:?}",
                    kind.protocol(),
                    capabilities.protocol
                ),
            ));
        }
        validate_identity(kind, &capabilities.provider)?;
        if capabilities.inventory.compute_version.trim().is_empty() {
            return Err(invalid(
                "inventory.compute_version",
                "Compute version is empty",
            ));
        }
        let platform = parse_platform(&capabilities.inventory.platform)
            .ok_or_else(|| invalid("inventory.platform", "platform must be <os>-<architecture>"))?;
        if let Some(id) = &capabilities.distribution_id {
            compute_core::validate_sha256_identity(id)
                .map_err(|error| invalid("distribution_id", error.to_string()))?;
        }

        let isolation_profiles =
            unique_sorted(&capabilities.isolation_profiles, "isolation_profiles")?;
        let network_capabilities =
            unique_sorted(&capabilities.network_policies, "network_policies")?;

        for (field, values) in [
            ("artifact_modes", &capabilities.artifact_modes),
            (
                "dependency_capsule_formats",
                &capabilities.dependency_capsule_formats,
            ),
        ] {
            if values.iter().any(|value| !is_token(value)) {
                return Err(invalid(field, "contains a malformed value"));
            }
            if values.iter().collect::<BTreeSet<_>>().len() != values.len() {
                return Err(invalid(field, "contains duplicate values"));
            }
        }
        if !capabilities
            .artifact_modes
            .iter()
            .any(|mode| mode == "bundle")
        {
            return Err(invalid(
                "artifact_modes",
                "provider must accept portable bundles",
            ));
        }
        if capabilities.max_request_bytes == 0 || capabilities.max_output_bytes == 0 {
            return Err(invalid(
                "artifact_limits",
                "artifact limits must be positive",
            ));
        }
        if capabilities.max_timeout_ms == Some(0) || capabilities.max_memory_bytes == Some(0) {
            return Err(invalid(
                "resource_capabilities",
                "resource limits must be positive when present",
            ));
        }
        for (name, available, capacity) in [
            (
                "cpu_count",
                capabilities.resources.available.cpu_count,
                capabilities.resources.capacity.cpu_count,
            ),
            (
                "memory_bytes",
                capabilities.resources.available.memory_bytes,
                capabilities.resources.capacity.memory_bytes,
            ),
            (
                "disk_bytes",
                capabilities.resources.available.disk_bytes,
                capabilities.resources.capacity.disk_bytes,
            ),
        ] {
            if available > capacity {
                return Err(invalid(
                    format!("resources.available.{name}"),
                    format!("available {available} exceeds capacity {capacity}"),
                ));
            }
        }
        if capabilities.max_concurrent_jobs == Some(0) {
            return Err(invalid(
                "max_concurrent_jobs",
                "job concurrency must be positive when present",
            ));
        }
        for capsule in &capabilities.dependency_capsules {
            compute_core::validate_sha256_identity(capsule)
                .map_err(|error| invalid("dependency_capsules", error.to_string()))?;
        }
        let resident = capabilities
            .dependency_capsules
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        if resident.len() != capabilities.dependency_capsules.len() {
            return Err(invalid("dependency_capsules", "contains duplicate values"));
        }

        let mut seen = BTreeSet::new();
        let mut runtimes = vec![];
        let mut unavailable_runtimes = vec![];
        for entry in &capabilities.inventory.runtimes {
            let field = format!("inventory.runtimes.{}", entry.id);
            if !seen.insert(entry.id) {
                return Err(invalid(field, "runtime is listed more than once"));
            }
            if !is_version(&entry.version) {
                return Err(invalid(field, "pinned version is malformed"));
            }
            if let Some(version) = &entry.detected_version
                && !is_version(version)
            {
                return Err(invalid(field, "detected version is malformed"));
            }
            if !entry.platform.is_empty() && entry.platform != capabilities.inventory.platform {
                return Err(invalid(
                    field,
                    "runtime platform differs from the inventory platform",
                ));
            }
            if !entry.distribution_id.is_empty() {
                compute_core::validate_sha256_identity(&entry.distribution_id)
                    .map_err(|error| invalid(field.clone(), error.to_string()))?;
            }
            if !entry.distribution_runtime_id.is_empty() {
                compute_core::validate_sha256_identity(&entry.distribution_runtime_id)
                    .map_err(|error| invalid(field.clone(), error.to_string()))?;
            }
            if let Some(executable) = &entry.executable_identity {
                compute_core::validate_sha256_identity(executable)
                    .map_err(|error| invalid(field.clone(), error.to_string()))?;
            }
            if let Some(distribution) = &capabilities.distribution_id
                && !entry.distribution_id.is_empty()
                && distribution != &entry.distribution_id
            {
                return Err(invalid(
                    field,
                    "runtime distribution differs from the provider distribution",
                ));
            }
            if let Some(artifact) = capabilities.runtime_artifacts.get(&entry.id)
                && !entry.distribution_runtime_id.is_empty()
                && artifact != &entry.distribution_runtime_id
            {
                return Err(invalid(
                    field,
                    "runtime distribution identity contradicts runtime_artifacts",
                ));
            }
            validate_runtime_capabilities(&entry.capabilities)
                .map_err(|message| invalid(field.clone(), message))?;
            let lifecycle = entry
                .lifecycle
                .unwrap_or(if entry.available && entry.compatible {
                    compute_core::RuntimeLifecycleStatus::Installed
                } else {
                    compute_core::RuntimeLifecycleStatus::Unavailable
                });
            if let Some(distribution) = &entry.distribution {
                distribution
                    .validate()
                    .map_err(|error| invalid(field.clone(), error.to_string()))?;
                if distribution.runtime != entry.id
                    || distribution.version != entry.version
                    || distribution.platform.label() != capabilities.inventory.platform
                    || distribution.capabilities != entry.capabilities
                {
                    return Err(invalid(
                        field.clone(),
                        "runtime distribution contradicts the inventory entry",
                    ));
                }
            }
            if lifecycle.can_satisfy() && entry.compatible {
                let artifact_id = if entry.distribution_runtime_id.is_empty() {
                    capabilities.runtime_artifacts.get(&entry.id).cloned()
                } else {
                    Some(entry.distribution_runtime_id.clone())
                };
                runtimes.push(RuntimeOffer {
                    kind: entry.id,
                    version: entry.version.clone(),
                    detected_version: entry.detected_version.clone(),
                    artifact_id,
                    lifecycle,
                    distribution: entry.distribution.clone(),
                    isolation_profiles: supported_isolation(entry.id, &entry.capabilities),
                    network_policies: entry
                        .capabilities
                        .network
                        .iter()
                        .filter(|(_, capability)| capability.supported)
                        .map(|(policy, _)| policy.clone())
                        .collect(),
                    capabilities: entry.capabilities.clone(),
                });
            } else {
                unavailable_runtimes.push(entry.id);
            }
        }
        for (runtime, artifact) in &capabilities.runtime_artifacts {
            if !seen.contains(runtime) {
                return Err(invalid(
                    "runtime_artifacts",
                    format!("artifact identity for unlisted runtime {runtime}"),
                ));
            }
            compute_core::validate_sha256_identity(artifact)
                .map_err(|error| invalid("runtime_artifacts", error.to_string()))?;
        }
        runtimes.sort_by_key(|runtime| runtime.kind);
        unavailable_runtimes.sort();

        if let Some(policy) = &capabilities.policy {
            policy
                .validate()
                .map_err(|error| invalid("policy", error.to_string()))?;
        }
        let formats = capabilities.dependency_capsule_formats.clone();
        let mut descriptor = Self {
            descriptor_version: DESCRIPTOR_VERSION.into(),
            provider_id: provider_id.into(),
            provider_kind: kind,
            provider_identity: capabilities.provider.clone(),
            protocol_version: capabilities.protocol.clone(),
            capability_version: String::new(),
            runtimes,
            unavailable_runtimes,
            distribution: DistributionOffer {
                id: capabilities.distribution_id.clone(),
                platform,
            },
            isolation_profiles,
            network_capabilities,
            resource_capabilities: ResourceCapabilities {
                max_timeout_ms: capabilities.max_timeout_ms,
                max_memory_bytes: capabilities.max_memory_bytes,
            },
            resources: capabilities.resources.clone(),
            dependency_capsules: DependencyCapsuleSupport {
                transfer: formats
                    .iter()
                    .any(|format| format == compute_core::DEPENDENCY_CAPSULE_FORMAT),
                formats,
                resident: resident.into_iter().collect(),
            },
            artifact_limits: ArtifactLimits {
                modes: {
                    let mut modes = capabilities.artifact_modes.clone();
                    modes.sort();
                    modes
                },
                max_request_bytes: capabilities.max_request_bytes,
                max_output_bytes: capabilities.max_output_bytes,
                jobs: capabilities.max_concurrent_jobs.is_some(),
                deployments: capabilities.application_deployments,
            },
            policy: capabilities
                .policy
                .clone()
                .map(compute_policy::Policy::canonical),
            availability,
        };
        descriptor.capability_version = descriptor.compute_capability_version();
        Ok(descriptor)
    }

    /// Digest over every capability field, excluding availability and the
    /// digest itself.
    pub fn compute_capability_version(&self) -> String {
        #[derive(Serialize)]
        struct Body<'a> {
            descriptor_version: &'a str,
            provider_id: &'a str,
            provider_kind: ProviderKind,
            provider_identity: &'a ProviderIdentity,
            protocol_version: &'a str,
            runtimes: &'a [RuntimeOffer],
            unavailable_runtimes: &'a [RuntimeKind],
            distribution: &'a DistributionOffer,
            isolation_profiles: &'a [IsolationProfile],
            network_capabilities: &'a [NetworkPolicy],
            resource_capabilities: &'a ResourceCapabilities,
            resource_capacity: &'a compute_core::ResourceVector,
            dependency_capsules: &'a DependencyCapsuleSupport,
            artifact_limits: &'a ArtifactLimits,
            #[serde(skip_serializing_if = "Option::is_none")]
            policy: &'a Option<compute_policy::Policy>,
        }
        canonical_identity(&Body {
            descriptor_version: &self.descriptor_version,
            provider_id: &self.provider_id,
            provider_kind: self.provider_kind,
            provider_identity: &self.provider_identity,
            protocol_version: &self.protocol_version,
            runtimes: &self.runtimes,
            unavailable_runtimes: &self.unavailable_runtimes,
            distribution: &self.distribution,
            isolation_profiles: &self.isolation_profiles,
            network_capabilities: &self.network_capabilities,
            resource_capabilities: &self.resource_capabilities,
            resource_capacity: &self.resources.capacity,
            dependency_capsules: &self.dependency_capsules,
            artifact_limits: &self.artifact_limits,
            policy: &self.policy,
        })
    }

    pub fn runtime(&self, kind: RuntimeKind) -> Option<&RuntimeOffer> {
        self.runtimes.iter().find(|runtime| runtime.kind == kind)
    }
}

fn validate_identity(
    kind: ProviderKind,
    identity: &ProviderIdentity,
) -> Result<(), DescriptorError> {
    let text_ok = |value: &str| {
        !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
    };
    match (kind, identity) {
        (ProviderKind::Local, ProviderIdentity::Local { id }) if text_ok(id) => Ok(()),
        (ProviderKind::Remote, ProviderIdentity::Remote { id, endpoint })
            if text_ok(id)
                && text_ok(endpoint)
                && (endpoint.starts_with("http://") || endpoint.starts_with("https://")) =>
        {
            Ok(())
        }
        (ProviderKind::Local, ProviderIdentity::Remote { .. })
        | (ProviderKind::Remote, ProviderIdentity::Local { .. }) => Err(invalid(
            "provider",
            format!("configured {kind} provider reported a different provider kind"),
        )),
        _ => Err(invalid("provider", "provider identity is malformed")),
    }
}

/// Reject self-contradictory runtime capability declarations. An adapter
/// that claims a boundary must also claim the capability it rests on.
fn validate_runtime_capabilities(capabilities: &RuntimeCapabilities) -> Result<(), String> {
    let isolation = &capabilities.isolation;
    let pairs = [
        (
            "filesystem boundary",
            isolation.filesystem_boundary,
            capabilities.filesystem_isolation.supported,
        ),
        (
            "timeout enforcement",
            isolation.timeout_enforcement,
            capabilities.timeout.supported,
        ),
        (
            "memory enforcement",
            isolation.memory_enforcement,
            capabilities.memory_limit.supported,
        ),
        (
            "CPU enforcement",
            isolation.cpu_enforcement,
            capabilities.cpu_limit.supported,
        ),
        (
            "process enforcement",
            isolation.process_enforcement,
            capabilities.process_limit.supported,
        ),
    ];
    for (name, boundary, capability) in pairs {
        if boundary != capability {
            return Err(format!("{name} contradicts the declared capability"));
        }
    }
    if isolation.network_boundary
        && !capabilities
            .network
            .get(&NetworkPolicy::None)
            .is_some_and(|value| value.supported)
    {
        return Err("network boundary is claimed but network none is unsupported".into());
    }
    if !capabilities.network.values().any(|value| value.supported) {
        return Err("no network policy is supported".into());
    }
    if !isolation.process_boundary {
        return Err("process boundary is required for every runtime".into());
    }
    Ok(())
}

/// Isolation profiles a runtime can satisfy, computed with the same function
/// execution uses.
fn supported_isolation(
    kind: RuntimeKind,
    capabilities: &RuntimeCapabilities,
) -> Vec<IsolationProfile> {
    IsolationProfile::ALL
        .into_iter()
        .filter(|profile| {
            capabilities.network.iter().any(|(network, capability)| {
                capability.supported
                    && capabilities
                        .resolve_isolation(
                            kind,
                            &probe(kind, *profile, network.clone(), ResourceLimits::default()),
                        )
                        .is_ok()
            })
        })
        .collect()
}

/// A minimal materialized request carrying only the fields capability
/// resolution reads.
pub(crate) fn probe(
    kind: RuntimeKind,
    isolation: IsolationProfile,
    network: NetworkPolicy,
    resources: ResourceLimits,
) -> compute_core::ExecutionRequest {
    probe_on_host(
        kind,
        isolation,
        compute_core::HostProfile::Trusted,
        network,
        resources,
    )
}

/// A probe under a host profile.
pub(crate) fn probe_on_host(
    kind: RuntimeKind,
    isolation: IsolationProfile,
    host_isolation: compute_core::HostProfile,
    network: NetworkPolicy,
    resources: ResourceLimits,
) -> compute_core::ExecutionRequest {
    compute_core::ExecutionRequest {
        runtime: compute_core::RuntimeSpec {
            kind,
            version: None,
        },
        entrypoint: "placement-probe".into(),
        args: vec![],
        stdin: vec![],
        env: vec![],
        inputs: vec![],
        outputs: vec![],
        mounts: vec![],
        network,
        resources,
        isolation,
        host_isolation,
        dependencies: None,
    }
}

fn unique_sorted<T: Ord + Clone>(values: &[T], field: &str) -> Result<Vec<T>, DescriptorError> {
    let set = values.iter().cloned().collect::<BTreeSet<_>>();
    if set.len() != values.len() {
        return Err(DescriptorError::new(field, "contains duplicate values"));
    }
    if set.is_empty() {
        return Err(DescriptorError::new(field, "must not be empty"));
    }
    Ok(set.into_iter().collect())
}

fn is_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'@' | b'-' | b'_'))
}

/// Runtime-reported versions are free text (some runtimes print several
/// lines), so only emptiness, size, and non-whitespace control characters
/// are rejected.
fn is_version(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= 1024
        && !value
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
}

/// Parse `<os>-<architecture>` as Compute reports it (`linux-x86_64`).
pub fn parse_platform(value: &str) -> Option<PlatformIdentity> {
    let (os, architecture) = value.split_once('-')?;
    let valid = |part: &str| {
        !part.is_empty()
            && part.len() <= 32
            && part
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    };
    (valid(os) && valid(architecture)).then(|| PlatformIdentity {
        os: os.into(),
        architecture: architecture.into(),
        runtime_abi: None,
    })
}

/// Group runtime offers by kind, used for inspection output.
pub fn runtime_summary(descriptor: &ProviderDescriptor) -> BTreeMap<RuntimeKind, String> {
    descriptor
        .runtimes
        .iter()
        .map(|runtime| (runtime.kind, runtime.effective_version().to_string()))
        .collect()
}
