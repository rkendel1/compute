//! Versioned local and remote execution-provider boundary for Compute.

use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use std::collections::{BTreeMap, BTreeSet};

use compute_core::{
    BundleInput, BundleWorkloadPlan, DependencyCapsule, ExecutionResult, IsolationProfile,
    NetworkPolicy, ProviderIdentity, ReceiptPlacement, RuntimeInventory, RuntimeKind,
    WorkloadBundle, WorkloadSpec,
};
use compute_runtime::Compute;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

mod jobs;
pub use jobs::JobEvent;
use jobs::JobManager;

pub const REMOTE_PROTOCOL: &str = "compute.remote@1";
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    ProviderUnavailable,
    ProtocolUnsupported,
    Unauthorized,
    ArtifactInvalid,
    RuntimeUnavailable,
    DistributionUnavailable,
    CapabilityMismatch,
    PolicyRejected,
    TransportFailure,
    RemoteExecutionFailure,
    UnknownJob,
    JobExpired,
    IdempotencyConflict,
    EvidenceInvalid,
    ProviderInterrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub message: String,
}

impl ProviderError {
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for ProviderError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase", deny_unknown_fields)]
pub enum ArtifactTransport {
    Bundle {
        data: Vec<u8>,
    },
    Inline {
        workload: Box<WorkloadSpec>,
        entrypoint: BundleInput,
        inputs: Vec<BundleInput>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dependency_capsule: Option<Box<DependencyCapsule>>,
    },
}

impl ArtifactTransport {
    pub(crate) fn bundle(&self) -> Result<WorkloadBundle, ProviderError> {
        match self {
            Self::Bundle { data } => WorkloadBundle::from_bytes(data),
            Self::Inline {
                workload,
                entrypoint,
                inputs,
                dependency_capsule,
            } => {
                let bundle = WorkloadBundle {
                    version: compute_core::WORKLOAD_BUNDLE_VERSION,
                    workload: workload.as_ref().clone(),
                    entrypoint: entrypoint.clone(),
                    inputs: inputs.clone(),
                    dependency_capsule: dependency_capsule
                        .as_ref()
                        .map(|value| value.as_ref().clone()),
                };
                bundle.validate().map(|()| bundle)
            }
        }
        .map_err(artifact_error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ExpectedIdentities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependency_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distribution_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ExecutionOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<IsolationProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_request_id: Option<String>,
    /// Placement evidence the executing provider binds into the receipt.
    /// Like the request ID, it is metadata and excluded from the request hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<ReceiptPlacement>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderRequest {
    pub protocol: String,
    pub artifact: ArtifactTransport,
    #[serde(default)]
    pub expected: ExpectedIdentities,
    #[serde(default)]
    pub execution: ExecutionOptions,
}

impl ProviderRequest {
    pub fn bundle(data: Vec<u8>) -> Self {
        Self {
            protocol: REMOTE_PROTOCOL.into(),
            artifact: ArtifactTransport::Bundle { data },
            expected: ExpectedIdentities::default(),
            execution: ExecutionOptions::default(),
        }
    }

    pub fn request_hash(&self) -> Result<String, ProviderError> {
        #[derive(Serialize)]
        struct SemanticRequest<'a> {
            protocol: &'a str,
            artifact: &'a ArtifactTransport,
            expected: &'a ExpectedIdentities,
            isolation: &'a Option<IsolationProfile>,
        }
        let bytes = serde_json::to_vec(&SemanticRequest {
            protocol: &self.protocol,
            artifact: &self.artifact,
            expected: &self.expected,
            isolation: &self.execution.isolation,
        })
        .map_err(transport_error)?;
        Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
    }

    fn validate(&self) -> Result<(), ProviderError> {
        if self.protocol != REMOTE_PROTOCOL {
            return Err(ProviderError::new(
                ProviderErrorKind::ProtocolUnsupported,
                format!("unsupported provider protocol: {}", self.protocol),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectResponse {
    pub protocol: String,
    pub provider: ProviderIdentity,
    pub request_hash: String,
    pub plan: BundleWorkloadPlan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecuteResponse {
    pub protocol: String,
    pub provider: ProviderIdentity,
    pub request_hash: String,
    pub result: ExecutionResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub protocol: String,
    pub provider: ProviderIdentity,
    pub artifact_modes: Vec<String>,
    pub isolation_profiles: Vec<IsolationProfile>,
    pub network_policies: Vec<NetworkPolicy>,
    pub dependency_capsule_formats: Vec<String>,
    pub max_request_bytes: u64,
    pub max_output_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distribution_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_jobs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_retention_seconds: Option<u64>,
    /// Dependency capsules already resident at the provider and resolvable
    /// by identity without transfer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependency_capsules: Vec<String>,
    /// Content identity of each runtime artifact, as recorded in receipts.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub runtime_artifacts: BTreeMap<RuntimeKind, String>,
    /// Largest wall-time limit this provider accepts, when bounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_timeout_ms: Option<u64>,
    /// Largest memory limit this provider accepts, when bounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_memory_bytes: Option<u64>,
    pub inventory: RuntimeInventory,
}

/// Operator-configured restriction of what a provider offers. A restricted
/// capability is both withheld from discovery and rejected at execution, so
/// advertised capabilities never exceed enforced ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ProviderPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtimes: Option<BTreeSet<RuntimeKind>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation_profiles: Option<BTreeSet<IsolationProfile>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_policies: Option<BTreeSet<NetworkPolicy>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_memory_bytes: Option<u64>,
}

impl ProviderPolicy {
    fn allows_runtime(&self, runtime: RuntimeKind) -> bool {
        self.runtimes
            .as_ref()
            .is_none_or(|allowed| allowed.contains(&runtime))
    }

    fn check(
        &self,
        bundle: &WorkloadBundle,
        isolation: IsolationProfile,
    ) -> Result<(), ProviderError> {
        let workload = &bundle.workload;
        let reject = |message: String| {
            Err(ProviderError::new(
                ProviderErrorKind::CapabilityMismatch,
                message,
            ))
        };
        if !self.allows_runtime(workload.runtime) {
            return reject(format!(
                "runtime {} is not offered by this provider",
                workload.runtime
            ));
        }
        if self
            .isolation_profiles
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(&isolation))
        {
            return reject(format!(
                "isolation profile {isolation} is not offered by this provider"
            ));
        }
        if self
            .network_policies
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(&workload.network))
        {
            return reject(format!(
                "network policy {} is not offered by this provider",
                workload.network
            ));
        }
        if let (Some(limit), Some(requested)) = (self.max_timeout_ms, workload.resources.wall_time)
            && requested.as_millis() > u128::from(limit)
        {
            return reject(format!(
                "timeout {}ms exceeds this provider's limit of {limit}ms",
                requested.as_millis()
            ));
        }
        if let (Some(limit), Some(requested)) =
            (self.max_memory_bytes, workload.resources.memory_bytes)
            && requested > limit
        {
            return reject(format!(
                "memory {requested} bytes exceeds this provider's limit of {limit} bytes"
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderHealth {
    pub protocol: String,
    pub provider: ProviderIdentity,
    pub healthy: bool,
}

#[async_trait]
pub trait ComputeProvider: Send + Sync {
    fn identity(&self) -> ProviderIdentity;
    async fn inspect(&self, request: ProviderRequest) -> Result<InspectResponse, ProviderError>;
    async fn execute(&self, request: ProviderRequest) -> Result<ExecuteResponse, ProviderError>;
    async fn capabilities(&self) -> Result<ProviderCapabilities, ProviderError>;
    async fn health(&self) -> Result<ProviderHealth, ProviderError>;
}

pub struct LocalProvider {
    compute: Compute,
    identity: ProviderIdentity,
    policy: ProviderPolicy,
}

impl Default for LocalProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalProvider {
    pub fn new() -> Self {
        Self::with_identity(ProviderIdentity::Local { id: "local".into() })
    }
    pub fn with_identity(identity: ProviderIdentity) -> Self {
        Self {
            compute: Compute::new(),
            identity,
            policy: ProviderPolicy::default(),
        }
    }

    pub fn with_policy(mut self, policy: ProviderPolicy) -> Self {
        self.policy = policy;
        self
    }

    fn prepare(&self, request: &ProviderRequest) -> Result<(NamedTempFile, String), ProviderError> {
        request.validate()?;
        let bundle = request.artifact.bundle()?;
        bundle
            .require_ids(
                request.expected.workload_id.as_deref(),
                request.expected.bundle_id.as_deref(),
            )
            .map_err(artifact_error)?;
        if let Some(expected) = &request.expected.dependency_id {
            let actual = bundle
                .dependency_capsule
                .as_ref()
                .ok_or_else(|| {
                    ProviderError::new(
                        ProviderErrorKind::ArtifactInvalid,
                        "expected dependency capsule is absent",
                    )
                })?
                .capsule_id()
                .map_err(artifact_error)?;
            if &actual != expected {
                return Err(ProviderError::new(
                    ProviderErrorKind::ArtifactInvalid,
                    format!("dependency identity mismatch: expected {expected}, found {actual}"),
                ));
            }
        }
        self.policy.check(
            &bundle,
            request
                .execution
                .isolation
                .unwrap_or(bundle.workload.isolation.profile)
                .max(bundle.workload.isolation.profile),
        )?;
        if let Some(placement) = &request.execution.placement {
            compute_core::validate_sha256_identity(&placement.placement_id).map_err(|error| {
                ProviderError::new(ProviderErrorKind::PolicyRejected, error.to_string())
            })?;
            if placement.provider_protocol != identity_protocol(&self.identity) {
                return Err(ProviderError::new(
                    ProviderErrorKind::PolicyRejected,
                    format!(
                        "placement names protocol {}, but this provider speaks {}",
                        placement.provider_protocol,
                        identity_protocol(&self.identity)
                    ),
                ));
            }
        }
        let file = NamedTempFile::new().map_err(transport_error)?;
        bundle.write(file.path()).map_err(artifact_error)?;
        Ok((file, request.request_hash()?))
    }
}

#[async_trait]
impl ComputeProvider for LocalProvider {
    fn identity(&self) -> ProviderIdentity {
        self.identity.clone()
    }

    async fn inspect(&self, request: ProviderRequest) -> Result<InspectResponse, ProviderError> {
        let (file, request_hash) = self.prepare(&request)?;
        let plan = self
            .compute
            .plan_bundle_with_isolation(
                file.path(),
                request.expected.workload_id.as_deref(),
                request.expected.bundle_id.as_deref(),
                request.execution.isolation,
            )
            .map_err(classify_compute_error)?;
        if let Some(expected) = &request.expected.distribution_id {
            let bundle = WorkloadBundle::read(file.path()).map_err(artifact_error)?;
            let materialized = bundle.materialize().map_err(artifact_error)?;
            let found = self
                .compute
                .distribution_identity(&materialized.request)
                .await
                .map_err(classify_compute_error)?;
            if &found.id != expected {
                return Err(ProviderError::new(
                    ProviderErrorKind::DistributionUnavailable,
                    format!(
                        "distribution identity mismatch: expected {expected}, found {}",
                        found.id
                    ),
                ));
            }
        }
        Ok(InspectResponse {
            protocol: identity_protocol(&self.identity).into(),
            provider: self.identity(),
            request_hash,
            plan,
        })
    }

    async fn execute(&self, request: ProviderRequest) -> Result<ExecuteResponse, ProviderError> {
        let (file, request_hash) = self.prepare(&request)?;
        let inspected = self.inspect(request.clone()).await?;
        let mut result = self
            .compute
            .run_bundle_with_isolation(
                file.path(),
                request.expected.workload_id.as_deref(),
                request.expected.bundle_id.as_deref(),
                request.execution.isolation,
            )
            .await
            .map_err(classify_compute_error)?;
        if let Some(expected) = &request.expected.distribution_id {
            let found = &result
                .receipt
                .as_ref()
                .ok_or_else(|| {
                    ProviderError::new(
                        ProviderErrorKind::RemoteExecutionFailure,
                        "execution result is missing its receipt",
                    )
                })?
                .distribution
                .id;
            if found != expected {
                return Err(ProviderError::new(
                    ProviderErrorKind::DistributionUnavailable,
                    format!("distribution identity mismatch: expected {expected}, found {found}"),
                ));
            }
        }
        let identity = self.identity();
        result.provider = Some(identity.clone());
        if let Some(receipt) = &mut result.receipt {
            receipt.provider = Some(identity.clone());
            receipt.provider_protocol = Some(match &identity {
                ProviderIdentity::Local { .. } => "compute.local@1".into(),
                ProviderIdentity::Remote { .. } => REMOTE_PROTOCOL.into(),
            });
            receipt.placement = request.execution.placement.clone();
            receipt.seal().map_err(classify_compute_error)?;
        }
        debug_assert_eq!(request_hash, inspected.request_hash);
        Ok(ExecuteResponse {
            protocol: identity_protocol(&identity).into(),
            provider: identity,
            request_hash,
            result,
        })
    }

    async fn capabilities(&self) -> Result<ProviderCapabilities, ProviderError> {
        let distribution = self
            .compute
            .installed_distribution_identity()
            .map_err(classify_compute_error)?;
        let mut inventory = self.compute.inventory().await;
        inventory
            .runtimes
            .retain(|runtime| self.policy.allows_runtime(runtime.id));
        let runtime_artifacts = self
            .compute
            .runtime_artifact_identities()
            .map_err(classify_compute_error)?
            .into_iter()
            .filter(|(runtime, _)| self.policy.allows_runtime(*runtime))
            .collect();
        Ok(ProviderCapabilities {
            protocol: identity_protocol(&self.identity).into(),
            provider: self.identity(),
            artifact_modes: vec!["bundle".into(), "inline".into()],
            isolation_profiles: IsolationProfile::ALL
                .into_iter()
                .filter(|profile| {
                    self.policy
                        .isolation_profiles
                        .as_ref()
                        .is_none_or(|allowed| allowed.contains(profile))
                })
                .collect(),
            network_policies: [
                NetworkPolicy::None,
                NetworkPolicy::Localhost,
                NetworkPolicy::Network,
            ]
            .into_iter()
            .filter(|policy| {
                self.policy
                    .network_policies
                    .as_ref()
                    .is_none_or(|allowed| allowed.contains(policy))
            })
            .collect(),
            dependency_capsule_formats: vec![compute_core::DEPENDENCY_CAPSULE_FORMAT.into()],
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES as u64,
            max_output_bytes: 16 * 1024 * 1024,
            distribution_id: Some(distribution.id),
            max_concurrent_jobs: None,
            job_retention_seconds: None,
            dependency_capsules: resident_dependency_capsules(),
            runtime_artifacts,
            max_timeout_ms: self.policy.max_timeout_ms,
            max_memory_bytes: self.policy.max_memory_bytes,
            inventory,
        })
    }

    async fn health(&self) -> Result<ProviderHealth, ProviderError> {
        Ok(ProviderHealth {
            protocol: identity_protocol(&self.identity).into(),
            provider: self.identity(),
            healthy: true,
        })
    }
}

pub struct RemoteProvider {
    endpoint: String,
    authorization: Option<String>,
}

impl RemoteProvider {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            authorization: None,
        }
    }
    pub fn with_bearer_token(mut self, token: impl Into<String>) -> Self {
        self.authorization = Some(format!("Bearer {}", token.into()));
        self
    }
    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ProviderError> {
        self.send::<(), T>("GET", path, None, None).await
    }
    async fn request<T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        request: &ProviderRequest,
    ) -> Result<T, ProviderError> {
        self.send(method, path, Some(request), None).await
    }
    async fn send<B: Serialize, T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&B>,
        idempotency_key: Option<&str>,
    ) -> Result<T, ProviderError> {
        let target = HttpTarget::parse(&self.endpoint)?;
        let payload = match body {
            Some(value) => serde_json::to_vec(value).map_err(transport_error)?,
            None => vec![],
        };
        let mut stream = TcpStream::connect((&*target.host, target.port))
            .await
            .map_err(transport_error)?;
        let auth = self
            .authorization
            .as_ref()
            .map(|value| format!("Authorization: {value}\r\n"))
            .unwrap_or_default();
        let idempotency = idempotency_key
            .map(|value| format!("Idempotency-Key: {value}\r\n"))
            .unwrap_or_default();
        let request = format!(
            "{method} {}{path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Type: application/json\r\nX-Compute-Protocol: {REMOTE_PROTOCOL}\r\n{auth}{idempotency}Content-Length: {}\r\n\r\n",
            target.base_path,
            target.host,
            payload.len()
        );
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(transport_error)?;
        stream.write_all(&payload).await.map_err(transport_error)?;
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .map_err(transport_error)?;
        let (_, status, response_body) = parse_http_response(&response)?;
        if !(200..300).contains(&status) {
            let error =
                serde_json::from_slice::<ProviderError>(response_body).unwrap_or_else(|_| {
                    ProviderError::new(
                        ProviderErrorKind::TransportFailure,
                        format!("provider returned HTTP {status}"),
                    )
                });
            return Err(error);
        }
        serde_json::from_slice(response_body).map_err(|error| {
            ProviderError::new(
                ProviderErrorKind::TransportFailure,
                format!("malformed remote response: {error}"),
            )
        })
    }

    pub async fn submit(
        &self,
        request: ProviderRequest,
        idempotency_key: Option<&str>,
    ) -> Result<compute_core::JobSubmission, ProviderError> {
        if idempotency_key.is_some_and(|value| {
            value.is_empty()
                || value.len() > 256
                || value.bytes().any(|byte| byte.is_ascii_control())
        }) {
            return Err(ProviderError::new(
                ProviderErrorKind::IdempotencyConflict,
                "invalid idempotency key",
            ));
        }
        self.send("POST", "/compute/jobs", Some(&request), idempotency_key)
            .await
    }

    pub async fn job_status(
        &self,
        job_id: &str,
    ) -> Result<compute_core::ExecutionJob, ProviderError> {
        let job_id = checked_job_id(job_id)?;
        self.get(&format!("/compute/jobs/{job_id}")).await
    }

    pub async fn job_result(&self, job_id: &str) -> Result<compute_core::JobResult, ProviderError> {
        let job_id = checked_job_id(job_id)?;
        self.get(&format!("/compute/jobs/{job_id}/result")).await
    }

    pub async fn job_receipt(
        &self,
        job_id: &str,
    ) -> Result<compute_core::JobReceipt, ProviderError> {
        let job_id = checked_job_id(job_id)?;
        let receipt: compute_core::JobReceipt =
            self.get(&format!("/compute/jobs/{job_id}/receipt")).await?;
        receipt.receipt.verify().map_err(|error| {
            ProviderError::new(ProviderErrorKind::EvidenceInvalid, error.to_string())
        })?;
        Ok(receipt)
    }

    pub async fn job_artifacts(
        &self,
        job_id: &str,
    ) -> Result<compute_core::JobArtifacts, ProviderError> {
        let job_id = checked_job_id(job_id)?;
        let artifacts: compute_core::JobArtifacts = self
            .get(&format!("/compute/jobs/{job_id}/artifacts"))
            .await?;
        if artifacts
            .artifacts
            .iter()
            .any(|artifact| !artifact.verify())
        {
            return Err(ProviderError::new(
                ProviderErrorKind::EvidenceInvalid,
                "downloaded artifact digest mismatch",
            ));
        }
        let receipt = self.job_receipt(&job_id.0).await?;
        let expected = receipt
            .receipt
            .outputs
            .iter()
            .filter_map(|output| output.sha256.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let actual = artifacts
            .artifacts
            .iter()
            .map(|artifact| artifact.digest.clone())
            .collect::<std::collections::BTreeSet<_>>();
        if actual != expected {
            return Err(ProviderError::new(
                ProviderErrorKind::EvidenceInvalid,
                "downloaded artifacts do not match receipt output digests",
            ));
        }
        Ok(artifacts)
    }

    pub async fn cancel_job(
        &self,
        job_id: &str,
    ) -> Result<compute_core::ExecutionJob, ProviderError> {
        let job_id = checked_job_id(job_id)?;
        self.send::<(), _>(
            "POST",
            &format!("/compute/jobs/{job_id}/cancel"),
            None,
            None,
        )
        .await
    }

    pub async fn job_events(&self, job_id: &str) -> Result<Vec<JobEvent>, ProviderError> {
        let job_id = checked_job_id(job_id)?;
        self.get(&format!("/compute/jobs/{job_id}/events")).await
    }
}

fn checked_job_id(value: &str) -> Result<compute_core::JobId, ProviderError> {
    compute_core::JobId::parse(value.to_string())
        .map_err(|_| ProviderError::new(ProviderErrorKind::UnknownJob, "malformed job identity"))
}

#[async_trait]
impl ComputeProvider for RemoteProvider {
    fn identity(&self) -> ProviderIdentity {
        let endpoint = public_endpoint(&self.endpoint);
        ProviderIdentity::Remote {
            id: endpoint.clone(),
            endpoint,
        }
    }
    async fn inspect(&self, request: ProviderRequest) -> Result<InspectResponse, ProviderError> {
        self.request("GET", "/compute/inspect", &request).await
    }
    async fn execute(&self, request: ProviderRequest) -> Result<ExecuteResponse, ProviderError> {
        self.request("POST", "/compute/execute", &request).await
    }
    async fn capabilities(&self) -> Result<ProviderCapabilities, ProviderError> {
        self.get("/compute/capabilities").await
    }
    async fn health(&self) -> Result<ProviderHealth, ProviderError> {
        self.get("/compute/health").await
    }
}

/// Capsules in `COMPUTE_DEPENDENCY_CACHE`, which execution resolves by
/// identity. Presence is advertised; verification still happens at execution.
fn resident_dependency_capsules() -> Vec<String> {
    let Some(root) = std::env::var_os("COMPUTE_DEPENDENCY_CACHE") else {
        return vec![];
    };
    let Ok(entries) = std::fs::read_dir(root) else {
        return vec![];
    };
    let mut capsules = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let digest = name.strip_suffix(".deps")?;
            let identity = format!("sha256:{digest}");
            compute_core::validate_sha256_identity(&identity).ok()?;
            Some(identity)
        })
        .collect::<Vec<_>>();
    capsules.sort();
    capsules
}

fn identity_protocol(identity: &ProviderIdentity) -> &'static str {
    match identity {
        ProviderIdentity::Local { .. } => "compute.local@1",
        ProviderIdentity::Remote { .. } => REMOTE_PROTOCOL,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderOperation {
    Inspect,
    Execute,
    Capabilities,
    Health,
    Submit,
    Status,
    Result,
    Receipt,
    Artifacts,
    Cancel,
    Events,
}

#[async_trait]
pub trait ProviderAuthorizer: Send + Sync {
    async fn authorize(
        &self,
        operation: ProviderOperation,
        authorization: Option<&str>,
    ) -> Result<(), ProviderError>;

    async fn owner(&self, authorization: Option<&str>) -> Result<String, ProviderError> {
        Ok(authorization.map_or_else(
            || "anonymous".into(),
            |value| compute_core::sha256_identity(value.as_bytes()),
        ))
    }
}

pub struct AllowAllAuthorizer;
#[async_trait]
impl ProviderAuthorizer for AllowAllAuthorizer {
    async fn authorize(&self, _: ProviderOperation, _: Option<&str>) -> Result<(), ProviderError> {
        Ok(())
    }
}

pub struct ServerConfig {
    pub provider: Arc<dyn ComputeProvider>,
    pub authorizer: Arc<dyn ProviderAuthorizer>,
    pub max_request_bytes: usize,
    pub job_store: std::path::PathBuf,
    pub job_retention: std::time::Duration,
    pub max_concurrent_jobs: usize,
}

impl ServerConfig {
    pub fn local(endpoint: impl Into<String>) -> Self {
        Self::local_with_policy(endpoint, ProviderPolicy::default())
    }

    /// A server backed by the local engine, offering only what `policy`
    /// permits.
    pub fn local_with_policy(endpoint: impl Into<String>, policy: ProviderPolicy) -> Self {
        let endpoint = public_endpoint(&endpoint.into());
        let store_id = format!("{:x}", Sha256::digest(endpoint.as_bytes()));
        Self {
            provider: Arc::new(
                LocalProvider::with_identity(ProviderIdentity::Remote {
                    id: endpoint.clone(),
                    endpoint,
                })
                .with_policy(policy),
            ),
            authorizer: Arc::new(AllowAllAuthorizer),
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
            job_store: std::env::temp_dir().join(format!("compute-jobs-{store_id}")),
            job_retention: std::time::Duration::from_secs(7 * 24 * 60 * 60),
            max_concurrent_jobs: 4,
        }
    }
}

struct ServerState {
    config: ServerConfig,
    jobs: Arc<JobManager>,
}

pub async fn serve(addr: SocketAddr, config: ServerConfig) -> Result<(), ProviderError> {
    let listener = TcpListener::bind(addr).await.map_err(transport_error)?;
    serve_listener(listener, config).await
}

pub async fn serve_listener(
    listener: TcpListener,
    config: ServerConfig,
) -> Result<(), ProviderError> {
    let jobs = JobManager::new(
        config.job_store.clone(),
        config.job_retention,
        config.max_concurrent_jobs,
        config.provider.clone(),
    )?;
    let state = Arc::new(ServerState { config, jobs });
    loop {
        let (stream, _) = listener.accept().await.map_err(transport_error)?;
        let state = state.clone();
        tokio::spawn(async move {
            let _ = handle_connection(stream, state).await;
        });
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    state: Arc<ServerState>,
) -> Result<(), ProviderError> {
    let request = match read_http_request(&mut stream, state.config.max_request_bytes).await {
        Ok(request) => request,
        Err(error) => {
            let status = error_status(error.kind);
            return write_error(&mut stream, status, error).await;
        }
    };
    let route = match parse_route(&request.method, &request.path) {
        Ok(route) => route,
        Err(error) => return write_error(&mut stream, error_status(error.kind), error).await,
    };
    let Some((operation, job_id)) = route else {
        return write_error(
            &mut stream,
            404,
            ProviderError::new(
                ProviderErrorKind::ProtocolUnsupported,
                "unknown provider endpoint",
            ),
        )
        .await;
    };
    let protocol = request
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("x-compute-protocol"))
        .map(|(_, value)| value.as_str());
    if protocol != Some(REMOTE_PROTOCOL) {
        return write_error(
            &mut stream,
            426,
            ProviderError::new(
                ProviderErrorKind::ProtocolUnsupported,
                format!("expected X-Compute-Protocol: {REMOTE_PROTOCOL}"),
            ),
        )
        .await;
    }
    let authorization = request
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .map(|(_, value)| value.as_str());
    if let Err(error) = state
        .config
        .authorizer
        .authorize(operation, authorization)
        .await
    {
        return write_error(&mut stream, 401, error).await;
    }
    let owner = match state.config.authorizer.owner(authorization).await {
        Ok(owner) => owner,
        Err(error) => return write_error(&mut stream, 401, error).await,
    };
    let result = match operation {
        ProviderOperation::Health => encode_result(state.config.provider.health().await),
        ProviderOperation::Capabilities => {
            let capabilities = state.config.provider.capabilities().await.map(|mut value| {
                value.max_concurrent_jobs = Some(state.config.max_concurrent_jobs as u64);
                value.job_retention_seconds = Some(state.config.job_retention.as_secs());
                value
            });
            encode_result(capabilities)
        }
        ProviderOperation::Inspect => match decode_provider_request(&request.body) {
            Ok(value) => encode_result(state.config.provider.inspect(value).await),
            Err(error) => Err(error),
        },
        ProviderOperation::Execute => match decode_provider_request(&request.body) {
            Ok(value) => encode_result(state.config.provider.execute(value).await),
            Err(error) => Err(error),
        },
        ProviderOperation::Submit => match decode_provider_request(&request.body) {
            Ok(value) => {
                let key = header(&request, "idempotency-key");
                encode_result(state.jobs.submit(value, owner, key).await)
            }
            Err(error) => Err(error),
        },
        ProviderOperation::Status => encode_result(
            state
                .jobs
                .status(job_id.as_ref().expect("job route"), &owner)
                .await,
        ),
        ProviderOperation::Result => encode_result(
            state
                .jobs
                .result(job_id.as_ref().expect("job route"), &owner)
                .await,
        ),
        ProviderOperation::Receipt => encode_result(
            state
                .jobs
                .receipt(job_id.as_ref().expect("job route"), &owner)
                .await,
        ),
        ProviderOperation::Artifacts => encode_result(
            state
                .jobs
                .artifacts(job_id.as_ref().expect("job route"), &owner)
                .await,
        ),
        ProviderOperation::Cancel => encode_result(
            state
                .jobs
                .cancel(job_id.as_ref().expect("job route"), &owner)
                .await,
        ),
        ProviderOperation::Events => encode_result(
            state
                .jobs
                .events(job_id.as_ref().expect("job route"), &owner)
                .await,
        ),
    };
    match result {
        Ok(body) => write_response(&mut stream, 200, &body).await,
        Err(error) => {
            let status = error_status(error.kind);
            write_error(&mut stream, status, error).await
        }
    }
}

fn parse_route(
    method: &str,
    path: &str,
) -> Result<Option<(ProviderOperation, Option<compute_core::JobId>)>, ProviderError> {
    let static_route = match (method, path) {
        ("GET", "/compute/health") => Some(ProviderOperation::Health),
        ("GET", "/compute/capabilities") => Some(ProviderOperation::Capabilities),
        ("GET", "/compute/inspect") => Some(ProviderOperation::Inspect),
        ("POST", "/compute/execute") => Some(ProviderOperation::Execute),
        ("POST", "/compute/jobs") => Some(ProviderOperation::Submit),
        _ => None,
    };
    if let Some(operation) = static_route {
        return Ok(Some((operation, None)));
    }
    let Some(remainder) = path.strip_prefix("/compute/jobs/") else {
        return Ok(None);
    };
    let mut parts = remainder.split('/');
    let raw_id = parts.next().unwrap_or_default();
    let suffix = parts.next();
    if raw_id.is_empty() || parts.next().is_some() {
        return Err(ProviderError::new(
            ProviderErrorKind::UnknownJob,
            "malformed job path",
        ));
    }
    let job_id = compute_core::JobId::parse(raw_id.to_string())
        .map_err(|_| ProviderError::new(ProviderErrorKind::UnknownJob, "malformed job identity"))?;
    let operation = match (method, suffix) {
        ("GET", None) => ProviderOperation::Status,
        ("GET", Some("result")) => ProviderOperation::Result,
        ("GET", Some("receipt")) => ProviderOperation::Receipt,
        ("GET", Some("artifacts")) => ProviderOperation::Artifacts,
        ("GET", Some("events")) => ProviderOperation::Events,
        ("POST", Some("cancel")) => ProviderOperation::Cancel,
        _ => return Ok(None),
    };
    Ok(Some((operation, Some(job_id))))
}

fn header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn encode_result<T: Serialize>(result: Result<T, ProviderError>) -> Result<Vec<u8>, ProviderError> {
    serde_json::to_vec(&result?).map_err(transport_error)
}
fn decode_provider_request(body: &[u8]) -> Result<ProviderRequest, ProviderError> {
    serde_json::from_slice(body).map_err(|error| {
        ProviderError::new(
            ProviderErrorKind::ArtifactInvalid,
            format!("malformed provider request: {error}"),
        )
    })
}

struct HttpRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}
async fn read_http_request(
    stream: &mut TcpStream,
    limit: usize,
) -> Result<HttpRequest, ProviderError> {
    let mut data = Vec::new();
    let header_end;
    loop {
        if data.len() > limit {
            return Err(ProviderError::new(
                ProviderErrorKind::PolicyRejected,
                "request exceeds server limit",
            ));
        }
        let mut chunk = [0u8; 8192];
        let count = stream.read(&mut chunk).await.map_err(transport_error)?;
        if count == 0 {
            return Err(ProviderError::new(
                ProviderErrorKind::TransportFailure,
                "incomplete HTTP request",
            ));
        }
        data.extend_from_slice(&chunk[..count]);
        if let Some(position) = find_bytes(&data, b"\r\n\r\n") {
            header_end = position + 4;
            break;
        }
    }
    let head = std::str::from_utf8(&data[..header_end - 4]).map_err(transport_error)?;
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split_whitespace();
    let method = first.next().unwrap_or_default().to_string();
    let path = first.next().unwrap_or_default().to_string();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(n, v)| (n.trim().to_string(), v.trim().to_string()))
        .collect::<Vec<_>>();
    let length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    if length > limit {
        return Err(ProviderError::new(
            ProviderErrorKind::PolicyRejected,
            "request exceeds server limit",
        ));
    }
    while data.len() < header_end + length {
        let mut chunk = [0u8; 8192];
        let count = stream.read(&mut chunk).await.map_err(transport_error)?;
        if count == 0 {
            return Err(ProviderError::new(
                ProviderErrorKind::TransportFailure,
                "incomplete HTTP body",
            ));
        }
        data.extend_from_slice(&chunk[..count]);
    }
    Ok(HttpRequest {
        method,
        path,
        headers,
        body: data[header_end..header_end + length].to_vec(),
    })
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    body: &[u8],
) -> Result<(), ProviderError> {
    let reason = if status == 200 { "OK" } else { "Error" };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(transport_error)?;
    stream.write_all(body).await.map_err(transport_error)
}
async fn write_error(
    stream: &mut TcpStream,
    status: u16,
    error: ProviderError,
) -> Result<(), ProviderError> {
    let body = serde_json::to_vec(&error).map_err(transport_error)?;
    write_response(stream, status, &body).await
}

struct HttpTarget {
    host: String,
    port: u16,
    base_path: String,
}
impl HttpTarget {
    fn parse(endpoint: &str) -> Result<Self, ProviderError> {
        let value = endpoint.strip_prefix("http://").ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::TransportFailure,
                "v1 built-in transport requires an http:// endpoint",
            )
        })?;
        if value.contains('@') || value.contains('?') || value.contains('#') {
            return Err(ProviderError::new(
                ProviderErrorKind::TransportFailure,
                "provider endpoints must not contain credentials, query strings, or fragments",
            ));
        }
        let (authority, base_path) = value
            .split_once('/')
            .map(|(a, p)| (a, format!("/{p}")))
            .unwrap_or((value, String::new()));
        let (host, port) = authority
            .rsplit_once(':')
            .map(|(h, p)| (h, p.parse::<u16>()))
            .unwrap_or((authority, Ok(80)));
        let port = port.map_err(transport_error)?;
        if host.is_empty() {
            return Err(ProviderError::new(
                ProviderErrorKind::TransportFailure,
                "endpoint host is empty",
            ));
        }
        Ok(Self {
            host: host.into(),
            port,
            base_path: base_path.trim_end_matches('/').into(),
        })
    }
}

fn public_endpoint(endpoint: &str) -> String {
    let without_fragment = endpoint.split(['?', '#']).next().unwrap_or(endpoint);
    if let Some((scheme, rest)) = without_fragment.split_once("://") {
        let authority_end = rest.find('/').unwrap_or(rest.len());
        let (authority, path) = rest.split_at(authority_end);
        let authority = authority
            .rsplit_once('@')
            .map(|(_, host)| host)
            .unwrap_or(authority);
        format!("{scheme}://{authority}{path}")
    } else {
        without_fragment.to_string()
    }
}

fn parse_http_response(data: &[u8]) -> Result<(&str, u16, &[u8]), ProviderError> {
    let end = find_bytes(data, b"\r\n\r\n").ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::TransportFailure,
            "malformed HTTP response",
        )
    })?;
    let head = std::str::from_utf8(&data[..end]).map_err(transport_error)?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| {
            ProviderError::new(ProviderErrorKind::TransportFailure, "malformed HTTP status")
        })?;
    Ok((head, status, &data[end + 4..]))
}
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|value| value == needle)
}
fn artifact_error(error: impl fmt::Display) -> ProviderError {
    ProviderError::new(ProviderErrorKind::ArtifactInvalid, error.to_string())
}
fn transport_error(error: impl fmt::Display) -> ProviderError {
    ProviderError::new(ProviderErrorKind::TransportFailure, error.to_string())
}
fn classify_compute_error(error: compute_core::ComputeError) -> ProviderError {
    let message = error.to_string();
    let kind = if message.contains("isolation") || message.contains("capability") {
        ProviderErrorKind::CapabilityMismatch
    } else if message.contains("runtime")
        && (message.contains("unavailable") || message.contains("not found"))
    {
        ProviderErrorKind::RuntimeUnavailable
    } else if message.contains("distribution") {
        ProviderErrorKind::DistributionUnavailable
    } else {
        ProviderErrorKind::RemoteExecutionFailure
    };
    ProviderError::new(kind, message)
}
fn error_status(kind: ProviderErrorKind) -> u16 {
    match kind {
        ProviderErrorKind::Unauthorized => 401,
        ProviderErrorKind::UnknownJob => 404,
        ProviderErrorKind::JobExpired => 410,
        ProviderErrorKind::IdempotencyConflict => 409,
        ProviderErrorKind::EvidenceInvalid => 422,
        ProviderErrorKind::ProviderInterrupted => 503,
        ProviderErrorKind::ProtocolUnsupported => 426,
        ProviderErrorKind::ArtifactInvalid
        | ProviderErrorKind::CapabilityMismatch
        | ProviderErrorKind::PolicyRejected => 400,
        ProviderErrorKind::RuntimeUnavailable | ProviderErrorKind::DistributionUnavailable => 409,
        _ => 500,
    }
}
