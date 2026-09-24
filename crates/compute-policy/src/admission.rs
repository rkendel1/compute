//! Pure admission: `Policy + ExecutionContract + ProviderFacts → decision`.
//!
//! Evaluation reads nothing but its arguments: no files, network,
//! environment, clock, randomness, or global state.

use compute_core::{PlatformIdentity, ProviderIdentity};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::canonical_identity;
use crate::contract::ExecutionContract;
use crate::policy::{EffectivePolicy, Policy};

pub const ADMISSION_VERSION: &str = "compute.admission@1";

/// The provider facts policy can constrain: where the execution would run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderFacts {
    pub identity: ProviderIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distribution_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<PlatformIdentity>,
    /// Version the provider's runtime for this contract reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_version: Option<String>,
}

/// Outcome of the capability check, carried beside (never merged into) the
/// policy evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum CapabilityStatus {
    Compatible,
    Incompatible {
        codes: Vec<String>,
    },
    /// Capabilities could not be established. Fail-closed: not admitted.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionStatus {
    Admitted,
    Denied,
}

impl AdmissionStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admitted => "admitted",
            Self::Denied => "denied",
        }
    }
}

/// Which decision a reason belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonKind {
    /// The contract itself is invalid or impossible.
    Contract,
    /// The provider cannot execute the contract.
    Capability,
    /// The policy does not permit the contract.
    Policy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionReason {
    pub code: String,
    pub kind: ReasonKind,
    pub dimension: String,
    pub requested: Value,
    pub allowed: Value,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionDecision {
    pub admission_version: String,
    pub admission_id: String,
    pub status: AdmissionStatus,
    pub admitted: bool,
    pub policy_id: String,
    pub provider: ProviderFacts,
    pub capability: CapabilityStatus,
    /// Every reason found, ordered by kind then code. Empty when admitted.
    pub reasons: Vec<AdmissionReason>,
    /// The contract exactly as evaluated; admission never rewrites it.
    pub contract: ExecutionContract,
}

impl AdmissionDecision {
    pub fn codes(&self) -> Vec<&str> {
        self.reasons
            .iter()
            .map(|reason| reason.code.as_str())
            .collect()
    }

    /// Whether any reason is of `kind`.
    pub fn has(&self, kind: ReasonKind) -> bool {
        self.reasons.iter().any(|reason| reason.kind == kind)
    }

    /// Recompute the decision from its inputs and confirm it is unchanged.
    pub fn reproduce(&self, policy: &Policy) -> bool {
        policy.policy_id() == self.policy_id
            && &admit(policy, &self.contract, &self.provider, &self.capability) == self
    }
}

struct Reasons(Vec<AdmissionReason>);

impl Reasons {
    fn push(
        &mut self,
        kind: ReasonKind,
        code: &str,
        dimension: &str,
        requested: Value,
        allowed: Value,
        message: String,
    ) {
        self.0.push(AdmissionReason {
            code: code.into(),
            kind,
            dimension: dimension.into(),
            requested,
            allowed,
            message,
        });
    }
}

/// Deterministic identity of an admission decision.
pub fn admission_identity(
    contract: &ExecutionContract,
    provider: &ProviderFacts,
    capability: &CapabilityStatus,
    policy_id: &str,
) -> String {
    #[derive(Serialize)]
    struct Material<'a> {
        admission_version: &'a str,
        contract: &'a ExecutionContract,
        provider: &'a ProviderFacts,
        capability: &'a CapabilityStatus,
        policy_id: &'a str,
    }
    canonical_identity(&Material {
        admission_version: ADMISSION_VERSION,
        contract,
        provider,
        capability,
        policy_id,
    })
}

