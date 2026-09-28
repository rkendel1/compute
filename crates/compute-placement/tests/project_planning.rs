//! PAX project requirements decide which targets are eligible: the project
//! is normalized by the adapter, resolved into a workload, and placed by the
//! ordinary placement engine. A generic runtime is not enough.

mod support;

use std::path::Path;

use compute_core::{
    DependencyCapsule, DependencyEntry, PlatformIdentity, ResourceLimits, RuntimeKind,
    WORKLOAD_SPEC_VERSION, WorkloadBundle, WorkloadSpec,
};
use compute_placement::{
    PlacementOutcome, PlacementRequirements, ReasonCode, RequirementOptions, SubmissionMode, place,
};
use compute_project::{
    CommandSelection, EnvironmentInputs, PaxObservation, materialize, select_command,
};
use serde_json::json;
use support::*;

fn observation(ecosystem: &str, tool: &str, start: &str) -> PaxObservation {
    PaxObservation::from_documents(
        json!({
            "schemaVersion": "1", "command": "info",
            "project": {"root": "/x", "name": "app", "packageJson": true, "workspace": false, "workspaceSource": null},
            "manager": {"name": tool, "version": null},
            "result": {"summary": "s", "runtime": null},
            "ecosystem": ecosystem,
            "components": [{"path": ".", "ecosystem": ecosystem, "tool": tool, "manifests": [], "lockfiles": [], "evidence": [], "workspacePackages": [], "dependencySources": []}],
            "nativeDependencies": [], "container": null
        }),
        json!({"schemaVersion": "1", "command": "deps", "dependencies": {"dependencies": {}, "devDependencies": {}, "optionalDependencies": {}, "peerDependencies": {}, "nativeDependencies": []}}),
        json!({"schemaVersion": "1", "command": "scripts", "scripts": {"start": start}}),
    )
    .unwrap()
}

/// What a project run asks placement for, built the way `compute run` builds
/// it: PAX → requirements → command → workload → bundle → placement.
#[derive(Default)]
struct Project {
    runtime_version: Option<String>,
    architecture: Option<String>,
    platform: Option<PlatformIdentity>,
    capsule: Option<DependencyCapsule>,
}

impl Project {
    fn requirements(&self, observation: &PaxObservation, root: &Path) -> PlacementRequirements {
        let requirements = observation.requirements().unwrap();
        let command = select_command(&requirements, &CommandSelection::Default).unwrap();
        let runtime = command.runtime.unwrap();
        let entrypoint = command.entrypoint.unwrap();
        std::fs::write(root.join(&entrypoint), "// entry\n").unwrap();
        let binding = materialize(
            &requirements,
            &EnvironmentInputs {
                runtime,
                runtime_constraint: self.runtime_version.as_deref(),
                os: self.platform.as_ref().map(|p| p.os.as_str()),
                architecture: self.architecture.as_deref(),
                capsule: self.capsule.as_ref(),
                environment_names: vec![],
                entrypoint: entrypoint.to_string_lossy().into_owned(),
                argument_count: command.args.len(),
                command_name: command.name,
                offered_tools: &[],
            },
        )
        .unwrap();
        // The workload carries exactly what the binding resolved.
        let workload = WorkloadSpec {
            version: WORKLOAD_SPEC_VERSION.into(),
            runtime: binding.resolved.runtime,
            runtime_version: binding.resolved.runtime_constraint.clone(),
            architecture: binding.resolved.architecture.clone(),
            entrypoint,
            args: command.args,
            env: Default::default(),
            inputs: vec![],
            outputs: vec![],
            resources: ResourceLimits::default(),
            network: compute_core::NetworkPolicy::Network,
            isolation: compute_core::IsolationRequirement {
                profile: compute_core::IsolationProfile::Process,
                host: Default::default(),
            },
            dependencies: self
                .capsule
                .as_ref()
                .map(|capsule| compute_core::WorkloadDependencies {
                    capsule: capsule.capsule_id().unwrap(),
                }),
        };
        let bundle =
            WorkloadBundle::create_from_with_capsule(workload, root, self.capsule.clone()).unwrap();
        PlacementRequirements::from_bundle(
            &bundle,
            4096,
            SubmissionMode::Synchronous,
            &RequirementOptions {
                platform: self.platform.clone(),
                ..RequirementOptions::default()
            },
        )
        .unwrap()
    }
}

