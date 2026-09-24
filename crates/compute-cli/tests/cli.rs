use assert_cmd::Command;
use predicates::prelude::*;

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
    command
        .args(["runtimes", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"kind\": "))
        .stdout(predicate::str::contains("wasm"));
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
