//! Stacks: declarative, versioned, portable desired environments, and the
//! application bundles that run on them.

use std::path::PathBuf;

use compute_core::{
    ComponentSource, DependencyCapsule, DependencyEntry, PlatformIdentity, ProjectIdentity,
    ProjectRequirements, ResolvedComponent, RuntimeKind, is_version,
};
use compute_project::{
    FailureKind, STACK_FILE, Stack, SuppliedFile, bind_bundle, declared_bundle, find_stack,
    parse_inspection, parse_probe_report, resolve_bundle,
};

const MINIMAL: &str = r#"
schema = "compute.stack@1"
[stack]
name = "demo"
version = "1.2.3"
description = "words that are not identity"
[requirements]
runtime = "node"
version = ">=20"
[[component]]
name = "core"
kind = "package"
source = "npm:@appport/core"
version = "1.0.3"
[[component]]
name = "db"
kind = "package"
source = "npm:@feltdb/core"
version = "^0.11.0"
credentials = ["FELT_TOKEN"]
platforms = ["linux-x86_64", "macos-arm64"]
"#;

fn code(text: &str) -> (FailureKind, String) {
    let error = Stack::parse(text).unwrap_err();
    (error.kind, error.message)
}

fn replaced(from: &str, to: &str) -> String {
    assert!(MINIMAL.contains(from), "{from}");
    MINIMAL.replacen(from, to, 1)
}

#[test]
fn a_valid_stack_parses_with_a_stable_identity() {
    let stack = Stack::parse(MINIMAL).unwrap();
    assert_eq!(stack.name, "demo");
    assert_eq!(stack.components.len(), 2);
    let identity = stack.identity();
    assert_eq!(
        (identity.name.as_str(), identity.version.as_str()),
        ("demo", "1.2.3")
    );
    assert!(identity.fingerprint.starts_with("sha256:"));
    assert_eq!(identity.fingerprint.len(), "sha256:".len() + 64);
    assert_eq!(stack.credentials(), ["FELT_TOKEN"]);
}

#[test]
fn the_fingerprint_covers_declarations_only() {
    let base = Stack::parse(MINIMAL).unwrap().identity().fingerprint;
    // Description, whitespace, comments, and component order are not identity.
    let noise = MINIMAL
        .replace("words that are not identity", "other words")
        .replace("[stack]", "# a comment\n\n[stack]");
    assert_eq!(Stack::parse(&noise).unwrap().identity().fingerprint, base);
    let (head, components) = MINIMAL.split_once("[[component]]").unwrap();
    let (core, db) = components.split_once("[[component]]").unwrap();
    let reordered = format!("{head}[[component]]{db}[[component]]{core}");
    assert_eq!(
        Stack::parse(&reordered).unwrap().identity().fingerprint,
        base
    );
    // Every declarative change is.
    for changed in [
        replaced("version = \"1.2.3\"", "version = \"1.2.4\""),
        replaced("version = \"1.0.3\"", "version = \"1.0.4\""),
        replaced("version = \">=20\"", "version = \">=22\""),
        replaced("name = \"demo\"", "name = \"demo2\""),
        replaced(
            "credentials = [\"FELT_TOKEN\"]",
            "credentials = [\"OTHER_TOKEN\"]",
        ),
        replaced("\"macos-arm64\"", "\"linux-arm64\""),
    ] {
        assert_ne!(
            Stack::parse(&changed).unwrap().identity().fingerprint,
            base,
            "{changed}"
        );
    }
    // Nothing about the machine or the path is in it: parsing is a pure
    // function of the text.
    assert_eq!(Stack::parse(MINIMAL).unwrap().identity().fingerprint, base);
}

