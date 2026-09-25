//! Deterministic provider selection and inspectable placement decisions.

use std::collections::BTreeMap;

use compute_core::{
    ExecutionReceipt, PlatformIdentity, ProviderIdentity, ProviderResourceInventory,
    ReceiptPlacement, ResourceVector, SelectionMode, SelectionReason,
};
use serde::{Deserialize, Serialize};

use compute_policy::{
    AdmissionDecision, CapabilityStatus, EffectivePolicy, ExecutionContract, Policy,
    PolicySourceKind, ProviderFacts, admit,
};

use crate::canonical_identity;
use crate::descriptor::{Health, ProviderDescriptor, ProviderKind};
use crate::matching::{IncompatibilityReason, match_provider};
use crate::pool::{DiscoveryError, DiscoveryRecord, DiscoveryStatus, PoolPolicy, ProviderConfig};
use crate::requirements::PlacementRequirements;

pub const PLACEMENT_VERSION: &str = "compute.placement@1";
const POOL_ORDERING: &str = "priority_descending,provider_id_ascending";
const EXPLICIT_ORDERING: &str = "explicit_provider";

/// Caller intent applied only after capability eligibility is known.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "mode", content = "provider", rename_all = "snake_case")]
pub enum PlacementPolicy {
    #[default]
    Auto,
    Local,
    Remote,
    Provider(String),
}

impl PlacementPolicy {
    fn accepts(&self, provider: &ProviderEvaluation) -> bool {
        match self {
            Self::Auto => true,
            Self::Local => provider.provider_kind == ProviderKind::Local,
            Self::Remote => provider.provider_kind == ProviderKind::Remote,
            Self::Provider(id) => provider.provider_id == *id,
        }
    }

    fn explicit(&self) -> Option<&str> {
        match self {
            Self::Provider(id) => Some(id),
            _ => None,
        }
    }
}

/// The documented, complete selection policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionPolicy {
    /// Applied in order; compatibility always comes first.
    pub ordering: Vec<String>,
    pub require_healthy: bool,
    pub allow_stale_capabilities: bool,
}

