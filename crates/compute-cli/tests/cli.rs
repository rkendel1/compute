use assert_cmd::Command;
use predicates::prelude::*;

fn write_json(path: &std::path::Path, value: serde_json::Value) {
    std::fs::write(path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
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