#[test]
fn invalid_stacks_are_structured_failures() {
    let cases: Vec<(String, &str)> = vec![
        ("not toml [".into(), "stack.toml"),
        (
            replaced(
                "schema = \"compute.stack@1\"",
                "schema = \"compute.stack@9\"",
            ),
            "schema",
        ),
        (
            replaced("name = \"demo\"", "name = \"Demo Stack\""),
            "stack name",
        ),
        (
            replaced("version = \"1.2.3\"", "version = \"latest\""),
            "major.minor.patch",
        ),
        (
            replaced("runtime = \"node\"", "runtime = \"cobol\""),
            "unknown runtime",
        ),
        (
            replaced("version = \">=20\"", "version = \"latest\""),
            "grammar",
        ),
        (
            replaced("source = \"npm:@appport/core\"", "source = \"appport\""),
            "<ecosystem>:<package>",
        ),
        (
            replaced(
                "source = \"npm:@appport/core\"",
                "source = \"pip:requests\"",
            ),
            "unknown source ecosystem",
        ),
        (
            replaced("version = \"1.0.3\"", "version = \"latest\""),
            "grammar",
        ),
        (
            replaced("kind = \"package\"", "kind = \"app\""),
            "unknown kind",
        ),
        (
            replaced(
                "credentials = [\"FELT_TOKEN\"]",
                "credentials = [\"sk-live-abc123\"]",
            ),
            "UPPER_SNAKE",
        ),
        (
            replaced(
                "platforms = [\"linux-x86_64\", \"macos-arm64\"]",
                "platforms = [\"everywhere\"]",
            ),
            "<os>-<architecture>",
        ),
        (
            replaced(
                "[[component]]\nname = \"db\"",
                "[[component]]\nname = \"core\"",
            ),
            "twice",
        ),
        (
            replaced(
                "version = \"1.0.3\"",
                "version = \"1.0.3\"\napi_key = \"secret\"",
            ),
            "api_key",
        ),
        (
            "schema = \"compute.stack@1\"\n[stack]\nname = \"x\"\nversion = \"1.0.0\"".into(),
            "at least one component",
        ),
    ];
    for (text, expected) in cases {
        let (kind, message) = code(&text);
        assert_eq!(kind, FailureKind::StackInvalid, "{text}");
        assert!(message.contains(expected), "{expected}: {message}");
    }
}

#[test]
fn secrets_have_no_place_in_a_stack() {
    // The manifest has no field that could hold a value...
    for field in [
        "value = \"x\"",
        "secret = \"x\"",
        "token = \"x\"",
        "env = {A = \"b\"}",
    ] {
        let text = replaced(
            "version = \"1.0.3\"",
            &format!("version = \"1.0.3\"\n{field}"),
        );
        assert_eq!(code(&text).0, FailureKind::StackInvalid, "{field}");
    }
    // ...and a credential reference must be a name, so a value pasted there
    // is refused rather than kept and hashed into the stack's identity.
    let pasted = replaced(
        "credentials = [\"FELT_TOKEN\"]",
        "credentials = [\"ghp_0123456789abcdef\"]",
    );
    assert!(code(&pasted).1.contains("never hold values"));
}

#[test]
fn a_stack_joins_the_projects_requirements_as_ordinary_requirements() {
    let stack = Stack::parse(MINIMAL).unwrap();
    let mut requirements = ProjectRequirements {
        requirements_version: compute_core::PROJECT_REQUIREMENTS_VERSION.into(),
        project: ProjectIdentity {
            source: "pax".into(),
            name: "app".into(),
            source_schema: "1".into(),
        },
        runtimes: vec![compute_core::RuntimeNeed {
            kind: RuntimeKind::Node,
            version: None,
            origin: "ecosystem:javascript".into(),
        }],
        tools: vec![],
        dependencies: vec![],
        platform: Default::default(),
        environment: vec![],
        commands: vec![],
        unresolved: vec![],
    };
    stack.apply(&mut requirements).unwrap();
    // The stack's runtime constraint became the project's runtime constraint.
    assert_eq!(requirements.runtimes[0].version.as_deref(), Some(">=20"));
    let deps: Vec<_> = requirements
        .runtime_dependencies()
        .map(|d| (d.name.as_str(), d.specifier.as_str(), d.origin.as_deref()))
        .collect();
    assert_eq!(
        deps,
        [
            ("@appport/core", "1.0.3", Some("stack:demo")),
            ("@feltdb/core", "^0.11.0", Some("stack:demo")),
        ]
    );

    // A project that constrains the runtime differently conflicts.
    let mut other = requirements.clone();
    other.runtimes[0].version = Some(">=18".into());
    let error = stack.apply(&mut other).unwrap_err();
    assert_eq!(error.kind, FailureKind::RequirementsUnresolved);
    // A project on another runtime conflicts.
    let mut python = requirements.clone();
    python.runtimes[0].kind = RuntimeKind::Python;
    assert!(stack.apply(&mut python).is_err());
}

