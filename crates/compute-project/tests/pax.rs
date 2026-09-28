//! The PAX adapter and project planning boundary, against PAX's real
//! `schemaVersion: "1"` documents (captured from `pax --json info|deps|
//! scripts`) and without any PAX executable.

use std::path::{Path, PathBuf};

use compute_core::{
    DependencyCapsule, DependencyEntry, DependencyGroup, PlatformIdentity, RuntimeKind, ToolRole,
};
use compute_project::{
    CommandSelection, EnvironmentInputs, FailureKind, PaxExecutable, PaxObservation, PaxSource,
    ProjectError, discover, materialize, select_command,
};
use serde_json::{Value, json};

fn info(name: &str, ecosystem: Value, manager: Value, tool: Value) -> Value {
    let components = if ecosystem.is_null() {
        json!([])
    } else {
        json!([{
            "path": ".", "ecosystem": ecosystem, "tool": tool,
            "manifests": ["package.json"], "lockfiles": [], "evidence": [],
            "workspacePackages": [], "dependencySources": []
        }])
    };
    json!({
        "schemaVersion": "1", "command": "info",
        "project": {"root": "/x", "name": name, "packageJson": true, "workspace": false, "workspaceSource": null},
        "manager": manager,
        "result": {"summary": "s", "runtime": null},
        "ecosystem": ecosystem, "components": components,
        "nativeDependencies": [], "container": null
    })
}

fn deps(groups: Value, native: Value) -> Value {
    json!({
        "schemaVersion": "1", "command": "deps",
        "dependencies": {
            "dependencies": groups["dependencies"].clone(),
            "devDependencies": groups["devDependencies"].clone(),
            "optionalDependencies": {}, "peerDependencies": groups["peerDependencies"].clone(),
            "nativeDependencies": native
        }
    })
}

fn scripts(map: Value) -> Value {
    json!({"schemaVersion": "1", "command": "scripts", "scripts": map})
}

fn npm() -> Value {
    json!({"name": "npm", "version": "10.8.0", "lockfile": "package-lock.json", "selectedBy": "lockfile precedence"})
}

fn javascript() -> PaxObservation {
    PaxObservation::from_documents(
        info("app", json!("javascript"), npm(), json!("npm")),
        deps(
            json!({
                "dependencies": {"left-pad": "1.3.0", "lodash": "^4.17.0"},
                "devDependencies": {"typescript": "^5.0.0"},
                "peerDependencies": {"react": ">=18"}
            }),
            json!([]),
        ),
        scripts(json!({"start": "node index.js --port 80", "build": "tsc -p .", "check": "node check.js"})),
    )
    .unwrap()
}

struct Static(PaxObservation);
impl PaxSource for Static {
    fn observe(&self, _: &Path) -> Result<PaxObservation, ProjectError> {
        Ok(self.0.clone())
    }
}

fn project_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

// ---- discovery ----------------------------------------------------------

#[test]
fn a_pax_project_is_discovered_and_keeps_its_identity() {
    let dir = project_dir();
    let found = discover(dir.path(), &Static(javascript())).unwrap();
    assert_eq!(found.root, dir.path().canonicalize().unwrap());
    assert_eq!(found.requirements.project.name, "app");
    assert_eq!(found.requirements.project.source, "pax");
    assert_eq!(found.requirements.project.source_schema, "1");
}

#[test]
fn a_directory_pax_recognizes_nothing_in_is_not_a_project() {
    let dir = project_dir();
    let empty = PaxObservation::from_documents(
        info("empty", Value::Null, json!({"name": null}), Value::Null),
        deps(json!({}), json!([])),
        scripts(json!({})),
    )
    .unwrap();
    let error = discover(dir.path(), &Static(empty)).unwrap_err();
    assert_eq!(error.code, "project_discovery_failed");
}

#[test]
fn a_missing_directory_fails_discovery_not_execution() {
    let error = discover(Path::new("/definitely/not/here"), &Static(javascript())).unwrap_err();
    assert_eq!(error.kind, FailureKind::ProjectDiscoveryFailed);
}