fn node_project() -> (PaxObservation, tempfile::TempDir) {
    (
        observation("javascript", "npm", "node index.js"),
        tempfile::tempdir().unwrap(),
    )
}

fn outcome(
    required: &PlacementRequirements,
    providers: &[(&str, i64, Synthetic)],
) -> compute_placement::PlacementReport {
    let config = config(
        &providers
            .iter()
            .map(|(id, priority, synthetic)| (*id, synthetic.kind, *priority))
            .collect::<Vec<_>>(),
    );
    let records = providers
        .iter()
        .map(|(id, _, synthetic)| synthetic.record(id))
        .collect::<Vec<_>>();
    place(
        &config.providers,
        &config.pool,
        &records,
        required,
        &baseline(required),
        None,
    )
}

fn codes(report: &compute_placement::PlacementReport, id: &str) -> Vec<ReasonCode> {
    report
        .providers
        .iter()
        .find(|p| p.provider_id == id)
        .unwrap()
        .reasons
        .iter()
        .map(|reason| reason.code)
        .collect()
}

fn remote(runtimes: &[RuntimeKind]) -> Synthetic {
    Synthetic::new(compute_placement::ProviderKind::Remote, runtimes)
}

#[test]
fn a_target_satisfying_the_project_is_eligible() {
    let (pax, root) = node_project();
    let required = Project::default().requirements(&pax, root.path());
    assert_eq!(required.runtime.kind, RuntimeKind::Node);
    let report = outcome(&required, &[("node", 0, remote(&[RuntimeKind::Node]))]);
    assert_eq!(report.outcome, PlacementOutcome::Placed);
    assert_eq!(report.selected.as_ref().unwrap().provider_id, "node");
}

#[test]
fn a_generic_runtime_is_not_the_projects_runtime() {
    // A target with a shell, wasm, and python is not a target for a node
    // project, however capable it is otherwise.
    let (pax, root) = node_project();
    let required = Project::default().requirements(&pax, root.path());
    let report = outcome(
        &required,
        &[(
            "generic",
            0,
            remote(&[RuntimeKind::Shell, RuntimeKind::Wasm, RuntimeKind::Python]),
        )],
    );
    assert_eq!(report.outcome, PlacementOutcome::PlacementFailed);
    assert_eq!(codes(&report, "generic"), [ReasonCode::RuntimeUnsupported]);
    assert!(report.selected.is_none(), "no silent fallback");
}

#[test]
fn a_python_project_needs_a_python_target() {
    let root = tempfile::tempdir().unwrap();
    let pax = observation("python", "uv", "python3 main.py");
    let required = Project::default().requirements(&pax, root.path());
    let report = outcome(
        &required,
        &[
            ("node", 0, remote(&[RuntimeKind::Node])),
            ("python", 0, remote(&[RuntimeKind::Python])),
        ],
    );
    assert_eq!(report.selected.as_ref().unwrap().provider_id, "python");
    assert_eq!(codes(&report, "node"), [ReasonCode::RuntimeUnsupported]);
}

#[test]
fn a_runtime_version_constraint_is_enforced() {
    let (pax, root) = node_project();
    let project = Project {
        runtime_version: Some(">=24".into()),
        ..Project::default()
    };
    let required = project.requirements(&pax, root.path());
    let report = outcome(&required, &[("old", 0, remote(&[RuntimeKind::Node]))]);
    assert_eq!(codes(&report, "old"), [ReasonCode::RuntimeVersionMismatch]);

    let satisfied = Project {
        runtime_version: Some(">=20".into()),
        ..Project::default()
    }
    .requirements(&pax, root.path());
    let report = outcome(&satisfied, &[("old", 0, remote(&[RuntimeKind::Node]))]);
    assert_eq!(report.outcome, PlacementOutcome::Placed);
}

#[test]
fn an_incompatible_architecture_is_rejected() {
    let (pax, root) = node_project();
    let required = Project {
        architecture: Some("arm64".into()),
        ..Project::default()
    }
    .requirements(&pax, root.path());
    let mut arm = remote(&[RuntimeKind::Node]);
    arm.platform = "linux-arm64".into();
    let report = outcome(
        &required,
        &[("x86", 0, remote(&[RuntimeKind::Node])), ("arm", 0, arm)],
    );
    assert_eq!(codes(&report, "x86"), [ReasonCode::ArchitectureMismatch]);
    assert_eq!(report.selected.as_ref().unwrap().provider_id, "arm");
}