fn capsule(packages: &[(&str, &str)]) -> DependencyCapsule {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("payload"), "x").unwrap();
    DependencyCapsule::create(
        dir.path(),
        RuntimeKind::Node,
        Some("24.0.0".into()),
        PlatformIdentity::current(),
        packages
            .iter()
            .map(|(name, version)| DependencyEntry {
                name: (*name).into(),
                version: (*version).into(),
                source: "test".into(),
                file_count: 1,
                sha256: format!("sha256:{}", "a".repeat(64)),
                license: None,
            })
            .collect(),
        None,
    )
    .unwrap()
}

#[test]
fn components_resolve_against_the_capsule_by_constraint() {
    let stack = Stack::parse(MINIMAL).unwrap();
    let capsule = capsule(&[("@appport/core", "1.0.3"), ("@feltdb/core", "0.11.9")]);
    let binding = stack.bind(Some(&capsule)).unwrap();
    assert_eq!(
        binding.capsule_id.as_deref(),
        Some(capsule.capsule_id().unwrap().as_str())
    );
    let resolved: Vec<_> = binding
        .components
        .iter()
        .map(|c| match c.resolved.as_ref().unwrap() {
            ResolvedComponent::Package { version, .. } => version.as_str(),
        })
        .collect();
    assert_eq!(resolved, ["1.0.3", "0.11.9"]);

    // A version outside the constraint, or an absent package, is unresolved:
    // recorded, not guessed.
    let wrong = self::capsule(&[("@appport/core", "1.0.4"), ("@feltdb/core", "0.12.0")]);
    let binding = stack.bind(Some(&wrong)).unwrap();
    assert!(binding.components.iter().all(|c| c.resolved.is_none()));
    assert!(
        stack
            .bind(None)
            .unwrap()
            .components
            .iter()
            .all(|c| c.resolved.is_none())
    );
}

#[test]
fn the_same_stack_is_realizable_on_some_computers_and_not_others() {
    let stack = Stack::parse(MINIMAL).unwrap();
    let binding = stack.bind(None).unwrap();
    let platform = |os: &str, arch: &str| PlatformIdentity {
        os: os.into(),
        architecture: arch.into(),
        runtime_abi: None,
    };
    assert!(
        binding
            .unsupported_on(&platform("linux", "x86_64"))
            .is_empty()
    );
    assert!(
        binding
            .unsupported_on(&platform("macos", "arm64"))
            .is_empty()
    );
    let excluded = binding.unsupported_on(&platform("linux", "arm64"));
    assert_eq!(excluded.len(), 1);
    assert_eq!(excluded[0].0, "db");
    assert!(excluded[0].1.contains("linux-arm64"));
    // A component declared unsupported is unsupported everywhere.
    let declared = replaced(
        "credentials = [\"FELT_TOKEN\"]",
        "unsupported = \"no native build yet\"",
    );
    let binding = Stack::parse(&declared).unwrap().bind(None).unwrap();
    assert_eq!(
        binding.unsupported_on(&platform("linux", "x86_64"))[0].1,
        "no native build yet"
    );
}

#[test]
fn stacks_are_found_by_name_or_path_and_are_just_files() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("stacks/demo");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(STACK_FILE), MINIMAL).unwrap();
    let roots = vec![root.path().to_path_buf()];
    assert_eq!(find_stack("demo", &roots).unwrap().name, "demo");
    let path = dir.to_string_lossy().into_owned();
    assert_eq!(find_stack(&path, &[]).unwrap().name, "demo");
    assert_eq!(
        find_stack(&dir.join(STACK_FILE).to_string_lossy(), &[])
            .unwrap()
            .name,
        "demo"
    );
    // Missing: discovery failure that says where it looked.
    let missing = find_stack("nope", &roots).unwrap_err();
    assert_eq!(missing.kind, FailureKind::ProjectDiscoveryFailed);
    assert!(missing.available["searched"].contains("stacks"));
    // A directory whose stack declares another name is refused.
    std::fs::create_dir_all(root.path().join("stacks/other")).unwrap();
    std::fs::write(root.path().join("stacks/other").join(STACK_FILE), MINIMAL).unwrap();
    assert_eq!(
        find_stack("other", &roots).unwrap_err().kind,
        FailureKind::StackInvalid
    );
}

