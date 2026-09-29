//! Structured project failures. Each names *which stage* failed, so a
//! failure to discover a project is never reported as a failed execution.

use std::collections::BTreeMap;
use std::fmt;

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// No project could be found, or the project tool is unavailable.
    ProjectDiscoveryFailed,
    /// The tool's observation is malformed or of an unknown schema.
    PaxMetadataInvalid,
    /// The project declares something Compute cannot normalize or run.
    RequirementsUnresolved,
    /// Every candidate target lacks something the project requires.
    NoTargetSatisfiesRequirements,
    /// The environment the requirements describe could not be built.
    EnvironmentMaterializationFailed,
    /// A required runtime is not available.
    RuntimeUnavailable,
    /// A required dependency is not available.
    DependencyUnavailable,
    /// A stack manifest is malformed, or declares something unknown.
    StackInvalid,
    /// A stack component cannot be realized on the target it needs.
    StackComponentUnsupported,
    /// A credential the stack references was not supplied.
    CredentialUnavailable,
    /// The workload ran and failed.
    ExecutionFailed,
    /// The receipt does not prove what the requirements need.
    ReceiptEvidenceIncomplete,
}

impl FailureKind {
    pub const fn code(self) -> &'static str {
        match self {
            Self::ProjectDiscoveryFailed => "project_discovery_failed",
            Self::PaxMetadataInvalid => "pax_metadata_invalid",
            Self::RequirementsUnresolved => "requirements_unresolved",
            Self::NoTargetSatisfiesRequirements => "no_target_satisfies_requirements",
            Self::EnvironmentMaterializationFailed => "environment_materialization_failed",
            Self::RuntimeUnavailable => "runtime_unavailable",
            Self::DependencyUnavailable => "dependency_unavailable",
            Self::StackInvalid => "stack_invalid",
            Self::StackComponentUnsupported => "stack_component_unsupported",
            Self::CredentialUnavailable => "credential_unavailable",
            Self::ExecutionFailed => "execution_failed",
            Self::ReceiptEvidenceIncomplete => "receipt_evidence_incomplete",
        }
    }
}

/// A failure with what was required, what was available, and the result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectError {
    pub code: &'static str,
    pub kind: FailureKind,
    pub message: String,
    /// What the project requires (`runtime = node`, ...).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub required: BTreeMap<String, String>,
    /// What was found instead.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub available: BTreeMap<String, String>,
    /// `unsupported` whenever the requirements cannot be met.
    pub result: &'static str,
}

impl ProjectError {
    pub fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            code: kind.code(),
            kind,
            message: message.into(),
            required: BTreeMap::new(),
            available: BTreeMap::new(),
            result: "unsupported",
        }
    }

    pub fn require(mut self, key: &str, value: impl Into<String>) -> Self {
        self.required.insert(key.into(), value.into());
        self
    }

    pub fn found(mut self, key: &str, value: impl Into<String>) -> Self {
        self.available.insert(key.into(), value.into());
        self
    }
}

impl fmt::Display for ProjectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)?;
        if !self.required.is_empty() {
            write!(formatter, "\n  required:")?;
            for (key, value) in &self.required {
                write!(formatter, "\n    {key} = {value}")?;
            }
        }
        if !self.available.is_empty() {
            write!(formatter, "\n  target:")?;
            for (key, value) in &self.available {
                write!(formatter, "\n    {key} = {value}")?;
            }
        }
        if !self.required.is_empty() || !self.available.is_empty() {
            write!(formatter, "\n  result: {}", self.result)?;
        }
        Ok(())
    }
}

impl std::error::Error for ProjectError {}
