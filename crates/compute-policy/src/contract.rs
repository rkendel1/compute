//! The canonical execution contract: exactly what a workload asks for,
//! stated explicitly. Admission evaluates it and never rewrites it.

use compute_core::{
    IsolationProfile, NetworkPolicy, PlatformIdentity, RuntimeKind, WorkloadBundle,
};
use serde::{Deserialize, Serialize};

use crate::PolicyError;
use crate::policy::OutputClass;

pub const CONTRACT_VERSION: &str = "compute.contract@1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractRuntime {
    pub kind: RuntimeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ContractResources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_time_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_bytes: Option<u64>,
}

impl ContractResources {
    /// Declared bound on captured output, when both streams are bounded.
    pub fn output_bound(&self) -> Option<u64> {
        Some(self.stdout_bytes?.saturating_add(self.stderr_bytes?))
    }
}

/// The explicit execution contract of one workload. Network and isolation
/// are always stated; an absent resource limit means the workload declared
/// none, which a policy limit treats as unbounded rather than as allowed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionContract {
    pub contract_version: String,
    pub workload_id: String,
    pub bundle_id: String,
    pub runtime: ContractRuntime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependency_id: Option<String>,
    pub isolation: IsolationProfile,
    pub network: NetworkPolicy,
    pub resources: ContractResources,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<PlatformIdentity>,
    pub input_bytes: u64,
    pub artifact_bytes: u64,
    pub output_classes: Vec<OutputClass>,
}

impl ExecutionContract {
    /// Derive the contract from a verified bundle. `isolation` may strengthen
    /// the declared profile; a weaker override is rejected, never applied.
    pub fn from_bundle(
        bundle: &WorkloadBundle,
        isolation: Option<IsolationProfile>,
    ) -> Result<Self, PolicyError> {
        let contract_error = |message: String| PolicyError::Contract(message);
        bundle
            .validate()
            .map_err(|error| contract_error(error.to_string()))?;
        let workload = &bundle.workload;
        let declared = workload.isolation.profile;
        let isolation = match isolation {
            Some(profile) if profile < declared => {
                return Err(contract_error(format!(
                    "isolation override {profile} cannot weaken declared profile {declared}"
                )));
            }
            Some(profile) => profile,
            None => declared,
        };
        let mut platform: Option<PlatformIdentity> = None;
        let mut bind = |candidate: PlatformIdentity, source: &str| {
            let candidate = PlatformIdentity {
                runtime_abi: None,
                ..candidate
            };
            match &platform {
                Some(existing) if existing != &candidate => Err(contract_error(format!(
                    "{source} requires {}, which conflicts with {}",
                    candidate.label(),
                    existing.label()
                ))),
                _ => {
                    platform = Some(candidate);
                    Ok(())
                }
            }
        };
        if let Some(capsule) = &bundle.dependency_capsule {
            bind(capsule.platform.clone(), "dependency capsule")?;
        }
        if workload.runtime == RuntimeKind::Native
            && let Some(elf) = elf_platform(&bundle.entrypoint.data)
        {
            bind(elf, "native entrypoint")?;
        }
        let millis =
            |value: std::time::Duration| u64::try_from(value.as_millis()).unwrap_or(u64::MAX);
        let resources = &workload.resources;
        let inline_bytes: u64 = workload
            .inputs
            .iter()
            .filter_map(|input| match &input.source {
                compute_core::InputSource::Inline { data } => Some(data.len() as u64),
                compute_core::InputSource::File { .. } => None,
            })
            .sum();
        let input_bytes = bundle.entrypoint.data.len() as u64
            + bundle
                .inputs
                .iter()
                .map(|input| input.data.len() as u64)
                .sum::<u64>()
            + inline_bytes;
        let mut output_classes = vec![OutputClass::Stdio];
        if !workload.outputs.is_empty() {
            output_classes.push(OutputClass::Files);
        }
        Ok(Self {
            contract_version: CONTRACT_VERSION.into(),
            workload_id: bundle
                .workload_id()
                .map_err(|error| contract_error(error.to_string()))?,
            bundle_id: bundle
                .bundle_id()
                .map_err(|error| contract_error(error.to_string()))?,
            runtime: ContractRuntime {
                kind: workload.runtime,
                version: workload.runtime_version.clone(),
            },
            dependency_id: workload
                .dependencies
                .as_ref()
                .map(|reference| reference.capsule.clone()),
            isolation,
            network: workload.network.clone(),
            resources: ContractResources {
                timeout_ms: resources.wall_time.map(millis),
                memory_bytes: resources.memory_bytes,
                cpu_time_ms: resources.cpu_time.map(millis),
                process_count: resources.process_count,
                stdout_bytes: resources.stdout_bytes,
                stderr_bytes: resources.stderr_bytes,
            },
            platform,
            input_bytes,
            artifact_bytes: bundle
                .to_bytes()
                .map_err(|error| contract_error(error.to_string()))?
                .len() as u64,
            output_classes,
        })
    }
}

/// Platform of an ELF executable, read from its header. Non-ELF entrypoints
/// impose no platform requirement.
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
