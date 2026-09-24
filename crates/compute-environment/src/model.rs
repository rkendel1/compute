//! The `compute.environment@1` model.
//!
//! Project = software. Environment = a deployed instance of software.
//! Workload = a service or task within a project in an environment.
//! Execution = one invocation of a workload.
//!
//! Durable records live in `compute-state`; this module defines what
//! clients submit and the live states the daemon observes.

use std::collections::BTreeMap;

use compute_policy::Policy;
use serde::{Deserialize, Serialize};

pub use compute_state::{
    DeploymentStatus, DesiredState, PortBinding, PortSpec, RestartPolicy, WorkloadKind,
};

use crate::EnvironmentError;

pub const ENVIRONMENT_VERSION: &str = "compute.environment@1";

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

/// Immutable project content: a revision label and its workloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevisionDefinition {
    /// Operator-supplied label, such as a commit. A label always names the
    /// same content.
    pub revision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub workloads: Vec<WorkloadDefinition>,
}

/// A revision plus how to run it in one environment: the shape
/// `compute project add` and manifests submit.
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

impl ProjectDefinition {
    pub fn revision_definition(&self) -> RevisionDefinition {
        RevisionDefinition {
            revision: self.revision.clone(),
            source: self.source.clone(),
            workloads: self.workloads.clone(),
        }
    }
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

/// Deploy a registered revision of a project to an environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DeployRequest {
    pub project: String,
    pub environment: String,
    /// A revision label or `rev_` ID. Defaults to the latest revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// Replace the project's configuration in this environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<BTreeMap<String, String>>,
    /// The project's desired state after deploying. Defaults to its
    /// current desired state, or running for a new membership.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_state: Option<DesiredState>,
}

/// Deploy the exact revision current in one environment to another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromoteRequest {
    pub project: String,
    pub from: String,
    pub to: String,
    /// Promote even when the source deployment is not healthy.
    #[serde(default)]
    pub allow_unhealthy: bool,
}

/// Register a shared service other projects can consume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceDefinition {
    pub name: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default = "local_provider")]
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

fn local_provider() -> String {
    "local".into()
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

pub fn validate_revision_label(revision: &str) -> Result<(), EnvironmentError> {
    if revision.is_empty()
        || revision.len() > 128
        || !revision
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:@".contains(&byte))
    {
        return Err(EnvironmentError::Invalid(
            "revision must be 1-128 letters, digits, '-', '_', '.', ':', or '@'".into(),
        ));
    }
    Ok(())
}