#[test]
fn invalid_metadata_is_a_structured_failure() {
    let good = || {
        (
            info("app", json!("javascript"), npm(), json!("npm")),
            deps(json!({}), json!([])),
            scripts(json!({})),
        )
    };
    let code = |result: Result<PaxObservation, ProjectError>| result.unwrap_err().code;

    let (mut i, d, s) = good();
    i["schemaVersion"] = json!("2");
    assert_eq!(
        code(PaxObservation::from_documents(i, d, s)),
        "pax_metadata_invalid"
    );

    let (i, mut d, s) = good();
    d["command"] = json!("info");
    assert_eq!(
        code(PaxObservation::from_documents(i, d, s)),
        "pax_metadata_invalid"
    );

    let (i, d, _) = good();
    assert_eq!(
        code(PaxObservation::from_documents(i, d, json!([]))),
        "pax_metadata_invalid"
    );

    let (mut i, d, s) = good();
    i["project"]["name"] = json!("");
    assert_eq!(
        code(PaxObservation::from_documents(i, d, s)),
        "pax_metadata_invalid"
    );

    let (mut i, d, s) = good();
    i.as_object_mut().unwrap().remove("components");
    assert_eq!(
        code(PaxObservation::from_documents(i, d, s)),
        "pax_metadata_invalid"
    );

    // A malformed dependency surfaces when requirements are extracted.
    let (i, _, s) = good();
    let bad = deps(json!({"dependencies": {"left-pad": 3}}), json!([]));
    let observation = PaxObservation::from_documents(i, bad, s).unwrap();
    assert_eq!(
        observation.requirements().unwrap_err().kind,
        FailureKind::PaxMetadataInvalid
    );

    // The schema mismatch names what was required and what was found.
    let (mut i, d, s) = good();
    i["schemaVersion"] = json!("9");
    let error = PaxObservation::from_documents(i, d, s).unwrap_err();
    assert_eq!(error.required["schemaVersion"], "1");
    assert_eq!(error.available["schemaVersion"], "9");
    assert!(error.to_string().contains("result: unsupported"));
}

// ---- requirement extraction ---------------------------------------------

#[test]
fn requirements_are_normalized_from_pax() {
    let requirements = javascript().requirements().unwrap();

    assert_eq!(requirements.runtimes.len(), 1);
    assert_eq!(requirements.runtimes[0].kind, RuntimeKind::Node);
    assert_eq!(requirements.runtimes[0].origin, "ecosystem:javascript");

    // The package manager is a provisioning tool with its version constraint.
    assert_eq!(requirements.tools.len(), 1);
    assert_eq!(requirements.tools[0].name, "npm");
    assert_eq!(requirements.tools[0].version.as_deref(), Some("10.8.0"));
    assert_eq!(requirements.tools[0].role, ToolRole::Provisioning);

    let group = |name: &str| {
        requirements
            .dependencies
            .iter()
            .find(|d| d.name == name)
            .map(|d| (d.group, d.specifier.as_str()))
    };
    assert_eq!(group("left-pad"), Some((DependencyGroup::Runtime, "1.3.0")));
    assert_eq!(group("lodash"), Some((DependencyGroup::Runtime, "^4.17.0")));
    assert_eq!(
        group("typescript"),
        Some((DependencyGroup::Development, "^5.0.0"))
    );
    assert_eq!(group("react"), Some((DependencyGroup::Peer, ">=18")));
    assert_eq!(requirements.runtime_dependencies().count(), 2);

    let commands: Vec<_> = requirements
        .commands
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(commands, ["build", "check", "start"]);

    // PAX reports no platform constraint; none is invented.
    assert_eq!(requirements.platform.os, None);
    assert_eq!(requirements.platform.architecture, None);
    assert!(requirements.unresolved.is_empty());
}

#[test]
fn pinned_versions_are_exact_versions_only() {
    let requirements = javascript().requirements().unwrap();
    let pin = |name: &str| {
        requirements
            .dependencies
            .iter()
            .find(|d| d.name == name)
            .unwrap()
            .pinned_version()
            .map(str::to_owned)
    };
    assert_eq!(pin("left-pad").as_deref(), Some("1.3.0"));
    assert_eq!(pin("lodash"), None);
    assert_eq!(pin("react"), None);
}