#[test]
fn an_incompatible_platform_is_rejected() {
    let (pax, root) = node_project();
    let required = Project {
        platform: Some(PlatformIdentity {
            os: "macos".into(),
            architecture: "x86_64".into(),
            runtime_abi: None,
        }),
        ..Project::default()
    }
    .requirements(&pax, root.path());
    let report = outcome(&required, &[("linux", 0, remote(&[RuntimeKind::Node]))]);
    assert_eq!(report.outcome, PlacementOutcome::PlacementFailed);
    assert_eq!(codes(&report, "linux"), [ReasonCode::PlatformMismatch]);
}

#[test]
fn a_dependency_capsule_binds_the_project_to_matching_targets() {
    let (pax, root) = node_project();
    let payload = tempfile::tempdir().unwrap();
    std::fs::write(payload.path().join("m.js"), "x").unwrap();
    let capsule = |version: &str| {
        DependencyCapsule::create(
            payload.path(),
            RuntimeKind::Node,
            Some(version.into()),
            PlatformIdentity {
                os: "linux".into(),
                architecture: "x86_64".into(),
                runtime_abi: None,
            },
            Vec::<DependencyEntry>::new(),
            None,
        )
        .unwrap()
    };
    let matching = Project {
        capsule: Some(capsule("22.12.0")),
        ..Project::default()
    }
    .requirements(&pax, root.path());
    let report = outcome(&matching, &[("node", 0, remote(&[RuntimeKind::Node]))]);
    assert_eq!(report.outcome, PlacementOutcome::Placed);

    let mismatched = Project {
        capsule: Some(capsule("24.0.0")),
        ..Project::default()
    }
    .requirements(&pax, root.path());
    let report = outcome(&mismatched, &[("node", 0, remote(&[RuntimeKind::Node]))]);
    assert_eq!(
        codes(&report, "node"),
        [ReasonCode::DependencyRuntimeMismatch]
    );
}

#[test]
fn several_eligible_targets_select_deterministically() {
    let (pax, root) = node_project();
    let required = Project::default().requirements(&pax, root.path());
    fn providers<'a>(order: &[&'a str]) -> Vec<(&'a str, i64, Synthetic)> {
        order
            .iter()
            .map(|id| (*id, 0, remote(&[RuntimeKind::Node])))
            .collect()
    }
    let forward = outcome(&required, &providers(&["a", "b", "c"]));
    let reversed = outcome(&required, &providers(&["c", "b", "a"]));
    assert_eq!(forward.outcome, PlacementOutcome::Placed);
    assert_eq!(forward.compatible_providers, ["a", "b", "c"]);
    assert_eq!(
        forward.selected.as_ref().unwrap().provider_id,
        reversed.selected.as_ref().unwrap().provider_id
    );
    assert_eq!(forward.placement_id, reversed.placement_id);

    // Priority decides among eligible targets; ineligible ones never win.
    let mut ranked = providers(&["a", "b"]);
    ranked[1].1 = 10;
    ranked.push(("wasm", 100, remote(&[RuntimeKind::Wasm])));
    let report = outcome(&required, &ranked);
    assert_eq!(report.selected.as_ref().unwrap().provider_id, "b");
    assert_eq!(codes(&report, "wasm"), [ReasonCode::RuntimeUnsupported]);
}

#[test]
fn no_eligible_target_is_an_explicit_unsupported_result() {
    let (pax, root) = node_project();
    let required = Project {
        architecture: Some("riscv64".into()),
        ..Project::default()
    }
    .requirements(&pax, root.path());
    let report = outcome(
        &required,
        &[
            ("a", 0, remote(&[RuntimeKind::Node])),
            ("b", 0, remote(&[RuntimeKind::Python])),
        ],
    );
    assert_eq!(report.outcome, PlacementOutcome::PlacementFailed);
    assert!(report.selected.is_none());
    assert_eq!(
        report.failure.as_ref().unwrap().code,
        "no_compatible_provider"
    );
    assert!(report.compatible_providers.is_empty());
    assert!(!codes(&report, "a").is_empty() && !codes(&report, "b").is_empty());
}
