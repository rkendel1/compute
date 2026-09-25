//! Placement requirements: the execution-compatibility contract derived from
//! a canonical workload bundle.

use compute_core::{
    DEPENDENCY_CAPSULE_FORMAT, IsolationProfile, NetworkPolicy, PlatformIdentity, RuntimeKind,
    WorkloadBundle,
};
use serde::{Deserialize, Serialize};

use crate::PlacementError;

pub const REQUIREMENTS_VERSION: &str = "compute.placement.requirements@1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeRequirement {
    pub kind: RuntimeKind,
    /// Workload-layer version alias or constraint. Placement resolves this to
    /// an exact distribution version before execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Required runtime artifact identity, when the caller pins it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DistributionRequirement {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyRequirement {
    pub id: String,
    pub format: String,
    /// Whether the capsule travels inside the workload bundle. When false,
    /// the provider must already hold the exact capsule.
    pub embedded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<PlatformIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ResourceRequirement {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_time_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_bytes: Option<u64>,
}

impl ResourceRequirement {
    pub fn to_limits(&self) -> compute_core::ResourceLimits {
        compute_core::ResourceLimits {
            memory_bytes: self.memory_bytes,
            cpu_time: self.cpu_time_ms.map(std::time::Duration::from_millis),
            wall_time: self.timeout_ms.map(std::time::Duration::from_millis),
            process_count: self.process_count,
            stdout_bytes: self.stdout_bytes,
            stderr_bytes: self.stderr_bytes,
        }
    }
}

/// How the workload will be handed to the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmissionMode {
    /// `compute pool run`: synchronous execution.
    Synchronous,
    /// `compute pool submit`: a durable asynchronous job.
    Job,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRequirement {
    pub mode: String,
    /// Size of the encoded provider request carrying the workload
    /// (placement metadata excluded).
    pub request_bytes: u64,
    /// Upper bound on captured output declared by the workload, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_bytes: Option<u64>,
    pub submission: SubmissionMode,
}

/// Everything that decides whether a provider can execute the workload
/// without altering its semantics — and nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacementRequirements {
    pub requirements_version: String,
    pub runtime: RuntimeRequirement,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distribution: Option<DistributionRequirement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependencies: Option<DependencyRequirement>,
    pub isolation: IsolationProfile,
    /// The host boundary a process runtime needs.
    #[serde(default, skip_serializing_if = "compute_core::HostProfile::is_trusted")]
    pub host: compute_core::HostProfile,
    pub network: NetworkPolicy,
    pub resources: ResourceRequirement,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<PlatformIdentity>,
    pub artifact: ArtifactRequirement,
}

/// Caller-supplied bindings that are not part of the workload itself.
#[derive(Debug, Clone, Default)]
pub struct RequirementOptions {
    /// Isolation override; it may strengthen but never weaken the workload.
    pub isolation: Option<IsolationProfile>,
    pub distribution_id: Option<String>,
    pub runtime_artifact_id: Option<String>,
    pub platform: Option<PlatformIdentity>,
}