fn repository_stack(name: &str) -> Stack {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    find_stack(name, &[root]).unwrap()
}

#[test]
fn the_reference_stacks_are_data_that_parse_and_hold_only_packages() {
    let randy = repository_stack("randy");
    let packages: Vec<_> = randy
        .components
        .iter()
        .map(|c| {
            let ComponentSource::Package {
                ecosystem,
                package,
                constraint,
            } = &c.source;
            assert_eq!(ecosystem, "npm");
            assert!(is_version(constraint), "{package} is pinned exactly");
            package.as_str()
        })
        .collect();
    assert_eq!(
        packages,
        [
            "@appport/appboundry",
            "@appport/core",
            "@appport/sdk",
            "@appport/services",
            "@authboundry/core",
            "@feltdb/core",
        ]
    );
    // The application is not a component of the stack that runs it.
    assert!(
        !packages
            .iter()
            .any(|p| p.contains("app") && p.ends_with("wasm"))
    );
    assert!(randy.credentials().is_empty());
    assert_eq!(repository_stack("minimal-node").components.len(), 1);
    // Another stack is another directory of data: nothing names "randy".
    assert_ne!(
        randy.identity().fingerprint,
        repository_stack("minimal-node").identity().fingerprint
    );
}

// ---- application bundles ------------------------------------------------

fn app_files() -> Vec<SuppliedFile> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../compute-cli/tests/fixtures/appboundry/AppBoundry.app");
    ["manifest", "application.wasm"]
        .iter()
        .map(|name| SuppliedFile {
            path: format!("AppBoundry.app/{name}"),
            bytes: std::fs::read(dir.join(name)).unwrap(),
        })
        .collect()
}

fn toml_project(text: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("compute.toml"), text).unwrap();
    dir
}

const APP: &str = r#"
[artifact]
application = "dev.appboundry.portal"
version = "1.0.0"
runtime = "wasm"
abi = "wasm/1"
artifact = "sha256:fef9a8b0ef770d37fcfe41f1547bff26e173e42bef767b3936ccd3ef8c2eb3ad"
"#;

#[test]
fn a_project_declares_the_application_bundle_it_runs() {
    let dir = toml_project(APP);
    let declared = declared_bundle(dir.path()).unwrap().unwrap();
    assert_eq!(declared.application, "dev.appboundry.portal");
    assert_eq!(declared.runtime, RuntimeKind::Wasm);
    assert_eq!(declared.abi, "wasm/1");
    // No [artifact], no bundle.
    assert!(
        declared_bundle(toml_project("[network]\nmode = \"network\"").path())
            .unwrap()
            .is_none()
    );
    assert!(
        declared_bundle(tempfile::tempdir().unwrap().path())
            .unwrap()
            .is_none()
    );
    for bad in [
        APP.replace("wasm/1", "wasm/1\"\nsurprise = \"x"),
        APP.replace("runtime = \"wasm\"", "runtime = \"cobol\""),
        APP.replace("sha256:fef9", "md5:fef9"),
        APP.replace("version = \"1.0.0\"", "version = \"latest\""),
    ] {
        let error = declared_bundle(toml_project(&bad).path()).unwrap_err();
        assert_eq!(error.kind, FailureKind::RequirementsUnresolved, "{bad}");
    }
}

