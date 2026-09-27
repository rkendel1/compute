//! Targets: the computers and infrastructure locations environments are
//! placed on.
//!
//! A target is a member of the caller-owned pool seen for what it can host:
//! its platform, its resources, whether it hosts computers (sessions), what
//! those computers can do, and the machine features it reports. It is not a
//! provider-specific abstraction: whatever implements the target (a local
//! workspace, a container host, a VM host) stays behind the provider and
//! session contracts.

use compute_core::{PlatformIdentity, ProviderResourceInventory, SessionCapabilities};
use serde::{Deserialize, Serialize};

use crate::descriptor::{Health, ProviderKind};
use crate::pool::{DiscoveryRecord, DiscoveryStatus};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputeTarget {
    /// The pool identifier: how placement and operators name the target.
    pub target_id: String,
    pub kind: ProviderKind,
    pub health: Health,
    /// Whether environments' computers can be placed here.
    pub hosts_computers: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<PlatformIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ProviderResourceInventory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<SessionCapabilities>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    /// How the target authenticates its callers, as it advertises it:
    /// `credential`, or `insecure-unauthenticated` for an open development
    /// target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authentication: Option<String>,
    /// Whether this control plane presents a credential to the target. The
    /// credential itself is never shown.
    #[serde(default)]
    pub credential: bool,
    /// Why the target could not be described, when it could not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ComputeTarget {
    pub fn from_record(record: &DiscoveryRecord, kind: ProviderKind) -> Self {
        let descriptor = record.descriptor.as_ref();
        Self {
            target_id: record.provider_id.clone(),
            kind,
            health: record.health(),
            hosts_computers: descriptor.is_some_and(|descriptor| {
                descriptor.artifact_limits.sessions && descriptor.sessions.is_some()
            }),
            platform: descriptor.map(|descriptor| descriptor.distribution.platform.clone()),
            resources: descriptor.map(|descriptor| descriptor.resources.clone()),
            capabilities: descriptor.and_then(|descriptor| descriptor.sessions),
            features: descriptor
                .map(|descriptor| descriptor.target_features.clone())
                .unwrap_or_default(),
            authentication: descriptor.and_then(|descriptor| descriptor.authentication.clone()),
            credential: false,
            error: (!matches!(
                record.status,
                DiscoveryStatus::Discovered | DiscoveryStatus::Cached
            ))
            .then(|| record.error.as_ref().map(|error| error.message.clone()))
            .flatten(),
        }
    }
}
