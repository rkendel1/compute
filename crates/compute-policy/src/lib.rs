//! Execution policy and admission control for Compute.
//!
//! Capability asks: *can it run?* Policy asks: *may it run?* Compute
//! executes only when both answers are yes.
//!
//! The evaluation core is pure: [`admit`] depends only on a [`Policy`], an
//! [`ExecutionContract`], [`ProviderFacts`], and a [`CapabilityStatus`], and
//! returns an [`AdmissionDecision`] with every applicable reason. Policy is
//! not authorization: it governs execution constraints, never who may ask.

use serde::Serialize;
use sha2::{Digest, Sha256};

pub mod admission;
pub mod contract;
pub mod policy;

pub use admission::{
    ADMISSION_VERSION, AdmissionDecision, AdmissionReason, AdmissionStatus, CapabilityStatus,
    ProviderFacts, ReasonKind, admission_identity, admit, admit_effective,
};
pub use contract::{
    CONTRACT_VERSION, ContractResources, ContractRuntime, ExecutionContract, elf_platform,
};
pub use policy::{
    EffectivePolicy, OutputClass, POLICY_FORMAT, POLICY_VERSION, Policy, PolicyDefaults,
    PolicyLimits, PolicySource, PolicySourceKind,
};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    #[error("unsupported policy version {0}; this evaluator understands compute.policy@1")]
    UnsupportedVersion(String),
    #[error("invalid policy: {field}: {message}")]
    Invalid { field: String, message: String },
    #[error("invalid execution contract: {0}")]
    Contract(String),
}

impl PolicyError {
    pub fn invalid(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Invalid {
            field: field.into(),
            message: message.into(),
        }
    }
}

/// `sha256:` identity of a value's canonical JSON encoding.
pub fn canonical_identity(value: &impl Serialize) -> String {
    let bytes = serde_json::to_vec(value).expect("policy values are serializable");
    format!("sha256:{:x}", Sha256::digest(bytes))
}
