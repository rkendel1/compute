//! Application bundle evidence: the application that *runs on* a configured
//! Computer, as opposed to the stack that configures it.
//!
//! An application bundle is a manifest plus the WASM module the manifest
//! names (AppBoundry's `.app` directory: `manifest` with protocol
//! `AppPort/application-bundle/1`, and `application.wasm`). It is not a
//! package and no stack contains it. A project declares the bundle it runs;
//! Compute resolves it from the files supplied to the workload, requires the
//! Computer to offer the bundle's runtime, and records what the target's own
//! platform package (`@appport/appboundry`) said about it.
//!
//! Compute does not parse or certify the manifest itself: AppBoundry's own
//! API does, inside the environment the stack materialized. What Compute
//! records is what that API returned.

use serde::{Deserialize, Serialize};

use crate::stack::{gate, short};
use crate::{
    ComponentState, ComputeError, ExecutionReceipt, Gate, Phase, ProbeEvidence, ProbeKind, Result,
    RuntimeKind, VerificationStatus, validate_sha256_identity,
};

/// What a project says about the application bundle it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppBundleDeclaration {
    /// The application id the manifest declares.
    pub application: String,
    /// The version the manifest declares, when the project pins one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The runtime that executes the module (`wasm`).
    pub runtime: RuntimeKind,
    /// The module ABI the manifest declares (`wasm/1`).
    pub abi: String,
    /// `sha256:` identity of the module, when the project pins one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
}

/// The supplied files that are the bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedAppBundle {
    /// Portable paths, exactly as declared workload inputs.
    pub manifest_path: String,
    pub manifest_sha256: String,
    pub module_path: String,
    pub module_sha256: String,
    /// The version the manifest declares.
    pub version: String,
    /// The manifest's own package content address.
    pub package_identity: String,
}

/// What `@appport/appboundry` (running on the target, inside the
/// materialized stack) reported about the bundle. The report is AppBoundry's,
/// not Compute's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppInspection {
    /// The probe execution's receipt hash.
    pub receipt: String,
    /// The version of the platform package that inspected the bundle.
    pub package_version: String,
    /// `CERTIFIED` or `FAILED`.
    pub certification: String,
    pub failed_checks: Vec<String>,
    pub application_id: String,
    pub artifact_hash: String,
    pub package_identity: String,
    /// `RUNNABLE`, `PROVIDERS_UNAVAILABLE`, or `ARTIFACT_FAILED`, given that
    /// Compute establishes no application providers.
    pub readiness: String,
    /// The providers the application requires that were not available.
    pub missing_providers: Vec<String>,
}

/// Everything about the application bundle known before execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppBundleBinding {
    pub declared: AppBundleDeclaration,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<ResolvedAppBundle>,
    /// The runtime probe: a minimal module run on the target's runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_probe: Option<ProbeEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inspection: Option<AppInspection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptAppBundle {
    pub binding: AppBundleBinding,
    pub gates: Vec<Gate>,
}

impl ReceiptAppBundle {
    pub fn attest(binding: &AppBundleBinding, receipt: &ExecutionReceipt) -> Self {
        Self {
            binding: binding.clone(),
            gates: gates(binding, receipt),
        }
    }