#[test]
fn requirement_identity_is_deterministic_and_order_independent() {
    let a = javascript().requirements().unwrap();
    let b = javascript().requirements().unwrap();
    assert_eq!(a.requirements_id().unwrap(), b.requirements_id().unwrap());

    let mut shuffled = a.clone();
    shuffled.dependencies.reverse();
    shuffled.commands.reverse();
    assert_eq!(
        a.requirements_id().unwrap(),
        shuffled.requirements_id().unwrap()
    );

    let mut changed = a.clone();
    changed.dependencies[0].specifier = "9.9.9".into();
    assert_ne!(
        a.requirements_id().unwrap(),
        changed.requirements_id().unwrap()
    );
    assert!(a.requirements_id().unwrap().starts_with("sha256:"));
}

#[test]
fn python_projects_map_to_the_python_runtime() {
    let observation = PaxObservation::from_documents(
        info("svc", json!("python"), json!({"name": null}), json!("uv")),
        deps(json!({}), json!([])),
        scripts(json!({"start": "python3 main.py"})),
    )
    .unwrap();
    let requirements = observation.requirements().unwrap();
    assert_eq!(requirements.runtimes[0].kind, RuntimeKind::Python);
    assert_eq!(requirements.tools[0].name, "uv");
}

#[test]
fn what_compute_cannot_normalize_is_never_dropped() {
    let rust = PaxObservation::from_documents(
        info(
            "crate",
            json!("rust"),
            json!({"name": null}),
            json!("cargo"),
        ),
        deps(json!({}), json!([])),
        scripts(json!({})),
    )
    .unwrap();
    let dir = project_dir();
    let error = discover(dir.path(), &Static(rust)).unwrap_err();
    assert_eq!(error.kind, FailureKind::RequirementsUnresolved);
    assert!(error.required.contains_key("ecosystem"), "{error}");

    let native = PaxObservation::from_documents(
        info("svc", json!("python"), json!({"name": null}), json!("uv")),
        deps(json!({}), json!([{"name": "requests", "specifier": ">=2"}])),
        scripts(json!({})),
    )
    .unwrap();
    let error = discover(dir.path(), &Static(native)).unwrap_err();
    assert_eq!(error.kind, FailureKind::RequirementsUnresolved);
    assert!(error.required.contains_key("dependencies"));
}

// ---- command selection --------------------------------------------------

#[test]
fn command_selection_defaults_to_start_and_can_be_named() {
    let requirements = javascript().requirements().unwrap();

    let start = select_command(&requirements, &CommandSelection::Default).unwrap();
    assert_eq!(start.name.as_deref(), Some("start"));
    assert_eq!(start.runtime, Some(RuntimeKind::Node));
    assert_eq!(start.entrypoint, Some(PathBuf::from("index.js")));
    assert_eq!(start.args, ["--port", "80"]);

    let check = select_command(&requirements, &CommandSelection::Named("check".into())).unwrap();
    assert_eq!(check.entrypoint, Some(PathBuf::from("check.js")));
    assert!(check.args.is_empty());

    let missing =
        select_command(&requirements, &CommandSelection::Named("nope".into())).unwrap_err();
    assert_eq!(missing.kind, FailureKind::RequirementsUnresolved);
    assert_eq!(missing.available["commands"], "build, check, start");
}

#[test]
fn a_project_without_start_falls_back_to_compute_conventions() {
    let mut requirements = javascript().requirements().unwrap();
    requirements.commands.retain(|c| c.name != "start");
    let selected = select_command(&requirements, &CommandSelection::Default).unwrap();
    assert_eq!(selected.name, None);
    assert_eq!(selected.entrypoint, None);
}

#[test]
fn commands_compute_cannot_execute_are_refused_not_shelled_out() {
    let requirements = javascript().requirements().unwrap();
    // `tsc` is another tool, not a runtime Compute runs an entrypoint with.
    let error =
        select_command(&requirements, &CommandSelection::Named("build".into())).unwrap_err();
    assert_eq!(error.kind, FailureKind::RequirementsUnresolved);
    assert!(error.message.contains("tsc"), "{error}");

    for (command, needle) in [
        ("node a.js && node b.js", "shell"),
        ("node -e x", "entrypoint"),
        ("NODE_ENV=1 node a.js", "runtime"),
        ("node", "entrypoint"),
        ("python main.py", "ecosystem"),
    ] {
        let mut requirements = requirements.clone();
        requirements.commands[0].command = command.into();
        let name = requirements.commands[0].name.clone();
        let error =
            select_command(&requirements, &CommandSelection::Named(name)).expect_err(command);
        assert!(error.to_string().contains(needle), "{command}: {error}");
    }
}

