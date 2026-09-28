//! A PAX project through the whole Compute chain: discovery → requirements →
//! planning → placement → materialization → execution → receipt.
//!
//! PAX is an external tool observed through its versioned JSON output. These
//! tests stand in for `pax` with a script that prints the documents the real
//! `pax --json info|deps|scripts` prints for `tests/fixtures/pax-project`;
//! `real_pax_observes_the_fixture` runs the real executable when `PAX_BIN`
//! names one.

#[path = "support/runtimes.rs"]
mod runtimes;
#[path = "support/targets.rs"]
mod targets;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pax-project")
}

fn copy_fixture(to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(fixture()).unwrap().flatten() {
        std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
    }
}

/// What `pax --json` prints for the fixture.
fn documents() -> [Value; 3] {
    let project = json!({"root": "/fixture", "name": "pax-fixture", "packageJson": true, "workspace": false, "workspaceSource": null});
    let manager = json!({"name": "npm", "version": null, "lockfile": "package-lock.json", "selectedBy": "lockfile precedence"});
    [
        json!({
            "schemaVersion": "1", "command": "info", "project": project, "manager": manager,
            "result": {"summary": "Detected npm package reality for pax-fixture", "runtime": null},
            "ecosystem": "javascript",
            "components": [{"path": ".", "ecosystem": "javascript", "tool": "npm",
                "manifests": ["package.json"], "lockfiles": ["package-lock.json"], "evidence": [],
                "workspacePackages": [], "dependencySources": []}],
            "nativeDependencies": [], "container": null
        }),
        json!({
            "schemaVersion": "1", "command": "deps", "project": project, "manager": manager,
            "dependencies": {
                "dependencies": {"left-pad": "1.3.0"},
                "devDependencies": {"typescript": "^5.0.0"},
                "optionalDependencies": {}, "peerDependencies": {}, "nativeDependencies": []
            }
        }),
        json!({
            "schemaVersion": "1", "command": "scripts", "project": project, "manager": manager,
            "scripts": {
                "start": "node index.js --greeting hello",
                "check": "node check.js",
                "build": "tsc -p ."
            }
        }),
    ]
}

/// A `pax` that answers `--json --dir <root> <command>` from `documents`.
#[cfg(unix)]
fn install_pax(dir: &Path, documents: &[Value; 3], exit: Option<i32>) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let mut script = String::from("#!/bin/sh\n");
    match exit {
        Some(code) => script.push_str(&format!(
            "echo 'failed to parse package.json' >&2\nexit {code}\n"
        )),
        None => {
            for document in documents {
                let command = document["command"].as_str().unwrap();
                script.push_str(&format!(
                    "if [ \"$4\" = {command} ]; then cat <<'JSON'\n{document}\nJSON\nexit 0; fi\n"
                ));
            }
            script.push_str("exit 2\n");
        }
    }
    let path = dir.join(format!(
        "pax-{}",
        documents[0]["project"]["name"].as_str().unwrap()
    ));
    let path = if path.exists() {
        dir.join("pax-alt")
    } else {
        path
    };
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

struct Setup {
    root: tempfile::TempDir,
    project: PathBuf,
    home: PathBuf,
    pax: PathBuf,
}

fn setup() -> Setup {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    copy_fixture(&project);
    let pax = install_pax(root.path(), &documents(), None);
    Setup {
        home: root.path().join("home"),
        project,
        pax,
        root,
    }
}

impl Setup {
    fn compute(&self, cwd: &Path, pax: &Path, arguments: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_compute"));
        runtimes::with_fixture_runtimes(&mut command);
        command
            .current_dir(cwd)
            .env("COMPUTE_HOME", &self.home)
            .env("COMPUTE_PAX", pax)
            .env("COMPUTE_DAEMON", "http://127.0.0.1:9")
            .env_remove("COMPUTE_POOL_CONFIG")
            .env_remove("COMPUTE_DAEMON_TOKEN")
            .args(arguments)
            .output()
            .unwrap()
    }

    fn run(&self, arguments: &[&str]) -> Output {
        self.compute(&self.project, &self.pax, arguments)
    }

