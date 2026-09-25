use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    ComputeError, ExecutionErrorKind, ExecutionInputSource, ExecutionRequest, ExecutionResult,
    ExecutionStatus, IsolationEvidence, NetworkPolicy, PlatformIdentity, ProviderResourceInventory,
    ResolvedRuntime, ResourceVector, Result, RuntimeKind,
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
    /// Canonical identity of the selected runtime distribution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distribution_id: Option<String>,
    /// Verified digest of the acquired runtime artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distribution_digest: Option<String>,
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
    /// Product-level application this execution belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application: Option<crate::ApplicationIdentity>,
    /// Placement decision that routed this execution to its provider. Absent
    /// for executions that were not placed through a provider pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<ReceiptPlacement>,
    /// Durable scheduler reservation that authorized this execution's use
    /// of provider capacity. Present for durable jobs, including jobs that
    /// were submitted directly rather than through pool placement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reservation: Option<ReceiptReservation>,
    /// Environment, project, and workload this execution belongs to, when
    /// it ran inside a Compute environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<ReceiptScope>,
    /// Identity of the exact policy snapshot admission evaluated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_id: Option<String>,
    /// Always `admitted` on an execution receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_status: Option<String>,
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

/// How the execution provider was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionMode {
    /// The caller named the provider; compatibility was validated, never
    /// substituted.
    Explicit,
    /// The provider pool selected the provider by its documented ordering.
    Pool,
}

impl std::fmt::Display for SelectionMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Explicit => "explicit",
            Self::Pool => "pool",
        })
    }
}

/// Factual account of why a provider was selected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionReason {
    /// Always `compatible`: selection only considers compatible providers.
    pub compatibility_result: String,
    pub selection_priority: i64,
    /// Ordering rule applied among compatible candidates.
    pub ordering: String,
    pub compatible_candidates: u64,
}

/// Where an execution sits in the environment model:
/// environment → project → workload → execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptScope {
    pub environment_id: String,
    pub environment: String,
    pub project_id: String,
    pub project: String,
    pub revision: String,
    pub workload_id: String,
    pub workload: String,
    /// `service` or `task`.
    pub workload_kind: String,
    /// The deployment whose instance this execution is, when it is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,
}

impl ReceiptScope {
    fn validate(&self) -> Result<()> {
        for value in [
            &self.environment_id,
            &self.environment,
            &self.project_id,
            &self.project,
            &self.revision,
            &self.workload_id,
            &self.workload,
        ] {
            if value.is_empty()
                || value.len() > 128
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:@".contains(&byte))
            {
                return Err(invalid("invalid execution scope"));
            }
        }
        if !matches!(self.workload_kind.as_str(), "service" | "task") {
            return Err(invalid("invalid workload kind in execution scope"));
        }
        if self.deployment_id.as_deref().is_some_and(|id| {
            !id.starts_with("dep_")
                || id.len() > 128
                || !id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        }) {
            return Err(invalid("invalid deployment in execution scope"));
        }
        Ok(())
    }
}

