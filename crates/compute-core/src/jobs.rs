use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    ExecutionReceipt, ExecutionResult, IsolationProfile, NetworkPolicy, ProviderIdentity,
    ResourceLimits, Result, RuntimeKind,
};

pub const EXECUTION_JOB_VERSION: &str = "compute.job@1";
static NEXT_JOB_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JobId(pub String);

impl JobId {
    pub fn generate() -> Self {
        let sequence = NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed);
        let seed = format!(
            "{}:{}:{}",
            Utc::now().timestamp_nanos_opt().unwrap_or_default(),
            std::process::id(),
            sequence
        );
        Self(format!("job_{:x}", Sha256::digest(seed.as_bytes())))
    }

    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        let digest = value
            .strip_prefix("job_")
            .ok_or_else(|| crate::ComputeError::InvalidWorkload("malformed job identity".into()))?;
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(crate::ComputeError::InvalidWorkload(
                "malformed job identity".into(),
            ));
        }
        Ok(Self(value))
    }
}

impl std::fmt::Display for JobId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Created,
    Accepted,
    Queued,
    Preparing,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
    Rejected,
}

impl JobStatus {
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::TimedOut | Self::Rejected
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct JobCancellation {
    pub requested: bool,
    pub effective: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobRequestedPolicy {
    pub isolation: IsolationProfile,
    pub network: NetworkPolicy,
    pub resources: ResourceLimits,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobRequest {
    pub request_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub workload_id: String,
    pub bundle_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependency_id: Option<String>,
    pub runtime: RuntimeKind,
    pub requested_execution: JobRequestedPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionJob {
    pub version: String,
    pub job_id: JobId,
    pub request: JobRequest,
    pub status: JobStatus,
    pub provider: ProviderIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_digest: Option<String>,
    pub cancellation: JobCancellation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobSubmission {
    pub job_id: JobId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub status: JobStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobResult {
    pub job_id: JobId,
    pub status: JobStatus,
    pub result: ExecutionResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobArtifact {
    pub digest: String,
    pub size: u64,
    #[serde(with = "crate::bytes_json")]
    pub data: Vec<u8>,
}

impl JobArtifact {
    pub fn verify(&self) -> bool {
        self.size == self.data.len() as u64 && self.digest == crate::sha256_identity(&self.data)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobArtifacts {
    pub job_id: JobId,
    pub artifacts: Vec<JobArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobReceipt {
    pub job_id: JobId,
    pub receipt: ExecutionReceipt,
}
