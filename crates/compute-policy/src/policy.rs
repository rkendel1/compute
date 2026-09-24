//! The `compute.policy@1` model: canonical form, identity, validation, and
//! composition by intersection.

use std::collections::{BTreeMap, BTreeSet};

use compute_core::{IsolationProfile, NetworkPolicy, RuntimeKind};
use serde::{Deserialize, Serialize};

use crate::{PolicyError, canonical_identity};

pub const POLICY_FORMAT: &str = "compute.policy@1";
pub const POLICY_VERSION: u32 = 1;

/// Classes of output a workload may produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputClass {
    /// Captured standard output and standard error.
    Stdio,
    /// Declared output files collected from `/output`.
    Files,
}

/// Contract values a generated workload uses when neither the caller nor
/// project configuration chose one. They never rewrite a stated value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PolicyDefaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<IsolationProfile>,
}

impl PolicyDefaults {
    fn is_empty(&self) -> bool {
        self.network.is_none() && self.isolation.is_none()
    }
}

/// Upper bounds. An absent limit places no bound; a present limit also
/// requires the workload to declare a bound it can be compared with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PolicyLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_memory_bytes: Option<u64>,
    /// Total size of the entrypoint and declared inputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_bytes: Option<u64>,
    /// Declared bound on captured stdout plus stderr.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_bytes: Option<u64>,
    /// Size of the transported `.compute` artifact, dependencies included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_artifact_bytes: Option<u64>,
}

impl PolicyLimits {
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    fn values(&self) -> [(&'static str, Option<u64>); 5] {
        [
            ("max_timeout_ms", self.max_timeout_ms),
            ("max_memory_bytes", self.max_memory_bytes),
            ("max_input_bytes", self.max_input_bytes),
            ("max_output_bytes", self.max_output_bytes),
            ("max_artifact_bytes", self.max_artifact_bytes),
        ]
    }
}

/// A versioned execution policy. Every restriction is optional: an absent
/// field places no restriction *from this policy*. The baseline policy
/// states every dimension explicitly, and effective policies are
/// intersections, so an unset field never widens what another source allows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub version: u32,
    /// Human-readable label such as `production-policy`. It is part of the
    /// canonical form and therefore of the policy identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "PolicyDefaults::is_empty")]
    pub defaults: PolicyDefaults,
    #[serde(default, skip_serializing_if = "PolicyLimits::is_empty")]
    pub limits: PolicyLimits,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_runtimes: Option<Vec<RuntimeKind>>,
    /// Per runtime, version prefixes the executing runtime must report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_runtime_versions: Option<BTreeMap<RuntimeKind, Vec<String>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_distributions: Option<Vec<String>>,
    /// Dependency capsule identities a workload may use. Workloads without
    /// a capsule are unaffected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_dependencies: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_isolation: Option<IsolationProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_networks: Option<Vec<NetworkPolicy>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_os: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_architectures: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_output_classes: Option<Vec<OutputClass>>,
}

impl Policy {
    /// A policy with no restrictions of its own.
    pub fn unrestricted() -> Self {
        Self {
            version: POLICY_VERSION,
            name: None,
            defaults: PolicyDefaults::default(),
            limits: PolicyLimits::default(),
            allowed_runtimes: None,
            allowed_runtime_versions: None,
            allowed_distributions: None,
            allowed_dependencies: None,
            minimum_isolation: None,
            allowed_networks: None,
            allowed_os: None,
            allowed_architectures: None,
            allowed_output_classes: None,
        }
    }

    /// The documented policy used when nothing else is configured. It
    /// admits everything Compute can execute and states the safe defaults
    /// explicitly: no network, process isolation.
    pub fn baseline() -> Self {
        Self {
            name: Some("compute-baseline".into()),
            defaults: PolicyDefaults {
                network: Some(NetworkPolicy::None),
                isolation: Some(IsolationProfile::Process),
            },
            allowed_runtimes: Some(RuntimeKind::ALL.to_vec()),
            minimum_isolation: Some(IsolationProfile::Process),
            allowed_networks: Some(vec![
                NetworkPolicy::None,
                NetworkPolicy::Localhost,
                NetworkPolicy::Network,
            ]),
            allowed_output_classes: Some(vec![OutputClass::Stdio, OutputClass::Files]),
            ..Self::unrestricted()
        }
        .canonical()
    }

