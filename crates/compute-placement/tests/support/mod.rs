#![allow(dead_code)]

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, TimeZone, Utc};
use compute_core::{
    IsolationProfile, NetworkPolicy, ProviderIdentity, RuntimeCapabilities, RuntimeInventory,
    RuntimeInventoryEntry, RuntimeKind, RuntimeSource,
};
use compute_placement::{
    ArtifactRequirement, Availability, DiscoveryRecord, DiscoveryStatus, Health,
    PlacementRequirements, PoolConfig, ProviderConfig, ProviderDescriptor, ProviderKind,
    REQUIREMENTS_VERSION, ResourceRequirement, RuntimeRequirement, SubmissionMode,
};
use compute_provider::ProviderCapabilities;

pub const DISTRIBUTION_A: &str =
    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub const DISTRIBUTION_B: &str =
    "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
pub const CAPSULE_X: &str =
    "sha256:1111111111111111111111111111111111111111111111111111111111111111";
pub const CAPSULE_Y: &str =
    "sha256:2222222222222222222222222222222222222222222222222222222222222222";
pub const ARTIFACT_P: &str =
    "sha256:3333333333333333333333333333333333333333333333333333333333333333";

pub fn fixed_now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
}

pub fn capabilities_for(kind: RuntimeKind) -> RuntimeCapabilities {
    match kind {
        RuntimeKind::Wasm => RuntimeCapabilities::wasm(),
        RuntimeKind::Deno => RuntimeCapabilities::deno(),
        _ => RuntimeCapabilities::process(),
    }
}

pub fn version_for(kind: RuntimeKind) -> &'static str {
    match kind {
        RuntimeKind::Wasm => "wasi",
        RuntimeKind::Python => "3.13.1",
        RuntimeKind::Node => "22.12.0",
        _ => "1.0.0",
    }
}

/// A synthetic provider offering `runtimes` with real adapter capabilities.
pub struct Synthetic {
    pub kind: ProviderKind,
    pub runtimes: Vec<RuntimeKind>,
    pub isolation: Vec<IsolationProfile>,
    pub network: Vec<NetworkPolicy>,
    pub distribution: Option<String>,
    pub platform: String,
    pub resident: Vec<String>,
    pub max_timeout_ms: Option<u64>,
    pub max_memory_bytes: Option<u64>,
    pub max_request_bytes: u64,
    pub max_output_bytes: u64,
    pub jobs: bool,
    pub artifacts: BTreeMap<RuntimeKind, String>,
    pub unavailable: Vec<RuntimeKind>,
    pub policy: Option<compute_policy::Policy>,
    pub resources: compute_core::ProviderResourceInventory,
}

impl Synthetic {
    pub fn new(kind: ProviderKind, runtimes: &[RuntimeKind]) -> Self {
        Self {
            kind,
            runtimes: runtimes.to_vec(),
            isolation: IsolationProfile::ALL.to_vec(),
            network: vec![
                NetworkPolicy::None,
                NetworkPolicy::Localhost,
                NetworkPolicy::Network,
            ],
            distribution: Some(DISTRIBUTION_A.into()),
            platform: "linux-x86_64".into(),
            resident: vec![],
            max_timeout_ms: None,
            max_memory_bytes: None,
            max_request_bytes: 64 * 1024 * 1024,
            max_output_bytes: 16 * 1024 * 1024,
            jobs: kind == ProviderKind::Remote,
            artifacts: BTreeMap::new(),
            unavailable: vec![],
            policy: None,
            resources: compute_core::ProviderResourceInventory {
                capacity: compute_core::ResourceVector {
                    cpu_count: 8,
                    memory_bytes: 16 * 1024 * 1024 * 1024,
                    disk_bytes: 100 * 1024 * 1024 * 1024,
                },
                available: compute_core::ResourceVector {
                    cpu_count: 8,
                    memory_bytes: 16 * 1024 * 1024 * 1024,
                    disk_bytes: 100 * 1024 * 1024 * 1024,
                },
            },
        }
    }