    /// Where the application stands.
    pub fn state(&self) -> ComponentState {
        use VerificationStatus::*;
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

    pub fn verify(&self, receipt: &ExecutionReceipt) -> Result<()> {
        let invalid = |message: &str| ComputeError::InvalidReceipt(message.into());
        let binding = &self.binding;
        if binding.declared.application.is_empty() || binding.declared.abi.is_empty() {
            return Err(invalid("application bundle identity is incomplete"));
        }
        if let Some(pin) = &binding.declared.artifact {
            validate_sha256_identity(pin)?;
        }
        if let Some(resolved) = &binding.resolved {
            validate_sha256_identity(&resolved.manifest_sha256)?;
            validate_sha256_identity(&resolved.module_sha256)?;
            validate_sha256_identity(&resolved.package_identity)?;
        }
        if let Some(probe) = &binding.runtime_probe {
            validate_sha256_identity(&probe.receipt)?;
        }
        if let Some(inspection) = &binding.inspection {
            validate_sha256_identity(&inspection.receipt)?;
        }
        if *self != Self::attest(binding, receipt) {
            return Err(invalid(
                "application bundle evidence does not follow from the receipt's evidence",
            ));
        }
        if self.state() == ComponentState::Failed {
            return Err(invalid(
                "an execution receipt cannot record a failed application bundle",
            ));
        }
        Ok(())
    }
}

fn gates(binding: &AppBundleBinding, receipt: &ExecutionReceipt) -> Vec<Gate> {
    use VerificationStatus::*;
    let declared = &binding.declared;
    let resolved = binding.resolved.as_ref();

    let manifest = match resolved {
        Some(found) => gate(
            "manifest resolved",
            Phase::Resolve,
            Satisfied,
            format!(
                "{} declares {} {}, module format {}, package identity {}",
                found.manifest_path,
                declared.application,
                found.version,
                declared.abi,
                short(&found.package_identity)
            ),
        ),
        None => gate(
            "manifest resolved",
            Phase::Resolve,
            Unsatisfied,
            format!(
                "no supplied manifest declares {} ({}){}",
                declared.application,
                declared.abi,
                declared
                    .version
                    .as_ref()
                    .map(|version| format!(" {version}"))
                    .unwrap_or_default()
            ),
        ),
    };
    let module = match resolved {
        Some(found) => gate(
            "module resolved",
            Phase::Resolve,
            Satisfied,
            format!(
                "{} ({}) is the module the manifest names",
                found.module_path, found.module_sha256
            ),
        ),
        None => gate(
            "module resolved",
            Phase::Resolve,
            Unsatisfied,
            "no supplied file is the module a matching manifest names".into(),
        ),
    };

    let materialized = |check: &str, item: Option<(&String, &String)>| match item {
        None => gate(
            check,
            Phase::Materialize,
            NotEvaluated,
            "not resolved".into(),
        ),
        Some((path, sha))
            if receipt.inputs.iter().any(|input| {
                input.path.to_string_lossy() == path.as_str() && input.sha256 == *sha
            }) =>
        {
            gate(
                check,
                Phase::Materialize,
                Satisfied,
                format!("{path} was materialized with identity {sha}"),
            )
        }
        Some((path, _)) => gate(
            check,
            Phase::Materialize,
            Unsatisfied,
            format!("{path} was not materialized as resolved"),
        ),
    };
    let manifest_materialized = materialized(
        "manifest materialized",
        resolved.map(|f| (&f.manifest_path, &f.manifest_sha256)),
    );
    let module_materialized = materialized(
        "module materialized",
        resolved.map(|f| (&f.module_path, &f.module_sha256)),
    );

    let certified = match (&binding.inspection, resolved) {
        (None, _) => gate(
            "certified by the AppBoundry platform package",
            Phase::Verify,
            NotEvaluated,
            "not inspected: no @appport/appboundry in the materialized environment".into(),
        ),
        (Some(_), None) => gate(
            "certified by the AppBoundry platform package",
            Phase::Verify,
            NotEvaluated,
            "not resolved".into(),
        ),
        (Some(inspection), Some(found)) => {
            let matches = inspection.artifact_hash
                == found.module_sha256.trim_start_matches("sha256:")
                && inspection.package_identity == found.package_identity
                && inspection.application_id == declared.application;
            if inspection.certification == "CERTIFIED" && matches {
                gate(
                    "certified by the AppBoundry platform package",
                    Phase::Verify,
                    Satisfied,
                    format!(
                        "@appport/appboundry {} certified {} (probe {})",
                        inspection.package_version,
                        inspection.application_id,
                        short(&inspection.receipt)
                    ),
                )
            } else if inspection.certification == "CERTIFIED" {
                gate(
                    "certified by the AppBoundry platform package",
                    Phase::Verify,
                    Unsatisfied,
                    "the certified artifact is not the one Compute resolved".into(),
                )
            } else {
                gate(
                    "certified by the AppBoundry platform package",
                    Phase::Verify,
                    Unsatisfied,
                    format!(
                        "certification failed: {}",
                        inspection.failed_checks.join(", ")
                    ),
                )
            }
        }
    };

    let runtime = match &binding.runtime_probe {
        None => gate(
            "runtime capability verified",
            Phase::Verify,
            NotEvaluated,
            format!("no probe ran a {} module on the target", declared.runtime),
        ),
        Some(probe)
            if probe.kind == ProbeKind::WasmRuntime
                && probe.runtime == declared.runtime
                && probe.observed.contains_key("wasm-runtime") =>
        {
            gate(
                "runtime capability verified",
                Phase::Verify,
                Satisfied,
                format!(
                    "probe {} ran a module on {} {}",
                    short(&probe.receipt),
                    probe.runtime,
                    probe.observed["wasm-runtime"]
                        .as_deref()
                        .unwrap_or("unknown")
                ),
            )
        }
        Some(probe) => gate(
            "runtime capability verified",
            Phase::Verify,
            Unsatisfied,
            format!("probe ran on {}, not {}", probe.runtime, declared.runtime),
        ),
    };

    // Whether the providers the application requires exist is not something
    // Compute establishes. AppBoundry's readiness API is asked with no
    // providers and answers accordingly; that is reported, not papered over.
    let providers = match &binding.inspection {
        Some(inspection) if inspection.readiness == "RUNNABLE" => gate(
            "required providers available",
            Phase::Verify,
            Satisfied,
            "AppBoundry reports the application runnable".into(),
        ),
        Some(inspection) => gate(
            "required providers available",
            Phase::Verify,
            NotEvaluated,
            format!(
                "unavailable: AppBoundry reports {}; Compute established no providers for: {}",
                inspection.readiness,
                inspection.missing_providers.join(", ")
            ),
        ),
        None => gate(
            "required providers available",
            Phase::Verify,
            NotEvaluated,
            "not inspected".into(),
        ),
    };
    let launch = gate(
        "application launch verified",
        Phase::Verify,
        NotEvaluated,
        format!(
            "unavailable: Compute's {} runtime hosts WASI modules; this application declares the {} host ABI, which the AppBoundry host provides and Compute does not",
            declared.runtime, declared.abi
        ),
    );
    vec![
        manifest,
        module,
        manifest_materialized,
        module_materialized,
        certified,
        runtime,
        providers,
        launch,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::tests::receipt;
    use crate::{InputReceipt, sha256_identity};
    use std::collections::BTreeMap;

    const MANIFEST: &str = "AppBoundry.app/manifest";
    const MODULE: &str = "AppBoundry.app/application.wasm";

    fn binding() -> AppBundleBinding {
        AppBundleBinding {
            declared: AppBundleDeclaration {
                application: "dev.appboundry.portal".into(),
                version: Some("1.0.0".into()),
                runtime: RuntimeKind::Wasm,
                abi: "wasm/1".into(),
                artifact: None,
            },
            resolved: Some(ResolvedAppBundle {
                manifest_path: MANIFEST.into(),
                manifest_sha256: sha256_identity(b"manifest"),
                module_path: MODULE.into(),
                module_sha256: sha256_identity(b"module"),
                version: "1.0.0".into(),
                package_identity: sha256_identity(b"package"),
            }),
            runtime_probe: Some(ProbeEvidence {
                kind: ProbeKind::WasmRuntime,
                receipt: sha256_identity(b"wasm probe"),
                program: sha256_identity(b"wasm"),
                runtime: RuntimeKind::Wasm,
                observed: BTreeMap::from([("wasm-runtime".to_owned(), Some("wasi".to_owned()))]),
            }),
            inspection: Some(AppInspection {
                receipt: sha256_identity(b"env probe"),
                package_version: "1.1.1".into(),
                certification: "CERTIFIED".into(),
                failed_checks: vec![],
                application_id: "dev.appboundry.portal".into(),
                artifact_hash: hex(b"module"),
                package_identity: sha256_identity(b"package"),
                readiness: "PROVIDERS_UNAVAILABLE".into(),
                missing_providers: vec!["feltdb.documents@1".into()],
            }),
        }
    }

    fn hex(bytes: &[u8]) -> String {
        sha256_identity(bytes)
            .trim_start_matches("sha256:")
            .to_owned()
    }

    fn executed(binding: &AppBundleBinding, materialize: bool) -> ExecutionReceipt {
        let mut receipt = receipt();
        if materialize {
            receipt.inputs = vec![
                InputReceipt {
                    path: MANIFEST.into(),
                    size: 8,
                    sha256: sha256_identity(b"manifest"),
                    required: true,
                },
                InputReceipt {
                    path: MODULE.into(),
                    size: 6,
                    sha256: sha256_identity(b"module"),
                    required: true,
                },
            ];
        }
        receipt.app_bundle = Some(ReceiptAppBundle::attest(binding, &receipt));
        receipt.seal().unwrap();
        receipt
    }

    fn status(receipt: &ExecutionReceipt, check: &str) -> VerificationStatus {
        receipt
            .app_bundle
            .as_ref()
            .unwrap()
            .gates
            .iter()
            .find(|gate| gate.check == check)
            .unwrap_or_else(|| panic!("{check}"))
            .verification
            .status
    }

    #[test]
    fn each_layer_of_the_application_has_its_own_evidence() {
        let receipt = executed(&binding(), true);
        receipt.verify().unwrap();
        use VerificationStatus::*;
        for (check, expected) in [
            ("manifest resolved", Satisfied),
            ("module resolved", Satisfied),
            ("manifest materialized", Satisfied),
            ("module materialized", Satisfied),
            ("certified by the AppBoundry platform package", Satisfied),
            ("runtime capability verified", Satisfied),
            ("required providers available", NotEvaluated),
            ("application launch verified", NotEvaluated),
        ] {
            assert_eq!(status(&receipt, check), expected, "{check}");
        }
        // Everything Compute can prove is proven; what it cannot is stated,
        // so the application is not "verified".
        assert_eq!(
            receipt.app_bundle.as_ref().unwrap().state(),
            ComponentState::VerificationUnavailable
        );
        let launch = receipt.app_bundle.as_ref().unwrap().gates.last().unwrap();
        assert!(launch.verification.evidence.starts_with("unavailable"));
        let providers = &receipt.app_bundle.as_ref().unwrap().gates[6];
        assert!(
            providers
                .verification
                .evidence
                .contains("feltdb.documents@1")
        );
        let parsed: ExecutionReceipt =
            serde_json::from_slice(&receipt.encoded_bytes().unwrap()).unwrap();
        parsed.verify().unwrap();
    }

    #[test]
    fn presence_of_a_manifest_does_not_prove_the_application_works() {
        // Resolved and even materialized, but never inspected and never run.
        let mut bound = binding();
        bound.inspection = None;
        bound.runtime_probe = None;
        let receipt = executed(&bound, true);
        receipt.verify().unwrap();
        assert_eq!(
            status(&receipt, "certified by the AppBoundry platform package"),
            VerificationStatus::NotEvaluated
        );
        assert_eq!(
            status(&receipt, "runtime capability verified"),
            VerificationStatus::NotEvaluated
        );
        assert_ne!(
            receipt.app_bundle.unwrap().state(),
            ComponentState::Verified
        );
    }

    #[test]
    fn unmaterialized_uncertified_or_unrunnable_bundles_are_not_recorded() {
        // Files not in the execution's inputs.
        let receipt = executed(&binding(), false);
        assert_eq!(
            status(&receipt, "module materialized"),
            VerificationStatus::Unsatisfied
        );
        assert!(receipt.verify().is_err());
        // AppBoundry's certification failed.
        let mut failed = binding();
        let inspection = failed.inspection.as_mut().unwrap();
        inspection.certification = "FAILED".into();
        inspection.failed_checks = vec!["immutable identity".into()];
        let receipt = executed(&failed, true);
        assert!(
            receipt.app_bundle.as_ref().unwrap().gates[4]
                .verification
                .evidence
                .contains("immutable identity")
        );
        assert!(receipt.verify().is_err());
        // AppBoundry certified some other artifact than the one resolved.
        let mut other = binding();
        other.inspection.as_mut().unwrap().artifact_hash = hex(b"another module");
        assert!(executed(&other, true).verify().is_err());
        // The runtime probe ran on the wrong runtime.
        let mut wrong = binding();
        wrong.runtime_probe.as_mut().unwrap().runtime = RuntimeKind::Node;
        assert!(executed(&wrong, true).verify().is_err());
        // Nothing resolved.
        let mut unresolved = binding();
        unresolved.resolved = None;
        assert!(executed(&unresolved, true).verify().is_err());
    }

    #[test]
    fn readiness_is_satisfied_only_when_appboundry_says_runnable() {
        let mut ready = binding();
        ready.inspection.as_mut().unwrap().readiness = "RUNNABLE".into();
        ready.inspection.as_mut().unwrap().missing_providers.clear();
        let receipt = executed(&ready, true);
        assert_eq!(
            status(&receipt, "required providers available"),
            VerificationStatus::Satisfied
        );
        // Launch stays unavailable: a runnable verdict is not a launch.
        assert_eq!(
            status(&receipt, "application launch verified"),
            VerificationStatus::NotEvaluated
        );
    }

    #[test]
    fn application_evidence_cannot_be_forged() {
        let mut receipt = executed(&binding(), true);
        receipt.app_bundle.as_mut().unwrap().gates[7]
            .verification
            .status = VerificationStatus::Satisfied;
        receipt.seal().unwrap();
        assert!(
            receipt
                .verify()
                .unwrap_err()
                .to_string()
                .contains("does not follow")
        );
    }
}
