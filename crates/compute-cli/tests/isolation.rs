//! Host isolation profiles are enforced by the kernel, reported per
//! dimension, and refused rather than downgraded.

use assert_cmd::Command;

fn host() -> serde_json::Value {
    let output = Command::cargo_bin("compute")
        .unwrap()
        .args(["isolation", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["host"].clone()
}

fn supported(host: &serde_json::Value, profile: &str) -> bool {
    let support = host["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|support| support["profile"] == profile)
        .unwrap()
        .clone();
    if support["supported"] != true {
        // Unsupported is a reported state, never a silent downgrade.
        assert!(support["refusal"]["code"].is_string());
        eprintln!(
            "{profile} is unsupported here: {}",
            support["refusal"]["message"]
        );
        return false;
    }
    true
}

fn run(dir: &std::path::Path, workload: serde_json::Value) -> (bool, serde_json::Value) {
    std::fs::write(
        dir.join("workload.json"),
        serde_json::to_vec(&workload).unwrap(),
    )
    .unwrap();
    let output = Command::cargo_bin("compute")
        .unwrap()
        .args(["run", "--workload"])
        .arg(dir.join("workload.json"))
        .arg("--json")
        .output()
        .unwrap();
    let value = serde_json::from_slice(&output.stdout).unwrap_or_else(
        |_| serde_json::json!({ "stderr": String::from_utf8_lossy(&output.stderr) }),
    );
    (output.status.success(), value)
}

const PROBE: &str = r#"
echo x > "$COMPUTE_OUTPUT_DIR/inside.txt" && echo wrote-workspace
echo x > "$OUTSIDE/written" 2>/dev/null && echo WROTE-OUTSIDE || echo denied-write-outside
cat "$OUTSIDE/secret" >/dev/null 2>&1 && echo READ-SECRET || echo denied-read-secret
cat /etc/hostname >/dev/null 2>&1 && echo read-etc || echo denied-etc
(exec 3<>/dev/tcp/1.1.1.1/80) 2>/dev/null && echo NETWORK || echo no-network
cat /proc/1/environ >/dev/null 2>&1 && echo READ-PID1-ENVIRONMENT || echo denied-pid1-environment
"#;

fn probe(profile: &str) -> Option<(String, serde_json::Value)> {
    if !supported(&host(), profile) {
        return None;
    }
    let temporary = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "s3cret").unwrap();
    std::fs::write(temporary.path().join("main.sh"), PROBE).unwrap();
    let (ok, result) = run(
        temporary.path(),
        serde_json::json!({
            "version": "1", "runtime": "shell", "entrypoint": "main.sh",
            "network": "none", "isolation": { "host": profile },
            "env": { "OUTSIDE": outside.path() },
        }),
    );
    assert!(ok, "{result}");
    assert!(!outside.path().join("written").exists());
    Some((
        result["stdout"]["text"].as_str().unwrap().to_string(),
        result,
    ))
}

#[test]
fn restricted_writes_only_its_workspace_and_has_no_network() {
    let Some((stdout, result)) = probe("restricted") else {
        return;
    };
    for line in [
        "wrote-workspace",
        "denied-write-outside",
        "read-etc",
        "no-network",
        "denied-pid1-environment",
    ] {
        assert!(stdout.contains(line), "{line} missing from {stdout}");
    }
    // Restricted reads the host; it never claims otherwise.
    let host = &result["isolation"]["host"];
    assert_eq!(host["profile"], "restricted");
    assert_eq!(host["filesystem"], "restricted");
    assert_eq!(host["network"], "enforced");
    assert_eq!(result["isolation"]["filesystem"], "unavailable");
    // The evidence is in the receipt too.
    assert_eq!(result["receipt"]["isolation"]["host"], *host);
}

#[test]
fn isolated_cannot_read_outside_its_workspace() {
    let Some((stdout, result)) = probe("isolated") else {
        return;
    };
    for line in [
        "wrote-workspace",
        "denied-write-outside",
        "denied-read-secret",
        "denied-etc",
        "no-network",
        "denied-pid1-environment",
    ] {
        assert!(stdout.contains(line), "{line} missing from {stdout}");
    }
    let host = &result["isolation"]["host"];
    for dimension in ["filesystem", "network", "memory", "cpu", "process"] {
        assert_eq!(host[dimension], "enforced", "{dimension}: {host}");
    }
    assert_eq!(result["isolation"]["filesystem"], "enforced");
    assert_eq!(result["receipt"]["isolation"]["host"], *host);
}

#[test]
fn isolated_enforces_its_memory_limit_and_keeps_localhost() {
    if !supported(&host(), "isolated") {
        return;
    }
    let temporary = tempfile::tempdir().unwrap();
    std::fs::write(
        temporary.path().join("main.sh"),
        r#"
(exec 3<>/dev/tcp/1.1.1.1/80) 2>/dev/null && echo NETWORK || echo no-network
echo allocating
held=$(head -c 200000000 /dev/zero | tr '\0' a)
echo ALLOCATED
"#,
    )
    .unwrap();
    let (ok, result) = run(
        temporary.path(),
        serde_json::json!({
            "version": "1", "runtime": "shell", "entrypoint": "main.sh",
            "network": "localhost", "isolation": { "host": "isolated" },
            "resources": { "memory_bytes": 64 * 1024 * 1024 },
        }),
    );
    let stdout = result["stdout"]["text"].as_str().unwrap_or_default();
    assert!(stdout.contains("no-network"), "{result}");
    assert!(stdout.contains("allocating"), "{result}");
    assert!(!stdout.contains("ALLOCATED"), "{result}");
    assert!(!ok || result["status"] != "completed", "{result}");
    assert_eq!(result["isolation"]["host"]["memory"], "enforced");
    assert_eq!(
        result["isolation"]["host"]["memory_bytes"],
        64 * 1024 * 1024
    );
}

#[test]
fn a_host_profile_is_refused_where_it_cannot_apply() {
    let temporary = tempfile::tempdir().unwrap();
    std::fs::write(temporary.path().join("main.sh"), "echo must-not-run").unwrap();
    // Trusted cannot deny the network: refused, not run unconfined.
    let (ok, result) = run(
        temporary.path(),
        serde_json::json!({
            "version": "1", "runtime": "shell", "entrypoint": "main.sh",
            "network": "none",
        }),
    );
    assert!(!ok);
    assert!(!result.to_string().contains("must-not-run"));
    // A host profile belongs to process runtimes; WASM has its own
    // boundary and does not pretend to take one.
    std::fs::write(temporary.path().join("main.wasm"), b"\0asm\x01\0\0\0").unwrap();
    let (ok, result) = run(
        temporary.path(),
        serde_json::json!({
            "version": "1", "runtime": "wasm", "entrypoint": "main.wasm",
            "network": "none", "isolation": { "host": "isolated" },
        }),
    );
    assert!(!ok, "{result}");
}