// ---- materialization ----------------------------------------------------

fn capsule(runtime: RuntimeKind, packages: &[(&str, &str)]) -> DependencyCapsule {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("payload.txt"), "x").unwrap();
    DependencyCapsule::create(
        dir.path(),
        runtime,
        Some("24.0.0".into()),
        PlatformIdentity::current(),
        packages
            .iter()
            .map(|(name, version)| DependencyEntry {
                name: (*name).into(),
                version: (*version).into(),
                source: "test".into(),
                file_count: 1,
                sha256: format!("sha256:{}", "0".repeat(64)),
                license: None,
            })
            .collect(),
        None,
    )
    .unwrap()
}

fn inputs<'a>(capsule: Option<&'a DependencyCapsule>) -> EnvironmentInputs<'a> {
    EnvironmentInputs {
        runtime: RuntimeKind::Node,
        runtime_constraint: Some(">=20"),
        os: None,
        architecture: Some("x86_64"),
        capsule,
        environment_names: vec!["B".into(), "A".into(), "A".into()],
        entrypoint: "index.js".into(),
        argument_count: 2,
        command_name: Some("start".into()),
        offered_tools: &[],
    }
}

#[test]
fn resolved_requirements_become_environment_inputs() {
    let requirements = javascript().requirements().unwrap();
    let capsule = capsule(
        RuntimeKind::Node,
        &[("left-pad", "1.3.0"), ("lodash", "4.17.21")],
    );
    let binding = materialize(&requirements, &inputs(Some(&capsule))).unwrap();

    assert_eq!(binding.identity.name, "app");
    assert_eq!(
        binding.requirements_id,
        requirements.requirements_id().unwrap()
    );
    assert_eq!(binding.declared.dependency_count, 2);
    assert_eq!(binding.declared.runtimes[0].kind, RuntimeKind::Node);
    assert_eq!(binding.resolved.runtime, RuntimeKind::Node);
    assert_eq!(binding.resolved.runtime_constraint.as_deref(), Some(">=20"));
    assert_eq!(binding.resolved.architecture.as_deref(), Some("x86_64"));
    assert_eq!(
        binding.resolved.capsule_id.as_deref(),
        Some(capsule.capsule_id().unwrap().as_str())
    );
    assert_eq!(binding.resolved.command.entrypoint, "index.js");
    assert_eq!(binding.resolved.command.argument_count, 2);
    assert_eq!(binding.resolved.environment_names, ["A", "B"]);
}

#[test]
fn a_missing_capsule_is_dependency_unavailable_and_says_what_is_needed() {
    let requirements = javascript().requirements().unwrap();
    let error = materialize(&requirements, &inputs(None)).unwrap_err();
    assert_eq!(error.kind, FailureKind::DependencyUnavailable);
    assert_eq!(error.required["left-pad"], "1.3.0");
    assert_eq!(error.available["capsule"], "none");
    assert!(error.to_string().ends_with("result: unsupported"));
}

#[test]
fn a_capsule_that_lacks_or_mispins_a_dependency_is_rejected() {
    let requirements = javascript().requirements().unwrap();

    let missing = capsule(RuntimeKind::Node, &[("left-pad", "1.3.0")]);
    let error = materialize(&requirements, &inputs(Some(&missing))).unwrap_err();
    assert_eq!(error.kind, FailureKind::DependencyUnavailable);
    assert_eq!(error.available["lodash"], "absent");

    let wrong = capsule(
        RuntimeKind::Node,
        &[("left-pad", "1.2.0"), ("lodash", "4.17.21")],
    );
    let error = materialize(&requirements, &inputs(Some(&wrong))).unwrap_err();
    assert_eq!(error.required["left-pad"], "1.3.0");
    assert_eq!(error.available["left-pad"], "1.2.0");

    let python = capsule(
        RuntimeKind::Python,
        &[("left-pad", "1.3.0"), ("lodash", "4.17.21")],
    );
    let error = materialize(&requirements, &inputs(Some(&python))).unwrap_err();
    assert_eq!(error.available["capsule runtime"], "python");
}