impl PlacementRequirements {
    /// Derive requirements from a verified bundle and the exact size of the
    /// request that will carry it.
    pub fn from_bundle(
        bundle: &WorkloadBundle,
        request_bytes: u64,
        submission: SubmissionMode,
        options: &RequirementOptions,
    ) -> Result<Self, PlacementError> {
        bundle
            .validate()
            .map_err(|error| PlacementError::InvalidRequirements(error.to_string()))?;
        let workload = &bundle.workload;
        let declared = workload.isolation.profile;
        let isolation = match options.isolation {
            Some(profile) if profile < declared => {
                return Err(PlacementError::InvalidRequirements(format!(
                    "isolation override {profile} cannot weaken declared profile {declared}"
                )));
            }
            Some(profile) => profile,
            None => declared,
        };
        for (name, value) in [
            ("distribution", &options.distribution_id),
            ("runtime artifact", &options.runtime_artifact_id),
        ] {
            if let Some(value) = value {
                compute_core::validate_sha256_identity(value).map_err(|error| {
                    PlacementError::InvalidRequirements(format!("{name} identity: {error}"))
                })?;
            }
        }

        let mut platform = options.platform.clone().map(strip_abi);
        let mut bind_platform = |candidate: PlatformIdentity, source: &str| {
            let candidate = strip_abi(candidate);
            match &platform {
                Some(existing) if existing != &candidate => {
                    Err(PlacementError::InvalidRequirements(format!(
                        "{source} requires platform {}, which conflicts with {}",
                        candidate.label(),
                        existing.label()
                    )))
                }
                _ => {
                    platform = Some(candidate);
                    Ok(())
                }
            }
        };

        let dependencies = match (&workload.dependencies, &bundle.dependency_capsule) {
            (None, None) => None,
            (None, Some(_)) => {
                return Err(PlacementError::InvalidRequirements(
                    "bundle embeds a dependency capsule the workload does not reference".into(),
                ));
            }
            (Some(reference), None) => Some(DependencyRequirement {
                id: reference.capsule.clone(),
                format: DEPENDENCY_CAPSULE_FORMAT.into(),
                embedded: false,
                runtime_version: None,
                platform: None,
            }),
            (Some(reference), Some(capsule)) => {
                let id = capsule
                    .capsule_id()
                    .map_err(|error| PlacementError::InvalidRequirements(error.to_string()))?;
                if id != reference.capsule {
                    return Err(PlacementError::InvalidRequirements(format!(
                        "workload references capsule {}, bundle embeds {id}",
                        reference.capsule
                    )));
                }
                if capsule.runtime != workload.runtime {
                    return Err(PlacementError::InvalidRequirements(format!(
                        "dependency capsule targets {}, workload runtime is {}",
                        capsule.runtime, workload.runtime
                    )));
                }
                bind_platform(capsule.platform.clone(), "dependency capsule")?;
                Some(DependencyRequirement {
                    id,
                    format: DEPENDENCY_CAPSULE_FORMAT.into(),
                    embedded: true,
                    runtime_version: capsule.runtime_version.clone(),
                    platform: Some(strip_abi(capsule.platform.clone())),
                })
            }
        };

        if workload.runtime == RuntimeKind::Native
            && let Some(entry) = elf_platform(&bundle.entrypoint.data)
        {
            bind_platform(entry, "native entrypoint")?;
        }

        let resources = &workload.resources;
        let output_bytes = match (resources.stdout_bytes, resources.stderr_bytes) {
            (None, None) => None,
            (stdout, stderr) => Some(stdout.unwrap_or(0).saturating_add(stderr.unwrap_or(0))),
        };
        Ok(Self {
            requirements_version: REQUIREMENTS_VERSION.into(),
            runtime: RuntimeRequirement {
                kind: workload.runtime,
                version: workload.runtime_version.clone(),
                artifact_id: options.runtime_artifact_id.clone(),
            },
            distribution: options
                .distribution_id
                .clone()
                .map(|id| DistributionRequirement { id }),
            dependencies,
            isolation,
            host: workload.isolation.host,
            network: workload.network.clone(),
            resources: ResourceRequirement {
                memory_bytes: resources.memory_bytes,
                timeout_ms: resources.wall_time.map(duration_ms),
                cpu_time_ms: resources.cpu_time.map(duration_ms),
                process_count: resources.process_count,
                stdout_bytes: resources.stdout_bytes,
                stderr_bytes: resources.stderr_bytes,
            },
            platform,
            artifact: ArtifactRequirement {
                mode: "bundle".into(),
                request_bytes,
                output_bytes,
                submission,
            },
        })
    }
}

fn duration_ms(value: std::time::Duration) -> u64 {
    u64::try_from(value.as_millis()).unwrap_or(u64::MAX)
}

fn strip_abi(platform: PlatformIdentity) -> PlatformIdentity {
    PlatformIdentity {
        runtime_abi: None,
        ..platform
    }
}

/// Platform of an ELF executable, read from its header. Non-ELF entrypoints
/// impose no platform requirement here.
pub fn elf_platform(bytes: &[u8]) -> Option<PlatformIdentity> {
    if bytes.len() < 20 || &bytes[..4] != b"\x7fELF" {
        return None;
    }
    let machine = match bytes[5] {
        1 => u16::from_le_bytes([bytes[18], bytes[19]]),
        2 => u16::from_be_bytes([bytes[18], bytes[19]]),
        _ => return None,
    };
    let architecture = match machine {
        0x03 => "x86",
        0x3e => "x86_64",
        0x28 => "arm",
        0xb7 => "aarch64",
        0xf3 => "riscv64",
        _ => return None,
    };
    Some(PlatformIdentity {
        os: "linux".into(),
        architecture: architecture.into(),
        runtime_abi: None,
    })
}
