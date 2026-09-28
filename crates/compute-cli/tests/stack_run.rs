//! A configured Computer and the application that runs on it:
//!
//! ```text
//! Randy stack (packages) ─▶ Computer ─▶ appboundry.app ─▶ Compute run ─▶ receipt
//! ```
//!
//! The stack is the repository's real `stacks/randy/stack.toml`; the
//! application bundle is the real `AppBoundry.app` (manifest and
//! `application.wasm`); PAX is a script printing the documents `pax --json`
//! prints. The six packages are *stand-ins*: directories holding a
//! `package.json` with each real package's name and pinned version, because
//! the real packages are ~125 MB and need a registry. They exercise resolution,
//! materialization, and probing; they cannot exercise AppBoundry's own
//! certification API, so those checks are honestly reported as not evaluated.
//! `real_randy_stack_certifies_the_real_appboundry_application` runs the real
//! packages when `RANDY_NODE_MODULES` names an npm install of them.

#[path = "support/runtimes.rs"]
mod runtimes;
#[path = "support/targets.rs"]
mod targets;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn randy() -> PathBuf {
    manifest_dir().join("../../stacks/randy")
}

/// The pinned packages of the Randy stack, read from the stack file itself.
fn randy_packages() -> Vec<(String, String)> {
    let text = std::fs::read_to_string(randy().join("stack.toml")).unwrap();
    let value: toml::Value = toml::from_str(&text).unwrap();
    value["component"]
        .as_array()
        .unwrap()
        .iter()
        .map(|component| {
            (
                component["source"]
                    .as_str()
                    .unwrap()
                    .trim_start_matches("npm:")
                    .to_owned(),
                component["version"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn documents(name: &str) -> [Value; 3] {
    let project = json!({"root": "/x", "name": name, "packageJson": true, "workspace": false, "workspaceSource": null});
    let manager = json!({"name": "npm", "version": null, "lockfile": "package-lock.json", "selectedBy": "lockfile precedence"});
    [
        json!({"schemaVersion": "1", "command": "info", "project": project, "manager": manager,
            "result": {"summary": "s", "runtime": null}, "ecosystem": "javascript",
            "components": [{"path": ".", "ecosystem": "javascript", "tool": "npm", "manifests": ["package.json"],
                "lockfiles": ["package-lock.json"], "evidence": [], "workspacePackages": [], "dependencySources": []}],
            "nativeDependencies": [], "container": null}),
        json!({"schemaVersion": "1", "command": "deps", "project": project, "manager": manager,
            "dependencies": {"dependencies": {"@appport/appboundry": "1.1.1", "@appport/core": "1.0.3", "@feltdb/core": "0.11.9"},
                "devDependencies": {}, "optionalDependencies": {}, "peerDependencies": {}, "nativeDependencies": []}}),
        json!({"schemaVersion": "1", "command": "scripts", "project": project, "manager": manager,
            "scripts": {"start": "node index.js"}}),
    ]
}

#[cfg(unix)]
fn install_pax(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let mut script = String::from("#!/bin/sh\n");
    for document in documents("appboundry-app") {
        let command = document["command"].as_str().unwrap();
        script.push_str(&format!(
            "if [ \"$4\" = {command} ]; then cat <<'JSON'\n{document}\nJSON\nexit 0; fi\n"
        ));
    }
    script.push_str("exit 2\n");
    let path = dir.join("pax");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

struct World {
    root: tempfile::TempDir,
    project: PathBuf,
    home: PathBuf,
    pax: PathBuf,
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn world() -> World {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    copy_dir(
        &manifest_dir().join("tests/fixtures/appboundry-project"),
        &project,
    );
    // The application bundle, as its build produced it.
    copy_dir(
        &manifest_dir().join("tests/fixtures/appboundry/AppBoundry.app"),
        &project.join("AppBoundry.app"),
    );
    let pax = install_pax(root.path());
    World {
        home: root.path().join("home"),
        root,
        project,
        pax,
    }
}

impl World {
    fn compute(&self, arguments: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_compute"));
        runtimes::with_fixture_runtimes(&mut command);
        command
            .current_dir(&self.project)
            .env("COMPUTE_HOME", &self.home)
            .env("COMPUTE_PAX", &self.pax)
            .env("COMPUTE_DAEMON", "http://127.0.0.1:9")
            .env_remove("COMPUTE_POOL_CONFIG")
            .env_remove("COMPUTE_DAEMON_TOKEN")
            .env_remove("COMPUTE_STACKS")
            .args(arguments)
            .output()
            .unwrap()
    }

    /// The dependency capsule of the six pinned packages (stand-ins).
    fn capsule(&self, omit: Option<&str>) -> PathBuf {
        let label = omit.unwrap_or("all").replace(['/', '@'], "_");
        let resolved = self.root.path().join(format!("resolved-{label}"));
        let mut packages = vec![];
        for (name, version) in randy_packages() {
            if Some(name.as_str()) == omit {
                continue;
            }
            let dir = resolved.join("node_modules").join(&name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("package.json"),
                json!({"name": name, "version": version}).to_string(),
            )
            .unwrap();
            packages.push(format!("{name}={version}"));
        }
        self.build_capsule(&resolved, &packages, &label)
    }

    fn build_capsule(&self, resolved: &Path, packages: &[String], label: &str) -> PathBuf {
        let catalog = self.compute(&["runtimes", "--json"]);
        let catalog: Value = serde_json::from_slice(&catalog.stdout).unwrap();
        let version = catalog["runtimes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|runtime| runtime["runtime"] == "node")
            .expect("the fixture catalog offers node")["version"]
            .as_str()
            .unwrap()
            .to_owned();
        let capsule = self.root.path().join(format!("{label}.deps"));
        let mut arguments = vec![
            "deps".to_owned(),
            "create".into(),
            "--runtime".into(),
            "node".into(),
            "--runtime-version".into(),
            version,
            "--resolved".into(),
            resolved.to_string_lossy().into_owned(),
            "--output".into(),
            capsule.to_string_lossy().into_owned(),
        ];
        for package in packages {
            arguments.push("--package".into());
            arguments.push(package.clone());
        }
        let refs: Vec<&str> = arguments.iter().map(String::as_str).collect();
        let output = self.compute(&refs);
        assert!(output.status.success(), "{output:?}");
        capsule
    }

    fn run(&self, extra: &[&str]) -> Output {
        let stack = randy();
        let mut arguments = vec!["run", "--stack", stack.to_str().unwrap()];
        arguments.extend(extra);
        self.compute(&arguments)
    }
}

const APP_INPUTS: [&str; 4] = [
    "--input",
    "AppBoundry.app/manifest",
    "--input",
    "AppBoundry.app/application.wasm",
];

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn receipt_of(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn gate<'a>(evidence: &'a Value, check: &str) -> &'a Value {
    evidence["gates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|gate| gate["check"] == check)
        .unwrap_or_else(|| panic!("no gate {check}: {evidence:#}"))
}

#[test]
fn the_randy_stack_configures_a_computer_and_appboundry_runs_on_it() {
    let world = world();
    let capsule = world.capsule(None);
    let receipt = world.root.path().join("receipt.json");
    let mut arguments = vec![
        "--deps",
        capsule.to_str().unwrap(),
        "--receipt",
        receipt.to_str().unwrap(),
    ];
    arguments.extend(APP_INPUTS);
    let output = world.run(&arguments);
    assert!(output.status.success(), "{}", stderr(&output));
    // 8. What command ran, 9. what it did.
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "appboundry-app:started\n"
    );

    let receipt = receipt_of(&receipt);
    // 1. Which project ran; 4. which Computer.
    assert_eq!(
        receipt["project"]["binding"]["identity"]["name"],
        "appboundry-app"
    );
    assert_eq!(receipt["placement"]["provider_id"], "local");
    // 2/3. Which Stack, which version and fingerprint: the repository's file.
    let stack = &receipt["stack"];
    let identity = compute_project::find_stack(randy().to_str().unwrap(), &[])
        .unwrap()
        .identity();
    assert_eq!(stack["binding"]["identity"]["name"], "randy");
    assert_eq!(
        stack["binding"]["identity"]["version"],
        identity.version.as_str()
    );
    assert_eq!(
        stack["binding"]["identity"]["fingerprint"],
        identity.fingerprint.as_str()
    );
    assert_eq!(
        stack["binding"]["capsule_id"],
        receipt["dependencies"]["capsule_id"]
    );

    // 5/6/7. The stack's components: package artifacts, each resolved,
    // materialized, and verified by a probe on the Computer.
    let components = stack["components"].as_array().unwrap();
    let names: Vec<_> = components
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "appboundry",
            "appport-core",
            "appport-sdk",
            "appport-services",
            "authboundry",
            "feltdb"
        ]
    );
    for component in components {
        for (check, status) in [
            ("package resolved", "satisfied"),
            ("package materialized", "satisfied"),
            ("package verified", "satisfied"),
        ] {
            assert_eq!(
                gate(component, check)["verification"]["status"],
                status,
                "{component:#}"
            );
        }
    }
    let declared = &stack["binding"]["components"][0]["declared"]["source"];
    assert_eq!(declared["kind"], "package");
    assert_eq!(declared["package"], "@appport/appboundry");

    // The application is a different layer: not a stack component.
    assert!(!names.iter().any(|name| name.contains("wasm")));
    let app = &receipt["app_bundle"];
    assert_eq!(
        app["binding"]["declared"]["application"],
        "dev.appboundry.portal"
    );
    assert_eq!(
        app["binding"]["resolved"]["manifest_path"],
        "AppBoundry.app/manifest"
    );
    assert_eq!(
        app["binding"]["resolved"]["module_path"],
        "AppBoundry.app/application.wasm"
    );
    let status = |check: &str| {
        gate(app, check)["verification"]["status"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(status("manifest resolved"), "satisfied");
    assert_eq!(status("module resolved"), "satisfied");
    assert_eq!(status("manifest materialized"), "satisfied");
    assert_eq!(status("module materialized"), "satisfied");
    // The Computer's WASM runtime was observed to run a module.
    assert_eq!(status("runtime capability verified"), "satisfied");
    // What the stand-in packages cannot show, and what Compute cannot do,
    // are reported as such and never as verified.
    assert_eq!(
        status("certified by the AppBoundry platform package"),
        "not_evaluated"
    );
    assert_eq!(status("required providers available"), "not_evaluated");
    assert_eq!(status("application launch verified"), "not_evaluated");
    assert!(
        gate(app, "application launch verified")["verification"]["evidence"]
            .as_str()
            .unwrap()
            .starts_with("unavailable")
    );
    // The module in the receipt's inputs is the real AppBoundry module.
    let inputs = receipt["inputs"].as_array().unwrap();
    assert!(
        inputs
            .iter()
            .any(|input| input["path"] == "AppBoundry.app/application.wasm"
                && input["sha256"]
                    == "sha256:fef9a8b0ef770d37fcfe41f1547bff26e173e42bef767b3936ccd3ef8c2eb3ad")
    );
    assert_eq!(receipt["execution"]["status"], "completed");

    // It verifies independently, and inspect shows the layers apart.
    let path = world.root.path().join("receipt.json");
    let verify = world.compute(&["receipt", "verify", path.to_str().unwrap()]);
    assert!(verify.status.success(), "{}", stderr(&verify));
    let inspect = world.compute(&["receipt", "inspect", path.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&inspect.stdout);
    assert!(
        text.contains("Stack") && text.contains("randy") && text.contains("Application bundle"),
        "{text}"
    );
    assert!(
        text.contains("dev.appboundry.portal (verification unavailable)"),
        "{text}"
    );
}

#[test]
fn the_same_stack_is_the_same_declared_environment_on_another_computer() {
    let world = world();
    let capsule = world.capsule(None);
    let root = world.root.path();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let credential = targets::issue(root, "remote-dev", "cli");
    let mut serve = Command::new(env!("CARGO_BIN_EXE_compute"));
    runtimes::with_fixture_runtimes(&mut serve);
    struct Server(Child);
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _server = Server(
        serve
            .args([
                "serve",
                "--listen",
                &format!("127.0.0.1:{port}"),
                "--job-store",
            ])
            .arg(root.join("jobs"))
            .arg("--credentials")
            .arg(&credential.credentials)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "compute serve did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    let pool = root.join("compute-pool.toml");
    std::fs::write(
        &pool,
        format!(
            "[pool]\n[providers.local]\nkind = \"local\"\npriority = 10\n\
             [providers.remote-dev]\nkind = \"remote\"\nendpoint = \"http://127.0.0.1:{port}\"\ntoken_file = {:?}\npriority = 50\n",
            credential.token_file.display().to_string()
        ),
    )
    .unwrap();

    let mut receipts = vec![];
    for provider in ["local", "remote-dev"] {
        let path = root.join(format!("{provider}.json"));
        let mut arguments = vec![
            "--provider",
            provider,
            "--pool-config",
            pool.to_str().unwrap(),
            "--deps",
            capsule.to_str().unwrap(),
            "--receipt",
            path.to_str().unwrap(),
        ];
        arguments.extend(APP_INPUTS);
        let output = world.run(&arguments);
        assert!(output.status.success(), "{provider}: {}", stderr(&output));
        receipts.push(receipt_of(&path));
    }
    let (a, b) = (&receipts[0], &receipts[1]);
    assert_eq!(a["placement"]["provider_id"], "local");
    assert_eq!(b["placement"]["provider_id"], "remote-dev");
    assert_eq!(b["provider_protocol"], "compute.remote@1");
    // Two Computers, one declared environment.
    assert_eq!(
        a["stack"]["binding"]["identity"],
        b["stack"]["binding"]["identity"]
    );
    assert!(
        a["stack"]["binding"]["identity"]["fingerprint"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    for receipt in &receipts {
        for component in receipt["stack"]["components"].as_array().unwrap() {
            assert_eq!(
                gate(component, "package verified")["verification"]["status"],
                "satisfied"
            );
        }
        assert_eq!(
            gate(&receipt["app_bundle"], "runtime capability verified")["verification"]["status"],
            "satisfied"
        );
    }
}

#[test]
fn a_stack_that_cannot_be_realized_says_which_stage_failed() {
    let world = world();
    let capsule = world.capsule(None);
    let capsule = capsule.to_str().unwrap();

    // No stack of that name, anywhere.
    let output = world.compute(&["run", "--stack", "nope", "--deps", capsule]);
    assert!(
        stderr(&output).contains("project_discovery_failed"),
        "{}",
        stderr(&output)
    );

    // A package the stack needs, absent from the capsule.
    let partial = world.capsule(Some("@feltdb/core"));
    let mut arguments = vec!["--deps", partial.to_str().unwrap()];
    arguments.extend(APP_INPUTS);
    let output = world.run(&arguments);
    let text = stderr(&output);
    assert!(text.contains("dependency_unavailable"), "{text}");
    assert!(
        text.contains("@feltdb/core (stack:randy)") && text.contains("result: unsupported"),
        "{text}"
    );

    // The application bundle not supplied, or not the declared one.
    let output = world.run(&["--deps", capsule]);
    assert!(
        stderr(&output).contains("requirements_unresolved"),
        "{}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("dev.appboundry.portal"),
        "{}",
        stderr(&output)
    );
    let module = world.project.join("AppBoundry.app/application.wasm");
    let mut bytes = std::fs::read(&module).unwrap();
    bytes.push(0);
    std::fs::write(&module, bytes).unwrap();
    let mut arguments = vec!["--deps", capsule];
    arguments.extend(APP_INPUTS);
    let output = world.run(&arguments);
    assert!(
        stderr(&output).contains("requirements_unresolved"),
        "{}",
        stderr(&output)
    );

    // A stack that is not valid.
    let bad = world.root.path().join("bad");
    std::fs::create_dir(&bad).unwrap();
    std::fs::write(
        bad.join("stack.toml"),
        "schema = \"compute.stack@1\"\n[stack]\nname = \"bad\"\nversion = \"1\"\n",
    )
    .unwrap();
    let output = world.compute(&["run", "--stack", bad.to_str().unwrap(), "--deps", capsule]);
    assert!(
        stderr(&output).contains("stack_invalid"),
        "{}",
        stderr(&output)
    );

    // --stack without a project run is refused, not ignored.
    let output = world.compute(&["run", "main.py", "--stack", "randy"]);
    assert!(!output.status.success());
    assert!(!world.home.exists(), "nothing ran and nothing was kept");
}

#[test]
fn credentials_are_referenced_by_name_and_never_recorded() {
    let world = world();
    let capsule = world.capsule(None);
    let stack = world.root.path().join("credentialed");
    std::fs::create_dir(&stack).unwrap();
    let text = std::fs::read_to_string(randy().join("stack.toml"))
        .unwrap()
        .replace("name = \"randy\"", "name = \"credentialed\"")
        .replacen(
            "name = \"feltdb\"\nkind = \"package\"",
            "name = \"feltdb\"\nkind = \"package\"\ncredentials = [\"FELTDB_TOKEN\"]",
            1,
        );
    std::fs::write(stack.join("stack.toml"), text).unwrap();
    let base = |receipt: &Path| {
        let mut arguments: Vec<String> = vec![
            "run".into(),
            "--stack".into(),
            stack.to_string_lossy().into_owned(),
            "--deps".into(),
            capsule.to_string_lossy().into_owned(),
            "--receipt".into(),
            receipt.to_string_lossy().into_owned(),
        ];
        arguments.extend(APP_INPUTS.map(str::to_owned));
        arguments
    };
    let receipt = world.root.path().join("credentialed.json");

    // Without the credential the run does not start.
    let arguments = base(&receipt);
    let refs: Vec<&str> = arguments.iter().map(String::as_str).collect();
    let output = world.compute(&refs);
    let text = stderr(&output);
    assert!(
        text.contains("credential_unavailable") && text.contains("FELTDB_TOKEN"),
        "{text}"
    );
    assert!(!receipt.exists());

    // With it, the value reaches the workload and nowhere else.
    let secret = "s3cr3t-value-that-must-not-leak";
    let mut arguments = base(&receipt);
    arguments.extend(["--env".into(), format!("FELTDB_TOKEN={secret}")]);
    let refs: Vec<&str> = arguments.iter().map(String::as_str).collect();
    let output = world.compute(&refs);
    assert!(output.status.success(), "{}", stderr(&output));
    let written = std::fs::read_to_string(&receipt).unwrap();
    assert!(
        !written.contains(secret),
        "the receipt holds the credential's value"
    );
    assert!(written.contains("FELTDB_TOKEN"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
    assert!(!stderr(&output).contains(secret));
    let parsed: Value = serde_json::from_str(&written).unwrap();
    assert!(
        parsed["policy"]["environment_names"]
            .as_array()
            .unwrap()
            .contains(&json!("FELTDB_TOKEN"))
    );
    assert_eq!(
        parsed["stack"]["binding"]["components"][5]["declared"]["credentials"],
        json!(["FELTDB_TOKEN"])
    );
    let plan = world.compute(&[
        "run",
        "--stack",
        stack.to_str().unwrap(),
        "--deps",
        capsule.to_str().unwrap(),
        "--dry-run",
        "--json",
        "--env",
        &format!("FELTDB_TOKEN={secret}"),
    ]);
    assert!(!String::from_utf8_lossy(&plan.stdout).contains(secret));
}

#[test]
fn unsupported_components_are_refused_not_dropped() {
    let world = world();
    let capsule = world.capsule(None);
    let stack = world.root.path().join("partial");
    std::fs::create_dir(&stack).unwrap();
    let text = std::fs::read_to_string(randy().join("stack.toml"))
        .unwrap()
        .replace("name = \"randy\"", "name = \"partial\"")
        .replacen(
            "name = \"feltdb\"\nkind = \"package\"",
            "name = \"feltdb\"\nkind = \"package\"\nunsupported = \"no build for this target yet\"",
            1,
        );
    std::fs::write(stack.join("stack.toml"), text).unwrap();
    let mut arguments = vec![
        "run",
        "--stack",
        stack.to_str().unwrap(),
        "--deps",
        capsule.to_str().unwrap(),
    ];
    arguments.extend(APP_INPUTS);
    let output = world.compute(&arguments);
    let text = stderr(&output);
    assert!(text.contains("stack_component_unsupported"), "{text}");
    assert!(
        text.contains("feltdb = no build for this target yet"),
        "{text}"
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn a_project_and_workloads_without_a_stack_are_unchanged() {
    let world = world();
    let capsule = world.capsule(None);
    let receipt = world.root.path().join("plain.json");
    // No --stack, no [stack]: the same project runs as a PAX project only
    // (its [artifact] still needs to be supplied).
    let mut arguments = vec![
        "run",
        "--deps",
        capsule.to_str().unwrap(),
        "--receipt",
        receipt.to_str().unwrap(),
    ];
    arguments.extend(APP_INPUTS);
    let output = world.compute(&arguments);
    assert!(output.status.success(), "{}", stderr(&output));
    let parsed = receipt_of(&receipt);
    assert!(parsed.get("stack").is_none(), "{parsed:#}");
    assert!(parsed["project"].is_object());
    // The application bundle is the project's own declaration; it is
    // verified without any stack.
    assert_eq!(
        gate(&parsed["app_bundle"], "module materialized")["verification"]["status"],
        "satisfied"
    );
    // A plain script is untouched by all of it.
    std::fs::write(world.project.join("main.py"), "print('plain')\n").unwrap();
    let output = world.compute(&["run", "main.py", "--network", "network"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "plain\n");
}

/// With a real npm install of the Randy packages:
/// `RANDY_NODE_MODULES=/path/to/node_modules cargo test -p compute-cli
/// --test stack_run -- --ignored`. The packages are large, so the capsule is
/// referenced from the target's dependency cache.
#[test]
#[ignore = "needs the real packages: set RANDY_NODE_MODULES"]
fn real_randy_stack_certifies_the_real_appboundry_application() {
    let node_modules =
        PathBuf::from(std::env::var("RANDY_NODE_MODULES").expect("RANDY_NODE_MODULES"));
    let world = world();
    let resolved = world.root.path().join("real");
    copy_dir(&node_modules, &resolved.join("node_modules"));
    let packages: Vec<String> = randy_packages()
        .iter()
        .map(|(n, v)| format!("{n}={v}"))
        .collect();
    let capsule = world.build_capsule(&resolved, &packages, "real");
    let id = String::from_utf8(
        world
            .compute(&["deps", "inspect", capsule.to_str().unwrap(), "--json"])
            .stdout,
    )
    .unwrap();
    let id = serde_json::from_str::<Value>(&id).unwrap()["capsule_id"]
        .as_str()
        .unwrap()
        .trim_start_matches("sha256:")
        .to_owned();
    let cache = world.root.path().join("cache");
    std::fs::create_dir(&cache).unwrap();
    std::fs::copy(&capsule, cache.join(format!("{id}.deps"))).unwrap();
    let receipt = world.root.path().join("real-receipt.json");
    let stack = randy();
    let mut command = Command::new(env!("CARGO_BIN_EXE_compute"));
    runtimes::with_fixture_runtimes(&mut command);
    let mut arguments = vec![
        "run",
        "--stack",
        stack.to_str().unwrap(),
        "--deps",
        capsule.to_str().unwrap(),
        "--deps-by-reference",
        "--receipt",
        receipt.to_str().unwrap(),
    ];
    arguments.extend(APP_INPUTS);
    let output = command
        .current_dir(&world.project)
        .env("COMPUTE_HOME", &world.home)
        .env("COMPUTE_PAX", &world.pax)
        .env("COMPUTE_DEPENDENCY_CACHE", &cache)
        .env("COMPUTE_DAEMON", "http://127.0.0.1:9")
        .args(arguments)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let parsed = receipt_of(&receipt);
    for component in parsed["stack"]["components"].as_array().unwrap() {
        assert_eq!(
            gate(component, "package verified")["verification"]["status"],
            "satisfied"
        );
    }
    let app = &parsed["app_bundle"];
    // AppBoundry's own API, running in the materialized environment,
    // certified the real artifact.
    assert_eq!(
        gate(app, "certified by the AppBoundry platform package")["verification"]["status"],
        "satisfied"
    );
    assert_eq!(
        gate(app, "runtime capability verified")["verification"]["status"],
        "satisfied"
    );
    // And it reported honestly that Compute establishes none of the
    // application's providers and cannot launch it.
    assert_eq!(
        gate(app, "required providers available")["verification"]["status"],
        "not_evaluated"
    );
    assert!(
        gate(app, "required providers available")["verification"]["evidence"]
            .as_str()
            .unwrap()
            .contains("feltdb.documents@1")
    );
    assert_eq!(
        gate(app, "application launch verified")["verification"]["status"],
        "not_evaluated"
    );
}

struct Launched<'a> {
    world: &'a World,
    listen: String,
    target: String,
}

impl Launched<'_> {
    fn command(&self, arguments: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_compute"));
        runtimes::with_fixture_runtimes(&mut command);
        command
            // Not the project: `compute up` reads ./compute.toml as control
            // plane configuration, whose `[network]` is not a project's.
            .current_dir(self.world.root.path())
            .args(arguments)
            .env("COMPUTE_HOME", &self.world.home)
            .env("COMPUTE_PAX", &self.world.pax)
            .env("COMPUTE_LISTEN", &self.listen)
            .env("COMPUTE_TARGET_LISTEN", &self.target)
            .env("COMPUTE_DAEMON", format!("http://{}", self.listen))
            .env("COMPUTE_NO_BROWSER", "1")
            .env_remove("COMPUTE_CONFIG")
            .env_remove("COMPUTE_POOL_CONFIG")
            .env_remove("COMPUTE_DAEMON_TOKEN")
            .output()
            .unwrap()
    }
}

impl Drop for Launched<'_> {
    fn drop(&mut self) {
        let _ = self.command(&["down"]);
    }
}

#[test]
fn compute_up_configures_a_computer_with_the_stack_and_reports_what_is_true() {
    let world = world();
    let port = || {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    };
    let launched = Launched {
        world: &world,
        listen: format!("127.0.0.1:{}", port()),
        target: format!("127.0.0.1:{}", port()),
    };
    let stack = randy();

    // A capsule that lacks a package: nothing is placed, nothing is claimed.
    let partial = world.capsule(Some("@authboundry/core"));
    let output = launched.command(&[
        "up",
        "--stack",
        stack.to_str().unwrap(),
        "--deps",
        partial.to_str().unwrap(),
    ]);
    let said = String::from_utf8_lossy(&output.stdout);
    assert!(!output.status.success(), "{said}{}", stderr(&output));
    assert!(
        said.contains("Status: not ready"),
        "{said}{}",
        stderr(&output)
    );
    assert!(said.contains("Target: none (nothing was placed)"), "{said}");
    assert!(
        said.contains("authboundry") && said.contains("declared"),
        "{said}"
    );
    assert!(!said.contains('✓'), "nothing may be shown verified: {said}");

    // The full set of pinned packages: the Computer is configured, and each
    // component is verified by a probe that ran on it.
    let capsule = world.capsule(None);
    let output = launched.command(&[
        "up",
        "--stack",
        stack.to_str().unwrap(),
        "--deps",
        capsule.to_str().unwrap(),
    ]);
    let said = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{said}{}", stderr(&output));
    let identity = compute_project::find_stack(stack.to_str().unwrap(), &[])
        .unwrap()
        .identity();
    assert!(
        said.contains(&format!(
            "Stack: randy {} ({})",
            identity.version, identity.fingerprint
        )),
        "{said}"
    );
    assert!(said.contains("Target: this-machine"), "{said}");
    assert!(said.contains("Status: ready for applications"), "{said}");
    for name in [
        "appboundry",
        "appport-core",
        "appport-sdk",
        "appport-services",
        "authboundry",
        "feltdb",
    ] {
        assert!(said.contains(&format!("✓ {name}")), "{name}: {said}");
    }
    assert!(said.contains("verified"), "{said}");

    // An application then runs on that Computer, against the same stack.
    let receipt = world.root.path().join("app.json");
    let pool = world.home.join("pool.toml");
    let project = world.project.to_string_lossy().into_owned();
    let mut arguments = vec![
        "run",
        "--project",
        project.as_str(),
        "--stack",
        stack.to_str().unwrap(),
        "--provider",
        "this-machine",
        "--pool-config",
        pool.to_str().unwrap(),
        "--deps",
        capsule.to_str().unwrap(),
        "--receipt",
        receipt.to_str().unwrap(),
    ];
    arguments.extend(APP_INPUTS);
    let output = launched.command(&arguments);
    assert!(output.status.success(), "{}", stderr(&output));
    let receipt = receipt_of(&receipt);
    assert_eq!(receipt["placement"]["provider_id"], "this-machine");
    assert_eq!(
        receipt["stack"]["binding"]["identity"]["fingerprint"],
        identity.fingerprint.as_str()
    );
}