#[test]
fn a_project_without_runtime_dependencies_needs_no_capsule() {
    let mut requirements = javascript().requirements().unwrap();
    requirements.dependencies.clear();
    let binding = materialize(&requirements, &inputs(None)).unwrap();
    assert_eq!(binding.resolved.capsule_id, None);
    assert_eq!(binding.declared.dependency_count, 0);
}

#[test]
fn a_runtime_the_project_does_not_need_is_a_conflict_not_a_fallback() {
    let mut requirements = javascript().requirements().unwrap();
    requirements.dependencies.clear();
    let mut wrong = inputs(None);
    wrong.runtime = RuntimeKind::Python;
    let error = materialize(&requirements, &wrong).unwrap_err();
    assert_eq!(error.kind, FailureKind::RequirementsUnresolved);
    assert_eq!(error.required["runtime"], "node");
    assert_eq!(error.available["runtime"], "python");
}

#[test]
fn an_execution_tool_no_target_offers_is_rejected_a_provisioning_tool_is_not() {
    let mut requirements = javascript().requirements().unwrap();
    requirements.dependencies.clear();
    // npm is provisioning: it built the environment and is never needed.
    materialize(&requirements, &inputs(None)).unwrap();

    requirements.tools.push(compute_core::ToolNeed {
        name: "ffmpeg".into(),
        version: None,
        role: ToolRole::Execution,
    });
    let error = materialize(&requirements, &inputs(None)).unwrap_err();
    assert_eq!(error.kind, FailureKind::NoTargetSatisfiesRequirements);
    assert_eq!(error.required["tool"], "ffmpeg");
    assert_eq!(error.available["tools"], "none advertised");

    let offered = ["ffmpeg".to_owned()];
    let mut ok = inputs(None);
    ok.offered_tools = &offered;
    materialize(&requirements, &ok).unwrap();
}

// ---- the PAX executable -------------------------------------------------

#[cfg(unix)]
mod executable {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fake_pax(dir: &Path, script: &str) -> PaxExecutable {
        let path = dir.join("pax");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        PaxExecutable::new(path)
    }

    fn respond(name: &str, value: &Value) -> String {
        format!("[ \"$4\" = {name} ] && cat <<'JSON'\n{value}\nJSON\n")
    }

    #[test]
    fn observation_runs_pax_read_only_with_json_and_dir() {
        let dir = project_dir();
        let script = [
            respond(
                "info",
                &info("app", json!("javascript"), npm(), json!("npm")),
            ),
            respond("deps", &deps(json!({}), json!([]))),
            respond("scripts", &scripts(json!({"start": "node index.js"}))),
        ]
        .concat()
            + "exit 0";
        let pax = fake_pax(dir.path(), &script);
        let found = discover(dir.path(), &pax).unwrap();
        assert_eq!(found.requirements.project.name, "app");
        assert_eq!(found.requirements.commands[0].command, "node index.js");
    }

    #[test]
    fn a_missing_executable_fails_discovery() {
        let dir = project_dir();
        let error =
            discover(dir.path(), &PaxExecutable::new(dir.path().join("no-pax"))).unwrap_err();
        assert_eq!(error.kind, FailureKind::ProjectDiscoveryFailed);
        assert!(error.message.contains("COMPUTE_PAX"), "{error}");
    }

    #[test]
    fn pax_failures_are_classified_by_its_exit_code() {
        let dir = project_dir();
        let invalid_input = fake_pax(dir.path(), "echo 'invalid project directory' >&2; exit 2");
        assert_eq!(
            discover(dir.path(), &invalid_input).unwrap_err().kind,
            FailureKind::ProjectDiscoveryFailed
        );
        let unparsable = fake_pax(
            dir.path(),
            "echo 'failed to parse package.json' >&2; exit 1",
        );
        let error = discover(dir.path(), &unparsable).unwrap_err();
        assert_eq!(error.kind, FailureKind::PaxMetadataInvalid);
        assert!(error.message.contains("failed to parse package.json"));
        let garbage = fake_pax(dir.path(), "echo 'not json'");
        assert_eq!(
            discover(dir.path(), &garbage).unwrap_err().kind,
            FailureKind::PaxMetadataInvalid
        );
    }
}
