//! Stack evidence: a stack is a *declared, versioned* desired environment
//! (see `compute-project`) made of heterogeneous software artifacts; this
//! module holds what an execution's receipt says about it — which stack, and
//! for each component what was declared, resolved, materialized, and
//! verified, according to the component's own artifact type.
//!
//! Like project evidence, the states are never stored as claims: they are
//! recomputed from the receipt's own observations when it is verified.
//!
//! A stack is the reusable configured software environment of a Computer.
//! Its components are **package artifacts**: published libraries consumed
//! through a dependency capsule (`@feltdb/core`, `@appport/appboundry`, ...).
//! Applications that *run on* that environment are a different layer and a
//! different artifact type (`app_bundle`); a stack never contains them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    ComputeError, ExecutionReceipt, PlatformIdentity, Result, RuntimeKind, Verification,
    VerificationStatus, validate_sha256_identity,
};

pub const STACK_VERSION: &str = "compute.stack@1";

/// What names a stack in evidence. The fingerprint is a hash of the
/// declarative contents alone: no timestamps, paths, or machine facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StackIdentity {
    pub name: String,
    pub version: String,
    pub fingerprint: String,
}

/// The artifact a component asks for, by identity, never by location. Other
/// artifact types can be added here; applications are not one of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ComponentSource {
    /// A published package at a version constraint.
    Package {
        /// `npm`.
        ecosystem: String,
        package: String,
        constraint: String,
    },
}

/// One component as the stack declares it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentDeclaration {
    pub name: String,
    pub source: ComponentSource,
    /// Names of the credentials the component needs. Never values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<String>,
    /// `<os>-<architecture>` platforms the component is available on; empty
    /// means every platform.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<String>,
    /// Why Compute cannot realize this component at all, when it cannot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsupported: Option<String>,
}

/// What resolution found for a component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResolvedComponent {
    /// The package in the dependency capsule that satisfies the constraint.
    Package {
        version: String,
        /// The inventory digest of the artifact.
        artifact: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentBinding {
    pub declared: ComponentDeclaration,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<ResolvedComponent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeKind {
    /// Reports the installed version of each named package (and, when an
    /// application bundle is supplied, what its platform package says of it).
    PackageVersions,
    /// Runs a minimal module on the target's WASM runtime.
    WasmRuntime,
}

/// What a verification probe observed on the target. A probe is an ordinary
/// execution with a receipt of its own, whose identity is recorded so it can
/// be checked independently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeEvidence {
    pub kind: ProbeKind,
    /// The probe execution's receipt hash.
    pub receipt: String,
    /// The probe program's identity.
    pub program: String,
    /// The runtime the probe executed on.
    pub runtime: RuntimeKind,
    /// `PackageVersions`: component → the version found (`None` = absent).
    /// `WasmRuntime`: `wasm-runtime` → the runtime version that ran it.
    pub observed: BTreeMap<String, Option<String>>,
}

/// Everything about a stack known before the execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StackBinding {
    pub identity: StackIdentity,
    pub components: Vec<ComponentBinding>,
    /// The capsule the package components were resolved from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capsule_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub probes: Vec<ProbeEvidence>,
}

impl StackBinding {
    /// Components this stack cannot realize on a target of `platform`, and
    /// why: those declared unsupported, and those not available on it. The
    /// same stack is realizable on some Computers and not on others.
    pub fn unsupported_on(&self, platform: &PlatformIdentity) -> Vec<(String, String)> {
        self.components
            .iter()
            .filter_map(|component| {
                let declared = &component.declared;
                let reason = declared.unsupported.clone().or_else(|| {
                    (!declared.platforms.is_empty()
                        && !declared.platforms.contains(&platform.label()))
                    .then(|| {
                        format!(
                            "available on {}, not {}",
                            declared.platforms.join(", "),
                            platform.label()
                        )
                    })
                });
                reason.map(|reason| (declared.name.clone(), reason))
            })
            .collect()
    }

    fn probe(&self, kind: ProbeKind) -> Option<&ProbeEvidence> {
        self.probes.iter().find(|probe| probe.kind == kind)
    }
}

/// When in realization a check happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// The declaration was matched to an exact artifact.
    Resolve,
    /// The artifact is present, byte for byte, in the executed environment.
    Materialize,
    /// A capability or behavior was observed on the target.
    Verify,
}

/// One check, and what supports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gate {
    pub check: String,
    pub phase: Phase,
    pub verification: Verification,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentEvidence {
    pub name: String,
    pub gates: Vec<Gate>,
}

/// Where a component stands, for humans and tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentState {
    Declared,
    Resolved,
    Materialized,
    Verified,
    /// Resolved and materialized; some check cannot be made on this target.
    VerificationUnavailable,
    Unsupported,
    Failed,
}