impl SelectionPolicy {
    pub fn from_pool(policy: &PoolPolicy) -> Self {
        Self {
            ordering: vec![
                "compatibility".into(),
                "priority_descending".into(),
                "provider_id_ascending".into(),
            ],
            require_healthy: policy.require_healthy,
            allow_stale_capabilities: policy.allow_stale_capabilities,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationStatus {
    Compatible,
    Incompatible,
    /// Capability data is stale and the pool does not permit stale data.
    CapabilitiesUnknown,
    /// The provider returned malformed or contradictory capability data.
    CapabilitiesInvalid,
    /// Capability discovery could not reach the provider.
    ProviderUnavailable,
    /// Compatible, but excluded by `require_healthy`.
    ExcludedUnhealthy,
    /// Capable, but the effective execution policy does not admit it.
    PolicyDenied,
}

/// Transient allocatability, evaluated only after static compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityStatus {
    Available,
    Unavailable,
    Unknown,
}

impl EvaluationStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compatible => "compatible",
            Self::Incompatible => "incompatible",
            Self::CapabilitiesUnknown => "capabilities_unknown",
            Self::CapabilitiesInvalid => "provider_capabilities_invalid",
            Self::ProviderUnavailable => "provider_unavailable",
            Self::ExcludedUnhealthy => "excluded_unhealthy",
            Self::PolicyDenied => "policy_denied",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderEvaluation {
    pub provider_id: String,
    pub provider_kind: ProviderKind,
    pub priority: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    pub discovery: DiscoveryStatus,
    pub health: Health,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_identity: Option<ProviderIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_lifecycle: Option<compute_core::RuntimeLifecycleStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_distribution: Option<compute_core::RuntimeDistribution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<PlatformIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ProviderResourceInventory>,
    pub status: EvaluationStatus,
    pub capacity_status: CapacityStatus,
    /// Temporary capacity shortages. These never make a provider
    /// statically incompatible and may cause a durable job to wait.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capacity_reasons: Vec<IncompatibilityReason>,
    /// Capability incompatibilities.
    pub reasons: Vec<IncompatibilityReason>,
    /// Admission under this provider's effective policy, evaluated
    /// independently of capability so both facts are preserved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission: Option<AdmissionDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<DiscoveryError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectedProvider {
    pub provider_id: String,
    pub provider_kind: ProviderKind,
    pub provider_identity: ProviderIdentity,
    pub provider_protocol: String,
    pub capability_version: String,
    pub policy_id: String,
    pub admission_id: String,
    pub selection_reason: SelectionReason,
    pub runtime_lifecycle: compute_core::RuntimeLifecycleStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_distribution: Option<compute_core::RuntimeDistribution>,
    pub platform: PlatformIdentity,
    pub resources: ProviderResourceInventory,
}

/// What admission evaluates during placement: the caller's effective policy
/// and the canonical execution contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionContext {
    /// Baseline ∩ local ∩ explicit policy. Each provider's advertised
    /// policy is intersected with it.
    pub policy: EffectivePolicy,
    /// The caller's own restrictions (local ∩ explicit), sent with the
    /// request so the provider enforces them too. `None` when the caller
    /// adds nothing to the baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_policy: Option<Policy>,
    pub contract: ExecutionContract,
}

impl AdmissionContext {
    /// Compose caller policy sources with the baseline.
    pub fn new(sources: &[(PolicySourceKind, Policy)], contract: ExecutionContract) -> Self {
        let request_policy = sources
            .iter()
            .map(|(_, policy)| policy.clone())
            .reduce(|left, right| left.intersect(&right))
            // One source keeps its name so explanations can cite it; an
            // intersection of several is unnamed.
            .map(|mut policy| {
                if sources.len() > 1 {
                    policy.name = None;
                }
                policy
            });
        Self {
            policy: EffectivePolicy::compose(sources),
            request_policy,
            contract,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementOutcome {
    Placed,
    PlacementFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementFailure {
    /// `no_compatible_provider`, `explicit_provider_incompatible`, or
    /// `provider_not_configured`.
    pub code: String,
    pub message: String,
}

/// Human-readable answers derived deterministically from the decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementExplanation {
    /// What does this workload require?
    pub requires: Vec<String>,
    /// Which providers were considered, and why is each compatible or not?
    pub considered: Vec<String>,
    /// Why was the selected provider selected (or why was none)?
    pub selection: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementReport {
    pub placement_version: String,
    pub placement_id: String,
    pub outcome: PlacementOutcome,
    pub selection_mode: SelectionMode,
    pub placement_policy: PlacementPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_provider: Option<String>,
    pub requirements: PlacementRequirements,
    pub selection_policy: SelectionPolicy,
    /// The caller's effective policy (before provider policies).
    pub policy_id: String,
    pub admission: AdmissionContext,
    /// Every evaluated provider, in selection order.
    pub providers: Vec<ProviderEvaluation>,
    pub compatible_providers: Vec<String>,
    /// Compatible providers able to reserve the workload immediately.
    pub capacity_available_providers: Vec<String>,
    /// Compatible providers that would currently queue the workload.
    pub capacity_unavailable_providers: Vec<String>,
    /// Providers proven unable to satisfy the requirements.
    pub incompatible_providers: Vec<String>,
    /// Providers whose compatibility could not be established (stale,
    /// invalid, or undiscoverable capabilities) or that the policy excluded.
    pub excluded_providers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<SelectedProvider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<PlacementFailure>,
    pub explanation: PlacementExplanation,
}

impl PlacementReport {
    /// The placement evidence a provider binds into the receipt.
    pub fn receipt_binding(&self) -> Option<ReceiptPlacement> {
        let selected = self.selected.as_ref()?;
        Some(ReceiptPlacement {
            placement_id: self.placement_id.clone(),
            provider_id: selected.provider_id.clone(),
            provider_protocol: selected.provider_protocol.clone(),
            selection_mode: self.selection_mode,
            selection_reason: selected.selection_reason.clone(),
            requested_resources: ResourceVector {
                cpu_count: self
                    .requirements
                    .resources
                    .cpu_count
                    .map(u64::from)
                    .unwrap_or(0),
                memory_bytes: self.requirements.resources.memory_bytes.unwrap_or(0),
                disk_bytes: self.requirements.resources.disk_bytes.unwrap_or(0),
            },
            provider_resources: selected.resources.clone(),
            allocated_resources: ResourceVector {
                cpu_count: self
                    .requirements
                    .resources
                    .cpu_count
                    .map(u64::from)
                    .unwrap_or(0),
                memory_bytes: self.requirements.resources.memory_bytes.unwrap_or(0),
                disk_bytes: self.requirements.resources.disk_bytes.unwrap_or(0),
            },
            execution_platform: Some(selected.platform.clone()),
            reservation: None,
        })
    }

    /// Confirm a receipt proves execution happened exactly where this
    /// placement decided.
    pub fn verify_receipt(&self, receipt: &ExecutionReceipt) -> Result<(), String> {
        let selected = self
            .selected
            .as_ref()
            .ok_or_else(|| "placement did not select a provider".to_string())?;
        receipt.verify().map_err(|error| error.to_string())?;
        let mut actual_placement = receipt.placement.clone();
        if let Some(actual) = actual_placement.as_mut() {
            actual.reservation = None;
        }
        if actual_placement.as_ref() != self.receipt_binding().as_ref() {
            return Err("receipt placement differs from the placement decision".into());
        }
        if receipt.provider.as_ref() != Some(&selected.provider_identity) {
            return Err("receipt provider differs from the selected provider".into());
        }
        if let Some(distribution) = &self.requirements.distribution
            && receipt.distribution.id != distribution.id
        {
            return Err("receipt distribution differs from the required distribution".into());
        }
        if let Some(dependencies) = &self.requirements.dependencies
            && receipt
                .dependencies
                .as_ref()
                .is_none_or(|evidence| evidence.capsule_id != dependencies.id || !evidence.verified)
        {
            return Err("receipt dependency capsule differs from the required capsule".into());
        }
        if receipt.runtime.observed != self.requirements.runtime.kind {
            return Err("receipt runtime differs from the required runtime".into());
        }
        if let Some(distribution) = &selected.runtime_distribution
            && (receipt.runtime.distribution_id.as_ref() != Some(&distribution.id)
                || receipt.runtime.distribution_digest.as_ref() != Some(&distribution.digest))
        {
            return Err("receipt runtime distribution differs from placement".into());
        }
        if receipt.isolation.effective != self.requirements.isolation {
            return Err("receipt isolation differs from the required isolation".into());
        }
        if receipt.admission_status.as_deref() != Some("admitted")
            || receipt.admission_id.as_ref() != Some(&selected.admission_id)
            || receipt.policy_id.as_ref() != Some(&selected.policy_id)
        {
            return Err("receipt admission differs from the admission placement evaluated".into());
        }
        Ok(())
    }
}

/// Evaluate discovered providers and select one deterministically.
///
/// `explicit` names a provider the caller requires; it is validated, never
/// substituted. Only providers present in `records` are evaluated.
pub fn place(
    configs: &BTreeMap<String, ProviderConfig>,
    policy: &PoolPolicy,
    records: &[DiscoveryRecord],
    requirements: &PlacementRequirements,
    admission: &AdmissionContext,
    explicit: Option<&str>,
) -> PlacementReport {
    let placement_policy = explicit
        .map(|id| PlacementPolicy::Provider(id.to_owned()))
        .unwrap_or_default();
    place_with_policy(
        configs,
        policy,
        records,
        requirements,
        admission,
        placement_policy,
    )
}

pub fn place_with_policy(
    configs: &BTreeMap<String, ProviderConfig>,
    policy: &PoolPolicy,
    records: &[DiscoveryRecord],
    requirements: &PlacementRequirements,
    admission: &AdmissionContext,
    placement_policy: PlacementPolicy,
) -> PlacementReport {
    let explicit = placement_policy.explicit();
    let selection_mode = if explicit.is_some() {
        SelectionMode::Explicit
    } else {
        SelectionMode::Pool
    };
    let selection_policy = SelectionPolicy::from_pool(policy);
    let mut providers = vec![];
    for record in records {
        if explicit.is_some_and(|id| id != record.provider_id) {
            continue;
        }
        let Some(config) = configs.get(&record.provider_id) else {
            continue;
        };
        providers.push(evaluate(record, config, policy, requirements, admission));
    }
    providers.sort_by(|left, right| {
        right
            .priority
            .cmp(&left.priority)
            .then_with(|| left.provider_id.cmp(&right.provider_id))
    });

    let compatible_providers = providers
        .iter()
        .filter(|provider| provider.status == EvaluationStatus::Compatible)
        .map(|provider| provider.provider_id.clone())
        .collect::<Vec<_>>();
    let with_status = |wanted: fn(EvaluationStatus) -> bool| {
        providers
            .iter()
            .filter(|provider| wanted(provider.status))
            .map(|provider| provider.provider_id.clone())
            .collect::<Vec<_>>()
    };
    let incompatible_providers = with_status(|status| status == EvaluationStatus::Incompatible);
    let policy_denied_providers = with_status(|status| status == EvaluationStatus::PolicyDenied);
    let excluded_providers = with_status(|status| {
        !matches!(
            status,
            EvaluationStatus::Compatible | EvaluationStatus::Incompatible
        )
    });

    let candidates = || {
        providers.iter().filter(|provider| {
            provider.status == EvaluationStatus::Compatible && placement_policy.accepts(provider)
        })
    };
    let chosen = candidates()
        .find(|provider| provider.capacity_status == CapacityStatus::Available)
        .or_else(|| candidates().next());
    let capacity_available_providers = compatible_providers
        .iter()
        .filter(|id| {
            providers.iter().any(|provider| {
                &provider.provider_id == *id
                    && provider.capacity_status == CapacityStatus::Available
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    let capacity_unavailable_providers = compatible_providers
        .iter()
        .filter(|id| !capacity_available_providers.contains(id))
        .cloned()
        .collect::<Vec<_>>();
    let (selected, failure) = match (chosen, explicit) {
        (Some(provider), _) => (
            Some(SelectedProvider {
                provider_id: provider.provider_id.clone(),
                provider_kind: provider.provider_kind,
                provider_identity: provider
                    .provider_identity
                    .clone()
                    .expect("compatible providers have descriptors"),
                provider_protocol: provider.provider_kind.protocol().into(),
                capability_version: provider
                    .capability_version
                    .clone()
                    .expect("compatible providers have descriptors"),
                policy_id: provider
                    .admission
                    .as_ref()
                    .map(|decision| decision.policy_id.clone())
                    .expect("compatible providers are admitted"),
                admission_id: provider
                    .admission
                    .as_ref()
                    .map(|decision| decision.admission_id.clone())
                    .expect("compatible providers are admitted"),
                selection_reason: SelectionReason {
                    compatibility_result: "compatible".into(),
                    selection_priority: provider.priority,
                    ordering: if explicit.is_some() {
                        EXPLICIT_ORDERING
                    } else {
                        POOL_ORDERING
                    }
                    .into(),
                    compatible_candidates: providers
                        .iter()
                        .filter(|candidate| {
                            candidate.status == EvaluationStatus::Compatible
                                && placement_policy.accepts(candidate)
                        })
                        .count() as u64,
                },
                runtime_lifecycle: provider
                    .runtime_lifecycle
                    .expect("compatible providers offer the required runtime"),
                runtime_distribution: provider.runtime_distribution.clone(),
                platform: provider
                    .platform
                    .clone()
                    .expect("compatible providers have a platform"),
                resources: provider
                    .resources
                    .clone()
                    .expect("compatible providers have resource inventory"),
            }),
            None,
        ),
        (None, Some(id)) if !configs.contains_key(id) => (
            None,
            Some(PlacementFailure {
                code: "provider_not_configured".into(),
                message: format!("provider {id} is not configured in this pool"),
            }),
        ),
        (None, Some(id)) => (
            None,
            Some(PlacementFailure {
                code: if policy_denied_providers.iter().any(|denied| denied == id) {
                    "explicit_provider_denied"
                } else {
                    "explicit_provider_incompatible"
                }
                .into(),
                message: if policy_denied_providers.iter().any(|denied| denied == id) {
                    format!(
                        "explicitly selected provider {id} is capable, but policy does not admit this execution; explicit selection never bypasses policy"
                    )
                } else {
                    format!(
                        "explicitly selected provider {id} cannot satisfy this workload; no other provider is substituted"
                    )
                },
            }),
        ),
        (None, None) => (
            None,
            Some(PlacementFailure {
                code: "no_compatible_provider".into(),
                message: if compatible_providers.is_empty() {
                    format!(
                        "no provider proved it satisfies this workload contract and is admitted by policy ({} evaluated: {} incompatible, {} policy-denied, {} excluded)",
                        providers.len(),
                        incompatible_providers.len(),
                        policy_denied_providers.len(),
                        excluded_providers.len() - policy_denied_providers.len()
                    )
                } else {
                    format!(
                        "{} provider(s) are eligible, but none matches placement policy {:?}",
                        compatible_providers.len(),
                        placement_policy
                    )
                },
            }),
        ),
    };

    let placement_id = placement_identity(
        requirements,
        admission,
        &selection_policy,
        selection_mode,
        &placement_policy,
        explicit,
        &providers,
    );
    let explanation = explain(
        requirements,
        &providers,
        selected.as_ref(),
        failure.as_ref(),
    );
    PlacementReport {
        placement_version: PLACEMENT_VERSION.into(),
        placement_id,
        outcome: if selected.is_some() {
            PlacementOutcome::Placed
        } else {
            PlacementOutcome::PlacementFailed
        },
        selection_mode,
        placement_policy: placement_policy.clone(),
        requested_provider: explicit.map(str::to_owned),
        requirements: requirements.clone(),
        selection_policy,
        policy_id: admission.policy.policy_id.clone(),
        admission: admission.clone(),
        providers,
        compatible_providers,
        capacity_available_providers,
        capacity_unavailable_providers,
        incompatible_providers,
        excluded_providers,
        selected,
        failure,
        explanation,
    }
}

fn evaluate(
    record: &DiscoveryRecord,
    config: &ProviderConfig,
    policy: &PoolPolicy,
    requirements: &PlacementRequirements,
    context: &AdmissionContext,
) -> ProviderEvaluation {
    let descriptor = record.descriptor.as_ref();
    let usable = match record.status {
        DiscoveryStatus::Discovered | DiscoveryStatus::Cached => descriptor,
        DiscoveryStatus::Stale if policy.allow_stale_capabilities => descriptor,
        _ => None,
    };
    let health = record.health();
    let mut admission = None;
    let (status, reasons) = match (record.status, usable) {
        (_, Some(descriptor)) => {
            let matched = match_provider(requirements, descriptor);
            let decision = admit_on(descriptor, &matched, context);
            let admitted = decision.admitted;
            admission = Some(decision);
            if !matched.compatible {
                (EvaluationStatus::Incompatible, matched.reasons)
            } else if !admitted {
                (EvaluationStatus::PolicyDenied, vec![])
            } else if policy.require_healthy && health != Health::Healthy {
                (EvaluationStatus::ExcludedUnhealthy, vec![])
            } else {
                (EvaluationStatus::Compatible, vec![])
            }
        }
        (DiscoveryStatus::Stale, None) => (EvaluationStatus::CapabilitiesUnknown, vec![]),
        (DiscoveryStatus::Invalid, None) => (EvaluationStatus::CapabilitiesInvalid, vec![]),
        _ => (EvaluationStatus::ProviderUnavailable, vec![]),
    };
    let capacity_reasons = usable
        .map(|descriptor| capacity_reasons(requirements, &descriptor.resources))
        .unwrap_or_default();
    let capacity_status = if usable.is_none() {
        CapacityStatus::Unknown
    } else if capacity_reasons.is_empty() {
        CapacityStatus::Available
    } else {
        CapacityStatus::Unavailable
    };
    let error = match status {
        EvaluationStatus::CapabilitiesUnknown => Some(DiscoveryError {
            code: "provider_capabilities_stale".into(),
            message: format!(
                "capabilities expired at {}; refresh them with `compute provider refresh`",
                descriptor
                    .map(|descriptor| descriptor.availability.expires_at.to_rfc3339())
                    .unwrap_or_default()
            ),
        }),
        _ => record.error.clone(),
    };
    ProviderEvaluation {
        provider_id: record.provider_id.clone(),
        provider_kind: config.kind,
        priority: config.priority,
        endpoint: config.endpoint.clone(),
        discovery: record.status,
        health,
        capability_version: descriptor.map(|descriptor| descriptor.capability_version.clone()),
        provider_identity: descriptor.map(|descriptor| descriptor.provider_identity.clone()),
        runtime_lifecycle: descriptor
            .and_then(|descriptor| descriptor.runtime(requirements.runtime.kind))
            .map(|runtime| runtime.lifecycle),
        runtime_distribution: descriptor
            .and_then(|descriptor| descriptor.runtime(requirements.runtime.kind))
            .and_then(|runtime| runtime.distribution.clone()),
        platform: descriptor.map(|descriptor| descriptor.distribution.platform.clone()),
        resources: descriptor.map(|descriptor| descriptor.resources.clone()),
        status,
        capacity_status,
        capacity_reasons,
        reasons,
        admission,
        error,
    }
}

fn capacity_reasons(
    requirements: &PlacementRequirements,
    inventory: &ProviderResourceInventory,
) -> Vec<IncompatibilityReason> {
    let mut reasons = Vec::new();
    let mut push = |code, required, available, detail| {
        reasons.push(IncompatibilityReason {
            code,
            dimension: "capacity".into(),
            required,
            available,
            detail: Some(detail),
        });
    };
    if let Some(required) = requirements.resources.cpu_count
        && u64::from(required) > inventory.available.cpu_count
    {
        push(
            crate::ReasonCode::CpuUnavailable,
            serde_json::json!(required),
            serde_json::json!(inventory.available.cpu_count),
            format!(
                "cpu requires {required}, currently available {}",
                inventory.available.cpu_count
            ),
        );
    }
    if let Some(required) = requirements.resources.memory_bytes
        && required > inventory.available.memory_bytes
    {
        push(
            crate::ReasonCode::MemoryUnavailable,
            serde_json::json!(required),
            serde_json::json!(inventory.available.memory_bytes),
            format!(
                "memory requires {required} bytes, currently available {} bytes",
                inventory.available.memory_bytes
            ),
        );
    }
    if let Some(required) = requirements.resources.disk_bytes
        && required > inventory.available.disk_bytes
    {
        push(
            crate::ReasonCode::DiskUnavailable,
            serde_json::json!(required),
            serde_json::json!(inventory.available.disk_bytes),
            format!(
                "disk requires {required} bytes, currently available {} bytes",
                inventory.available.disk_bytes
            ),
        );
    }
    reasons
}

/// Admission on one provider: the caller's policy intersected with the
/// provider's advertised policy, over the provider's facts. Capability is
/// supplied as a separate input and never decides policy reasons.
fn admit_on(
    descriptor: &ProviderDescriptor,
    matched: &crate::CapabilityMatch,
    context: &AdmissionContext,
) -> AdmissionDecision {
    let effective = match &descriptor.policy {
        Some(provider_policy) => context
            .policy
            .with(PolicySourceKind::Provider, provider_policy),
        None => context.policy.clone(),
    };
    let facts = ProviderFacts {
        identity: descriptor.provider_identity.clone(),
        distribution_id: descriptor.distribution.id.clone(),
        platform: Some(descriptor.distribution.platform.clone()),
        runtime_version: descriptor
            .runtime(context.contract.runtime.kind)
            .map(|offer| offer.effective_version().to_string()),
    };
    let capability = if matched.compatible {
        CapabilityStatus::Compatible
    } else {
        CapabilityStatus::Incompatible {
            codes: matched
                .codes()
                .into_iter()
                .map(|code| code.as_str())
                .collect(),
        }
    };
    admit(&effective.policy, &context.contract, &facts, &capability)
}

/// Deterministic placement identity. It covers requirements, the pool
/// configuration that was evaluated, the capability descriptors that were
/// used, and the selection policy — never timestamps, job IDs, credentials,
/// or transient transport metadata.
fn placement_identity(
    requirements: &PlacementRequirements,
    admission: &AdmissionContext,
    policy: &SelectionPolicy,
    mode: SelectionMode,
    placement_policy: &PlacementPolicy,
    explicit: Option<&str>,
    providers: &[ProviderEvaluation],
) -> String {
    #[derive(Serialize)]
    struct Member<'a> {
        provider_id: &'a str,
        kind: ProviderKind,
        endpoint: &'a Option<String>,
        priority: i64,
        capability_version: &'a Option<String>,
        status: EvaluationStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        admission_id: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        health: Option<Health>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cpu_available: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        memory_available: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        disk_available: Option<u64>,
    }
    #[derive(Serialize)]
    struct Material<'a> {
        placement_version: &'a str,
        requirements: &'a PlacementRequirements,
        policy_id: &'a str,
        contract: &'a ExecutionContract,
        selection_policy: &'a SelectionPolicy,
        selection_mode: SelectionMode,
        placement_policy: &'a PlacementPolicy,
        requested_provider: Option<&'a str>,
        providers: Vec<Member<'a>>,
    }
    canonical_identity(&Material {
        placement_version: PLACEMENT_VERSION,
        requirements,
        policy_id: &admission.policy.policy_id,
        contract: &admission.contract,
        selection_policy: policy,
        selection_mode: mode,
        placement_policy,
        requested_provider: explicit,
        providers: providers
            .iter()
            .map(|provider| Member {
                provider_id: &provider.provider_id,
                kind: provider.provider_kind,
                endpoint: &provider.endpoint,
                priority: provider.priority,
                capability_version: &provider.capability_version,
                status: provider.status,
                admission_id: provider
                    .admission
                    .as_ref()
                    .map(|decision| decision.admission_id.as_str()),
                health: policy.require_healthy.then_some(provider.health),
                cpu_available: requirements.resources.cpu_count.and_then(|_| {
                    provider
                        .resources
                        .as_ref()
                        .map(|resources| resources.available.cpu_count)
                }),
                memory_available: requirements.resources.memory_bytes.and_then(|_| {
                    provider
                        .resources
                        .as_ref()
                        .map(|resources| resources.available.memory_bytes)
                }),
                disk_available: requirements.resources.disk_bytes.and_then(|_| {
                    provider
                        .resources
                        .as_ref()
                        .map(|resources| resources.available.disk_bytes)
                }),
            })
            .collect(),
    })
}

fn policy_reasons(provider: &ProviderEvaluation) -> String {
    provider
        .admission
        .iter()
        .flat_map(|decision| decision.reasons.iter())
        .filter(|reason| reason.kind != compute_policy::ReasonKind::Capability)
        .map(|reason| reason.message.clone())
        .collect::<Vec<_>>()
        .join("; ")
}

fn explain(
    requirements: &PlacementRequirements,
    providers: &[ProviderEvaluation],
    selected: Option<&SelectedProvider>,
    failure: Option<&PlacementFailure>,
) -> PlacementExplanation {
    let mut requires = vec![];
    let runtime = &requirements.runtime;
    requires.push(match &runtime.version {
        Some(version) => format!("runtime {} version {version}", runtime.kind),
        None => format!("runtime {} (any version)", runtime.kind),
    });
    if let Some(artifact) = &runtime.artifact_id {
        requires.push(format!("runtime artifact {artifact}"));
    }
    requires.push(match &requirements.distribution {
        Some(distribution) => format!("distribution {}", distribution.id),
        None => "distribution: not pinned by the caller".into(),
    });
    if let Some(dependencies) = &requirements.dependencies {
        requires.push(format!(
            "dependency capsule {} ({})",
            dependencies.id,
            if dependencies.embedded {
                "embedded; the provider must accept and verify it"
            } else {
                "not embedded; the provider must already hold it"
            }
        ));
    } else {
        requires.push("dependency capsule: none".into());
    }
    requires.push(format!("isolation {}", requirements.isolation));
    requires.push(format!("network {}", requirements.network));
    let resources = &requirements.resources;
    let mut limits = vec![];
    if let Some(value) = resources.cpu_count {
        limits.push(format!("cpu count {value}"));
    }
    if let Some(value) = resources.timeout_ms {
        limits.push(format!("timeout {value}ms"));
    }
    if let Some(value) = resources.memory_bytes {
        limits.push(format!("memory {value} bytes"));
    }
    if let Some(value) = resources.disk_bytes {
        limits.push(format!("disk {value} bytes"));
    }
    if let Some(value) = resources.cpu_time_ms {
        limits.push(format!("cpu time {value}ms"));
    }
    if let Some(value) = resources.process_count {
        limits.push(format!("process count {value}"));
    }
    if let Some(value) = resources.stdout_bytes {
        limits.push(format!("stdout {value} bytes"));
    }
    if let Some(value) = resources.stderr_bytes {
        limits.push(format!("stderr {value} bytes"));
    }
    requires.push(if limits.is_empty() {
        "resources: no enforced limits requested".into()
    } else {
        format!("resources: {}", limits.join(", "))
    });
    requires.push(match &requirements.platform {
        Some(platform) => format!("platform {}", platform.label()),
        None => "platform: any".into(),
    });
    if let Some(architecture) = &requirements.architecture {
        requires.push(format!("architecture {architecture}"));
    }
    requires.push(format!(
        "artifact {} transport of {} bytes, {} submission",
        requirements.artifact.mode,
        requirements.artifact.request_bytes,
        match requirements.artifact.submission {
            crate::SubmissionMode::Synchronous => "synchronous",
            crate::SubmissionMode::Job => "job",
        }
    ));

    let considered = providers
        .iter()
        .map(|provider| {
            let mut head = format!(
                "{} ({}, priority {}, health {})",
                provider.provider_id, provider.provider_kind, provider.priority, provider.health
            );
            if let Some(lifecycle) = provider.runtime_lifecycle {
                let status = serde_json::to_value(lifecycle)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_default();
                head.push_str(&format!(", runtime {} {status}", requirements.runtime.kind));
                if let Some(distribution) = &provider.runtime_distribution {
                    head.push_str(&format!(", distribution {}", distribution.id));
                }
            }
            match provider.status {
                EvaluationStatus::Compatible => match provider.capacity_status {
                    CapacityStatus::Available => {
                        format!("{head}: compatible; capacity available")
                    }
                    CapacityStatus::Unavailable => format!(
                        "{head}: compatible; capacity temporarily unavailable (will queue): {}",
                        provider
                            .capacity_reasons
                            .iter()
                            .filter_map(|reason| reason.detail.as_deref())
                            .collect::<Vec<_>>()
                            .join("; ")
                    ),
                    CapacityStatus::Unknown => {
                        format!("{head}: compatible; capacity unknown")
                    }
                },
                EvaluationStatus::PolicyDenied => format!(
                    "{head}: capable, but policy denied: {}",
                    policy_reasons(provider)
                ),
                EvaluationStatus::Incompatible => format!(
                    "{head}: incompatible{}: {}",
                    if provider
                        .admission
                        .as_ref()
                        .is_some_and(|decision| decision.has(compute_policy::ReasonKind::Policy))
                    {
                        format!(" (policy would also deny: {})", policy_reasons(provider))
                    } else {
                        " (policy would admit)".into()
                    },
                    provider
                        .reasons
                        .iter()
                        .map(|reason| {
                            format!(
                                "{} (required {}, available {})",
                                reason.code.as_str(),
                                reason.required,
                                reason.available
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
                EvaluationStatus::ExcludedUnhealthy => format!(
                    "{head}: compatible, but excluded because the pool requires healthy providers"
                ),
                status => format!(
                    "{head}: {}{}",
                    status.as_str(),
                    provider
                        .error
                        .as_ref()
                        .map(|error| format!(": {}", error.message))
                        .unwrap_or_default()
                ),
            }
        })
        .collect();

    let selection = match (selected, failure) {
        (Some(selected), _) if selected.selection_reason.ordering == EXPLICIT_ORDERING => format!(
            "selected provider {}: explicitly requested, compatible, and admitted by policy",
            selected.provider_id
        ),
        (Some(selected), _) => format!(
            "selected provider {}: compatible and admitted by policy, with selection priority {}, first among {} candidate(s) ordered by priority (descending) then provider ID (ascending)",
            selected.provider_id,
            selected.selection_reason.selection_priority,
            selected.selection_reason.compatible_candidates
        ),
        (None, Some(failure)) => format!("placement_failed: {}: {}", failure.code, failure.message),
        (None, None) => "placement_failed".into(),
    };
    PlacementExplanation {
        requires,
        considered,
        selection,
    }
}