    /// Parse and validate a JSON policy document.
    pub fn from_json(bytes: &[u8]) -> Result<Self, PolicyError> {
        let value: serde_json::Value = serde_json::from_slice(bytes)
            .map_err(|error| PolicyError::invalid("document", error.to_string()))?;
        match value.get("version") {
            Some(serde_json::Value::Number(number)) if number.as_u64() == Some(1) => {}
            Some(other) => return Err(PolicyError::UnsupportedVersion(other.to_string())),
            None => {
                return Err(PolicyError::invalid(
                    "version",
                    "policy version is required",
                ));
            }
        }
        let policy: Self = serde_json::from_value(value)
            .map_err(|error| PolicyError::invalid("document", error.to_string()))?;
        policy.validate()?;
        Ok(policy.canonical())
    }

    /// Static validation. It rejects anything whose meaning would be
    /// ambiguous, contradictory, or impossible to satisfy.
    pub fn validate(&self) -> Result<(), PolicyError> {
        if self.version != POLICY_VERSION {
            return Err(PolicyError::UnsupportedVersion(self.version.to_string()));
        }
        if let Some(name) = &self.name
            && (name.is_empty()
                || name.len() > 128
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_.@".contains(&byte)))
        {
            return Err(PolicyError::invalid(
                "name",
                "use 1-128 ASCII letters, digits, '-', '_', '.', or '@'",
            ));
        }
        for (field, value) in self.limits.values() {
            if value == Some(0) {
                return Err(PolicyError::invalid(
                    format!("limits.{field}"),
                    "a zero limit admits nothing; omit the limit or use a positive value",
                ));
            }
        }
        if let (Some(input), Some(artifact)) =
            (self.limits.max_input_bytes, self.limits.max_artifact_bytes)
            && input > artifact
        {
            return Err(PolicyError::invalid(
                "limits",
                "max_input_bytes cannot exceed max_artifact_bytes: inputs travel inside the artifact",
            ));
        }
        unique_non_empty(&self.allowed_runtimes, "allowed_runtimes")?;
        unique_non_empty(&self.allowed_networks, "allowed_networks")?;
        unique_non_empty(&self.allowed_output_classes, "allowed_output_classes")?;
        for (field, values) in [
            ("allowed_distributions", &self.allowed_distributions),
            ("allowed_dependencies", &self.allowed_dependencies),
        ] {
            unique_non_empty(values, field)?;
            for value in values.iter().flatten() {
                compute_core::validate_sha256_identity(value)
                    .map_err(|error| PolicyError::invalid(field, error.to_string()))?;
            }
        }
        for (field, values) in [
            ("allowed_os", &self.allowed_os),
            ("allowed_architectures", &self.allowed_architectures),
        ] {
            unique_non_empty(values, field)?;
            if values
                .iter()
                .flatten()
                .any(|value| !is_platform_part(value))
            {
                return Err(PolicyError::invalid(
                    field,
                    "use lowercase names such as linux or x86_64",
                ));
            }
        }
        if let Some(versions) = &self.allowed_runtime_versions {
            for (runtime, prefixes) in versions {
                let field = format!("allowed_runtime_versions.{runtime}");
                unique_non_empty(&Some(prefixes.clone()), &field)?;
                if prefixes
                    .iter()
                    .any(|value| value.trim().is_empty() || value.trim() != value)
                {
                    return Err(PolicyError::invalid(
                        field,
                        "versions must be non-empty and trimmed",
                    ));
                }
                if self
                    .allowed_runtimes
                    .as_ref()
                    .is_some_and(|allowed| !allowed.contains(runtime))
                {
                    return Err(PolicyError::invalid(
                        field,
                        format!(
                            "{runtime} versions are listed, but {runtime} is not an allowed runtime"
                        ),
                    ));
                }
            }
        }
        if let (Some(network), Some(allowed)) = (&self.defaults.network, &self.allowed_networks)
            && !allowed.contains(network)
        {
            return Err(PolicyError::invalid(
                "defaults.network",
                format!("default network {network} is not an allowed network"),
            ));
        }
        if let (Some(isolation), Some(minimum)) = (self.defaults.isolation, self.minimum_isolation)
            && isolation < minimum
        {
            return Err(PolicyError::invalid(
                "defaults.isolation",
                format!("default isolation {isolation} is weaker than the minimum {minimum}"),
            ));
        }
        Ok(())
    }

    /// Canonical form: every list sorted. Two policies with the same meaning
    /// serialize identically.
    pub fn canonical(mut self) -> Self {
        fn sort<T: Ord>(values: &mut Option<Vec<T>>) {
            if let Some(values) = values {
                values.sort();
                values.dedup();
            }
        }
        sort(&mut self.allowed_runtimes);
        sort(&mut self.allowed_distributions);
        sort(&mut self.allowed_dependencies);
        sort(&mut self.allowed_networks);
        sort(&mut self.allowed_os);
        sort(&mut self.allowed_architectures);
        sort(&mut self.allowed_output_classes);
        if let Some(versions) = &mut self.allowed_runtime_versions {
            for prefixes in versions.values_mut() {
                prefixes.sort();
                prefixes.dedup();
            }
        }
        self
    }

    pub fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.clone().canonical()).expect("policies are serializable")
    }

    /// `sha256:` identity of the canonical serialization.
    pub fn policy_id(&self) -> String {
        canonical_identity(&self.clone().canonical())
    }

    /// A readable label: `name@version`, or the policy ID when unnamed.
    pub fn label(&self) -> String {
        match &self.name {
            Some(name) => format!("{name}@{}", self.version),
            None => self.policy_id(),
        }
    }

    /// The intersection of two policies: a contract satisfies the result
    /// exactly when it satisfies both. The result is never less
    /// restrictive than either input.
    pub fn intersect(&self, other: &Self) -> Self {
        fn lists<T: Ord + Clone>(left: &Option<Vec<T>>, right: &Option<Vec<T>>) -> Option<Vec<T>> {
            match (left, right) {
                (Some(left), Some(right)) => {
                    let right = right.iter().collect::<BTreeSet<_>>();
                    Some(
                        left.iter()
                            .filter(|value| right.contains(value))
                            .cloned()
                            .collect(),
                    )
                }
                (Some(values), None) | (None, Some(values)) => Some(values.clone()),
                (None, None) => None,
            }
        }
        fn minimum(left: Option<u64>, right: Option<u64>) -> Option<u64> {
            match (left, right) {
                (Some(left), Some(right)) => Some(left.min(right)),
                (value, None) | (None, value) => value,
            }
        }
        let versions = match (
            &self.allowed_runtime_versions,
            &other.allowed_runtime_versions,
        ) {
            (Some(left), Some(right)) => {
                let mut merged = left.clone();
                for (runtime, prefixes) in right {
                    merged
                        .entry(*runtime)
                        .and_modify(|existing| {
                            *existing = lists(&Some(existing.clone()), &Some(prefixes.clone()))
                                .unwrap_or_default();
                        })
                        .or_insert_with(|| prefixes.clone());
                }
                Some(merged)
            }
            (Some(value), None) | (None, Some(value)) => Some(value.clone()),
            (None, None) => None,
        };
        Self {
            version: POLICY_VERSION,
            name: None,
            defaults: PolicyDefaults {
                // The more restrictive default wins.
                network: match (&self.defaults.network, &other.defaults.network) {
                    (Some(left), Some(right)) => {
                        Some(if network_rank(left) <= network_rank(right) {
                            left.clone()
                        } else {
                            right.clone()
                        })
                    }
                    (value, None) | (None, value) => value.clone(),
                },
                isolation: self.defaults.isolation.max(other.defaults.isolation),
            },
            limits: PolicyLimits {
                max_timeout_ms: minimum(self.limits.max_timeout_ms, other.limits.max_timeout_ms),
                max_memory_bytes: minimum(
                    self.limits.max_memory_bytes,
                    other.limits.max_memory_bytes,
                ),
                max_input_bytes: minimum(self.limits.max_input_bytes, other.limits.max_input_bytes),
                max_output_bytes: minimum(
                    self.limits.max_output_bytes,
                    other.limits.max_output_bytes,
                ),
                max_artifact_bytes: minimum(
                    self.limits.max_artifact_bytes,
                    other.limits.max_artifact_bytes,
                ),
            },
            allowed_runtimes: lists(&self.allowed_runtimes, &other.allowed_runtimes),
            allowed_runtime_versions: versions,
            allowed_distributions: lists(&self.allowed_distributions, &other.allowed_distributions),
            allowed_dependencies: lists(&self.allowed_dependencies, &other.allowed_dependencies),
            minimum_isolation: self.minimum_isolation.max(other.minimum_isolation),
            allowed_networks: lists(&self.allowed_networks, &other.allowed_networks),
            allowed_os: lists(&self.allowed_os, &other.allowed_os),
            allowed_architectures: lists(&self.allowed_architectures, &other.allowed_architectures),
            allowed_output_classes: lists(
                &self.allowed_output_classes,
                &other.allowed_output_classes,
            ),
        }
        .canonical()
    }
}

