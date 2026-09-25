//! Provider pools and capability-aware placement for Compute.
//!
//! Placement answers one question: *which provider can prove it satisfies
//! this workload contract?* It never asks where a workload would probably
//! work. A provider is either compatible with every requirement — runtime,
//! distribution, dependencies, isolation, network, resources, platform, and
//! artifact transport — or it is incompatible for structured reasons.
//! Nothing is substituted, downgraded, relaxed, or retried elsewhere.
//!
//! This crate evaluates and selects. Providers execute: the selected
//! provider receives the canonical workload through the existing provider
//! contract and binds the placement into its receipt.

use serde::Serialize;
use sha2::{Digest, Sha256};

pub mod descriptor;
pub mod dispatch;
pub mod matching;
pub mod placement;
pub mod pool;
pub mod requirements;

pub use descriptor::{
    ArtifactLimits, Availability, DESCRIPTOR_VERSION, DependencyCapsuleSupport, DescriptorError,
    DistributionOffer, Health, ProviderDescriptor, ProviderKind, ResourceCapabilities,
    RuntimeOffer, parse_platform,
};
pub use dispatch::{DispatchError, DispatchErrorCode, PlacedSubmission};
pub use matching::{
    CapabilityMatch, IncompatibilityReason, ReasonCode, match_provider, runtime_version_matches,
};
pub use placement::{
    AdmissionContext, EvaluationStatus, PLACEMENT_VERSION, PlacementExplanation, PlacementFailure,
    PlacementOutcome, PlacementPolicy, PlacementReport, ProviderEvaluation, SelectedProvider,
    SelectionPolicy, place, place_with_policy,
};
pub use pool::{
    CAPABILITY_CACHE_VERSION, CacheEntry, CapabilityCache, DEFAULT_CAPABILITY_TTL_SECONDS,
    DiscoveryError, DiscoveryMode, DiscoveryRecord, DiscoveryStatus, PoolConfig, PoolInspection,
    PoolMember, PoolMemberInspection, PoolPolicy, ProviderConfig, ProviderPool,
    validate_provider_id,
};
pub use requirements::{
    ArtifactRequirement, DependencyRequirement, DistributionRequirement, PlacementRequirements,
    REQUIREMENTS_VERSION, RequirementOptions, ResourceRequirement, RuntimeRequirement,
    SubmissionMode, elf_platform,
};

pub const LOCAL_PROTOCOL: &str = "compute.local@1";

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum PlacementError {
    #[error("invalid provider pool configuration: {0}")]
    InvalidConfig(String),
    #[error("invalid capability cache: {0}")]
    InvalidCache(String),
    #[error("invalid placement requirements: {0}")]
    InvalidRequirements(String),
}

/// `sha256:` identity of a value's canonical JSON encoding. Struct fields
/// serialize in declaration order and maps are ordered, so equal values
/// always produce equal identities.
pub fn canonical_identity(value: &impl Serialize) -> String {
    let bytes = serde_json::to_vec(value).expect("placement values are serializable");
    format!("sha256:{:x}", Sha256::digest(bytes))
}