impl ComponentState {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Declared => "declared",
            Self::Resolved => "resolved",
            Self::Materialized => "materialized",
            Self::Verified => "verified",
            Self::VerificationUnavailable => "verification unavailable",
            Self::Unsupported => "unsupported",
            Self::Failed => "failed",
        }
    }
}

impl ComponentEvidence {
    pub fn state(&self, declaration: &ComponentDeclaration) -> ComponentState {
        use VerificationStatus::*;
        if declaration.unsupported.is_some() {
            return ComponentState::Unsupported;
        }
        if self
            .gates
            .iter()
            .any(|gate| gate.verification.status == Unsatisfied)
        {
            return ComponentState::Failed;
        }
        let done = |phase| {
            self.gates
                .iter()
                .filter(|gate| gate.phase == phase)
                .all(|gate| gate.verification.status == Satisfied)
        };
        if !done(Phase::Resolve) {
            ComponentState::Declared
        } else if !done(Phase::Materialize) {
            ComponentState::Resolved
        } else if !done(Phase::Verify) {
            ComponentState::VerificationUnavailable
        } else {
            ComponentState::Verified
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptStack {
    pub binding: StackBinding,
    pub components: Vec<ComponentEvidence>,
}

impl ReceiptStack {
    /// Derive each component's evidence from the receipt, by artifact type.
    pub fn attest(binding: &StackBinding, receipt: &ExecutionReceipt) -> Self {
        Self {
            binding: binding.clone(),
            components: binding
                .components
                .iter()
                .map(|component| ComponentEvidence {
                    name: component.declared.name.clone(),
                    gates: match &component.declared.source {
                        ComponentSource::Package { .. } => {
                            package_gates(binding, component, receipt)
                        }
                    },
                })
                .collect(),
        }
    }

    pub fn verify(&self, receipt: &ExecutionReceipt) -> Result<()> {
        let invalid = |message: &str| ComputeError::InvalidReceipt(message.into());
        let identity = &self.binding.identity;
        validate_sha256_identity(&identity.fingerprint)?;
        if identity.name.is_empty() || identity.version.is_empty() {
            return Err(invalid("stack identity is incomplete"));
        }
        if let Some(capsule) = &self.binding.capsule_id {
            validate_sha256_identity(capsule)?;
        }
        for probe in &self.binding.probes {
            validate_sha256_identity(&probe.receipt)?;
            validate_sha256_identity(&probe.program)?;
        }
        if self.binding.components.is_empty() {
            return Err(invalid("a stack has no components"));
        }
        let recomputed = Self::attest(&self.binding, receipt);
        if recomputed != *self {
            return Err(invalid(
                "stack evidence does not follow from the receipt's evidence",
            ));
        }
        if let Some(platform) = receipt
            .placement
            .as_ref()
            .and_then(|placement| placement.execution_platform.as_ref())
            && !self.binding.unsupported_on(platform).is_empty()
        {
            return Err(invalid(
                "an execution receipt cannot record a stack component the target excludes",
            ));
        }
        for (component, evidence) in self.binding.components.iter().zip(&self.components) {
            match evidence.state(&component.declared) {
                ComponentState::Unsupported => {
                    return Err(invalid(
                        "an execution receipt cannot record an unsupported stack component",
                    ));
                }
                ComponentState::Failed => {
                    return Err(invalid(
                        "an execution receipt cannot record a failed stack component",
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

pub(crate) fn gate(
    check: &str,
    phase: Phase,
    status: VerificationStatus,
    evidence: String,
) -> Gate {
    Gate {
        check: check.into(),
        phase,
        verification: Verification { status, evidence },
    }
}

pub(crate) fn short(identity: &str) -> &str {
    &identity[..identity.len().min(19)]
}

fn package_gates(
    binding: &StackBinding,
    component: &ComponentBinding,
    receipt: &ExecutionReceipt,
) -> Vec<Gate> {
    use VerificationStatus::*;
    let ComponentSource::Package {
        package,
        constraint,
        ..
    } = &component.declared.source;
    let name = &component.declared.name;
    let found = component
        .resolved
        .as_ref()
        .map(|ResolvedComponent::Package { version, artifact }| (version, artifact));

    let resolved = match found {
        Some((version, artifact)) => gate(
            "package resolved",
            Phase::Resolve,
            Satisfied,
            format!("{package} {version} satisfies {constraint} ({artifact})"),
        ),
        None => gate(
            "package resolved",
            Phase::Resolve,
            Unsatisfied,
            format!("nothing satisfies {constraint}"),
        ),
    };
    let materialized = match (&binding.capsule_id, &receipt.dependencies, found) {
        (Some(expected), Some(actual), Some(_))
            if actual.verified && &actual.capsule_id == expected =>
        {
            gate(
                "package materialized",
                Phase::Materialize,
                Satisfied,
                format!("present in verified capsule {expected}"),
            )
        }
        (_, _, None) => gate(
            "package materialized",
            Phase::Materialize,
            NotEvaluated,
            "not resolved".into(),
        ),
        _ => gate(
            "package materialized",
            Phase::Materialize,
            Unsatisfied,
            "the capsule it resolved from was not verified at execution".into(),
        ),
    };
    let verified = match (binding.probe(ProbeKind::PackageVersions), found) {
        (_, None) => gate(
            "package verified",
            Phase::Verify,
            NotEvaluated,
            "not resolved".into(),
        ),
        (None, _) => gate(
            "package verified",
            Phase::Verify,
            NotEvaluated,
            "no probe ran on the target: presence is declared, not observed".into(),
        ),
        (Some(probe), Some((version, _))) => match probe.observed.get(name) {
            Some(Some(seen)) if seen == version => gate(
                "package verified",
                Phase::Verify,
                Satisfied,
                format!("probe {} observed {seen}", short(&probe.receipt)),
            ),
            Some(Some(seen)) => gate(
                "package verified",
                Phase::Verify,
                Unsatisfied,
                format!("probe observed {seen}, resolved {version}"),
            ),
            Some(None) => gate(
                "package verified",
                Phase::Verify,
                Unsatisfied,
                "probe found it absent".into(),
            ),
            None => gate(
                "package verified",
                Phase::Verify,
                NotEvaluated,
                "the probe did not check this component".into(),
            ),
        },
    };
    vec![resolved, materialized, verified]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::tests::receipt;
    use crate::{InputReceipt, ReceiptDependencies, sha256_identity};

    fn capsule() -> String {
        sha256_identity(b"capsule")
    }

    fn binding(resolved: bool) -> StackBinding {
        let component = |name: &str, package: &str, version: &str| ComponentBinding {
            declared: ComponentDeclaration {
                name: name.into(),
                source: ComponentSource::Package {
                    ecosystem: "npm".into(),
                    package: package.into(),
                    constraint: version.into(),
                },
                credentials: vec!["FELT_TOKEN".into()],
                platforms: vec![],
                unsupported: None,
            },
            resolved: resolved.then(|| ResolvedComponent::Package {
                version: version.into(),
                artifact: sha256_identity(b"artifact"),
            }),
        };
        StackBinding {
            identity: StackIdentity {
                name: "demo".into(),
                version: "1.0.0".into(),
                fingerprint: sha256_identity(b"demo"),
            },
            components: vec![
                component("core", "@appport/core", "1.0.3"),
                component("db", "@feltdb/core", "0.11.9"),
            ],
            capsule_id: Some(capsule()),
            probes: vec![],
        }
    }

    fn probe(core: Option<&str>, db: Option<&str>) -> ProbeEvidence {
        ProbeEvidence {
            kind: ProbeKind::PackageVersions,
            receipt: sha256_identity(b"probe receipt"),
            program: sha256_identity(b"probe"),
            runtime: RuntimeKind::Node,
            observed: [
                ("core".to_owned(), core.map(str::to_owned)),
                ("db".to_owned(), db.map(str::to_owned)),
            ]
            .into_iter()
            .collect(),
        }
    }

    fn executed(binding: &StackBinding, verified_capsule: bool) -> ExecutionReceipt {
        let mut receipt = receipt();
        receipt.dependencies = Some(ReceiptDependencies {
            capsule_id: capsule(),
            verified: verified_capsule,
        });
        receipt.stack = Some(ReceiptStack::attest(binding, &receipt));
        receipt.seal().unwrap();
        receipt
    }

    fn states(receipt: &ExecutionReceipt) -> Vec<ComponentState> {
        let stack = receipt.stack.as_ref().unwrap();
        stack
            .binding
            .components
            .iter()
            .zip(&stack.components)
            .map(|(component, evidence)| evidence.state(&component.declared))
            .collect()
    }

    #[test]
    fn a_component_is_verified_only_when_a_probe_observed_it() {
        let mut bound = binding(true);
        // Declared and resolved, executed in a verified capsule, no probe:
        // materialized, and honestly not verified.
        let receipt = executed(&bound, true);
        receipt.verify().unwrap();
        assert_eq!(
            states(&receipt),
            [ComponentState::VerificationUnavailable; 2]
        );

        bound.probes = vec![probe(Some("1.0.3"), Some("0.11.9"))];
        let receipt = executed(&bound, true);
        receipt.verify().unwrap();
        assert_eq!(states(&receipt), [ComponentState::Verified; 2]);
        // The portable encoding preserves it.
        let parsed: ExecutionReceipt =
            serde_json::from_slice(&receipt.encoded_bytes().unwrap()).unwrap();
        parsed.verify().unwrap();
        assert_eq!(parsed.stack, receipt.stack);

        // A probe that did not check one component does not verify it.
        bound.probes[0].observed.remove("db");
        let receipt = executed(&bound, true);
        assert_eq!(
            states(&receipt),
            [
                ComponentState::Verified,
                ComponentState::VerificationUnavailable
            ]
        );
    }

    #[test]
    fn failed_and_unresolved_components_cannot_be_recorded_in_a_receipt() {
        let mut bound = binding(true);
        for (probe_evidence, expected) in [
            (probe(Some("1.0.3"), Some("0.11.8")), "observed 0.11.8"),
            (probe(Some("1.0.3"), None), "absent"),
        ] {
            bound.probes = vec![probe_evidence];
            let receipt = executed(&bound, true);
            assert_eq!(states(&receipt)[1], ComponentState::Failed);
            assert!(
                receipt.stack.as_ref().unwrap().components[1]
                    .gates
                    .iter()
                    .any(|gate| gate.verification.evidence.contains(expected))
            );
            assert!(receipt.verify().unwrap_err().to_string().contains("failed"));
        }
        // Not resolved.
        let receipt = executed(&binding(false), true);
        assert_eq!(states(&receipt), [ComponentState::Failed; 2]);
        assert!(receipt.verify().is_err());
        // The capsule it resolved from was not the one verified at execution.
        let receipt = executed(&binding(true), false);
        assert_eq!(states(&receipt), [ComponentState::Failed; 2]);
        assert!(receipt.verify().is_err());
    }

    #[test]
    fn unsupported_components_and_platforms_are_never_recorded_as_realized() {
        let mut bound = binding(true);
        bound.components[1].declared.unsupported = Some("no native build".into());
        let receipt = executed(&bound, true);
        assert_eq!(states(&receipt)[1], ComponentState::Unsupported);
        assert!(
            receipt
                .verify()
                .unwrap_err()
                .to_string()
                .contains("unsupported")
        );

        let mut bound = binding(true);
        bound.components[0].declared.platforms = vec!["macos-arm64".into()];
        let platform = |os: &str, arch: &str| PlatformIdentity {
            os: os.into(),
            architecture: arch.into(),
            runtime_abi: None,
        };
        assert!(bound.unsupported_on(&platform("macos", "arm64")).is_empty());
        let excluded = bound.unsupported_on(&platform("linux", "x86_64"));
        assert_eq!(excluded[0].0, "core");
        assert!(excluded[0].1.contains("linux-x86_64"));
    }

    #[test]
    fn stack_evidence_cannot_be_forged_or_altered() {
        let mut bound = binding(true);
        bound.probes = vec![probe(Some("1.0.3"), Some("0.11.9"))];
        let mut receipt = executed(&bound, true);
        // Claim a gate held that the evidence does not support, and reseal.
        receipt.stack.as_mut().unwrap().components[0].gates[2]
            .verification
            .evidence = "trust me".into();
        receipt.seal().unwrap();
        assert!(
            receipt
                .verify()
                .unwrap_err()
                .to_string()
                .contains("does not follow")
        );
        // Change the identity after sealing.
        let mut receipt = executed(&bound, true);
        receipt.stack.as_mut().unwrap().binding.identity.version = "9.9.9".into();
        assert!(receipt.verify().unwrap_err().to_string().contains("hash"));
        // A stack with a bad fingerprint is not evidence.
        let mut bad = binding(true);
        bad.identity.fingerprint = "nope".into();
        assert!(executed(&bad, true).verify().is_err());
    }

    #[test]
    fn credentials_appear_by_name_only() {
        let mut bound = binding(true);
        bound.probes = vec![probe(Some("1.0.3"), Some("0.11.9"))];
        let receipt = executed(&bound, true);
        let text = String::from_utf8(receipt.encoded_bytes().unwrap()).unwrap();
        assert!(text.contains("FELT_TOKEN"));
        // There is nowhere for a value to be recorded: the types hold names.
        assert!(!text.contains("secret") && !text.contains("sk-"));
    }

    #[test]
    fn a_receipt_without_a_stack_is_unchanged() {
        let plain = receipt();
        let text = String::from_utf8(plain.canonical_bytes().unwrap()).unwrap();
        assert!(!text.contains("stack") && !text.contains("app_bundle"));
        plain.verify().unwrap();
        let _ = InputReceipt {
            path: "x".into(),
            size: 0,
            sha256: sha256_identity(b""),
            required: true,
        };
    }
}