/// Network policies ordered from most to least restrictive.
pub(crate) fn network_rank(policy: &NetworkPolicy) -> u8 {
    match policy {
        NetworkPolicy::None => 0,
        NetworkPolicy::Localhost => 1,
        NetworkPolicy::Network => 2,
    }
}

fn unique_non_empty<T: Ord>(values: &Option<Vec<T>>, field: &str) -> Result<(), PolicyError> {
    let Some(values) = values else {
        return Ok(());
    };
    if values.is_empty() {
        return Err(PolicyError::invalid(
            field,
            "an empty allow-list admits nothing; omit the field to leave it unrestricted",
        ));
    }
    if values.iter().collect::<BTreeSet<_>>().len() != values.len() {
        return Err(PolicyError::invalid(field, "contains duplicate entries"));
    }
    Ok(())
}

fn is_platform_part(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// Where a policy came from. Sources are intersected; none can widen
/// another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicySourceKind {
    Baseline,
    Local,
    Server,
    Provider,
    Explicit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicySource {
    pub kind: PolicySourceKind,
    pub policy_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// The policy actually evaluated: the intersection of its sources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectivePolicy {
    pub policy_id: String,
    pub policy: Policy,
    pub sources: Vec<PolicySource>,
}

impl EffectivePolicy {
    /// Compose the baseline with every supplied source, in source order.
    /// Composition is intersection, so order does not change the result.
    pub fn compose(sources: &[(PolicySourceKind, Policy)]) -> Self {
        let baseline = Policy::baseline();
        let mut policy = baseline.clone();
        let mut recorded = vec![PolicySource {
            kind: PolicySourceKind::Baseline,
            policy_id: baseline.policy_id(),
            label: Some(baseline.label()),
        }];
        let mut sorted = sources.to_vec();
        sorted.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.policy_id().cmp(&right.1.policy_id()))
        });
        for (kind, source) in &sorted {
            policy = policy.intersect(source);
            recorded.push(PolicySource {
                kind: *kind,
                policy_id: source.policy_id(),
                label: source.name.as_ref().map(|_| source.label()),
            });
        }
        // A single named source keeps its name, so explanations can say
        // `production-policy@1` rather than only a digest.
        if sorted.len() == 1 {
            policy.name = sorted[0].1.name.clone();
        } else if sorted.is_empty() {
            policy.name = baseline.name.clone();
        }
        Self {
            policy_id: policy.policy_id(),
            policy,
            sources: recorded,
        }
    }

    /// Intersect a further source, such as a provider's advertised policy.
    pub fn with(&self, kind: PolicySourceKind, source: &Policy) -> Self {
        let mut policy = self.policy.intersect(source);
        policy.name = None;
        let mut sources = self.sources.clone();
        sources.push(PolicySource {
            kind,
            policy_id: source.policy_id(),
            label: source.name.as_ref().map(|_| source.label()),
        });
        Self {
            policy_id: policy.policy_id(),
            policy,
            sources,
        }
    }
}