    /// The dependency capsule the project's package manager would have
    /// produced, packaged the documented way (`compute deps create`).
    fn capsule(&self) -> PathBuf {
        let resolved = self.root.path().join("resolved");
        std::fs::create_dir_all(resolved.join("node_modules/left-pad")).unwrap();
        std::fs::write(
            resolved.join("node_modules/left-pad/index.js"),
            "module.exports = 1\n",
        )
        .unwrap();
        // Bound to the runtime version the targets in these tests offer.
        let catalog = self.compute(self.root.path(), &self.pax, &["runtimes", "--json"]);
        let catalog = stdout_json(&catalog);
        let version = catalog["runtimes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|runtime| runtime["runtime"] == "node")
            .expect("the fixture catalog offers node")["version"]
            .as_str()
            .unwrap()
            .to_owned();
        let capsule = self.root.path().join("node.deps");
        let output = self.compute(
            self.root.path(),
            &self.pax,
            &[
                "deps",
                "create",
                "--runtime",
                "node",
                "--runtime-version",
                &version,
                "--resolved",
                resolved.to_str().unwrap(),
                "--package",
                "left-pad=1.3.0",
                "--output",
                capsule.to_str().unwrap(),
            ],
        );
        assert!(output.status.success(), "{output:?}");
        capsule
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{error}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            stderr(output)
        )
    })
}

