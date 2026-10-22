//! Cloudflare Workers as a Compute execution provider.
//!
//! ```text
//! Compute contract ──▶ CloudflareProvider (this crate) ──▶ Cloudflare API / Workers runtime
//! ```
//!
//! The adapter runs short-lived, request-driven WASI modules in a Worker
//! that Compute deploys explicitly. It implements the same
//! [`ComputeProvider`] boundary as every other provider: capabilities are
//! reported through `ProviderCapabilities`, admission is Compute's, and each
//! execution returns an `ExecutionResult` with a `compute.receipt@1`.
//!
//! Deployment, invocation, and deletion are distinct operations with
//! distinct evidence (see [`evidence`]). Execution never deploys: a workload
//! whose Worker is not deployed is refused, and deploying is an explicit,
//! authenticated call to [`CloudflareProvider::deploy`].
//!
//! Compute state stays Compute's: Cloudflare holds Workers, not records.
//! The adapter keeps no database; the only thing it remembers in memory is
//! the account's workers.dev subdomain, a cache rebuilt from the API.

pub mod api;
pub mod config;
pub mod evidence;
pub mod profile;
pub mod worker;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use chrono::Utc;
use compute_core::{
    BoundaryStatus, BundleIdentity, Capability, DistributionIdentity, ExecutionError,
    ExecutionErrorKind, ExecutionId, ExecutionPhase, ExecutionPolicySummary, ExecutionProvenance,
    ExecutionReceipt, ExecutionReceiptStatus, ExecutionRequestSummary, ExecutionResult,
    ExecutionStatus, IsolationCapabilities, IsolationEvidence, IsolationProfile, NetworkPolicy,
    Output, PlatformIdentity, ProviderIdentity, ProviderResourceInventory, ReceiptError,
    ReceiptFailureKind, ReceiptHash, ResourceUsage, ResourceVector, RuntimeCapabilities,
    RuntimeIdentity, RuntimeInventory, RuntimeInventoryEntry, RuntimeKind, RuntimeLifecycleStatus,
    RuntimeSource, WorkloadBundle, WorkloadIdentity,
};
use compute_policy::{CapabilityStatus, EffectivePolicy, ExecutionContract, PolicySourceKind};
use compute_provider::{
    Admission, ComputeProvider, ExecuteResponse, ExecutionModes, InspectResponse,
    ProviderCapabilities, ProviderError, ProviderErrorKind, ProviderHealth, ProviderRequest,
};
use serde::Serialize;
use tokio::sync::{OnceCell, Semaphore};

pub use api::{ApiClient, Credential, CredentialSource};
pub use config::{CloudflareSettings, ConfigError, ExecutionLimits};
pub use evidence::{
    DeletionRecord, DeletionResult, DeploymentAction, DeploymentReceipt, InvocationOutcome,
    InvocationRecord,
};
pub use profile::{CloudflareProfile, WORKERS_FREE};
pub use worker::{WorkerPlan, invoke_token};

pub use compute_core::CLOUDFLARE_PROTOCOL;

/// `<os>-<architecture>` Compute reports for the Workers runtime.
pub const PLATFORM: &str = "cloudflare-workers";
/// The runtime version a Cloudflare provider offers for `wasm`.
pub const WASM_RUNTIME_VERSION: &str = "wasi-preview1 (compute-harness/1)";

const MIB: u64 = 1024 * 1024;

/// What one invocation carries besides the module: the request-driven
/// workload's arguments, environment, and standard input.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Invocation {
    /// `argv[0]` is the module's name; these follow it.
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub stdin: Vec<u8>,
    /// Wall-clock limit for this invocation, at most the configured one.
    pub timeout: Option<Duration>,
}

/// A deployment as Cloudflare reports it now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeploymentStatus {
    pub worker: String,
    pub deployment_id: String,
    pub workload_digest: String,
    /// `deployed`, `not_deployed`, or `different` (a Worker with this name
    /// carries another deployment).
    pub state: String,
}

/// A Worker Compute deployed on this account (by its prefix).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeployedWorker {
    pub worker: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_on: Option<String>,
}

/// The account check `compute cloudflare validate` reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccountCheck {
    pub account_id: String,
    pub credential: String,
    pub workers_dev_subdomain: String,
}

pub struct CloudflareProvider {
    id: String,
    settings: CloudflareSettings,
    profile: CloudflareProfile,
    api: ApiClient,
    invocations: Arc<Semaphore>,
    /// Working cache: the account's workers.dev subdomain. Rebuilt from the
    /// API on first use by each process; never persisted.
    subdomain: OnceCell<String>,
}

impl std::fmt::Debug for CloudflareProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CloudflareProvider")
            .field("id", &self.id)
            .field("account_id", &self.settings.account_id)
            .field("profile", &self.profile.name)
            .finish_non_exhaustive()
    }
}

