use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    ComputeError, ExecutionErrorKind, ExecutionInputSource, ExecutionRequest, ExecutionResult,
    ExecutionStatus, IsolationEvidence, NetworkPolicy, ResolvedRuntime, Result, RuntimeKind,
};

pub const RECEIPT_VERSION: &str = "compute.receipt@1";

macro_rules! digest_identity {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn sha256(bytes: &[u8]) -> Self {
                Self(format!("sha256:{:x}", Sha256::digest(bytes)))
            }

            pub fn parse(value: impl Into<String>) -> Result<Self> {
                let value = value.into();
                validate_sha256_identity(&value)?;
                Ok(Self(value))
            }

            pub fn algorithm(&self) -> &'static str {
                "sha256"
            }
            pub fn digest(&self) -> &str {
                &self.0["sha256:".len()..]
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

digest_identity!(WorkloadIdentity);
digest_identity!(BundleIdentity);
digest_identity!(ReceiptHash);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExecutionId(pub String);

impl ExecutionId {
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.starts_with("exec_") && value.len() > 5 && !value.chars().any(char::is_whitespace)
        {
            Ok(Self(value))
        } else {
            Err(invalid("malformed execution identity"))
        }
    }
}

impl std::fmt::Display for ExecutionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DistributionIdentity {
    pub id: String,
    pub platform: String,
    pub manifest_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeIdentity {
    pub declared: RuntimeKind,
    pub selected: RuntimeKind,
    pub observed: RuntimeKind,
    pub version: String,
    pub distribution_runtime_id: String,
    pub executable_identity: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRequestSummary {
    pub entrypoint: String,
    pub argument_count: u64,
    pub stdin_size: u64,
    pub stdin_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPolicySummary {
    pub network: NetworkPolicy,
    pub filesystem: String,
    pub timeout_ms: Option<u64>,
    pub memory_bytes: Option<u64>,
    pub environment: String,
    pub environment_names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputReceipt {
    pub path: PathBuf,
    pub size: u64,
    pub sha256: String,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputCollectionStatus {
    Collected,
    MissingRequired,
    MissingOptional,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputReceipt {
    pub path: PathBuf,
    pub size: Option<u64>,
    pub sha256: Option<String>,
    pub collection_status: OutputCollectionStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptFailureKind {
    Workload,
    Compute,
    Timeout,
    Policy,
    Cancelled,
    Killed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptError {
    pub kind: ReceiptFailureKind,
    pub code: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionReceiptStatus {
    pub status: ExecutionStatus,
    pub exit_code: Option<i32>,
    pub error: Option<ReceiptError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionProvenance {
    pub distribution_id: String,
    pub runtime_lock_id: String,
    pub manifest_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionReceipt {
    pub receipt_version: String,
    pub execution_id: ExecutionId,
    pub workload: WorkloadIdentity,
    pub bundle: Option<BundleIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<crate::ProviderIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_protocol: Option<String>,
    pub distribution: DistributionIdentity,
    pub runtime: RuntimeIdentity,
    pub request: ExecutionRequestSummary,
    pub policy: ExecutionPolicySummary,
    pub isolation: IsolationEvidence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependencies: Option<ReceiptDependencies>,
    pub inputs: Vec<InputReceipt>,
    pub outputs: Vec<OutputReceipt>,
    pub execution: ExecutionReceiptStatus,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub provenance: ExecutionProvenance,
    pub receipt_hash: ReceiptHash,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptDependencies {
    pub capsule_id: String,
    pub verified: bool,
}

#[derive(Serialize)]
struct ReceiptBody<'a> {
    receipt_version: &'a str,
    execution_id: &'a ExecutionId,
    workload: &'a WorkloadIdentity,
    bundle: &'a Option<BundleIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: &'a Option<crate::ProviderIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_protocol: &'a Option<String>,
    distribution: &'a DistributionIdentity,
    runtime: &'a RuntimeIdentity,
    request: &'a ExecutionRequestSummary,
    policy: &'a ExecutionPolicySummary,
    isolation: &'a IsolationEvidence,
    dependencies: &'a Option<ReceiptDependencies>,
    inputs: &'a [InputReceipt],
    outputs: &'a [OutputReceipt],
    execution: &'a ExecutionReceiptStatus,
    started_at: &'a Option<DateTime<Utc>>,
    finished_at: &'a Option<DateTime<Utc>>,
    provenance: &'a ExecutionProvenance,
}

impl ExecutionReceipt {
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(&self.body()).map_err(Into::into)
    }

    pub fn encoded_bytes(&self) -> Result<Vec<u8>> {
        let mut bytes = serde_json::to_vec(self)?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    pub fn hash(&self) -> Result<ReceiptHash> {
        Ok(ReceiptHash::sha256(&self.canonical_bytes()?))
    }

    pub fn seal(&mut self) -> Result<()> {
        self.normalize();
        self.receipt_hash = self.hash()?;
        Ok(())
    }

    pub fn verify(&self) -> Result<()> {
        if self.receipt_version != RECEIPT_VERSION {
            return Err(invalid(format!(
                "unsupported receipt version: {}",
                self.receipt_version
            )));
        }
        ExecutionId::parse(self.execution_id.0.clone())?;
        validate_sha256_identity(&self.workload.0)?;
        if let Some(bundle) = &self.bundle {
            validate_sha256_identity(&bundle.0)?;
        }
        match &self.provider {
            Some(crate::ProviderIdentity::Local { id }) => {
                if id.is_empty() || self.provider_protocol.as_deref() != Some("compute.local@1") {
                    return Err(invalid("invalid local provider binding"));
                }
            }
            Some(crate::ProviderIdentity::Remote { id, endpoint }) => {
                if id.is_empty()
                    || endpoint.is_empty()
                    || self.provider_protocol.as_deref() != Some("compute.remote@1")
                {
                    return Err(invalid("invalid remote provider binding"));
                }
            }
            None if self.provider_protocol.is_some() => {
                return Err(invalid(
                    "provider protocol is present without provider identity",
                ));
            }
            None => {}
        }
        validate_sha256_identity(&self.distribution.id)?;
        validate_sha256_identity(&self.runtime.distribution_runtime_id)?;
        validate_sha256_identity(&self.runtime.executable_identity)?;
        validate_sha256_identity(&self.provenance.distribution_id)?;
        validate_sha256_identity(&self.provenance.runtime_lock_id)?;
        validate_sha256_identity(&self.provenance.manifest_id)?;
        validate_sha256_identity(&self.receipt_hash.0)?;
        if let Some(dependencies) = &self.dependencies {
            validate_sha256_identity(&dependencies.capsule_id)?;
            if !dependencies.verified {
                return Err(invalid("receipt dependency capsule is not verified"));
            }
        }
        if self.distribution.id != self.provenance.distribution_id {
            return Err(invalid("distribution identity mismatch"));
        }
        if self.runtime.declared != self.runtime.selected
            || self.runtime.selected != self.runtime.observed
        {
            return Err(invalid("runtime identity mismatch"));
        }
        if self.isolation.requested != self.isolation.effective
            || self.isolation.profile != self.isolation.effective
        {
            return Err(invalid("isolation profile downgrade"));
        }
        for input in &self.inputs {
            validate_sha256_identity(&input.sha256)?;
        }
        for output in &self.outputs {
            if let Some(digest) = &output.sha256 {
                validate_sha256_identity(digest)?;
            }
            if matches!(output.collection_status, OutputCollectionStatus::Collected)
                && (output.size.is_none() || output.sha256.is_none())
            {
                return Err(invalid("collected output is missing identity"));
            }
        }
        let mut normalized = self.clone();
        normalized.normalize();
        if normalized.inputs != self.inputs
            || normalized.outputs != self.outputs
            || normalized.policy.environment_names != self.policy.environment_names
        {
            return Err(invalid("invalid canonical ordering"));
        }
        let actual = self.hash()?;
        if actual != self.receipt_hash {
            return Err(invalid(format!(
                "receipt hash mismatch: expected {}, actual {}",
                self.receipt_hash, actual
            )));
        }
        Ok(())
    }

    fn body(&self) -> ReceiptBody<'_> {
        ReceiptBody {
            receipt_version: &self.receipt_version,
            execution_id: &self.execution_id,
            workload: &self.workload,
            bundle: &self.bundle,
            provider: &self.provider,
            provider_protocol: &self.provider_protocol,
            distribution: &self.distribution,
            runtime: &self.runtime,
            request: &self.request,
            policy: &self.policy,
            isolation: &self.isolation,
            dependencies: &self.dependencies,
            inputs: &self.inputs,
            outputs: &self.outputs,
            execution: &self.execution,
            started_at: &self.started_at,
            finished_at: &self.finished_at,
            provenance: &self.provenance,
        }
    }

    fn normalize(&mut self) {
        self.inputs.sort_by(|a, b| a.path.cmp(&b.path));
        self.outputs.sort_by(|a, b| a.path.cmp(&b.path));
        self.policy.environment_names.sort();
        self.policy.environment_names.dedup();
    }
}

#[derive(Debug, Clone)]
pub struct ReceiptEnvironment {
    pub distribution: DistributionIdentity,
    pub distribution_runtime_id: String,
    pub executable_identity: String,
    pub runtime_lock_id: String,
    pub manifest_id: String,
}

pub fn input_receipts(request: &ExecutionRequest) -> Result<Vec<InputReceipt>> {
    let mut receipts = request
        .inputs
        .iter()
        .map(|input| {
            let bytes = match &input.source {
                ExecutionInputSource::Inline { data } => data.clone(),
                ExecutionInputSource::File { path } => std::fs::read(path)?,
            };
            Ok(InputReceipt {
                path: input.path.clone(),
                size: bytes.len() as u64,
                sha256: sha256_identity(&bytes),
                required: true,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    receipts.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(receipts)
}

#[allow(clippy::too_many_arguments)]
pub fn create_execution_receipt(
    request: &ExecutionRequest,
    resolved: &ResolvedRuntime,
    result: &ExecutionResult,
    workload: WorkloadIdentity,
    bundle: Option<BundleIdentity>,
    inputs: Vec<InputReceipt>,
    environment: ReceiptEnvironment,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
) -> Result<ExecutionReceipt> {
    if request.runtime.kind != resolved.kind || result.runtime != resolved.kind {
        return Err(invalid("runtime identity mismatch"));
    }
    let entrypoint = request
        .entrypoint
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("<entrypoint>")
        .to_string();
    let mut outputs = result
        .outputs
        .iter()
        .map(|output| OutputReceipt {
            path: output.path.clone(),
            size: Some(output.size),
            sha256: Some(sha256_identity(&output.data)),
            collection_status: OutputCollectionStatus::Collected,
        })
        .collect::<Vec<_>>();
    outputs.extend(result.missing_outputs.iter().map(|output| OutputReceipt {
        path: output.path.clone(),
        size: None,
        sha256: None,
        collection_status: if output.required {
            OutputCollectionStatus::MissingRequired
        } else {
            OutputCollectionStatus::MissingOptional
        },
    }));
    let error = result.error.as_ref().map(|error| ReceiptError {
        kind: match error.kind {
            ExecutionErrorKind::Timeout => ReceiptFailureKind::Timeout,
            ExecutionErrorKind::Cancelled => ReceiptFailureKind::Cancelled,
            ExecutionErrorKind::Killed => ReceiptFailureKind::Killed,
            ExecutionErrorKind::UnsupportedCapability => ReceiptFailureKind::Policy,
            ExecutionErrorKind::Runtime if error.started => ReceiptFailureKind::Workload,
            _ => ReceiptFailureKind::Compute,
        },
        code: format!("{:?}", error.kind).to_ascii_lowercase(),
    });
    let version = resolved
        .resolved_version
        .clone()
        .or_else(|| resolved.requested_version.clone())
        .unwrap_or_else(|| "unknown".into());
    let mut receipt = ExecutionReceipt {
        receipt_version: RECEIPT_VERSION.into(),
        execution_id: ExecutionId::parse(result.execution_id.clone())?,
        workload,
        bundle,
        provider: None,
        provider_protocol: None,
        distribution: environment.distribution.clone(),
        runtime: RuntimeIdentity {
            declared: request.runtime.kind,
            selected: resolved.kind,
            observed: result.runtime,
            version,
            distribution_runtime_id: environment.distribution_runtime_id,
            executable_identity: environment.executable_identity,
        },
        request: ExecutionRequestSummary {
            entrypoint,
            argument_count: request.args.len() as u64,
            stdin_size: request.stdin.len() as u64,
            stdin_sha256: sha256_identity(&request.stdin),
        },
        policy: ExecutionPolicySummary {
            network: request.network.clone(),
            filesystem: "workspace".into(),
            timeout_ms: request.resources.wall_time.map(|v| v.as_millis() as u64),
            memory_bytes: request.resources.memory_bytes,
            environment: "cleared".into(),
            environment_names: request.env.iter().map(|v| v.key.clone()).collect(),
        },
        isolation: result
            .isolation
            .clone()
            .ok_or_else(|| invalid("execution result is missing isolation evidence"))?,
        dependencies: result
            .dependencies
            .as_ref()
            .map(|dependencies| ReceiptDependencies {
                capsule_id: dependencies.capsule_id.clone(),
                verified: dependencies.verified,
            }),
        inputs,
        outputs,
        execution: ExecutionReceiptStatus {
            status: result.status.clone(),
            exit_code: result.exit_code,
            error,
        },
        started_at: Some(started_at),
        finished_at: Some(finished_at),
        provenance: ExecutionProvenance {
            distribution_id: environment.distribution.id,
            runtime_lock_id: environment.runtime_lock_id,
            manifest_id: environment.manifest_id,
        },
        receipt_hash: ReceiptHash(String::new()),
    };
    receipt.seal()?;
    Ok(receipt)
}

pub fn request_workload_identity(request: &ExecutionRequest) -> Result<WorkloadIdentity> {
    WorkloadIdentity::parse(sha256_identity(&serde_json::to_vec(request)?))
}

pub fn sha256_file_identity(path: &Path) -> Result<String> {
    Ok(sha256_identity(&std::fs::read(path)?))
}

pub fn sha256_identity(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

pub fn validate_sha256_identity(value: &str) -> Result<()> {
    let Some(digest) = value.strip_prefix("sha256:") else {
        return Err(invalid("digest algorithm must be sha256"));
    };
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(invalid("malformed sha256 digest"));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> ComputeError {
    ComputeError::InvalidReceipt(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BoundaryStatus, IsolationEvidence, IsolationProfile, Output, ResourceLimits, ResourceUsage,
        RuntimeSpec,
    };
    use std::time::Duration;

    #[test]
    fn identities_are_algorithm_explicit_and_validated() {
        let identity = WorkloadIdentity::sha256(b"workload");
        assert_eq!(identity.algorithm(), "sha256");
        assert_eq!(identity.digest().len(), 64);
        assert!(WorkloadIdentity::parse(identity.0).is_ok());
        assert!(WorkloadIdentity::parse("sha256:nope").is_err());
        assert!(WorkloadIdentity::parse("md5:0000").is_err());
    }

    #[test]
    fn canonical_receipts_are_deterministic_and_execution_specific() {
        let request = ExecutionRequest {
            runtime: RuntimeSpec {
                kind: RuntimeKind::Python,
                version: Some("3".into()),
            },
            entrypoint: "main.py".into(),
            args: vec!["hidden".into()],
            stdin: b"secret".to_vec(),
            env: vec![],
            inputs: vec![],
            outputs: vec![],
            mounts: vec![],
            network: NetworkPolicy::Network,
            resources: ResourceLimits::default(),
            isolation: IsolationProfile::Process,
            dependencies: None,
        };
        let resolved = ResolvedRuntime {
            kind: RuntimeKind::Python,
            requested_version: Some("3".into()),
            resolved_version: Some("3.13.0".into()),
            executable: None,
        };
        let result = ExecutionResult {
            execution_id: "exec_test_1".into(),
            runtime: RuntimeKind::Python,
            network: NetworkPolicy::Network,
            lifecycle: vec![ExecutionStatus::Created, ExecutionStatus::Completed],
            status: ExecutionStatus::Completed,
            exit_code: Some(0),
            stdout: Output::from_bytes(vec![], None),
            stderr: Output::from_bytes(vec![], None),
            duration: Duration::ZERO,
            resource_usage: ResourceUsage::default(),
            artifacts: vec![],
            outputs: vec![],
            missing_outputs: vec![],
            error: None,
            isolation: Some(IsolationEvidence {
                profile: IsolationProfile::Process,
                requested: IsolationProfile::Process,
                effective: IsolationProfile::Process,
                filesystem: BoundaryStatus::Unavailable,
                network: BoundaryStatus::NotRequested,
                environment: BoundaryStatus::Enforced,
                resources: BoundaryStatus::NotRequested,
            }),
            dependencies: None,
            provider: None,
            receipt: None,
        };
        let environment = ReceiptEnvironment {
            distribution: DistributionIdentity {
                id: sha256_identity(b"distribution"),
                platform: "linux-x86_64".into(),
                manifest_version: "2".into(),
            },
            distribution_runtime_id: sha256_identity(b"runtime"),
            executable_identity: sha256_identity(b"executable"),
            runtime_lock_id: sha256_identity(b"lock"),
            manifest_id: sha256_identity(b"manifest"),
        };
        let timestamp = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .to_utc();
        let first = create_execution_receipt(
            &request,
            &resolved,
            &result,
            WorkloadIdentity::sha256(b"workload"),
            None,
            vec![],
            environment.clone(),
            timestamp,
            timestamp,
        )
        .unwrap();
        let second = create_execution_receipt(
            &request,
            &resolved,
            &result,
            WorkloadIdentity::sha256(b"workload"),
            None,
            vec![],
            environment,
            timestamp,
            timestamp,
        )
        .unwrap();
        assert_eq!(
            first.canonical_bytes().unwrap(),
            second.canonical_bytes().unwrap()
        );
        assert_eq!(first.receipt_hash, second.receipt_hash);
        first.verify().unwrap();

        let mut changed = second;
        changed.execution_id = ExecutionId::parse("exec_test_2").unwrap();
        changed.seal().unwrap();
        assert_ne!(first.receipt_hash, changed.receipt_hash);
        let mut changed_isolation = first.clone();
        changed_isolation.isolation.filesystem = BoundaryStatus::Enforced;
        changed_isolation.seal().unwrap();
        assert_ne!(first.receipt_hash, changed_isolation.receipt_hash);
        assert_eq!(first.request.argument_count, 1);
        assert!(
            !String::from_utf8(first.encoded_bytes().unwrap())
                .unwrap()
                .contains("hidden")
        );
        assert!(
            !String::from_utf8(first.encoded_bytes().unwrap())
                .unwrap()
                .contains("secret")
        );
    }
}
