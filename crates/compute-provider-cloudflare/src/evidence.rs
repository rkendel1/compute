//! Evidence for each Cloudflare operation, kept distinct:
//!
//! * a **deployment receipt** proves Cloudflare accepted a Worker with this
//!   exact content. It proves nothing ran.
//! * an **invocation record** says what one request to that Worker returned,
//!   and whether the Worker that answered reported the deployed workload.
//! * a **deletion record** says whether the Worker existed and is gone.
//!
//! The execution result and its `compute.receipt@1` are the outcome; the
//! records here are the provider's own evidence behind it. None of them
//! holds a credential.

use chrono::{DateTime, Utc};
use compute_core::ProviderIdentity;
use serde::{Deserialize, Serialize};

use crate::worker::sha256;

pub const DEPLOYMENT_FORMAT: &str = "compute.cloudflare.deployment@1";
pub const INVOCATION_FORMAT: &str = "compute.cloudflare.invocation@1";
pub const DELETION_FORMAT: &str = "compute.cloudflare.deletion@1";

pub const DEPLOYMENT_NOTE: &str = "A deployment receipt proves Cloudflare accepted this Worker. It does not prove that any workload ran or succeeded.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentAction {
    /// Compute uploaded the Worker.
    Deployed,
    /// The Worker already carried exactly this deployment; nothing was
    /// uploaded.
    AlreadyDeployed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentReceipt {
    pub format: String,
    pub provider_id: String,
    pub provider: ProviderIdentity,
    pub profile: String,
    pub worker: String,
    pub deployment_id: String,
    pub workload_digest: String,
    pub harness_digest: String,
    pub harness_version: String,
    pub compatibility_date: String,
    pub manifest_id: String,
    pub action: DeploymentAction,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloudflare_etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloudflare_modified_on: Option<String>,
    pub recorded_at: DateTime<Utc>,
    pub note: String,
    pub receipt_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationOutcome {
    /// The harness ran the module to an exit status (zero or not).
    Exited,
    /// The module trapped.
    Trapped,
    /// The module is not a WASI command the harness can run.
    InvalidModule,
    /// Cloudflare stopped the invocation at the plan's CPU or memory limit
    /// (error 1102).
    ResourceLimitExceeded,
    /// Cloudflare refused the request: the daily request quota is spent
    /// (error 1027) or the request was rate limited.
    QuotaExhausted,
    /// Compute stopped waiting at the wall-clock limit. The invocation's
    /// own outcome is unknown.
    TimedOut,
    /// Cancelled by the caller. The invocation's own outcome is unknown.
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvocationRecord {
    pub format: String,
    pub execution_id: String,
    pub provider_id: String,
    pub worker: String,
    pub deployment_id: String,
    pub expected_workload: String,
    /// What the answering Worker says it runs. Absent when the harness did
    /// not answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_workload: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_harness: Option<String>,
    /// The Worker that answered reported exactly the deployed workload.
    pub workload_identity_matched: bool,
    pub outcome: InvocationOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cf_ray: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub record_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeletionResult {
    Deleted,
    /// Nothing to delete: the Worker did not exist.
    Absent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeletionRecord {
    pub format: String,
    pub provider_id: String,
    pub worker: String,
    pub result: DeletionResult,
    pub recorded_at: DateTime<Utc>,
    pub record_hash: String,
}

/// `sha256:` over the canonical JSON of a record with its hash field empty.
pub(crate) fn seal<T: Serialize>(record: &T, field: &str) -> String {
    let mut value = serde_json::to_value(record).expect("serializable");
    value[field] = serde_json::Value::String(String::new());
    sha256(&serde_json::to_vec(&value).expect("serializable"))
}

impl DeploymentReceipt {
    pub(crate) fn sealed(mut self) -> Self {
        self.receipt_hash = seal(&self, "receipt_hash");
        self
    }

    pub fn verify(&self) -> bool {
        self.format == DEPLOYMENT_FORMAT && seal(self, "receipt_hash") == self.receipt_hash
    }
}

impl InvocationRecord {
    pub(crate) fn sealed(mut self) -> Self {
        self.record_hash = seal(&self, "record_hash");
        self
    }

    pub fn verify(&self) -> bool {
        self.format == INVOCATION_FORMAT && seal(self, "record_hash") == self.record_hash
    }
}

impl DeletionRecord {
    pub(crate) fn sealed(mut self) -> Self {
        self.record_hash = seal(&self, "record_hash");
        self
    }
}