impl CloudflareProvider {
    /// A provider for one pool member. Without a credential it still
    /// reports its capabilities; every operation on the account fails with
    /// `unauthorized` naming where the credential is expected.
    pub fn new(
        id: impl Into<String>,
        settings: CloudflareSettings,
        credential: Option<Credential>,
        source: CredentialSource,
    ) -> Result<Self, ConfigError> {
        let profile = settings.validate()?;
        let api = ApiClient::new(
            settings.api_base(),
            &settings.account_id,
            credential,
            source,
        );
        Ok(Self {
            id: id.into(),
            invocations: Arc::new(Semaphore::new(
                settings.limits.max_concurrent_invocations as usize,
            )),
            settings,
            profile,
            api,
            subdomain: OnceCell::new(),
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn settings(&self) -> &CloudflareSettings {
        &self.settings
    }

    pub fn profile(&self) -> &CloudflareProfile {
        &self.profile
    }

    pub fn has_credential(&self) -> bool {
        self.api.has_credential()
    }

    pub fn credential_source(&self) -> &CredentialSource {
        self.api.source()
    }

    fn cloudflare_identity(&self) -> ProviderIdentity {
        ProviderIdentity::Cloudflare {
            id: self.id.clone(),
            account_id: self.settings.account_id.clone(),
        }
    }

    /// Plan the Worker for a module, refusing anything this profile cannot
    /// deploy. Pure: nothing is sent.
    pub fn plan(&self, module: &[u8]) -> Result<WorkerPlan, ProviderError> {
        if !module.starts_with(b"\0asm") {
            return Err(ProviderError::new(
                ProviderErrorKind::ArtifactInvalid,
                "not a WebAssembly module: the Cloudflare provider deploys WASI modules only",
            ));
        }
        let limit = self.settings.limits.max_module_bytes;
        if module.len() as u64 > limit {
            return Err(ProviderError::new(
                ProviderErrorKind::CapabilityMismatch,
                format!(
                    "the module is {} bytes; provider {} deploys modules of at most {limit} bytes (limits.max_module_bytes, within the {} profile's {}-byte Worker limit)",
                    module.len(),
                    self.id,
                    self.profile.name,
                    self.profile.limits.worker_size_bytes
                ),
            ));
        }
        Ok(WorkerPlan::new(&self.settings, module))
    }

    async fn subdomain(&self) -> Result<String, ProviderError> {
        self.subdomain
            .get_or_try_init(|| self.api.subdomain())
            .await
            .cloned()
    }

    async fn worker_url(&self, worker: &str) -> Result<String, ProviderError> {
        if let Some(base) = &self.settings.workers_base {
            return Ok(format!("{}/{worker}", base.trim_end_matches('/')));
        }
        Ok(format!(
            "https://{worker}.{}.workers.dev/",
            self.subdomain().await?
        ))
    }

    /// Check the credential and the account: the token is accepted, the
    /// account exists, and it has a workers.dev subdomain.
    pub async fn validate_account(&self) -> Result<AccountCheck, ProviderError> {
        let subdomain = self.api.subdomain().await?;
        Ok(AccountCheck {
            account_id: self.settings.account_id.clone(),
            credential: self.api.source().to_string(),
            workers_dev_subdomain: subdomain,
        })
    }

    /// Where a module's deployment stands on Cloudflare.
    pub async fn status(&self, module: &[u8]) -> Result<DeploymentStatus, ProviderError> {
        let plan = self.plan(module)?;
        let settings = self.api.script_settings(&plan.worker).await?;
        Ok(DeploymentStatus {
            state: match settings {
                None => "not_deployed",
                Some(settings) if deployment_binding(&settings) == Some(&plan.deployment_id) => {
                    "deployed"
                }
                Some(_) => "different",
            }
            .into(),
            worker: plan.worker,
            deployment_id: plan.deployment_id,
            workload_digest: plan.workload_digest,
        })
    }

    /// Deploy a module as a Worker: the explicit, authenticated provider
    /// operation. Idempotent: the Worker's name is a function of what is
    /// deployed, an identical deployment is not uploaded again, and a
    /// retried upload replaces the Worker with the same content.
    pub async fn deploy(&self, module: &[u8]) -> Result<DeploymentReceipt, ProviderError> {
        let plan = self.plan(module)?;
        let credential = self.api.credential()?.clone();
        let existing = self.api.script_settings(&plan.worker).await?;
        let (action, result) = if existing.as_ref().and_then(deployment_binding)
            == Some(&plan.deployment_id)
        {
            (DeploymentAction::AlreadyDeployed, serde_json::Value::Null)
        } else {
            let token = invoke_token(credential.expose(), &self.settings.account_id, &plan.worker);
            let (content_type, body) = plan.upload(module, &token);
            let result = self.api.upload(&plan.worker, content_type, body).await?;
            (DeploymentAction::Deployed, result)
        };
        if self.settings.workers_base.is_none() {
            self.api.enable_workers_dev(&plan.worker).await?;
        }
        let url = self.worker_url(&plan.worker).await?;
        let text = |key: &str| {
            result
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        Ok(DeploymentReceipt {
            format: evidence::DEPLOYMENT_FORMAT.into(),
            provider_id: self.id.clone(),
            provider: self.cloudflare_identity(),
            profile: self.profile.name.clone(),
            manifest_id: plan.manifest_id(),
            worker: plan.worker,
            deployment_id: plan.deployment_id,
            workload_digest: plan.workload_digest,
            harness_digest: plan.harness_digest,
            harness_version: plan.harness_version,
            compatibility_date: plan.compatibility_date,
            action,
            url,
            cloudflare_etag: text("etag"),
            cloudflare_modified_on: text("modified_on"),
            recorded_at: Utc::now(),
            note: evidence::DEPLOYMENT_NOTE.into(),
            receipt_hash: String::new(),
        }
        .sealed())
    }

    /// Delete a Worker Compute deployed. Only names carrying this
    /// provider's prefix are accepted, so Compute never deletes a Worker it
    /// does not own. Deleting an absent Worker succeeds as `absent`.
    pub async fn delete(&self, worker: &str) -> Result<DeletionRecord, ProviderError> {
        let suffix = worker.strip_prefix(&self.settings.worker_prefix);
        if !suffix.is_some_and(|suffix| {
            suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) {
            return Err(ProviderError::new(
                ProviderErrorKind::OperationUnsupported,
                format!(
                    "{worker:?} is not a Worker Compute deployed on provider {} (its names are {}<32 hex>); Compute does not delete other Workers",
                    self.id, self.settings.worker_prefix
                ),
            ));
        }
        let existed = self.api.delete(worker).await?;
        Ok(DeletionRecord {
            format: evidence::DELETION_FORMAT.into(),
            provider_id: self.id.clone(),
            worker: worker.into(),
            result: if existed {
                DeletionResult::Deleted
            } else {
                DeletionResult::Absent
            },
            recorded_at: Utc::now(),
            record_hash: String::new(),
        }
        .sealed())
    }

    /// The Workers on the account that this provider's prefix names.
    pub async fn list(&self) -> Result<Vec<DeployedWorker>, ProviderError> {
        let prefix = &self.settings.worker_prefix;
        Ok(self
            .api
            .list()
            .await?
            .into_iter()
            .filter(|(name, _)| {
                name.strip_prefix(prefix.as_str()).is_some_and(|suffix| {
                    suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
            })
            .map(|(worker, modified_on)| DeployedWorker {
                worker,
                modified_on,
            })
            .collect())
    }

    /// Invoke a deployed module directly, outside the portable workload
    /// contract: `compute cloudflare invoke`. Returns the raw outcome and
    /// its invocation record.
    pub async fn invoke(
        &self,
        module: &[u8],
        invocation: Invocation,
        control: Option<&compute_core::ExecutionControl>,
    ) -> Result<(Invoked, InvocationRecord), ProviderError> {
        let plan = self.plan(module)?;
        let execution_id = control
            .and_then(|control| control.execution_id().map(str::to_owned))
            .unwrap_or_else(compute_core::new_execution_id);
        self.call(&plan, &execution_id, invocation, control).await
    }

    async fn call(
        &self,
        plan: &WorkerPlan,
        execution_id: &str,
        invocation: Invocation,
        control: Option<&compute_core::ExecutionControl>,
    ) -> Result<(Invoked, InvocationRecord), ProviderError> {
        let limits = &self.settings.limits;
        if invocation.stdin.len() as u64 > limits.max_stdin_bytes {
            return Err(ProviderError::new(
                ProviderErrorKind::CapabilityMismatch,
                format!(
                    "standard input is {} bytes; provider {} sends at most {} (limits.max_stdin_bytes)",
                    invocation.stdin.len(),
                    self.id,
                    limits.max_stdin_bytes
                ),
            ));
        }
        let configured = Duration::from_millis(limits.timeout_ms);
        let timeout = invocation
            .timeout
            .map_or(configured, |requested| requested.min(configured));
        let credential = self.api.credential()?.clone();
        let Ok(_permit) = self.invocations.clone().try_acquire_owned() else {
            return Err(ProviderError::new(
                ProviderErrorKind::QuotaExhausted,
                format!(
                    "provider {} already runs {} invocations (limits.max_concurrent_invocations); nothing was started",
                    self.id, limits.max_concurrent_invocations
                ),
            ));
        };
        let url = self.worker_url(&plan.worker).await?;
        let token = invoke_token(credential.expose(), &self.settings.account_id, &plan.worker);
        let mut args = vec!["workload.wasm".to_owned()];
        args.extend(invocation.args);
        let body = serde_json::to_vec(&serde_json::json!({
            "protocol": worker::INVOKE_PROTOCOL,
            "execution_id": execution_id,
            "args": args,
            "env": invocation.env.into_iter().collect::<Vec<_>>(),
            "stdin": base64::engine::general_purpose::STANDARD.encode(&invocation.stdin),
            "max_output_bytes": limits.max_output_bytes,
        }))
        .expect("serializable");

        let started_at = Utc::now();
        let clock = Instant::now();
        let request = self.api.invoke(&url, &token, body);
        tokio::pin!(request);
        let cancelled = async {
            loop {
                if control.is_some_and(compute_core::ExecutionControl::is_cancelled) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        let interrupted = |outcome| Invoked {
            outcome,
            exit_code: None,
            message: None,
            stdout: vec![],
            stdout_bytes: 0,
            stderr: vec![],
            stderr_bytes: 0,
            duration: clock.elapsed(),
        };
        let (response, invoked) = tokio::select! {
            response = &mut request => (Some(response?), None),
            () = tokio::time::sleep(timeout) => (None, Some(interrupted(InvocationOutcome::TimedOut))),
            () = cancelled => (None, Some(interrupted(InvocationOutcome::Cancelled))),
        };
        let mut record = InvocationRecord {
            format: evidence::INVOCATION_FORMAT.into(),
            execution_id: execution_id.into(),
            provider_id: self.id.clone(),
            worker: plan.worker.clone(),
            deployment_id: plan.deployment_id.clone(),
            expected_workload: plan.workload_digest.clone(),
            reported_workload: None,
            reported_harness: None,
            workload_identity_matched: false,
            outcome: InvocationOutcome::TimedOut,
            http_status: None,
            cf_ray: None,
            exit_code: None,
            stdout_bytes: 0,
            stderr_bytes: 0,
            started_at,
            finished_at: Utc::now(),
            record_hash: String::new(),
        };
        let invoked = match (response, invoked) {
            (_, Some(invoked)) => invoked,
            (Some(response), None) => {
                record.http_status = Some(response.status);
                record.cf_ray = response.cf_ray.clone();
                self.interpret(plan, response, clock.elapsed(), &mut record)
                    .await?
            }
            (None, None) => unreachable!("select yields a response or an interruption"),
        };
        record.outcome = invoked.outcome;
        record.exit_code = invoked.exit_code;
        record.stdout_bytes = invoked.stdout_bytes;
        record.stderr_bytes = invoked.stderr_bytes;
        Ok((invoked, record.sealed()))
    }

    async fn interpret(
        &self,
        plan: &WorkerPlan,
        response: api::InvocationResponse,
        duration: Duration,
        record: &mut InvocationRecord,
    ) -> Result<Invoked, ProviderError> {
        let ray = response
            .cf_ray
            .as_deref()
            .map(|ray| format!(", cf-ray {ray}"))
            .unwrap_or_default();
        if let Some(json) = &response.json
            && json.get("protocol").and_then(serde_json::Value::as_str)
                == Some(worker::INVOKE_PROTOCOL)
        {
            let text = |key: &str| {
                json.get(key)
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            };
            record.reported_workload = text("workload");
            record.reported_harness = text("harness");
            record.workload_identity_matched =
                record.reported_workload.as_deref() == Some(plan.workload_digest.as_str());
            if !record.workload_identity_matched {
                return Err(ProviderError::new(
                    ProviderErrorKind::EvidenceInvalid,
                    format!(
                        "the Worker {} answered for workload {}, not the deployed {}; its result is not reported",
                        plan.worker,
                        record.reported_workload.as_deref().unwrap_or("(none)"),
                        plan.workload_digest
                    ),
                ));
            }
            let decode = |key: &str| {
                text(key)
                    .and_then(|value| base64::engine::general_purpose::STANDARD.decode(value).ok())
                    .unwrap_or_default()
            };
            let count = |key: &str| json.get(key).and_then(serde_json::Value::as_u64);
            let status = text("status").unwrap_or_default();
            let stdout = decode("stdout");
            let stderr = decode("stderr");
            return Ok(Invoked {
                outcome: match status.as_str() {
                    "exited" => InvocationOutcome::Exited,
                    "trapped" => InvocationOutcome::Trapped,
                    _ => InvocationOutcome::InvalidModule,
                },
                exit_code: json
                    .get("exit_code")
                    .and_then(serde_json::Value::as_i64)
                    .and_then(|code| i32::try_from(code).ok()),
                message: text("trap").or_else(|| text("message")),
                stdout_bytes: count("stdout_bytes").unwrap_or(stdout.len() as u64),
                stderr_bytes: count("stderr_bytes").unwrap_or(stderr.len() as u64),
                stdout,
                stderr,
                duration,
            });
        }
        if response.status == 401 && response.json.is_some() {
            return Err(ProviderError::new(
                ProviderErrorKind::Unauthorized,
                format!(
                    "the Worker {} refused Compute's invocation secret; nothing ran. The API token in {} changed since the deployment, or another Compute deployed it: deploy it again with `compute cloudflare deploy`",
                    plan.worker,
                    self.api.source()
                ),
            ));
        }
        let body = response.text.to_ascii_lowercase();
        if body.contains("1027") {
            return Err(ProviderError::new(
                ProviderErrorKind::QuotaExhausted,
                format!(
                    "Cloudflare refused the invocation with error 1027: the account's daily request quota ({} requests a day on {}) is exhausted until 00:00 UTC; nothing ran (HTTP {}{ray})",
                    self.profile.limits.requests_per_day, self.profile.name, response.status
                ),
            ));
        }
        if response.status == 429 {
            return Err(ProviderError::new(
                ProviderErrorKind::QuotaExhausted,
                format!("Cloudflare rate limited the invocation; nothing ran (HTTP 429{ray})"),
            ));
        }
        if body.contains("1102") || body.contains("exceeded resource limits") {
            return Ok(Invoked {
                outcome: InvocationOutcome::ResourceLimitExceeded,
                exit_code: None,
                message: Some(format!(
                    "Cloudflare stopped the invocation at the {} profile's limits ({} ms CPU, {} MiB memory): error 1102{ray}",
                    self.profile.name,
                    self.profile.limits.cpu_ms_per_invocation,
                    self.profile.limits.memory_bytes / MIB
                )),
                stdout: vec![],
                stdout_bytes: 0,
                stderr: vec![],
                stderr_bytes: 0,
                duration,
            });
        }
        if response.status == 404 {
            return Err(match self.api.script_settings(&plan.worker).await? {
                None => not_deployed(&self.id, plan),
                Some(_) => ProviderError::new(
                    ProviderErrorKind::ProviderUnavailable,
                    format!(
                        "the Worker {} exists but its workers.dev route did not answer (HTTP 404{ray}); deploy it again to enable the route",
                        plan.worker
                    ),
                ),
            });
        }
        let detail = response.text.chars().take(200).collect::<String>();
        Err(ProviderError::new(
            if response.status >= 500 {
                ProviderErrorKind::ProviderUnavailable
            } else {
                ProviderErrorKind::RemoteExecutionFailure
            },
            format!(
                "the invocation did not reach Compute's harness, so whether anything ran is unknown (HTTP {}{ray}): {detail}",
                response.status
            ),
        ))
    }

    /// Why this profile cannot run a workload, as a provider error. `Ok`
    /// when every requirement is one the profile provides.
    pub fn check_workload(
        &self,
        bundle: &WorkloadBundle,
        isolation: Option<IsolationProfile>,
    ) -> Result<(), ProviderError> {
        let workload = &bundle.workload;
        let profile = &self.profile.name;
        let unsupported = |capability: &str, why: String| {
            Err(ProviderError::new(
                ProviderErrorKind::OperationUnsupported,
                format!(
                    "unsupported capability {capability} on provider {} ({profile}): {why}",
                    self.id
                ),
            ))
        };
        let mismatch = |why: String| {
            Err(ProviderError::new(
                ProviderErrorKind::CapabilityMismatch,
                format!(
                    "provider {} ({profile}) cannot satisfy this workload: {why}",
                    self.id
                ),
            ))
        };
        match workload.runtime {
            RuntimeKind::Wasm => {}
            RuntimeKind::Native => {
                return unsupported(
                    "native_executable",
                    "a Cloudflare Worker is a V8 isolate, not a Linux VM; it cannot run native executables. Use a local or remote Compute provider".into(),
                );
            }
            RuntimeKind::Shell => {
                return unsupported(
                    "process_runtime",
                    "Workers has no shell and no persistent process. Use a local or remote Compute provider".into(),
                );
            }
            runtime => {
                return unsupported(
                    "process_runtime",
                    format!(
                        "{runtime} runs as a process, which a Worker cannot host; this profile runs WASI modules only. Compile to WASI, or use a local or remote Compute provider"
                    ),
                );
            }
        }
        if !bundle.inputs.is_empty() || !workload.inputs.is_empty() {
            return unsupported(
                "filesystem",
                "the Worker has no filesystem, so it cannot receive input files".into(),
            );
        }
        if !workload.outputs.is_empty() || workload.resources.disk_bytes.is_some_and(|d| d > 0) {
            return unsupported(
                "filesystem",
                "the Worker has no filesystem, so it cannot produce output files or scratch disk"
                    .into(),
            );
        }
        if bundle.dependency_capsule.is_some() || workload.dependencies.is_some() {
            return unsupported(
                "dependency_capsule",
                "dependency capsules need a filesystem the Worker does not have".into(),
            );
        }
        if workload.network != NetworkPolicy::None {
            return unsupported(
                "outbound_network",
                format!(
                    "network policy {} is not offered; the harness gives modules no sockets",
                    workload.network
                ),
            );
        }
        let isolation = isolation
            .unwrap_or_default()
            .max(workload.isolation.profile);
        if isolation == IsolationProfile::Strict {
            return mismatch(
                "strict isolation needs CPU and memory limits the workload chooses; Cloudflare enforces only its plan's fixed limits".into(),
            );
        }
        let resources = &workload.resources;
        if resources.memory_bytes.is_some() {
            return mismatch(format!(
                "a memory limit cannot be enforced at a chosen value; the isolate's fixed limit is {} MiB",
                self.profile.limits.memory_bytes / MIB
            ));
        }
        if resources.cpu_time.is_some() {
            return mismatch(format!(
                "a CPU-time limit cannot be enforced at a chosen value; Cloudflare fixes it at {} ms per invocation",
                self.profile.limits.cpu_ms_per_invocation
            ));
        }
        if resources.process_count.is_some() {
            return mismatch("a Worker has no processes to count".into());
        }
        if resources
            .memory_required_bytes
            .is_some_and(|bytes| bytes > self.profile.limits.memory_bytes)
        {
            return mismatch(format!(
                "it requires more memory than the isolate's {} MiB",
                self.profile.limits.memory_bytes / MIB
            ));
        }
        if resources.cpu_count.is_some_and(|count| count > 1) {
            return mismatch("an invocation has one thread of execution".into());
        }
        if resources.wall_time.is_some_and(|timeout| {
            timeout.as_millis() > u128::from(self.settings.limits.timeout_ms)
        }) {
            return mismatch(format!(
                "its timeout exceeds limits.timeout_ms ({} ms)",
                self.settings.limits.timeout_ms
            ));
        }
        for (name, value) in [
            ("stdout_bytes", resources.stdout_bytes),
            ("stderr_bytes", resources.stderr_bytes),
        ] {
            if value.is_some_and(|value| value > self.settings.limits.max_output_bytes) {
                return mismatch(format!(
                    "resources.{name} exceeds limits.max_output_bytes ({})",
                    self.settings.limits.max_output_bytes
                ));
            }
        }
        self.plan(&bundle.entrypoint.data)?;
        Ok(())
    }

    fn platform() -> PlatformIdentity {
        PlatformIdentity {
            os: "cloudflare".into(),
            architecture: "workers".into(),
            runtime_abi: None,
        }
    }

    fn runtime_capabilities(&self) -> RuntimeCapabilities {
        let mut capabilities = RuntimeCapabilities::wasm();
        capabilities.stdin = Capability::supported();
        // No filesystem: no inputs, outputs, or artifacts.
        capabilities.artifacts = Capability::unsupported();
        // Cancellation stops waiting and drops the request; Cloudflare then
        // cancels the invocation. Compute cannot confirm it stopped.
        capabilities.cancellation = Capability::unsupported();
        // Cloudflare enforces its plan's fixed memory and CPU limits, not
        // limits a workload chooses.
        capabilities.memory_limit = Capability::unsupported();
        capabilities.cpu_limit = Capability::unsupported();
        capabilities.isolation = IsolationCapabilities {
            process_boundary: true,
            filesystem_boundary: true,
            network_boundary: true,
            environment_boundary: true,
            timeout_enforcement: true,
            memory_enforcement: false,
            cpu_enforcement: false,
            process_enforcement: false,
        };
        capabilities
    }

    fn effective_policy(request: &ProviderRequest) -> EffectivePolicy {
        let mut sources = vec![];
        if let Some(policy) = &request.execution.policy {
            sources.push((PolicySourceKind::Explicit, policy.clone()));
        }
        EffectivePolicy::compose(&sources)
    }

    fn admit_with(
        &self,
        request: &ProviderRequest,
        policy: EffectivePolicy,
    ) -> Result<(Admission, WorkloadBundle), ProviderError> {
        request.validate()?;
        let bundle = request.artifact.bundle()?;
        bundle
            .require_ids(
                request.expected.workload_id.as_deref(),
                request.expected.bundle_id.as_deref(),
            )
            .map_err(|error| {
                ProviderError::new(ProviderErrorKind::ArtifactInvalid, error.to_string())
            })?;
        for (what, expected) in [
            ("distribution", &request.expected.distribution_id),
            ("dependency capsule", &request.expected.dependency_id),
            (
                "runtime distribution",
                &request.expected.runtime_distribution_id,
            ),
        ] {
            if expected.is_some() {
                return Err(ProviderError::new(
                    ProviderErrorKind::DistributionUnavailable,
                    format!(
                        "provider {} runs no Compute distribution, so it cannot satisfy a required {what}",
                        self.id
                    ),
                ));
            }
        }
        self.check_workload(&bundle, request.execution.isolation)?;
        let contract = ExecutionContract::from_bundle(&bundle, request.execution.isolation)
            .map_err(|error| {
                ProviderError::new(ProviderErrorKind::PolicyRejected, error.to_string())
            })?;
        let facts = compute_policy::ProviderFacts {
            identity: self.cloudflare_identity(),
            distribution_id: None,
            platform: Some(Self::platform()),
            runtime_version: Some(WASM_RUNTIME_VERSION.into()),
        };
        let decision = compute_policy::admit(
            &policy.policy,
            &contract,
            &facts,
            &CapabilityStatus::Compatible,
        );
        Ok((Admission { decision, policy }, bundle))
    }

    async fn run_admitted(
        &self,
        request: ProviderRequest,
        admission: Admission,
        control: Option<&compute_core::ExecutionControl>,
    ) -> Result<ExecuteResponse, ProviderError> {
        if admission.policy.policy.policy_id() != admission.policy.policy_id
            || admission.decision.policy_id != admission.policy.policy_id
        {
            return Err(ProviderError::new(
                ProviderErrorKind::PolicyRejected,
                "admission policy snapshot is inconsistent",
            ));
        }
        let (snapshot, bundle) = self.admit_with(&request, admission.policy.clone())?;
        if snapshot.decision != admission.decision {
            return Err(ProviderError::new(
                ProviderErrorKind::PolicyRejected,
                "admission decision does not reproduce for this request",
            ));
        }
        if !snapshot.decision.admitted {
            return Err(ProviderError::denied(snapshot.decision));
        }
        if request.execution.process_runtime.is_some() {
            return Err(ProviderError::new(
                ProviderErrorKind::OperationUnsupported,
                format!(
                    "unsupported capability persistent_process on provider {}: Workers starts no Computer processes",
                    self.id
                ),
            ));
        }
        let summary = snapshot.summary();
        let request_hash = request.request_hash()?;
        let module = &bundle.entrypoint.data;
        let plan = self.plan(module)?;
        let workload = &bundle.workload;
        let execution_id = control
            .and_then(|control| control.execution_id().map(str::to_owned))
            .unwrap_or_else(compute_core::new_execution_id);
        let started_at = Utc::now();
        let (invoked, _record) = self
            .call(
                &plan,
                &execution_id,
                Invocation {
                    args: workload.args.clone(),
                    env: workload.env.clone(),
                    stdin: vec![],
                    timeout: workload.resources.wall_time,
                },
                control,
            )
            .await?;
        let finished_at = Utc::now();
        let identity = self.cloudflare_identity();
        let isolation = request
            .execution
            .isolation
            .unwrap_or_default()
            .max(workload.isolation.profile);
        let mut result = invoked.result(
            &execution_id,
            workload.resources.stdout_bytes,
            workload.resources.stderr_bytes,
        );
        let isolation_evidence = IsolationEvidence {
            profile: isolation,
            requested: workload.isolation.profile,
            effective: isolation,
            filesystem: BoundaryStatus::Disabled,
            network: BoundaryStatus::Disabled,
            environment: BoundaryStatus::Enforced,
            resources: if workload.resources.wall_time.is_some() {
                BoundaryStatus::Enforced
            } else {
                BoundaryStatus::NotRequested
            },
            host: None,
        };
        result.isolation = Some(isolation_evidence.clone());
        result.provider = Some(identity.clone());
        result.admission = Some(summary.clone());
        let error = result.error.as_ref().map(|error| ReceiptError {
            kind: match error.kind {
                ExecutionErrorKind::Timeout => ReceiptFailureKind::Timeout,
                ExecutionErrorKind::Cancelled => ReceiptFailureKind::Cancelled,
                ExecutionErrorKind::Killed | ExecutionErrorKind::ResourceLimit => {
                    ReceiptFailureKind::Killed
                }
                ExecutionErrorKind::Runtime if error.started => ReceiptFailureKind::Workload,
                _ => ReceiptFailureKind::Compute,
            },
            code: format!("{:?}", error.kind).to_ascii_lowercase(),
        });
        let mut receipt = ExecutionReceipt {
            receipt_version: compute_core::RECEIPT_VERSION.into(),
            execution_id: ExecutionId::parse(execution_id.clone()).map_err(evidence_error)?,
            workload: WorkloadIdentity::parse(bundle.workload_id().map_err(evidence_error)?)
                .map_err(evidence_error)?,
            bundle: Some(
                BundleIdentity::parse(bundle.bundle_id().map_err(evidence_error)?)
                    .map_err(evidence_error)?,
            ),
            provider: Some(identity.clone()),
            provider_protocol: Some(CLOUDFLARE_PROTOCOL.into()),
            application: request.execution.application.clone(),
            process_runtime: None,
            placement: request.execution.placement.clone(),
            reservation: None,
            scope: request.execution.scope.clone(),
            project: None,
            stack: None,
            app_bundle: None,
            policy_id: None,
            admission_id: None,
            admission_status: None,
            // What executed is the deployed Worker: its deployment identity
            // stands where a Compute distribution would.
            distribution: DistributionIdentity {
                id: plan.deployment_id.clone(),
                platform: PLATFORM.into(),
                manifest_version: evidence::DEPLOYMENT_FORMAT.into(),
            },
            runtime: RuntimeIdentity {
                declared: RuntimeKind::Wasm,
                selected: RuntimeKind::Wasm,
                observed: RuntimeKind::Wasm,
                version: WASM_RUNTIME_VERSION.into(),
                distribution_runtime_id: plan.harness_digest.clone(),
                executable_identity: plan.workload_digest.clone(),
                distribution_id: None,
                distribution_digest: None,
            },
            request: ExecutionRequestSummary {
                entrypoint: workload
                    .entrypoint
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("<entrypoint>")
                    .into(),
                argument_count: workload.args.len() as u64,
                stdin_size: 0,
                stdin_sha256: compute_core::sha256_identity(&[]),
            },
            policy: ExecutionPolicySummary {
                network: NetworkPolicy::None,
                filesystem: "none".into(),
                timeout_ms: workload
                    .resources
                    .wall_time
                    .map(|timeout| timeout.as_millis() as u64),
                memory_bytes: None,
                environment: "explicit".into(),
                environment_names: workload.env.keys().cloned().collect(),
            },
            isolation: isolation_evidence,
            dependencies: None,
            inputs: vec![],
            outputs: vec![],
            execution: ExecutionReceiptStatus {
                status: result.status.clone(),
                exit_code: result.exit_code,
                error,
            },
            started_at: Some(started_at),
            finished_at: Some(finished_at),
            provenance: ExecutionProvenance {
                distribution_id: plan.deployment_id.clone(),
                runtime_lock_id: worker::sha256(
                    &serde_json::to_vec(&self.profile).expect("serializable"),
                ),
                manifest_id: plan.manifest_id(),
            },
            receipt_hash: ReceiptHash(String::new()),
            substrate: None,
            signature: None,
        };
        receipt.project = request
            .execution
            .project
            .as_ref()
            .map(|binding| compute_core::ReceiptProject::attest(binding, &receipt));
        receipt.stack = request
            .execution
            .stack
            .as_ref()
            .map(|binding| compute_core::ReceiptStack::attest(binding, &receipt));
        receipt.app_bundle = request
            .execution
            .app_bundle
            .as_ref()
            .map(|binding| compute_core::ReceiptAppBundle::attest(binding, &receipt));
        receipt.bind_admission(&summary);
        receipt.seal().map_err(evidence_error)?;
        result.receipt = Some(receipt);
        Ok(ExecuteResponse {
            protocol: CLOUDFLARE_PROTOCOL.into(),
            provider: identity,
            request_hash,
            result,
        })
    }
}

fn evidence_error(error: compute_core::ComputeError) -> ProviderError {
    ProviderError::new(ProviderErrorKind::EvidenceInvalid, error.to_string())
}

fn not_deployed(id: &str, plan: &WorkerPlan) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::RuntimeUnavailable,
        format!(
            "workload {} is not deployed on provider {id} (Worker {}); nothing ran. Deploy it explicitly first: compute cloudflare deploy --provider {id} <module.wasm>",
            plan.workload_digest, plan.worker
        ),
    )
}

fn deployment_binding(settings: &serde_json::Value) -> Option<&String> {
    settings
        .get("bindings")?
        .as_array()?
        .iter()
        .find(|binding| {
            binding.get("name").and_then(serde_json::Value::as_str)
                == Some(worker::BINDING_DEPLOYMENT)
        })
        .and_then(|binding| match binding.get("text") {
            Some(serde_json::Value::String(text)) => Some(text),
            _ => None,
        })
}

/// What one invocation returned, before it becomes an execution result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invoked {
    pub outcome: InvocationOutcome,
    pub exit_code: Option<i32>,
    pub message: Option<String>,
    pub stdout: Vec<u8>,
    pub stdout_bytes: u64,
    pub stderr: Vec<u8>,
    pub stderr_bytes: u64,
    pub duration: Duration,
}

impl Invoked {
    fn output(bytes: &[u8], total: u64, limit: Option<u64>) -> Output {
        let keep = limit.map_or(bytes.len(), |limit| bytes.len().min(limit as usize));
        Output {
            text: String::from_utf8_lossy(&bytes[..keep]).into_owned(),
            truncated: total > keep as u64,
            bytes: total,
        }
    }

    /// The execution result this invocation means, in Compute's terms.
    pub fn result(
        &self,
        execution_id: &str,
        stdout_limit: Option<u64>,
        stderr_limit: Option<u64>,
    ) -> ExecutionResult {
        let failure = |kind, message: String, status| {
            (
                status,
                Some(ExecutionError {
                    execution_id: execution_id.into(),
                    phase: ExecutionPhase::Running,
                    kind,
                    message,
                    runtime: Some(RuntimeKind::Wasm),
                    exit_code: None,
                    started: true,
                }),
            )
        };
        let message = self.message.clone().unwrap_or_default();
        let (status, error) = match self.outcome {
            InvocationOutcome::Exited => (ExecutionStatus::Completed, None),
            InvocationOutcome::Trapped => failure(
                ExecutionErrorKind::Runtime,
                format!("the module trapped: {message}"),
                ExecutionStatus::Failed,
            ),
            InvocationOutcome::InvalidModule => (
                ExecutionStatus::Failed,
                Some(ExecutionError {
                    execution_id: execution_id.into(),
                    phase: ExecutionPhase::Prepared,
                    kind: ExecutionErrorKind::Start,
                    message,
                    runtime: Some(RuntimeKind::Wasm),
                    exit_code: None,
                    started: false,
                }),
            ),
            InvocationOutcome::ResourceLimitExceeded => failure(
                ExecutionErrorKind::ResourceLimit,
                message,
                ExecutionStatus::Killed,
            ),
            InvocationOutcome::TimedOut => failure(
                ExecutionErrorKind::Timeout,
                "Compute stopped waiting at the wall-clock limit; Cloudflare cancels the request, but whether the module finished is unknown".into(),
                ExecutionStatus::TimedOut,
            ),
            InvocationOutcome::Cancelled => failure(
                ExecutionErrorKind::Cancelled,
                "cancelled; Cloudflare cancels the request, but whether the module finished is unknown".into(),
                ExecutionStatus::Cancelled,
            ),
            InvocationOutcome::QuotaExhausted => failure(
                ExecutionErrorKind::ResourceLimit,
                message,
                ExecutionStatus::Failed,
            ),
        };
        let mut lifecycle = vec![
            ExecutionStatus::Created,
            ExecutionStatus::Resolved,
            ExecutionStatus::Prepared,
        ];
        if self.outcome != InvocationOutcome::InvalidModule {
            lifecycle.extend([ExecutionStatus::Started, ExecutionStatus::Running]);
        }
        lifecycle.push(status.clone());
        ExecutionResult {
            execution_id: execution_id.into(),
            runtime: RuntimeKind::Wasm,
            network: NetworkPolicy::None,
            lifecycle,
            status,
            exit_code: self.exit_code,
            stdout: Self::output(&self.stdout, self.stdout_bytes, stdout_limit),
            stderr: Self::output(&self.stderr, self.stderr_bytes, stderr_limit),
            duration: self.duration,
            resource_usage: ResourceUsage {
                max_memory_bytes: None,
            },
            artifacts: vec![],
            outputs: vec![],
            missing_outputs: vec![],
            error,
            isolation: None,
            dependencies: None,
            provider: None,
            admission: None,
            receipt: None,
        }
    }
}

#[async_trait]
impl ComputeProvider for CloudflareProvider {
    fn identity(&self) -> ProviderIdentity {
        self.cloudflare_identity()
    }

    async fn inspect(&self, request: ProviderRequest) -> Result<InspectResponse, ProviderError> {
        let _ = request;
        Err(ProviderError::new(
            ProviderErrorKind::OperationUnsupported,
            format!(
                "provider {} runs no Compute engine to plan a workload; use `compute placement explain --provider {}`",
                self.id, self.id
            ),
        ))
    }

    async fn execute(&self, request: ProviderRequest) -> Result<ExecuteResponse, ProviderError> {
        let admission = self.admit(request.clone()).await?;
        self.run_admitted(request, admission, None).await
    }

    async fn admit(&self, request: ProviderRequest) -> Result<Admission, ProviderError> {
        let policy = Self::effective_policy(&request);
        self.admit_with(&request, policy)
            .map(|(admission, _)| admission)
    }

    async fn execute_admitted(
        &self,
        request: ProviderRequest,
        admission: Admission,
    ) -> Result<ExecuteResponse, ProviderError> {
        self.run_admitted(request, admission, None).await
    }

    async fn execute_admitted_controlled(
        &self,
        request: ProviderRequest,
        admission: Admission,
        control: &compute_core::ExecutionControl,
    ) -> Result<ExecuteResponse, ProviderError> {
        self.run_admitted(request, admission, Some(control)).await
    }

    /// Capabilities come from the profile and the configuration alone, so
    /// they are reported without a credential and without a network call.
    async fn capabilities(&self) -> Result<ProviderCapabilities, ProviderError> {
        let limits = &self.settings.limits;
        let memory = self.profile.limits.memory_bytes;
        let resources = ResourceVector {
            cpu_count: 1,
            memory_bytes: memory,
            disk_bytes: 0,
        };
        let harness = worker::harness_digest();
        Ok(ProviderCapabilities {
            protocol: CLOUDFLARE_PROTOCOL.into(),
            provider: self.cloudflare_identity(),
            artifact_modes: vec!["bundle".into(), "inline".into()],
            isolation_profiles: vec![IsolationProfile::Process, IsolationProfile::Sandboxed],
            network_policies: vec![NetworkPolicy::None],
            dependency_capsule_formats: vec![],
            // A bundle carries the module encoded; allow for the encoding.
            max_request_bytes: limits.max_module_bytes * 2 + MIB,
            max_output_bytes: limits.max_output_bytes,
            distribution_id: None,
            max_concurrent_jobs: None,
            available_concurrent_jobs: None,
            reserved_resources: None,
            job_retention_seconds: None,
            dependency_capsules: vec![],
            runtime_artifacts: BTreeMap::from([(RuntimeKind::Wasm, harness.clone())]),
            max_timeout_ms: Some(limits.timeout_ms),
            max_memory_bytes: Some(memory),
            resources: ProviderResourceInventory {
                capacity: resources.clone(),
                available: resources,
            },
            policy: None,
            execution: Some(ExecutionModes {
                run: true,
                jobs: false,
                deployments: false,
                sessions: false,
            }),
            sessions: None,
            target_features: vec![],
            authentication: Some("credential".into()),
            inventory: RuntimeInventory {
                compute_version: env!("CARGO_PKG_VERSION").into(),
                platform: PLATFORM.into(),
                runtimes: vec![RuntimeInventoryEntry {
                    id: RuntimeKind::Wasm,
                    version: WASM_RUNTIME_VERSION.into(),
                    platform: PLATFORM.into(),
                    distribution_id: String::new(),
                    distribution_runtime_id: harness,
                    executable_identity: None,
                    lifecycle: Some(RuntimeLifecycleStatus::Installed),
                    distribution: None,
                    executable: "cloudflare-workers".into(),
                    available: true,
                    compatible: true,
                    detected_version: None,
                    detected_executable: None,
                    source: RuntimeSource::Provider,
                    capabilities: self.runtime_capabilities(),
                    remediation: Some(
                        "deploy each module first: compute cloudflare deploy --provider <id> <module.wasm>"
                            .into(),
                    ),
                }],
            },
        })
    }

    /// Healthy when the credential reaches the account. Without a
    /// credential the provider is reported unhealthy, not unavailable: its
    /// capabilities are still known.
    async fn health(&self) -> Result<ProviderHealth, ProviderError> {
        let healthy = self.has_credential() && self.subdomain().await.is_ok();
        Ok(ProviderHealth {
            protocol: CLOUDFLARE_PROTOCOL.into(),
            provider: self.cloudflare_identity(),
            healthy,
            executions_started: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invocation_outcomes_become_compute_results() {
        let invoked = |outcome, exit_code| Invoked {
            outcome,
            exit_code,
            message: Some("detail".into()),
            stdout: b"hello".to_vec(),
            stdout_bytes: 9,
            stderr: vec![],
            stderr_bytes: 0,
            duration: Duration::from_millis(3),
        };
        let exited = invoked(InvocationOutcome::Exited, Some(3)).result("exec_1", None, None);
        assert_eq!(exited.status, ExecutionStatus::Completed);
        assert_eq!(exited.exit_code, Some(3));
        assert!(exited.stdout.truncated && exited.stdout.bytes == 9);

        let limited = invoked(InvocationOutcome::Exited, Some(0)).result("exec_1", Some(2), None);
        assert_eq!(limited.stdout.text, "he");

        for (outcome, status, kind) in [
            (
                InvocationOutcome::Trapped,
                ExecutionStatus::Failed,
                ExecutionErrorKind::Runtime,
            ),
            (
                InvocationOutcome::InvalidModule,
                ExecutionStatus::Failed,
                ExecutionErrorKind::Start,
            ),
            (
                InvocationOutcome::ResourceLimitExceeded,
                ExecutionStatus::Killed,
                ExecutionErrorKind::ResourceLimit,
            ),
            (
                InvocationOutcome::TimedOut,
                ExecutionStatus::TimedOut,
                ExecutionErrorKind::Timeout,
            ),
            (
                InvocationOutcome::Cancelled,
                ExecutionStatus::Cancelled,
                ExecutionErrorKind::Cancelled,
            ),
        ] {
            let result = invoked(outcome, None).result("exec_1", None, None);
            assert_eq!(result.status, status, "{outcome:?}");
            assert_eq!(result.error.unwrap().kind, kind, "{outcome:?}");
            assert_eq!(result.lifecycle.last(), Some(&status));
        }
    }
}
