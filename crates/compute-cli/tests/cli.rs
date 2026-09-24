use assert_cmd::Command;
use predicates::prelude::*;
use sha2::{Digest, Sha256};

fn write_json(path: &std::path::Path, value: serde_json::Value) {
    std::fs::write(path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn fixture_distribution_lock(
    artifact: &[u8],
    platform: &str,
    url: &std::path::Path,
) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 2,
        "runtimes": {
            "fixture": {
                "version": "1.2.3",
                "executable": "runtimes/fixture/bin/fixture",
                "artifacts": {
                    (platform): {
                        "url": format!("file://{}", url.display()),
                        "sha256": sha256(artifact),
                        "format": "file",
                        "install": [{
                            "source": "artifact",
                            "destination": "runtimes/fixture/bin/fixture"
                        }]
                    }
                }
            },
            "wasm": { "version": "embedded", "executable": "<embedded>" }
        }
    })
}

#[test]
fn distribution_build_is_reproducible_and_verify_detects_tampering() {
    let temporary = tempfile::tempdir().unwrap();
    let artifact = b"#!/bin/sh\necho fixture 1.2.3\n";
    let artifact_path = temporary.path().join("fixture");
    std::fs::write(&artifact_path, artifact).unwrap();
    let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let lock_path = temporary.path().join("runtime-lock.json");
    write_json(
        &lock_path,
        fixture_distribution_lock(artifact, &platform, &artifact_path),
    );
    let cache = temporary.path().join("cache/sha256");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join(sha256(artifact)), artifact).unwrap();

    let first = temporary.path().join("first");
    let second = temporary.path().join("second");
    for output in [&first, &second] {
        Command::cargo_bin("compute")
            .unwrap()
            .args([
                "distribution",
                "build",
                "--offline",
                "--output",
                output.to_str().unwrap(),
                "--cache",
                temporary.path().join("cache").to_str().unwrap(),
                "--lock",
                lock_path.to_str().unwrap(),
            ])
            .assert()
            .success();
    }
    assert_eq!(
        std::fs::read(first.join("runtime-manifest.json")).unwrap(),
        std::fs::read(second.join("runtime-manifest.json")).unwrap()
    );
    assert_eq!(
        std::fs::read(format!("{}.tar", first.display())).unwrap(),
        std::fs::read(format!("{}.tar", second.display())).unwrap()
    );
    Command::cargo_bin("compute")
        .unwrap()
        .args(["distribution", "verify", first.to_str().unwrap(), "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"passed\": true"));

    std::fs::write(first.join("runtimes/fixture/bin/fixture"), b"tampered").unwrap();
    Command::cargo_bin("compute")
        .unwrap()
        .args(["distribution", "verify", first.to_str().unwrap(), "--json"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("payload hash mismatch"));

    std::fs::remove_file(second.join("runtimes/fixture/bin/fixture")).unwrap();
    Command::cargo_bin("compute")
        .unwrap()
        .args(["distribution", "verify", second.to_str().unwrap(), "--json"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("runtime executable is missing"));

    let manifest_path = second.join("runtime-manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    manifest["runtimes"]["fixture"]["version"] = "9.9.9".into();
    write_json(&manifest_path, manifest);
    Command::cargo_bin("compute")
        .unwrap()
        .args(["distribution", "verify", second.to_str().unwrap(), "--json"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("manifest/lock"));
}

#[test]
fn distribution_offline_cache_miss_and_wrong_platform_fail_explicitly() {
    let temporary = tempfile::tempdir().unwrap();
    let artifact = b"#!/bin/sh\necho fixture 1.2.3\n";
    let artifact_path = temporary.path().join("fixture");
    std::fs::write(&artifact_path, artifact).unwrap();
    let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let lock_path = temporary.path().join("runtime-lock.json");
    write_json(
        &lock_path,
        fixture_distribution_lock(artifact, &platform, &artifact_path),
    );
    Command::cargo_bin("compute")
        .unwrap()
        .args([
            "distribution",
            "build",
            "--offline",
            "--output",
            temporary.path().join("miss").to_str().unwrap(),
            "--cache",
            temporary.path().join("empty-cache").to_str().unwrap(),
            "--lock",
            lock_path.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("offline cache miss"));

    Command::cargo_bin("compute")
        .unwrap()
        .args([
            "distribution",
            "build",
            "--output",
            temporary.path().join("wrong").to_str().unwrap(),
            "--platform",
            "unsupported-architecture",
            "--cache",
            temporary.path().join("cache").to_str().unwrap(),
            "--lock",
            lock_path.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unsupported platform"));

    let wrong_runtime = b"#!/bin/sh\necho fixture 9.9.9\n";
    let wrong_digest = sha256(wrong_runtime);
    std::fs::create_dir_all(temporary.path().join("wrong-cache/sha256")).unwrap();
    std::fs::write(
        temporary
            .path()
            .join("wrong-cache/sha256")
            .join(&wrong_digest),
        wrong_runtime,
    )
    .unwrap();
    let wrong_path = temporary.path().join("wrong-runtime");
    std::fs::write(&wrong_path, wrong_runtime).unwrap();
    let wrong_lock = temporary.path().join("wrong-lock.json");
    write_json(
        &wrong_lock,
        fixture_distribution_lock(wrong_runtime, &platform, &wrong_path),
    );
    Command::cargo_bin("compute")
        .unwrap()
        .args([
            "distribution",
            "build",
            "--offline",
            "--output",
            temporary.path().join("wrong-version").to_str().unwrap(),
            "--cache",
            temporary.path().join("wrong-cache").to_str().unwrap(),
            "--lock",
            wrong_lock.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("version mismatch"));
}

#[test]
fn inspect_json_reports_ambiguous_js_runtime() {
    let mut command = Command::cargo_bin("compute").unwrap();
    command
        .args(["inspect", "hello.js", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"ambiguous\": true"))
        .stdout(predicate::str::contains("\"node\""))
        .stdout(predicate::str::contains("\"bun\""))
        .stdout(predicate::str::contains("\"deno\""));
}

#[test]
fn version_json_is_machine_readable() {
    let mut command = Command::cargo_bin("compute").unwrap();
    command
        .args(["version", "--json"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("\"name\":\"compute\"")
                .or(predicate::str::contains("\"name\": ")),
        );
}

#[test]
fn runtimes_json_lists_wasm() {
    let mut command = Command::cargo_bin("compute").unwrap();
    let output = command.args(["runtimes", "--json"]).output().unwrap();
    assert!(output.status.success());
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let runtimes = document["runtimes"].as_array().unwrap();
    assert_eq!(runtimes.len(), 11);
    assert!(runtimes.iter().any(|runtime| {
        runtime["id"] == "wasm" && runtime["version"].is_string() && runtime["available"] == true
    }));
}

#[test]
fn exec_accepts_issue_description() {
    let mut command = Command::cargo_bin("compute").unwrap();
    command
        .args(["exec", "harden", "execution", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"status\":\"accepted\""));
}

#[test]
fn doctor_json_reports_each_runtime_capability_model() {
    let mut command = Command::cargo_bin("compute").unwrap();
    let output = command.args(["doctor", "--json"]).output().unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let runtimes = document["runtimes"].as_array().unwrap();
    assert_eq!(runtimes.len(), 11);
    assert!(runtimes.iter().all(|runtime| {
        runtime["descriptor"]["version"].is_string()
            && runtime["availability"]["source"].is_string()
            && runtime["capabilities"]["stdin"]["supported"].is_boolean()
            && runtime["capabilities"]["network"].is_object()
    }));
}

#[test]
fn certify_refuses_to_certify_the_source_tree() {
    let mut command = Command::cargo_bin("compute").unwrap();
    command
        .args(["certify", "--json"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "requires an assembled distribution",
        ));
}

#[test]
fn certification_report_detects_a_broken_assembled_distribution() {
    let root = tempfile::tempdir().unwrap();
    let lock: serde_json::Value =
        serde_json::from_str(include_str!("../../../distribution/runtime-lock.json")).unwrap();
    write_json(&root.path().join("runtime-lock.json"), lock.clone());
    write_json(
        &root.path().join("runtime-manifest.json"),
        serde_json::json!({
            "compute_version": env!("CARGO_PKG_VERSION"),
            "distribution_version": format!(
                "compute-{}-{}-{}",
                env!("CARGO_PKG_VERSION"),
                std::env::consts::OS,
                std::env::consts::ARCH
            ),
            "platform": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
            "runtimes": lock["runtimes"],
        }),
    );
    std::fs::create_dir_all(root.path().join("certification")).unwrap();
    let runtime_fixtures = compute_core::RuntimeKind::ALL
        .iter()
        .map(|runtime| {
            (
                runtime.as_str().to_string(),
                serde_json::json!({ "entrypoint": format!("{}/missing", runtime.as_str()) }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    write_json(
        &root.path().join("certification/fixtures.json"),
        serde_json::json!({
            "schema_version": 1,
            "appport_runner": "appport/missing.js",
            "runtimes": runtime_fixtures,
        }),
    );

    let output = Command::cargo_bin("compute")
        .unwrap()
        .env("COMPUTE_HOME", root.path())
        .args(["certify", "--internal-clean-environment", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["passed"], false);
    assert!(
        report["runtimes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|runtime| runtime["result"] == "fail")
    );
}

#[test]
fn run_and_exec_share_execution_semantics() {
    let temp = tempfile::tempdir().unwrap();
    let invalid = temp.path().join("invalid.wasm");
    std::fs::write(&invalid, b"not wasm").unwrap();
    let path = invalid.to_string_lossy();

    let run = Command::cargo_bin("compute")
        .unwrap()
        .args(["run", &path, "--network", "none", "--json"])
        .output()
        .unwrap();
    let exec = Command::cargo_bin("compute")
        .unwrap()
        .args(["exec", &path, "--network", "none", "--json"])
        .output()
        .unwrap();
    let run: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();
    let exec: serde_json::Value = serde_json::from_slice(&exec.stdout).unwrap();

    for field in ["runtime", "network", "lifecycle", "status", "exit_code"] {
        assert_eq!(run[field], exec[field], "field {field}");
    }
    assert_eq!(run["error"]["kind"], exec["error"]["kind"]);
    assert_eq!(run["error"]["phase"], exec["error"]["phase"]);
}

#[test]
fn python_workload_spec_materializes_inputs_and_reports_outputs() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("main.py"),
        r#"import json, os, sys
value = open(os.path.join(os.environ["COMPUTE_WORK_DIR"], "nested/input.txt")).read()
open(os.path.join(os.environ["COMPUTE_OUTPUT_DIR"], "result.json"), "w").write(json.dumps({"value": value}))
print(json.dumps({"args": sys.argv[1:], "mode": os.environ.get("MODE"), "host": os.environ.get("COMPUTE_HOST_SECRET"), "input": value}))
"#,
    )
    .unwrap();
    std::fs::write(temp.path().join("payload.txt"), "portable input").unwrap();
    let workload = temp.path().join("workload.json");
    write_json(
        &workload,
        serde_json::json!({
            "version": "1",
            "runtime": "python",
            "entrypoint": "main.py",
            "args": ["hello world", "--example"],
            "env": {"MODE": "test"},
            "inputs": [{
                "path": "nested/input.txt",
                "source": {"type": "file", "path": "payload.txt"}
            }],
            "outputs": [{"path": "result.json", "required": true}],
            "resources": {"timeout_ms": 5000},
            "network": "network"
        }),
    );

    let output = Command::cargo_bin("compute")
        .unwrap()
        .env("COMPUTE_HOST_SECRET", "must-not-leak")
        .args(["run", "--workload", workload.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let observed: serde_json::Value =
        serde_json::from_str(result["stdout"]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        observed["args"],
        serde_json::json!(["hello world", "--example"])
    );
    assert_eq!(observed["mode"], "test");
    assert!(observed["host"].is_null());
    assert_eq!(observed["input"], "portable input");
    assert_eq!(result["artifacts"][0]["path"], "/output/result.json");
    assert_eq!(result["outputs"][0]["path"], "result.json");
    assert_eq!(
        result["outputs"][0]["data"],
        "{\"value\": \"portable input\"}"
    );
}

#[test]
fn wasm_workload_spec_executes_through_existing_contract() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("module.wasm"),
        wat::parse_str(r#"(module (func (export "_start")))"#).unwrap(),
    )
    .unwrap();
    let workload = temp.path().join("workload.json");
    write_json(
        &workload,
        serde_json::json!({
            "version": "1",
            "runtime": "wasm",
            "entrypoint": "module.wasm",
            "outputs": [{"path": "declared-but-optional.txt", "required": false}],
            "network": "none"
        }),
    );

    let output = Command::cargo_bin("compute")
        .unwrap()
        .args(["run", "--workload", workload.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(result["runtime"], "wasm");
}

#[test]
fn direct_and_workload_file_execution_are_semantically_equivalent() {
    let temp = tempfile::tempdir().unwrap();
    let module = temp.path().join("module.wasm");
    std::fs::write(
        &module,
        wat::parse_str(r#"(module (func (export "_start")))"#).unwrap(),
    )
    .unwrap();
    let workload = temp.path().join("workload.json");
    write_json(
        &workload,
        serde_json::json!({
            "version": "1",
            "runtime": "wasm",
            "entrypoint": "module.wasm",
            "network": "none"
        }),
    );
    let direct = Command::cargo_bin("compute")
        .unwrap()
        .args([
            "run",
            module.to_str().unwrap(),
            "--network",
            "none",
            "--json",
        ])
        .output()
        .unwrap();
    let portable = Command::cargo_bin("compute")
        .unwrap()
        .args(["run", "--workload", workload.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(direct.status.success());
    assert!(portable.status.success());
    let direct: serde_json::Value = serde_json::from_slice(&direct.stdout).unwrap();
    let portable: serde_json::Value = serde_json::from_slice(&portable.stdout).unwrap();
    for field in [
        "runtime",
        "network",
        "lifecycle",
        "status",
        "exit_code",
        "stdout",
        "stderr",
        "artifacts",
    ] {
        assert_eq!(direct[field], portable[field], "field {field}");
    }
    assert_ne!(direct["execution_id"], portable["execution_id"]);
}

#[test]
fn node_workload_spec_executes_when_node_is_installed() {
    if std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("main.js"),
        "console.log(JSON.stringify(process.argv.slice(2)))",
    )
    .unwrap();
    let workload = temp.path().join("workload.json");
    write_json(
        &workload,
        serde_json::json!({
            "version": "1",
            "runtime": "node",
            "entrypoint": "main.js",
            "args": ["αβγ", "hello world"],
            "network": "network"
        }),
    );
    let output = Command::cargo_bin("compute")
        .unwrap()
        .args(["run", "--workload", workload.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["stdout"]["text"], "[\"αβγ\",\"hello world\"]\n");
}

#[test]
fn inspect_and_dry_run_plan_without_execution() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("executed");
    std::fs::write(
        temp.path().join("main.py"),
        format!("open({:?}, 'w').write('ran')", marker),
    )
    .unwrap();
    let workload = temp.path().join("workload.json");
    write_json(
        &workload,
        serde_json::json!({
            "version": "1",
            "runtime": "python",
            "entrypoint": "main.py",
            "args": ["--example"],
            "env": {"MODE": "inspect"},
            "inputs": [{
                "path": "data/input.txt",
                "source": {"type": "inline", "data": "planned"}
            }],
            "outputs": [{"path": "result.json", "required": false}],
            "network": "network"
        }),
    );

    for args in [
        vec![
            "inspect",
            "--workload",
            workload.to_str().unwrap(),
            "--json",
        ],
        vec![
            "run",
            "--workload",
            workload.to_str().unwrap(),
            "--dry-run",
            "--json",
        ],
    ] {
        let output = Command::cargo_bin("compute")
            .unwrap()
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(plan["valid"], true);
        assert_eq!(plan["validation"], "valid");
        assert_eq!(plan["workload"]["entrypoint"], "main.py");
        assert_eq!(plan["workload"]["inputs"][0]["source"]["type"], "inline");
        assert_eq!(plan["workload"]["outputs"][0]["required"], false);
        assert!(plan["backend_capabilities"].is_object());
        assert!(!marker.exists());
    }
}

#[test]
fn missing_required_output_is_reported_separately() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("main.py"), "print('runtime succeeded')").unwrap();
    let workload = temp.path().join("workload.json");
    write_json(
        &workload,
        serde_json::json!({
            "version": 1,
            "runtime": "python",
            "entrypoint": "main.py",
            "outputs": [{"path": "required.txt", "required": true}],
            "network": "network"
        }),
    );
    let output = Command::cargo_bin("compute")
        .unwrap()
        .args(["run", "--workload", workload.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "failed");
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["error"]["kind"], "output_contract");
    assert_eq!(result["error"]["phase"], "completed");
    assert_eq!(result["missing_outputs"][0]["required"], true);
}

#[test]
fn repeated_workload_executions_do_not_leak_outputs() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("main.py"),
        r#"import os
path = os.path.join(os.environ["COMPUTE_OUTPUT_DIR"], "result.txt")
print("leaked" if os.path.exists(path) else "isolated")
open(path, "w").write("created")
"#,
    )
    .unwrap();
    let workload = temp.path().join("workload.json");
    write_json(
        &workload,
        serde_json::json!({
            "version": 1,
            "runtime": "python",
            "entrypoint": "main.py",
            "outputs": [{"path": "result.txt", "required": true}],
            "network": "network"
        }),
    );

    let mut ids = Vec::new();
    for _ in 0..2 {
        let output = Command::cargo_bin("compute")
            .unwrap()
            .args(["run", "--workload", workload.to_str().unwrap(), "--json"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["stdout"]["text"], "isolated\n");
        assert_eq!(result["outputs"][0]["data"], "created");
        ids.push(result["execution_id"].as_str().unwrap().to_string());
    }
    assert_ne!(ids[0], ids[1]);
}

#[test]
fn workload_network_policy_cannot_be_silently_broadened() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("main.py"), "print('must not run')").unwrap();
    let workload = temp.path().join("workload.json");
    write_json(
        &workload,
        serde_json::json!({
            "version": "1",
            "runtime": "python",
            "entrypoint": "main.py",
            "network": "none"
        }),
    );
    Command::cargo_bin("compute")
        .unwrap()
        .args(["run", "--workload", workload.to_str().unwrap(), "--json"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unsupported capability"))
        .stdout(predicate::str::is_empty());
}

#[test]
fn bundle_create_verify_inspect_dry_run_and_execute_are_one_engine_path() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("main.py"),
        r#"import os
value = open(os.path.join(os.environ["COMPUTE_WORK_DIR"], "nested/input.txt"), "rb").read()
open(os.path.join(os.environ["COMPUTE_OUTPUT_DIR"], "result.bin"), "wb").write(value)
"#,
    )
    .unwrap();
    std::fs::write(temp.path().join("payload.bin"), [0, 255, 7]).unwrap();
    let workload = temp.path().join("workload.json");
    write_json(
        &workload,
        serde_json::json!({
            "version": "1",
            "runtime": "python",
            "entrypoint": "main.py",
            "inputs": [{
                "path": "nested/input.txt",
                "source": {"type": "file", "path": "payload.bin"}
            }],
            "outputs": [{"path": "result.bin", "required": true}],
            "network": "network"
        }),
    );
    let first = temp.path().join("first.compute");
    let second = temp.path().join("second.compute");
    let create = |output: &std::path::Path| {
        Command::cargo_bin("compute")
            .unwrap()
            .args([
                "bundle",
                "create",
                "--workload",
                workload.to_str().unwrap(),
                "--output",
                output.to_str().unwrap(),
                "--json",
            ])
            .output()
            .unwrap()
    };
    let created = create(&first);
    assert!(created.status.success());
    assert!(create(&second).status.success());
    assert_eq!(
        std::fs::read(&first).unwrap(),
        std::fs::read(&second).unwrap()
    );
    let identity: serde_json::Value = serde_json::from_slice(&created.stdout).unwrap();

    let verify = Command::cargo_bin("compute")
        .unwrap()
        .args(["bundle", "verify", first.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(verify.status.success());
    let verified: serde_json::Value = serde_json::from_slice(&verify.stdout).unwrap();
    assert_eq!(verified["valid"], true);
    assert_eq!(verified["bundle_id"], identity["bundle_id"]);
    assert_eq!(verified["workload_id"], identity["workload_id"]);

    let inspect = Command::cargo_bin("compute")
        .unwrap()
        .args(["bundle", "inspect", first.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(inspect.status.success());
    let inspection: serde_json::Value = serde_json::from_slice(&inspect.stdout).unwrap();
    assert_eq!(inspection["inputs"][0]["path"], "nested/input.txt");
    assert_eq!(inspection["inputs"][0]["size"], 3);

    std::fs::remove_file(temp.path().join("main.py")).unwrap();
    std::fs::remove_file(temp.path().join("payload.bin")).unwrap();
    let dry_run = Command::cargo_bin("compute")
        .unwrap()
        .args([
            "run",
            "--bundle",
            first.to_str().unwrap(),
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(dry_run.status.success());
    let plan: serde_json::Value = serde_json::from_slice(&dry_run.stdout).unwrap();
    assert_eq!(plan["bundle_verification"]["valid"], true);
    assert_eq!(plan["plan"]["workload_id"], identity["workload_id"]);

    let run = Command::cargo_bin("compute")
        .unwrap()
        .args(["run", "--bundle", first.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();
    assert_eq!(result["outputs"][0]["data"], serde_json::json!([0, 255, 7]));

    let mut tampered = std::fs::read(&second).unwrap();
    let offset = tampered
        .windows(3)
        .position(|window| window == [0, 255, 7])
        .unwrap();
    tampered[offset] = 1;
    std::fs::write(&second, tampered).unwrap();
    let invalid = Command::cargo_bin("compute")
        .unwrap()
        .args(["bundle", "verify", second.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    let diagnostic: serde_json::Value = serde_json::from_slice(&invalid.stdout).unwrap();
    assert_eq!(diagnostic["valid"], false);
    assert_eq!(diagnostic["error"]["kind"], "bundle_identity_mismatch");
}

#[test]
fn bundle_expected_identity_mismatch_fails_before_execution() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("main.py"),
        "import os\nopen(os.path.join(os.environ['COMPUTE_OUTPUT_DIR'], 'ran'), 'w').write('bad')\n",
    )
    .unwrap();
    let workload = temp.path().join("workload.json");
    write_json(
        &workload,
        serde_json::json!({
            "version": "1",
            "runtime": "python",
            "entrypoint": "main.py",
            "outputs": [{"path": "ran", "required": true}],
            "network": "network"
        }),
    );
    let bundle = temp.path().join("workload.compute");
    Command::cargo_bin("compute")
        .unwrap()
        .args([
            "bundle",
            "create",
            "--workload",
            workload.to_str().unwrap(),
            "--output",
            bundle.to_str().unwrap(),
        ])
        .assert()
        .success();
    Command::cargo_bin("compute")
        .unwrap()
        .args([
            "run",
            "--bundle",
            bundle.to_str().unwrap(),
            "--expected-bundle-id",
            "sha256:wrong",
            "--json",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("bundle identity mismatch"))
        .stdout(predicate::str::is_empty());
}