#[test]
fn the_real_appboundry_bundle_is_found_by_content_not_by_name() {
    let declared = declared_bundle(toml_project(APP).path()).unwrap().unwrap();
    let files = app_files();
    let resolved = resolve_bundle(&declared, &files).expect("resolves");
    assert_eq!(resolved.manifest_path, "AppBoundry.app/manifest");
    assert_eq!(resolved.module_path, "AppBoundry.app/application.wasm");
    assert_eq!(resolved.version, "1.0.0");
    assert_eq!(
        resolved.module_sha256,
        "sha256:fef9a8b0ef770d37fcfe41f1547bff26e173e42bef767b3936ccd3ef8c2eb3ad"
    );
    assert_eq!(
        resolved.package_identity,
        "sha256:cae5adb5eca65a3fd857cc5810b53a5581127d7e6a9fbe8ebd489350933e9624"
    );

    // Any file name and directory works: the manifest is recognized by its
    // protocol, whatever it is called (`manifest`, `something.apph`, ...).
    let renamed: Vec<_> = files
        .iter()
        .map(|file| SuppliedFile {
            path: file
                .path
                .replace("AppBoundry.app/manifest", "AppBoundry.app/portal.apph"),
            bytes: file.bytes.clone(),
        })
        .collect();
    assert_eq!(
        resolve_bundle(&declared, &renamed).unwrap().manifest_path,
        "AppBoundry.app/portal.apph"
    );
}

#[test]
fn a_bundle_that_is_not_the_declared_one_does_not_resolve() {
    let declared = declared_bundle(toml_project(APP).path()).unwrap().unwrap();
    let files = app_files();
    // Tampered module: its hash is not the one the manifest carries.
    let mut tampered = files.clone();
    tampered[1].bytes.push(0);
    assert!(resolve_bundle(&declared, &tampered).is_none());
    // Another application, another format, another version, another pin.
    type Change = Box<dyn Fn(&mut compute_core::AppBundleDeclaration)>;
    let changes: Vec<Change> = vec![
        Box::new(|d| d.application = "dev.other.app".into()),
        Box::new(|d| d.abi = "wasm/2".into()),
        Box::new(|d| d.version = Some("2.0.0".into())),
        Box::new(|d| d.artifact = Some(format!("sha256:{}", "0".repeat(64)))),
    ];
    for change in changes {
        let mut other = declared.clone();
        change(&mut other);
        assert!(resolve_bundle(&other, &files).is_none());
    }
    // A manifest alone, or a module alone, is not a bundle.
    assert!(resolve_bundle(&declared, &files[..1]).is_none());
    assert!(resolve_bundle(&declared, &files[1..]).is_none());
    let binding = bind_bundle(declared, &tampered);
    assert!(binding.resolved.is_none() && binding.inspection.is_none());
}

#[test]
fn the_platform_packages_verdict_is_relayed_not_recomputed() {
    let receipt = format!("sha256:{}", "b".repeat(64));
    let report = concat!(
        "{\"package\":\"@appport/core\",\"version\":\"1.0.3\"}\n",
        "{\"application\":{\"packageVersion\":\"1.1.1\",\"certification\":\"CERTIFIED\",\"failedChecks\":[],",
        "\"applicationId\":\"dev.appboundry.portal\",\"artifactHash\":\"fef9\",\"packageIdentity\":\"sha256:cae5\",",
        "\"readiness\":\"PROVIDERS_UNAVAILABLE\",\"missingProviders\":[\"feltdb.documents@1\"]}}\n"
    );
    let inspection = parse_inspection(report, &receipt).unwrap();
    assert_eq!(inspection.certification, "CERTIFIED");
    assert_eq!(inspection.readiness, "PROVIDERS_UNAVAILABLE");
    assert_eq!(inspection.missing_providers, ["feltdb.documents@1"]);
    assert_eq!(inspection.receipt, receipt);
    // An error line, or no line, is "not inspected": never a certification.
    assert!(
        parse_inspection(
            "{\"application\":{\"error\":\"package_missing\"}}\n",
            &receipt
        )
        .is_none()
    );
    assert!(parse_inspection("", &receipt).is_none());
    assert!(parse_inspection("not json\n", &receipt).is_none());

    // The package report maps back to component names.
    let binding = Stack::parse(MINIMAL).unwrap().bind(None).unwrap();
    let observed = parse_probe_report(
        "{\"package\":\"@appport/core\",\"version\":\"1.0.3\"}\n{\"package\":\"@feltdb/core\",\"version\":null}\nnoise\n",
        &binding,
    );
    assert_eq!(observed["core"].as_deref(), Some("1.0.3"));
    assert_eq!(observed["db"], None);
    assert_eq!(observed.len(), 2);
}
