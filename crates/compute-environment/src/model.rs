//! The `compute.environment@1` model.
//!
//! Project = software. Environment = a deployed instance of software.
//! Workload = a service or task within a project in an environment.
//! Execution = one invocation of a workload.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use compute_policy::Policy;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::EnvironmentError;

pub const ENVIRONMENT_VERSION: &str = "compute.environment@1";

/// What the operator wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DesiredState {
    #[default]
    Running,
    Stopped,
}

/// What Compute observes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActualState {
    /// Not started yet.
    Pending,
    Starting,
    Running,
    Stopping,
    Stopped,
    /// A task finished successfully.
    Completed,
    /// The workload exited unsuccessfully; siblings are unaffected.
    Failed,
    /// Admission denied execution; nothing ran.
    Denied,
    /// Some children run and some do not.
    Degraded,
}

impl ActualState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Denied => "denied",
            Self::Degraded => "degraded",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadKind {
    /// Long-lived: an API, a worker, a web server. Runs until stopped.
    Service,
    /// Runs to completion on request: build, test, migration, lint.
    Task,
}

impl WorkloadKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::Task => "task",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    /// A service that exits stays down until explicitly started.
    #[default]
    Never,
    /// A service that exits unsuccessfully is started again after a delay.
    OnFailure,
}

/// A logical port. The environment layer chooses the host binding and
/// passes it to the workload as `COMPUTE_PORT_<NAME>` (and `PORT` when the
/// workload declares exactly one port).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortSpec {
    pub name: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadDefinition {
    pub name: String,
    pub kind: WorkloadKind,
    /// The canonical `.compute` bundle.
    #[serde(with = "compute_core::bytes_json")]
    pub bundle: Vec<u8>,
    #[serde(default)]
    pub ports: Vec<PortSpec>,
    #[serde(default)]
    pub restart: RestartPolicy,
    /// For services: whether this service should run while its project
    /// runs. Tasks run only when invoked.
    #[serde(default)]
    pub desired_state: DesiredState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectDefinition {
    pub name: String,
    /// Operator-supplied revision label, such as a commit.
    pub revision: String,
    /// Where the project came from; informational.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default)]
    pub desired_state: DesiredState,
    /// Project configuration for this environment.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub workloads: Vec<WorkloadDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentDefinition {
    pub name: String,
    #[serde(default)]
    pub desired_state: DesiredState,
    /// Environment configuration, visible to every project workload.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Execution policy for every workload in this environment. It is
    /// intersected with the daemon's policy by the existing admission
    /// boundary; it can only restrict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<Policy>,
    /// Pin workloads to one provider of the daemon's pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// Stored workload: definition without bundle bytes, plus identities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadRecord {
    pub workload_id: String,
    pub name: String,
    pub kind: WorkloadKind,
    pub bundle_id: String,
    pub workload_identity: String,
    pub ports: Vec<PortSpec>,
    pub restart: RestartPolicy,
    pub desired_state: DesiredState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectRecord {
    pub project_id: String,
    pub name: String,
    pub revision: String,
    /// Digest of every workload bundle: the deployed content.
    pub revision_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub desired_state: DesiredState,
    pub env: BTreeMap<String, String>,
    pub workloads: Vec<WorkloadRecord>,
    pub deployed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentRecord {
    pub version: String,
    pub environment_id: String,
    pub name: String,
    pub desired_state: DesiredState,
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<Policy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub created_at: DateTime<Utc>,
    pub projects: BTreeMap<String, ProjectRecord>,
}

/// Names are ordinary identifiers: `preprod`, `prod`, `review-123`,
/// `customer-acme`. Nothing is reserved.
pub fn validate_name(kind: &str, name: &str) -> Result<(), EnvironmentError> {
    let valid = !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(EnvironmentError::Invalid(format!(
            "invalid {kind} name {name:?}: use 1-63 lowercase letters, digits, or '-', starting with a letter or digit"
        )))
    }
}

/// Configuration keys: environment variable names that Compute does not
/// own.
pub fn validate_env(scope: &str, env: &BTreeMap<String, String>) -> Result<(), EnvironmentError> {
    for (key, value) in env {
        if key.is_empty()
            || key.contains('=')
            || key.contains('\0')
            || value.contains('\0')
            || key.starts_with("COMPUTE_")
            || key == "PORT"
        {
            return Err(EnvironmentError::Invalid(format!(
                "{scope} configuration key {key:?} is invalid or reserved by Compute"
            )));
        }
    }
    Ok(())
}

pub(crate) fn short_digest(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    format!("{:x}", hasher.finalize())[..24].to_string()
}

pub(crate) fn project_id(environment_id: &str, project: &str) -> String {
    format!("prj_{}", short_digest(&[environment_id, project]))
}

pub(crate) fn workload_id(project_id: &str, workload: &str) -> String {
    format!("wl_{}", short_digest(&[project_id, workload]))
}