    pub fn clone_for_test(&self) -> Self {
        Self {
            kind: self.kind,
            runtimes: self.runtimes.clone(),
            isolation: self.isolation.clone(),
            network: self.network.clone(),
            distribution: self.distribution.clone(),
            platform: self.platform.clone(),
            resident: self.resident.clone(),
            max_timeout_ms: self.max_timeout_ms,
            max_memory_bytes: self.max_memory_bytes,
            max_request_bytes: self.max_request_bytes,
            max_output_bytes: self.max_output_bytes,
            jobs: self.jobs,
            artifacts: self.artifacts.clone(),
            unavailable: self.unavailable.clone(),
            policy: self.policy.clone(),
            resources: self.resources.clone(),
        }
    }

    pub fn capabilities(&self, id: &str) -> ProviderCapabilities {
        let mut entries = self
            .runtimes
            .iter()
            .map(|kind| {
                let artifact = self.artifacts.get(kind).cloned().unwrap_or_else(|| {
                    compute_core::sha256_identity(format!("runtime:{kind}").as_bytes())
                });
                RuntimeInventoryEntry {
                    id: *kind,
                    version: version_for(*kind).into(),
                    platform: self.platform.clone(),
                    distribution_id: self
                        .distribution
                        .clone()
                        .unwrap_or_else(|| DISTRIBUTION_A.into()),
                    distribution_runtime_id: artifact.clone(),
                    executable_identity: Some(artifact),
                    lifecycle: Some(compute_core::RuntimeLifecycleStatus::Ready),
                    distribution: None,
                    executable: format!("runtimes/{kind}/bin/{kind}"),
                    available: true,
                    compatible: true,
                    detected_version: Some(version_for(*kind).into()),
                    detected_executable: None,
                    source: RuntimeSource::Distribution,
                    capabilities: capabilities_for(*kind),
                    remediation: None,
                }
            })
            .collect::<Vec<_>>();
        for kind in &self.unavailable {
            let artifact = self.artifacts.get(kind).cloned().unwrap_or_else(|| {
                compute_core::sha256_identity(format!("runtime:{kind}").as_bytes())
            });
            entries.push(RuntimeInventoryEntry {
                id: *kind,
                version: version_for(*kind).into(),
                platform: self.platform.clone(),
                distribution_id: self
                    .distribution
                    .clone()
                    .unwrap_or_else(|| DISTRIBUTION_A.into()),
                distribution_runtime_id: artifact,
                executable_identity: None,
                lifecycle: Some(compute_core::RuntimeLifecycleStatus::Unavailable),
                distribution: None,
                executable: format!("runtimes/{kind}/bin/{kind}"),
                available: false,
                compatible: false,
                detected_version: None,
                detected_executable: None,
                source: RuntimeSource::Unavailable,
                capabilities: capabilities_for(*kind),
                remediation: Some("install it".into()),
            });
        }
        ProviderCapabilities {
            protocol: self.kind.protocol().into(),
            provider: match self.kind {
                ProviderKind::Local => ProviderIdentity::Local { id: "local".into() },
                ProviderKind::Remote => ProviderIdentity::Remote {
                    id: format!("https://{id}.example"),
                    endpoint: format!("https://{id}.example"),
                },
            },
            artifact_modes: vec!["bundle".into(), "inline".into()],
            isolation_profiles: self.isolation.clone(),
            network_policies: self.network.clone(),
            dependency_capsule_formats: vec![compute_core::DEPENDENCY_CAPSULE_FORMAT.into()],
            max_request_bytes: self.max_request_bytes,
            max_output_bytes: self.max_output_bytes,
            distribution_id: self.distribution.clone(),
            max_concurrent_jobs: self.jobs.then_some(4),
            available_concurrent_jobs: self.jobs.then_some(4),
            reserved_resources: None,
            job_retention_seconds: self.jobs.then_some(3600),
            dependency_capsules: self.resident.clone(),
            runtime_artifacts: self.artifacts.clone(),
            max_timeout_ms: self.max_timeout_ms,
            max_memory_bytes: self.max_memory_bytes,
            resources: self.resources.clone(),
            policy: self.policy.clone(),
            inventory: RuntimeInventory {
                compute_version: "0.1.0".into(),
                platform: self.platform.clone(),
                runtimes: entries,
            },
        }
    }