/// Placement evidence bound into a receipt by the executing provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptPlacement {
    pub placement_id: String,
    /// Caller-configured pool identifier of the selected provider.
    pub provider_id: String,
    pub provider_protocol: String,
    pub selection_mode: SelectionMode,
    pub selection_reason: SelectionReason,
    /// Placement/scheduling policy applied after eligibility was known.
    #[serde(default)]
    pub policy: ReceiptPlacementPolicy,
    /// Deterministic candidate order, including rejected providers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<ReceiptPlacementCandidate>,
    /// Resources requested by the workload when placement ran.
    #[serde(default)]
    pub requested_resources: ResourceVector,
    /// Capacity and availability observed on the selected provider.
    #[serde(default)]
    pub provider_resources: ProviderResourceInventory,
    /// Allocation admitted for this execution. Usage is reported separately.
    #[serde(default)]
    pub allocated_resources: ResourceVector,
    /// Platform the selected provider advertised and executed on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_platform: Option<PlatformIdentity>,
    /// Durable capacity grant established before provider admission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reservation: Option<ReceiptReservation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ReceiptPlacementPolicy {
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptPlacementCandidate {
    pub provider_id: String,
    pub eligible: bool,
    pub capacity_available: bool,
    pub selected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank: Option<u64>,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptReservation {
    pub reservation_id: crate::ReservationId,
    pub requested_resources: crate::ResourceRequirements,
    pub reserved_resources: crate::ResourceRequirements,
    pub provider_capacity_snapshot: crate::CapacitySnapshot,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    application: &'a Option<crate::ApplicationIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    placement: &'a Option<ReceiptPlacement>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reservation: &'a Option<ReceiptReservation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: &'a Option<ReceiptScope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    policy_id: &'a Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    admission_id: &'a Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    admission_status: &'a Option<String>,
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
    /// Bind admission evidence. The caller reseals the receipt.
    pub fn bind_admission(&mut self, admission: &crate::ExecutionAdmission) {
        self.policy_id = Some(admission.policy_id.clone());
        self.admission_id = Some(admission.admission_id.clone());
        self.admission_status = Some(admission.admission_status.clone());
    }

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
        if let Some(placement) = &self.placement {
            validate_sha256_identity(&placement.placement_id)?;
            if self.provider.is_none() {
                return Err(invalid("placement is present without provider identity"));
            }
            if self.provider_protocol.as_deref() != Some(placement.provider_protocol.as_str()) {
                return Err(invalid("placement provider protocol differs from receipt"));
            }
            if placement.provider_id.is_empty()
                || placement.provider_id.len() > 64
                || !placement
                    .provider_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            {
                return Err(invalid("invalid placement provider identifier"));
            }
            if placement.selection_reason.compatibility_result != "compatible"
                || placement.selection_reason.compatible_candidates == 0
            {
                return Err(invalid("placement selected an incompatible provider"));
            }
            let selected = placement
                .candidates
                .iter()
                .filter(|candidate| candidate.selected)
                .collect::<Vec<_>>();
            if !placement.candidates.is_empty()
                && (selected.len() != 1
                    || selected[0].provider_id != placement.provider_id
                    || !selected[0].eligible)
            {
                return Err(invalid("placement candidate selection is inconsistent"));
            }
        }
        if let Some(application) = &self.application {
            application.verify()?;
        }
        if let Some(reservation) = &self.reservation {
            crate::ReservationId::parse(reservation.reservation_id.0.clone())?;
            if reservation.requested_resources != reservation.reserved_resources {
                return Err(invalid(
                    "reserved resources differ from requested resources",
                ));
            }
            let snapshot = &reservation.provider_capacity_snapshot;
            if snapshot.capacity.cpu_millis
                != snapshot
                    .reserved
                    .cpu_millis
                    .saturating_add(snapshot.available.cpu_millis)
                || snapshot.capacity.memory_bytes
                    != snapshot
                        .reserved
                        .memory_bytes
                        .saturating_add(snapshot.available.memory_bytes)
                || snapshot.capacity.disk_bytes
                    != snapshot
                        .reserved
                        .disk_bytes
                        .saturating_add(snapshot.available.disk_bytes)
                || snapshot.capacity.max_concurrency
                    != snapshot
                        .reserved
                        .concurrency
                        .saturating_add(snapshot.available.concurrency)
            {
                return Err(invalid("capacity snapshot does not balance"));
            }
            if reservation.reserved_resources.cpu_millis > snapshot.reserved.cpu_millis
                || reservation.reserved_resources.memory_bytes > snapshot.reserved.memory_bytes
                || reservation.reserved_resources.disk_bytes > snapshot.reserved.disk_bytes
                || reservation.reserved_resources.concurrency > snapshot.reserved.concurrency
            {
                return Err(invalid(
                    "reservation exceeds the recorded capacity snapshot",
                ));
            }
        }
        if self
            .placement
            .as_ref()
            .and_then(|placement| placement.reservation.as_ref())
            .is_some_and(|placement| Some(placement) != self.reservation.as_ref())
        {
            return Err(invalid(
                "placement reservation differs from receipt reservation",
            ));
        }
        if let Some(scope) = &self.scope {
            scope.validate()?;
        }
        match (&self.policy_id, &self.admission_id, &self.admission_status) {
            (None, None, None) => {}
            (Some(policy), Some(admission), Some(status)) => {
                validate_sha256_identity(policy)?;
                validate_sha256_identity(admission)?;
                if status != "admitted" {
                    return Err(invalid(
                        "an execution receipt requires an admitted decision",
                    ));
                }
            }
            _ => return Err(invalid("incomplete admission evidence")),
        }
        validate_sha256_identity(&self.distribution.id)?;
        validate_sha256_identity(&self.runtime.distribution_runtime_id)?;
        validate_sha256_identity(&self.runtime.executable_identity)?;
        if let Some(identity) = &self.runtime.distribution_id {
            validate_sha256_identity(identity)?;
        }
        if let Some(digest) = &self.runtime.distribution_digest {
            validate_sha256_identity(digest)?;
        }
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
            application: &self.application,
            placement: &self.placement,
            reservation: &self.reservation,
            scope: &self.scope,
            policy_id: &self.policy_id,
            admission_id: &self.admission_id,
            admission_status: &self.admission_status,
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
    pub runtime_distribution_id: String,
    pub runtime_distribution_digest: String,
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
        application: None,
        placement: None,
        reservation: None,
        scope: None,
        policy_id: None,
        admission_id: None,
        admission_status: None,
        distribution: environment.distribution.clone(),
        runtime: RuntimeIdentity {
            declared: request.runtime.kind,
            selected: resolved.kind,
            observed: result.runtime,
            version,
            distribution_runtime_id: environment.distribution_runtime_id,
            executable_identity: environment.executable_identity,
            distribution_id: Some(environment.runtime_distribution_id),
            distribution_digest: Some(environment.runtime_distribution_digest),
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

/// What must stay the same for a file's cached identity to be reused: a
/// file that is replaced, rewritten, or touched is hashed again.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileFingerprint {
    device: u64,
    inode: u64,
    size: u64,
    modified: Option<std::time::SystemTime>,
}

fn fingerprint(path: &Path) -> Result<FileFingerprint> {
    let metadata = std::fs::metadata(path)?;
    #[cfg(unix)]
    let (device, inode) = {
        use std::os::unix::fs::MetadataExt;
        (metadata.dev(), metadata.ino())
    };
    #[cfg(not(unix))]
    let (device, inode) = (0, 0);
    Ok(FileFingerprint {
        device,
        inode,
        size: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

/// A file's SHA-256 identity, hashed once per process for as long as the
/// file's device, inode, size, and modification time stay the same. Runtime
/// executables are tens of megabytes; hashing them on every execution
/// dominated short workloads.
pub fn sha256_file_identity_cached(path: &Path) -> Result<String> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<std::path::PathBuf, (FileFingerprint, String)>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let before = fingerprint(path)?;
    if let Some((cached, identity)) = cache.lock().expect("identity cache").get(path)
        && *cached == before
    {
        return Ok(identity.clone());
    }
    let identity = match identity_cache::read(path) {
        Some(identity) => identity,
        None => {
            let identity = sha256_file_identity(path)?;
            // Cache only what was hashed from an unchanged file.
            if fingerprint(path)? != before {
                return Ok(identity);
            }
            identity_cache::write(path, &before, &identity);
            identity
        }
    };
    cache
        .lock()
        .expect("identity cache")
        .insert(path.to_path_buf(), (before, identity.clone()));
    Ok(identity)
}

/// The identity of the running Compute executable, established once per
/// process and reused by every execution and receipt.
pub fn compute_executable_identity() -> Option<&'static str> {
    use std::sync::OnceLock;
    static IDENTITY: OnceLock<Option<String>> = OnceLock::new();
    IDENTITY
        .get_or_init(|| {
            let path = std::env::current_exe().ok()?;
            if let Some(identity) = identity_cache::read(&path) {
                return Some(identity);
            }
            let before = fingerprint(&path).ok()?;
            let identity = sha256_file_identity(&path).ok()?;
            if fingerprint(&path).ok()? == before {
                identity_cache::write(&path, &before, &identity);
            }
            Some(identity)
        })
        .as_deref()
}

/// Short-lived Compute processes (one `compute run`) would otherwise hash
/// the Compute executable and the runtime executable on every invocation.
/// Their identities are kept in a per-user cache keyed by the executable's path, device, inode, size, and
/// modification time. The cache is trusted only when this user owns it and
/// nobody else can write it: anyone who could forge it could equally
/// replace the executable it describes.
mod identity_cache {
    use super::{FileFingerprint, fingerprint};
    use std::path::{Path, PathBuf};

    fn location() -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os("COMPUTE_CACHE_DIR") {
            return Some(PathBuf::from(dir).join("executable-identities.json"));
        }
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
        Some(base.join("compute").join("executable-identities.json"))
    }

    fn key(path: &Path, print: &FileFingerprint) -> String {
        format!(
            "{}|{}|{}|{}|{:?}",
            path.display(),
            print.device,
            print.inode,
            print.size,
            print.modified
        )
    }

    #[cfg(unix)]
    fn trusted(file: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        let Ok(metadata) = std::fs::symlink_metadata(file) else {
            return false;
        };
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        metadata.is_file() && metadata.uid() == uid && metadata.mode() & 0o022 == 0
    }

    #[cfg(not(unix))]
    fn trusted(_: &Path) -> bool {
        false
    }

    /// Entries kept: one per executable path, at most this many.
    const CAPACITY: usize = 64;

    fn entries(file: &Path) -> serde_json::Map<String, serde_json::Value> {
        if !trusted(file) {
            return serde_json::Map::new();
        }
        std::fs::read(file)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn read(executable: &Path) -> Option<String> {
        let file = location()?;
        let print = fingerprint(executable).ok()?;
        let identity = entries(&file)
            .get(&key(executable, &print))?
            .as_str()?
            .to_owned();
        super::validate_sha256_identity(&identity).ok()?;
        Some(identity)
    }

    pub fn write(executable: &Path, print: &FileFingerprint, identity: &str) {
        let Some(file) = location() else { return };
        let Some(parent) = file.parent() else { return };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
        // One entry per path: a replaced executable's stale entry goes.
        let prefix = format!("{}|", executable.display());
        let mut entries = entries(&file);
        entries.retain(|name, _| !name.starts_with(&prefix));
        while entries.len() >= CAPACITY {
            let Some(name) = entries.keys().next().cloned() else {
                break;
            };
            entries.remove(&name);
        }
        entries.insert(key(executable, print), identity.into());
        let value = serde_json::Value::Object(entries);
        let temporary = parent.join(format!(".executable-identity.{}", std::process::id()));
        let written = {
            #[cfg(unix)]
            {
                use std::io::Write;
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&temporary)
                    .and_then(|mut handle| handle.write_all(value.to_string().as_bytes()))
            }
            #[cfg(not(unix))]
            {
                std::fs::write(&temporary, value.to_string())
            }
        };
        if written.is_ok() {
            let _ = std::fs::rename(&temporary, &file);
        } else {
            let _ = std::fs::remove_file(&temporary);
        }
    }
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
    fn cached_file_identities_follow_the_file_and_ignore_an_untrusted_cache() {
        let cache = tempfile::tempdir().unwrap();
        // SAFETY: only this test reads COMPUTE_CACHE_DIR in this process.
        unsafe { std::env::set_var("COMPUTE_CACHE_DIR", cache.path()) };
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("runtime");
        std::fs::write(&file, b"one").unwrap();
        assert_eq!(
            sha256_file_identity_cached(&file).unwrap(),
            sha256_identity(b"one")
        );
        // A rewritten file is hashed again, whatever the caches hold.
        std::fs::write(&file, b"two, longer").unwrap();
        assert_eq!(
            sha256_file_identity_cached(&file).unwrap(),
            sha256_identity(b"two, longer")
        );
        // A cache anyone else could write is never believed.
        let stored = cache.path().join("executable-identities.json");
        assert!(stored.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let forged = serde_json::json!({
                identity_cache_key_for_test(&file): sha256_identity(b"forged")
            });
            std::fs::write(&stored, forged.to_string()).unwrap();
            std::fs::set_permissions(&stored, std::fs::Permissions::from_mode(0o666)).unwrap();
            assert_eq!(identity_cache::read(&file), None);
            std::fs::set_permissions(&stored, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(
                identity_cache::read(&file).as_deref(),
                Some(sha256_identity(b"forged").as_str()),
                "the key matches, so a trusted entry would be used"
            );
        }
        assert!(compute_executable_identity().is_some());
    }

    fn identity_cache_key_for_test(path: &std::path::Path) -> String {
        let print = fingerprint(path).unwrap();
        format!(
            "{}|{}|{}|{}|{:?}",
            path.display(),
            print.device,
            print.inode,
            print.size,
            print.modified
        )
    }

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
            host_isolation: crate::HostProfile::Trusted,
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
                host: None,
            }),
            dependencies: None,
            provider: None,
            admission: None,
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
            runtime_distribution_id: sha256_identity(b"runtime-distribution"),
            runtime_distribution_digest: sha256_identity(b"runtime-artifact"),
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
        assert_eq!(
            first.runtime.distribution_id.as_deref(),
            Some(sha256_identity(b"runtime-distribution").as_str())
        );
        assert_eq!(
            first.runtime.distribution_digest.as_deref(),
            Some(sha256_identity(b"runtime-artifact").as_str())
        );
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
