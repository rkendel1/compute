//! The machine-readable command contract: `compute doctor` (`--strict`,
//! `--json`) and the failure envelope every `--json` command shares.

#[path = "support/runtimes.rs"]
mod runtimes;

use std::process::{Command, Output};

fn compute(arguments: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_compute"));
    runtimes::with_fixture_runtimes(&mut command);
    command
        // Nothing listens here: a controller is "down".
        .env("COMPUTE_DAEMON", "http://127.0.0.1:1")
        .args(arguments)
        .output()
        .unwrap()
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "not JSON ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

/// The envelope is the last line of stderr; the line before is the human
/// message.
fn stderr_envelope(output: &Output) -> serde_json::Value {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let last = stderr.trim_end().lines().last().unwrap_or_default();
    serde_json::from_str(last)
        .unwrap_or_else(|error| panic!("the last stderr line is not JSON ({error}): {stderr}"))
}

#[test]
fn doctor_json_is_an_envelope_that_keeps_the_original_keys() {
    let output = compute(&["doctor", "--json", "--runtimes-only"]);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let document = json(&output);
    assert_eq!(document["ok"], true);
    assert_eq!(document["command"], "compute doctor");
    assert_eq!(document["exit_code"], 0);
    assert!(document["error"].is_null());
    // The original contract is untouched.
    assert_eq!(document["runtimes"].as_array().unwrap().len(), 11);
    assert!(document["controller"].is_null());
    // And the new, flat view an agent can iterate.
    let checks = document["data"]["checks"].as_array().unwrap();
    assert_eq!(checks.len(), 11);
    for check in checks {
        assert!(check["id"].as_str().unwrap().starts_with("runtime:"));
        // An uninstalled runtime is a warning, never a failure.
        assert!(matches!(check["status"].as_str(), Some("pass" | "warn")));
        if check["status"] == "warn" {
            assert!(check["remediation"].is_string() || check["remediation"].is_null());
        }
    }
    assert_eq!(document["data"]["strict"], false);
    assert_eq!(
        document["data"]["healthy"],
        checks.iter().all(|check| check["status"] == "pass")
    );
    // Deterministic: the same host reports the same checks in the same order.
    let again = json(&compute(&["doctor", "--json", "--runtimes-only"]));
    assert_eq!(again["data"]["checks"], document["data"]["checks"]);
}

#[test]
fn doctor_without_strict_reports_and_exits_zero_even_with_findings() {
    let output = compute(&["doctor", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document = json(&output);
    assert_eq!(document["ok"], true);
    // The controller on port 1 is unreachable: a finding, not an error.
    let controller = document["data"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["id"] == "controller")
        .expect("a controller check");
    assert_eq!(controller["status"], "fail");
    assert!(
        controller["remediation"]
            .as_str()
            .unwrap()
            .contains("compute start")
    );
    assert_eq!(document["data"]["healthy"], false);
}

#[test]
fn doctor_strict_fails_when_a_check_fails() {
    let output = compute(&["doctor", "--strict", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    let document = json(&output);
    assert_eq!(document["ok"], false);
    assert_eq!(document["exit_code"], 1);
    assert_eq!(document["error"]["code"], "doctor_checks_failed");
    // The evidence is still there to act on.
    assert!(document["data"]["checks"].as_array().unwrap().len() > 11);
    assert_eq!(document["data"]["strict"], true);

    // Without `--json` the same failure is an error on stderr and exit 1.
    let text = compute(&["doctor", "--strict"]);
    assert_eq!(text.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&text.stderr).contains("doctor check(s) did not pass"));
}

#[test]
fn doctor_strict_agrees_with_the_report_on_this_host() {
    // Strict exits 1 iff any check is not `pass` (a warning counts).
    let report = json(&compute(&["doctor", "--json", "--runtimes-only"]));
    let healthy = report["data"]["healthy"].as_bool().unwrap();
    let strict = compute(&["doctor", "--strict", "--runtimes-only", "--json"]);
    assert_eq!(strict.status.code(), Some(i32::from(!healthy)));
    let document = json(&strict);
    assert_eq!(document["ok"], healthy);
    assert_eq!(document["exit_code"], i32::from(!healthy));
    if !healthy {
        assert_eq!(document["error"]["code"], "doctor_checks_failed");
        // Runtimes only: every finding is a warning, and warnings alone fail
        // strict mode.
        assert!(
            document["data"]["checks"]
                .as_array()
                .unwrap()
                .iter()
                .all(|check| check["status"] != "fail")
        );
    }
}

#[test]
fn a_failed_json_command_ends_stderr_with_one_envelope_and_leaves_stdout_alone() {
    let output = compute(&["environment", "inspect", "web", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    // stdout is the command's result channel; there is none.
    assert!(output.stdout.is_empty());
    let document = stderr_envelope(&output);
    assert_eq!(document["ok"], false);
    assert_eq!(document["command"], "compute environment inspect");
    assert_eq!(document["exit_code"], 1);
    assert!(document["data"].is_null());
    assert_eq!(document["error"]["code"], "controller_unavailable");
    // `COMPUTE_DAEMON` points somewhere other than the default local
    // controller, so `compute start` is not claimed as the fix.
    assert!(document["error"].get("recovery").is_none());
    // The human message is unchanged, and is the line before the envelope.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = stderr.lines().next().unwrap();
    assert!(
        message.contains("cannot reach the Compute daemon"),
        "{stderr}"
    );
    assert_eq!(document["error"]["message"].as_str().unwrap(), message);
}

#[test]
fn a_failure_without_a_deterministic_recovery_claims_none() {
    let output = compute(&["run", "/definitely/not/here", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let document = stderr_envelope(&output);
    assert_eq!(document["ok"], false);
    assert_eq!(document["command"], "compute run");
    assert!(
        document["error"]["code"]
            .as_str()
            .is_some_and(|code| !code.is_empty())
    );
    assert!(document["error"].get("recovery").is_none());
}

#[test]
fn without_json_a_failure_is_only_the_human_message() {
    let output = compute(&["environment", "inspect", "web"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.trim_end().lines().count(), 1, "{stderr}");
    assert!(serde_json::from_str::<serde_json::Value>(stderr.trim()).is_err());
}

#[test]
fn success_output_of_other_commands_is_unchanged() {
    // The envelope is for failures and for commands that adopted it; a command
    // that did not (here `isolation`) keeps its original shape.
    let output = compute(&["isolation", "--json"]);
    assert!(output.status.success());
    let document = json(&output);
    assert_eq!(document["version"], "1");
    assert!(document.get("ok").is_none());
}

#[test]
fn a_usage_error_keeps_clap_s_message_and_status_and_adds_the_envelope_in_json_mode() {
    let output = compute(&["environment", "inspect", "--json", "--no-such-flag"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let document = stderr_envelope(&output);
    assert_eq!(document["ok"], false);
    assert_eq!(document["exit_code"], 2);
    assert_eq!(document["error"]["code"], "invalid_arguments");
    assert!(document["error"].get("recovery").is_none());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unexpected argument"), "{stderr}");

    // Without --json, clap's own output only.
    let plain = compute(&["environment", "inspect", "--no-such-flag"]);
    assert_eq!(plain.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&plain.stderr);
    assert!(
        stderr.contains("unexpected argument") && !stderr.contains("\"ok\""),
        "{stderr}"
    );
}

#[test]
fn help_and_version_are_not_failures_even_with_json() {
    for arguments in [
        ["doctor", "--help", "--json"],
        ["--version", "--json", "--"],
    ] {
        let output = compute(&arguments);
        assert!(output.status.success(), "{arguments:?}");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("\"ok\""));
    }
}

#[test]
fn a_workload_that_times_out_is_a_result_not_an_error_envelope() {
    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("slow.sh");
    std::fs::write(&script, "sleep 30\n").unwrap();
    let output = compute(&[
        "run",
        script.to_str().unwrap(),
        "--runtime",
        "shell",
        "--timeout",
        "1s",
        "--network",
        "network",
        "--json",
    ]);
    assert_ne!(output.status.code(), Some(0));
    // stdout is the execution result, exactly one JSON document...
    let result = json(&output);
    assert_eq!(result["status"], "timed_out");
    assert!(result.get("ok").is_none());
    // ...and no error envelope is added: the run itself reported the failure.
    assert!(!String::from_utf8_lossy(&output.stderr).contains("\"ok\":false"));
}

#[test]
fn every_error_code_is_documented() {
    let docs = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/cli-contract.md"),
    )
    .unwrap();
    let core = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../compute-core/src/lib.rs"),
    )
    .unwrap();
    let start = core.find("pub fn code(&self) -> &str").unwrap();
    let body = &core[start..core[start..].find("\n    }\n").unwrap() + start];
    let mut codes: Vec<String> = body
        .split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect();
    let worker = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../compute-worker-github/src/error.rs"),
    )
    .unwrap();
    let start = worker.find("pub fn code(&self)").unwrap();
    codes.extend(
        worker[start..]
            .split('"')
            .skip(1)
            .step_by(2)
            .map(str::to_owned),
    );
    codes.extend(
        [
            "doctor_checks_failed",
            "invalid_arguments",
            "job_failed",
            "runner_failed",
        ]
        .map(str::to_owned),
    );
    for code in codes {
        assert!(
            docs.contains(&format!("`{code}`")),
            "`{code}` is not in docs/cli-contract.md"
        );
    }
}

/// A real controller (not a mock): the shapes `doctor` classifies are the ones
/// `controller_diagnosis` really produces. A mock once let a healthy
/// controller read as `fail`.
#[test]
fn doctor_classifies_a_real_controller_healthy_and_a_refused_credential_as_failing() {
    let temporary = tempfile::tempdir().unwrap();
    let listen = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("127.0.0.1:{}", probe.local_addr().unwrap().port())
    };
    let endpoint = format!("http://{listen}");
    let started = {
        let mut command = Command::new(env!("CARGO_BIN_EXE_compute"));
        runtimes::with_fixture_runtimes(&mut command);
        command
            .args(["start", "--detach", "--listen", &listen])
            .args(["--require-token-env", "COMPUTE_DAEMON_TOKEN", "--state-dir"])
            .arg(temporary.path().join("state"))
            .env("COMPUTE_DAEMON_TOKEN", "secret")
            .current_dir(temporary.path())
            .output()
            .unwrap()
    };
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    struct Stop(String);
    impl Drop for Stop {
        fn drop(&mut self) {
            let _ = Command::new(env!("CARGO_BIN_EXE_compute"))
                .args(["stop", "--daemon", &self.0])
                .env("COMPUTE_DAEMON_TOKEN", "secret")
                .output();
        }
    }
    let _stop = Stop(endpoint.clone());

    let doctor = |token: Option<&str>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_compute"));
        runtimes::with_fixture_runtimes(&mut command);
        command.env_remove("COMPUTE_DAEMON_TOKEN");
        if let Some(token) = token {
            command.env("COMPUTE_DAEMON_TOKEN", token);
        }
        command
            .args(["doctor", "--json", "--daemon", &endpoint])
            .output()
            .unwrap()
    };
    let controller = |output: &Output| {
        json(output)["data"]["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["id"] == "controller")
            .cloned()
            .expect("a controller check")
    };

    let healthy = controller(&doctor(Some("secret")));
    assert_eq!(healthy["status"], "pass", "{healthy}");
    assert!(healthy["remediation"].is_null());

    let refused = controller(&doctor(None));
    assert_eq!(refused["status"], "fail", "{refused}");
    assert!(
        refused["remediation"]
            .as_str()
            .unwrap()
            .contains("credential"),
        "{refused}"
    );
}