    pub fn descriptor(&self, id: &str) -> ProviderDescriptor {
        ProviderDescriptor::from_capabilities(id, self.kind, &self.capabilities(id), availability())
            .expect("synthetic capabilities are valid")
    }

    pub fn record(&self, id: &str) -> DiscoveryRecord {
        DiscoveryRecord {
            provider_id: id.into(),
            status: DiscoveryStatus::Discovered,
            descriptor: Some(self.descriptor(id)),
            error: None,
        }
    }
}

pub fn availability() -> Availability {
    Availability {
        health: Health::Healthy,
        fetched_at: fixed_now(),
        expires_at: fixed_now() + Duration::seconds(300),
    }
}

pub fn requirements(kind: RuntimeKind) -> PlacementRequirements {
    PlacementRequirements {
        requirements_version: REQUIREMENTS_VERSION.into(),
        runtime: RuntimeRequirement {
            kind,
            version: None,
            artifact_id: None,
        },
        distribution: None,
        dependencies: None,
        isolation: IsolationProfile::Process,
        host: compute_core::HostProfile::Trusted,
        network: if kind == RuntimeKind::Wasm {
            NetworkPolicy::None
        } else {
            NetworkPolicy::Network
        },
        resources: ResourceRequirement::default(),
        platform: None,
        architecture: None,
        artifact: ArtifactRequirement {
            mode: "bundle".into(),
            request_bytes: 4096,
            output_bytes: None,
            submission: SubmissionMode::Synchronous,
        },
    }
}

pub fn config(entries: &[(&str, ProviderKind, i64)]) -> PoolConfig {
    PoolConfig {
        pool: Default::default(),
        providers: entries
            .iter()
            .map(|(id, kind, priority)| {
                (
                    id.to_string(),
                    ProviderConfig {
                        kind: *kind,
                        endpoint: (*kind == ProviderKind::Remote)
                            .then(|| format!("https://{id}.example")),
                        priority: *priority,
                        token_env: None,
                    },
                )
            })
            .collect(),
    }
}

/// The canonical contract a workload with `requirements` would carry.
pub fn contract_for(requirements: &PlacementRequirements) -> compute_policy::ExecutionContract {
    let resources = &requirements.resources;
    compute_policy::ExecutionContract {
        contract_version: compute_policy::CONTRACT_VERSION.into(),
        workload_id: format!("sha256:{}", "c".repeat(64)),
        bundle_id: format!("sha256:{}", "d".repeat(64)),
        runtime: compute_policy::ContractRuntime {
            kind: requirements.runtime.kind,
            version: requirements.runtime.version.clone(),
        },
        dependency_id: requirements
            .dependencies
            .as_ref()
            .map(|dependency| dependency.id.clone()),
        isolation: requirements.isolation,
        network: requirements.network.clone(),
        resources: compute_policy::ContractResources {
            timeout_ms: resources.timeout_ms,
            memory_bytes: resources.memory_bytes,
            cpu_time_ms: resources.cpu_time_ms,
            process_count: resources.process_count,
            stdout_bytes: resources.stdout_bytes,
            stderr_bytes: resources.stderr_bytes,
        },
        platform: requirements.platform.clone(),
        input_bytes: 0,
        artifact_bytes: requirements.artifact.request_bytes,
        output_classes: vec![compute_policy::OutputClass::Stdio],
    }
}

/// Admission under the baseline policy alone.
pub fn baseline(requirements: &PlacementRequirements) -> compute_placement::AdmissionContext {
    compute_placement::AdmissionContext::new(&[], contract_for(requirements))
}

/// Admission under a caller policy.
pub fn with_policy(
    requirements: &PlacementRequirements,
    policy: compute_policy::Policy,
) -> compute_placement::AdmissionContext {
    compute_placement::AdmissionContext::new(
        &[(compute_policy::PolicySourceKind::Explicit, policy)],
        contract_for(requirements),
    )
}
