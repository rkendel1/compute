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
        assert!(matches!(check["status"].as_str(), Some("pass" | "fail")));
        if check["status"] == "fail" {
            assert!(check.get("remediation").is_some());
        }
    }
    assert_eq!(document["data"]["strict"], false);
    assert_eq!(
        document["data"]["healthy"],
        checks.iter().all(|check| check["status"] == "pass")
    );
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
fn doctor_strict_passes_when_every_check_passes() {
    // Restrict to the runtimes of this host, and require only that the
    // outcome agrees with the report: strict exits zero iff nothing failed.
    let report = json(&compute(&["doctor", "--json", "--runtimes-only"]));
    let healthy = report["data"]["healthy"].as_bool().unwrap();
    let strict = compute(&["doctor", "--strict", "--runtimes-only", "--json"]);
    assert_eq!(strict.status.success(), healthy);
    assert_eq!(json(&strict)["ok"], healthy);
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
    assert_eq!(document["error"]["recovery"]["command"], "compute start");
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