/// Evaluate every dimension and return every reason found.
pub fn admit(
    policy: &Policy,
    contract: &ExecutionContract,
    provider: &ProviderFacts,
    capability: &CapabilityStatus,
) -> AdmissionDecision {
    let mut reasons = Reasons(vec![]);
    let runtime = contract.runtime.kind;

    // Contract sanity: an impossible request is rejected, not reinterpreted.
    let resources = &contract.resources;
    for (name, value) in [
        ("timeout_ms", resources.timeout_ms),
        ("memory_bytes", resources.memory_bytes),
        ("cpu_time_ms", resources.cpu_time_ms),
        ("process_count", resources.process_count.map(u64::from)),
        ("stdout_bytes", resources.stdout_bytes),
        ("stderr_bytes", resources.stderr_bytes),
    ] {
        if value == Some(0) {
            reasons.push(
                ReasonKind::Contract,
                "resource_request_invalid",
                "resources",
                json!({ name: 0 }),
                json!("a positive value"),
                format!("a {name} of zero cannot be satisfied"),
            );
        }
    }
    if let (Some(required), Some(actual)) = (&contract.platform, &provider.platform)
        && (required.os != actual.os || required.architecture != actual.architecture)
    {
        reasons.push(
            ReasonKind::Contract,
            "platform_conflict",
            "platform",
            json!(required.label()),
            json!(actual.label()),
            format!(
                "the workload requires {}, but the provider executes on {}",
                required.label(),
                actual.label()
            ),
        );
    }

    // Capability: reported beside policy, never folded into it.
    match capability {
        CapabilityStatus::Compatible => {}
        CapabilityStatus::Incompatible { codes } => reasons.push(
            ReasonKind::Capability,
            "capability_mismatch",
            "capability",
            json!(codes),
            json!(null),
            format!(
                "the provider cannot execute this contract: {}",
                codes.join(", ")
            ),
        ),
        CapabilityStatus::Unknown => reasons.push(
            ReasonKind::Capability,
            "capability_unknown",
            "capability",
            json!(null),
            json!(null),
            "the provider's capabilities could not be established".into(),
        ),
    }

    // Policy.
    let deny = |reasons: &mut Reasons,
                code: &str,
                dimension: &str,
                requested: Value,
                allowed: Value,
                message: String| {
        reasons.push(
            ReasonKind::Policy,
            code,
            dimension,
            requested,
            allowed,
            message,
        )
    };
    if let Some(allowed) = &policy.allowed_runtimes
        && !allowed.contains(&runtime)
    {
        deny(
            &mut reasons,
            "runtime_denied",
            "runtime",
            json!(runtime),
            json!(allowed),
            format!("requested runtime \"{runtime}\" is not allowed by policy"),
        );
    }
    if let Some(prefixes) = policy
        .allowed_runtime_versions
        .as_ref()
        .and_then(|versions| versions.get(&runtime))
    {
        match provider
            .runtime_version
            .as_deref()
            .or(contract.runtime.version.as_deref())
        {
            Some(version)
                if prefixes
                    .iter()
                    .any(|prefix| version.contains(prefix.as_str())) => {}
            Some(version) => deny(
                &mut reasons,
                "runtime_version_denied",
                "runtime",
                json!(version),
                json!(prefixes),
                format!("{runtime} version \"{version}\" is not allowed by policy"),
            ),
            None => deny(
                &mut reasons,
                "runtime_version_unknown",
                "runtime",
                json!(null),
                json!(prefixes),
                format!("policy restricts {runtime} versions, but the version is unknown"),
            ),
        }
    }
    if let Some(allowed) = &policy.allowed_distributions {
        match &provider.distribution_id {
            Some(id) if allowed.contains(id) => {}
            Some(id) => deny(
                &mut reasons,
                "distribution_denied",
                "distribution",
                json!(id),
                json!(allowed),
                format!("distribution {id} is not allowed by policy"),
            ),
            None => deny(
                &mut reasons,
                "distribution_unknown",
                "distribution",
                json!(null),
                json!(allowed),
                "policy restricts distributions, but the provider's distribution is unknown".into(),
            ),
        }
    }
    if let (Some(allowed), Some(id)) = (&policy.allowed_dependencies, &contract.dependency_id)
        && !allowed.contains(id)
    {
        deny(
            &mut reasons,
            "dependency_denied",
            "dependencies",
            json!(id),
            json!(allowed),
            format!("dependency capsule {id} is not allowed by policy"),
        );
    }
    if let Some(minimum) = policy.minimum_isolation
        && contract.isolation < minimum
    {
        deny(
            &mut reasons,
            "isolation_below_minimum",
            "isolation",
            json!(contract.isolation),
            json!(minimum),
            format!(
                "requested isolation {} is weaker than the policy minimum {minimum}",
                contract.isolation
            ),
        );
    }
    if let Some(allowed) = &policy.allowed_networks
        && !allowed.contains(&contract.network)
    {
        deny(
            &mut reasons,
            "network_denied",
            "network",
            json!(contract.network),
            json!(allowed),
            format!(
                "requested network \"{}\" is not allowed by policy",
                contract.network
            ),
        );
    }
    let limits = &policy.limits;
    for (dimension, requested, limit, unbounded, exceeded, what) in [
        (
            "timeout",
            resources.timeout_ms,
            limits.max_timeout_ms,
            "timeout_unbounded",
            "timeout_exceeds_policy",
            "timeout (ms)",
        ),
        (
            "memory",
            resources.memory_bytes,
            limits.max_memory_bytes,
            "memory_unbounded",
            "memory_exceeds_policy",
            "memory (bytes)",
        ),
        (
            "output",
            resources.output_bound(),
            limits.max_output_bytes,
            "output_unbounded",
            "output_exceeds_policy",
            "stdout plus stderr (bytes)",
        ),
    ] {
        match (requested, limit) {
            (_, None) => {}
            (None, Some(limit)) => deny(
                &mut reasons,
                unbounded,
                dimension,
                json!(null),
                json!(limit),
                format!("policy limits {what} to {limit}, but the workload declares no bound"),
            ),
            (Some(requested), Some(limit)) if requested > limit => deny(
                &mut reasons,
                exceeded,
                dimension,
                json!(requested),
                json!(limit),
                format!("requested {what} {requested} exceeds the policy limit {limit}"),
            ),
            _ => {}
        }
    }
    for (dimension, requested, limit, code) in [
        (
            "input",
            contract.input_bytes,
            limits.max_input_bytes,
            "input_exceeds_policy",
        ),
        (
            "artifact",
            contract.artifact_bytes,
            limits.max_artifact_bytes,
            "artifact_exceeds_policy",
        ),
    ] {
        if let Some(limit) = limit
            && requested > limit
        {
            deny(
                &mut reasons,
                code,
                dimension,
                json!(requested),
                json!(limit),
                format!("{dimension} size {requested} bytes exceeds the policy limit {limit}"),
            );
        }
    }
    let platform = provider.platform.as_ref().or(contract.platform.as_ref());
    for (allowed, part, code, name) in [
        (
            &policy.allowed_os,
            platform.map(|platform| platform.os.clone()),
            "platform_denied",
            "operating system",
        ),
        (
            &policy.allowed_architectures,
            platform.map(|platform| platform.architecture.clone()),
            "architecture_denied",
            "architecture",
        ),
    ] {
        match (allowed, part) {
            (None, _) => {}
            (Some(allowed), Some(value)) if allowed.contains(&value) => {}
            (Some(allowed), value) => deny(
                &mut reasons,
                if value.is_some() {
                    code
                } else {
                    "platform_unknown"
                },
                "platform",
                json!(value),
                json!(allowed),
                format!(
                    "{name} {} is not allowed by policy",
                    value.as_deref().unwrap_or("unknown")
                ),
            ),
        }
    }
    if let Some(allowed) = &policy.allowed_output_classes {
        let denied = contract
            .output_classes
            .iter()
            .filter(|class| !allowed.contains(class))
            .collect::<Vec<_>>();
        if !denied.is_empty() {
            deny(
                &mut reasons,
                "output_class_denied",
                "artifact",
                json!(denied),
                json!(allowed),
                "the workload produces an output class the policy does not allow".into(),
            );
        }
    }

    let mut reasons = reasons.0;
    reasons.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.code.cmp(&right.code))
    });
    let admitted = reasons.is_empty();
    let policy_id = policy.policy_id();
    AdmissionDecision {
        admission_version: ADMISSION_VERSION.into(),
        admission_id: admission_identity(contract, provider, capability, &policy_id),
        status: if admitted {
            AdmissionStatus::Admitted
        } else {
            AdmissionStatus::Denied
        },
        admitted,
        policy_id,
        provider: provider.clone(),
        capability: capability.clone(),
        reasons,
        contract: contract.clone(),
    }
}

/// Admission against an effective (composed) policy.
pub fn admit_effective(
    effective: &EffectivePolicy,
    contract: &ExecutionContract,
    provider: &ProviderFacts,
    capability: &CapabilityStatus,
) -> AdmissionDecision {
    admit(&effective.policy, contract, provider, capability)
}