fn receipt_of(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[test]
fn compute_run_discovers_places_materializes_executes_and_proves_a_pax_project() {
    let setup = setup();
    let capsule = setup.capsule();
    let receipt = setup.root.path().join("receipt.json");

    // `cd my-project && compute run`
    let output = setup.run(&[
        "run",
        "--deps",
        capsule.to_str().unwrap(),
        "--receipt",
        receipt.to_str().unwrap(),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stderr(&output);
    // The project's `start` command was selected: index.js with its arguments.
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "pax-fixture:--greeting,hello\n"
    );
    assert!(text.contains("Selected: local"), "{text}");
    assert!(text.contains("Project: pax-fixture (pax)"), "{text}");

    let receipt = receipt_of(&receipt);
    // Which project, which requirements, which target, which runtime.
    let project = &receipt["project"];
    assert_eq!(project["binding"]["identity"]["name"], "pax-fixture");
    assert_eq!(project["binding"]["identity"]["source"], "pax");
    assert!(
        project["binding"]["requirements_id"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert_eq!(receipt["placement"]["provider_id"], "local");
    assert_eq!(receipt["provider_protocol"], "compute.local@1");
    assert_eq!(receipt["runtime"]["observed"], "node");

    // Declared → resolved → verified → executed, kept apart.
    let declared = &project["binding"]["declared"];
    assert_eq!(declared["runtimes"][0]["kind"], "node");
    assert_eq!(declared["tools"][0]["name"], "npm");
    assert_eq!(declared["dependency_count"], 1);
    let resolved = &project["binding"]["resolved"];
    assert_eq!(resolved["runtime"], "node");
    assert_eq!(resolved["command"]["name"], "start");
    assert_eq!(resolved["command"]["entrypoint"], "index.js");
    assert_eq!(
        resolved["capsule_id"],
        receipt["dependencies"]["capsule_id"]
    );
    let verified = &project["verified"];
    assert_eq!(verified["runtime"]["status"], "satisfied");
    assert_eq!(verified["dependencies"]["status"], "satisfied");
    assert_eq!(verified["command"]["status"], "satisfied");
    assert_eq!(verified["platform"]["status"], "not_required");
    // The package manager built the environment; nothing observed it, and
    // the receipt does not claim it did.
    assert_eq!(
        verified["tools"][0]["verification"]["status"],
        "not_evaluated"
    );
    assert_eq!(receipt["request"]["entrypoint"], "index.js");
    assert_eq!(receipt["execution"]["status"], "completed");

    // The receipt verifies independently and names the project.
    let path = setup.root.path().join("receipt.json");
    let verify = setup.compute(
        setup.root.path(),
        &setup.pax,
        &["receipt", "verify", path.to_str().unwrap()],
    );
    assert!(verify.status.success(), "{}", stderr(&verify));
    let inspect = setup.compute(
        setup.root.path(),
        &setup.pax,
        &["receipt", "inspect", path.to_str().unwrap()],
    );
    let inspected = String::from_utf8_lossy(&inspect.stdout);
    assert!(inspected.contains("Project"), "{inspected}");
    assert!(inspected.contains("pax-fixture (pax)"), "{inspected}");
    assert!(inspected.contains("Satisfied"), "{inspected}");
    assert!(inspected.contains("NotEvaluated"), "{inspected}");

    // Nothing was kept: no Compute home, and the project is untouched.
    assert!(!setup.home.exists());
}

#[test]
fn a_project_command_is_selected_by_name_and_an_explicit_project_needs_no_cwd() {
    let setup = setup();
    let capsule = setup.capsule();
    let receipt = setup.root.path().join("check.json");
    let elsewhere = setup.root.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    let output = setup.compute(
        &elsewhere,
        &setup.pax,
        &[
            "run",
            "--project",
            setup.project.to_str().unwrap(),
            "--command",
            "check",
            "--deps",
            capsule.to_str().unwrap(),
            "--receipt",
            receipt.to_str().unwrap(),
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "pax-fixture:check\n"
    );
    let receipt = receipt_of(&receipt);
    assert_eq!(
        receipt["project"]["binding"]["resolved"]["command"]["name"],
        "check"
    );
    assert_eq!(receipt["request"]["entrypoint"], "check.js");

    // Arguments after `--` follow the project command's own.
    let receipt = setup.root.path().join("args.json");
    let output = setup.run(&[
        "run",
        "--deps",
        capsule.to_str().unwrap(),
        "--receipt",
        receipt.to_str().unwrap(),
        "--",
        "extra",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "pax-fixture:--greeting,hello,extra\n"
    );
    assert_eq!(
        receipt_of(&receipt)["project"]["binding"]["resolved"]["command"]["argument_count"],
        3
    );
}

#[test]
fn a_dry_run_plans_and_places_a_project_without_executing_it() {
    let setup = setup();
    let capsule = setup.capsule();
    let output = setup.run(&[
        "run",
        "--dry-run",
        "--json",
        "--deps",
        capsule.to_str().unwrap(),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let report = stdout_json(&output);
    assert_eq!(report["outcome"], "placed");
    assert_eq!(report["requirements"]["runtime"]["kind"], "node");
    assert_eq!(report["requirements"]["dependencies"]["embedded"], true);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("pax-fixture:"));
}

#[test]
fn project_failures_name_the_stage_that_failed() {
    let setup = setup();
    let capsule = setup.capsule();
    let capsule = capsule.to_str().unwrap();

    // Discovery: nothing PAX recognizes.
    let empty = setup.root.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    let mut nothing = documents();
    for document in &mut nothing {
        document["project"]["name"] = json!("empty");
    }
    nothing[0]["ecosystem"] = Value::Null;
    nothing[0]["components"] = json!([]);
    let pax_empty = install_pax(setup.root.path(), &nothing, None);
    let output = setup.compute(&empty, &pax_empty, &["run"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("project_discovery_failed"),
        "{}",
        stderr(&output)
    );

    // Discovery: no PAX at all.
    let output = setup.compute(&setup.project, &setup.root.path().join("no-pax"), &["run"]);
    assert!(
        stderr(&output).contains("project_discovery_failed"),
        "{}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("COMPUTE_PAX"),
        "{}",
        stderr(&output)
    );

    // Metadata: PAX cannot parse the project.
    let broken = install_pax(
        setup.root.path(),
        &[
            json!({"project": {"name": "broken"}, "command": "info"}),
            json!({"command": "deps"}),
            json!({"command": "scripts"}),
        ],
        Some(1),
    );
    let output = setup.compute(&setup.project, &broken, &["run", "--deps", capsule]);
    assert!(
        stderr(&output).contains("pax_metadata_invalid"),
        "{}",
        stderr(&output)
    );

    // Metadata: an unknown schema.
    let mut future = documents();
    future[0]["schemaVersion"] = json!("7");
    let pax_future = install_pax(setup.root.path(), &future, None);
    let output = setup.compute(&setup.project, &pax_future, &["run", "--deps", capsule]);
    let text = stderr(&output);
    assert!(text.contains("pax_metadata_invalid"), "{text}");
    assert!(
        text.contains("schemaVersion = 7") && text.contains("result: unsupported"),
        "{text}"
    );

    // Requirements: a command Compute cannot run, and one that does not exist.
    let output = setup.run(&["run", "--command", "build", "--deps", capsule]);
    assert!(
        stderr(&output).contains("requirements_unresolved"),
        "{}",
        stderr(&output)
    );
    assert!(stderr(&output).contains("tsc"), "{}", stderr(&output));
    let output = setup.run(&["run", "--command", "missing", "--deps", capsule]);
    let text = stderr(&output);
    assert!(
        text.contains("requirements_unresolved") && text.contains("build, check, start"),
        "{text}"
    );

    // Materialization: the project needs a dependency and no capsule exists.
    let output = setup.run(&["run"]);
    let text = stderr(&output);
    assert!(text.contains("dependency_unavailable"), "{text}");
    assert!(
        text.contains("left-pad = 1.3.0") && text.contains("result: unsupported"),
        "{text}"
    );

    // Materialization: a runtime that contradicts the project.
    let output = setup.run(&["run", "--runtime", "python"]);
    let text = stderr(&output);
    assert!(text.contains("requirements_unresolved"), "{text}");
    assert!(
        text.contains("runtime = node") && text.contains("runtime = python"),
        "{text}"
    );

    // None of these executed anything or left state behind.
    assert!(!setup.home.exists());
}

#[test]
fn no_target_satisfying_the_project_is_explicit_evidence_not_a_fallback() {
    let setup = setup();
    let capsule = setup.capsule();
    // The project needs an architecture no target has.
    std::fs::write(
        setup.project.join("compute.toml"),
        "[runtime]\narchitecture = \"riscv64\"\n\n[network]\nmode = \"network\"\n",
    )
    .unwrap();
    let receipt = setup.root.path().join("never.json");
    let output = setup.run(&[
        "run",
        "--deps",
        capsule.to_str().unwrap(),
        "--receipt",
        receipt.to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    let text = stderr(&output);
    assert!(text.contains("no_target_satisfies_requirements"), "{text}");
    assert!(
        text.contains("required:") && text.contains("architecture = riscv64"),
        "{text}"
    );
    assert!(text.contains("runtime = node"), "{text}");
    assert!(
        text.contains("target local") && text.contains("architecture_mismatch"),
        "{text}"
    );
    assert!(text.contains("result: unsupported"), "{text}");
    assert!(!output.status.success() && output.stdout.is_empty());
    assert!(!receipt.exists(), "nothing executed, so no receipt");

    let output = setup.run(&["run", "--json", "--deps", capsule.to_str().unwrap()]);
    let value = stdout_json(&output);
    assert_eq!(value["placement"]["outcome"], "placement_failed");
    assert_eq!(
        value["project_failure"]["code"],
        "no_target_satisfies_requirements"
    );
    assert_eq!(value["project_failure"]["result"], "unsupported");
    assert_eq!(
        value["project_failure"]["required"]["architecture"],
        "riscv64"
    );
}

#[test]
fn non_pax_workloads_keep_their_existing_behavior() {
    let setup = setup();
    let work = setup.root.path().join("plain");
    std::fs::create_dir(&work).unwrap();
    std::fs::write(work.join("main.py"), "print('plain')\n").unwrap();
    let receipt = setup.root.path().join("plain.json");
    // No PAX is even reachable: a path argument never consults it.
    let output = setup.compute(
        &work,
        &setup.root.path().join("no-pax"),
        &[
            "run",
            "main.py",
            "--network",
            "network",
            "--receipt",
            receipt.to_str().unwrap(),
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "plain\n");
    let evidence = receipt_of(&receipt);
    assert!(evidence.get("project").is_none(), "{evidence:#}");
    assert_eq!(evidence["runtime"]["observed"], "python");

    // Even the project directory itself, addressed as a path, is the
    // directory workload it always was: PAX is not consulted.
    let output = setup.compute(
        setup.root.path(),
        &setup.root.path().join("no-pax"),
        &[
            "run",
            setup.project.to_str().unwrap(),
            "--dry-run",
            "--json",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout_json(&output).get("project_failure").is_none());
}

#[cfg(unix)]
mod remote {
    use super::*;

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    struct Server(Child);
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn the_same_project_runs_on_a_remote_target_with_the_same_evidence() {
        let setup = setup();
        let capsule = setup.capsule();
        let root = setup.root.path();
        let port = free_port();
        let credential = targets::issue(root, "remote-dev", "cli");
        let mut server = Command::new(env!("CARGO_BIN_EXE_compute"));
        runtimes::with_fixture_runtimes(&mut server);
        let server = Server(
            server
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
                 [providers.remote-dev]\nkind = \"remote\"\nendpoint = \"http://127.0.0.1:{port}\"\n\
                 token_file = {:?}\npriority = 50\n",
                credential.token_file.display().to_string()
            ),
        )
        .unwrap();
        let receipt = root.join("remote.json");
        let output = setup.run(&[
            "run",
            "--provider",
            "remote-dev",
            "--pool-config",
            pool.to_str().unwrap(),
            "--deps",
            capsule.to_str().unwrap(),
            "--receipt",
            receipt.to_str().unwrap(),
        ]);
        assert!(output.status.success(), "{}", stderr(&output));
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "pax-fixture:--greeting,hello\n"
        );

        let receipt = receipt_of(&receipt);
        assert_eq!(receipt["placement"]["provider_id"], "remote-dev");
        assert_eq!(receipt["provider_protocol"], "compute.remote@1");
        assert_eq!(
            receipt["project"]["binding"]["identity"]["name"],
            "pax-fixture"
        );
        assert_eq!(
            receipt["project"]["verified"]["runtime"]["status"],
            "satisfied"
        );
        assert_eq!(
            receipt["project"]["verified"]["dependencies"]["status"],
            "satisfied"
        );
        assert_eq!(
            receipt["project"]["verified"]["command"]["status"],
            "satisfied"
        );

        // A remote target that cannot satisfy the project is rejected the
        // same way, with the reasons the target reported.
        std::fs::write(
            setup.project.join("compute.toml"),
            "[runtime]\narchitecture = \"riscv64\"\n\n[network]\nmode = \"network\"\n",
        )
        .unwrap();
        let output = setup.run(&[
            "run",
            "--provider",
            "remote-dev",
            "--pool-config",
            pool.to_str().unwrap(),
            "--deps",
            capsule.to_str().unwrap(),
        ]);
        let text = stderr(&output);
        assert!(!output.status.success());
        assert!(
            text.contains("target remote-dev") && text.contains("architecture_mismatch"),
            "{text}"
        );
        drop(server);
    }
}

/// With a real `pax` (`PAX_BIN=/path/to/pax cargo test -- --ignored`).
#[test]
#[ignore = "needs a real pax executable: set PAX_BIN"]
fn real_pax_observes_the_fixture() {
    let pax = PathBuf::from(std::env::var("PAX_BIN").expect("PAX_BIN"));
    let setup = setup();
    let capsule = setup.capsule();
    let receipt = setup.root.path().join("real.json");
    let output = setup.compute(
        &setup.project,
        &pax,
        &[
            "run",
            "--deps",
            capsule.to_str().unwrap(),
            "--receipt",
            receipt.to_str().unwrap(),
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "pax-fixture:--greeting,hello\n"
    );
    let receipt = receipt_of(&receipt);
    assert_eq!(
        receipt["project"]["binding"]["declared"]["tools"][0]["name"],
        "npm"
    );
    assert_eq!(
        receipt["project"]["binding"]["declared"]["dependency_count"],
        1
    );
}
