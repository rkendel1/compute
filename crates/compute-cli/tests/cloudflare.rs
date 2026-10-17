//! `compute cloudflare` and a cloudflare pool member through the CLI,
//! without a Cloudflare account or credential: what Compute Configured's
//! verification and the Homebrew checks rely on.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;

fn example() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/cloudflare-workers")
}

/// `compute <arguments>` against the example pool, with no credential.
fn compute(cache: &Path, arguments: &[&std::ffi::OsStr]) -> Command {
    let mut command = Command::cargo_bin("compute").unwrap();
    command
        .env_remove("CLOUDFLARE_API_TOKEN")
        .env_remove("COMPUTE_POOL_CONFIG")
        .args(arguments)
        .arg("--pool-config")
        .arg(example().join("compute-pool.toml"))
        .arg("--capability-cache")
        .arg(cache.join("capabilities.json"));
    command
}

fn os(values: &[&'static str]) -> Vec<&'static std::ffi::OsStr> {
    values
        .iter()
        .map(|value| std::ffi::OsStr::new(*value))
        .collect()
}

#[test]
fn the_free_profile_is_shown_without_an_account() {
    let output = Command::cargo_bin("compute")
        .unwrap()
        .args(["cloudflare", "profile", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let profile: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(profile["name"], "workers-free");
    assert_eq!(profile["limits"]["cpu_ms_per_invocation"], 10);
    assert_eq!(profile["capabilities"]["persistent_processes"], false);
    assert_eq!(profile["capabilities"]["containers"], false);
    assert!(
        profile["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|note| note.as_str().unwrap().contains("responsible"))
    );
    Command::cargo_bin("compute")
        .unwrap()
        .args(["cloudflare", "profile", "workers-unlimited"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown Cloudflare profile"));
}

#[test]
fn validation_reports_configuration_and_a_missing_credential() {
    let cache = tempfile::tempdir().unwrap();
    compute(
        cache.path(),
        &os(&[
            "cloudflare",
            "validate",
            "--provider",
            "cloudflare",
            "--offline",
        ]),
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("Configuration  valid"))
    .stdout(predicate::str::contains(
        "Credential     missing (environment variable CLOUDFLARE_API_TOKEN)",
    ));
    compute(
        cache.path(),
        &os(&["cloudflare", "validate", "--provider", "cloudflare"]),
    )
    .assert()
    .failure()
    .stderr(predicate::str::contains("unauthorized"))
    .stderr(predicate::str::contains("CLOUDFLARE_API_TOKEN"));
    compute(
        cache.path(),
        &os(&["cloudflare", "validate", "--provider", "local"]),
    )
    .assert()
    .failure()
    .stderr(predicate::str::contains("not a cloudflare provider"));
}

#[test]
fn capabilities_are_discovered_without_a_credential() {
    let cache = tempfile::tempdir().unwrap();
    let output = compute(
        cache.path(),
        &os(&["provider", "capabilities", "cloudflare", "--json"]),
    )
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let capabilities: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(capabilities["protocol"], "compute.cloudflare@1");
    assert_eq!(capabilities["provider"]["kind"], "cloudflare");
    assert_eq!(capabilities["execution"]["run"], true);
    assert_eq!(capabilities["execution"]["deployments"], false);
    assert_eq!(capabilities["inventory"]["runtimes"][0]["id"], "wasm");

    compute(cache.path(), &os(&["provider", "list"]))
        .assert()
        .success()
        .stdout(predicate::str::is_match(r"cloudflare\s+cloudflare\s+-100\s+discovered").unwrap());
}

#[test]
fn unsupported_workloads_are_refused_before_anything_runs() {
    let cache = tempfile::tempdir().unwrap();
    let script = cache.path().join("app.py");
    std::fs::write(&script, "print(1)\n").unwrap();
    compute(
        cache.path(),
        &[
            "run".as_ref(),
            script.as_os_str(),
            "--provider".as_ref(),
            "cloudflare".as_ref(),
        ],
    )
    .assert()
    .failure()
    .stderr(predicate::str::contains("runtime_unsupported"))
    .stderr(predicate::str::contains("nothing was executed"));
}

#[test]
fn execution_without_a_credential_is_unauthorized_and_not_moved() {
    let cache = tempfile::tempdir().unwrap();
    let module = example().join("hello.wasm");
    compute(
        cache.path(),
        &[
            "run".as_ref(),
            module.as_os_str(),
            "--provider".as_ref(),
            "cloudflare".as_ref(),
        ],
    )
    .assert()
    .failure()
    .stderr(predicate::str::contains("CLOUDFLARE_API_TOKEN"))
    .stderr(predicate::str::contains("not retried on another provider"));
}

#[test]
fn existing_placement_is_unchanged_by_a_cloudflare_member() {
    let cache = tempfile::tempdir().unwrap();
    let module = example().join("hello.wasm");
    compute(cache.path(), &["run".as_ref(), module.as_os_str()])
        .assert()
        .success()
        .stderr(predicate::str::contains("Selected: local"))
        .stdout(predicate::str::contains(
            "hello from Compute on Cloudflare Workers",
        ));
}

#[test]
fn init_writes_a_credential_free_pool_and_never_overwrites() {
    let dir = tempfile::tempdir().unwrap();
    let pool = dir.path().join("state/cloudflare-pool.toml");
    let init = |account: &str| {
        let mut command = Command::cargo_bin("compute").unwrap();
        command
            .env("CLOUDFLARE_API_TOKEN", "cf-init-sentinel-token")
            .args(["cloudflare", "init", "--account-id", account, "--output"])
            .arg(&pool);
        command
    };
    init("0123456789abcdef0123456789abcdef")
        .assert()
        .success()
        .stdout(predicate::str::contains("Wrote"));
    let written = std::fs::read_to_string(&pool).unwrap();
    assert!(written.contains("token_env = \"CLOUDFLARE_API_TOKEN\""));
    assert!(!written.contains("cf-init-sentinel-token"));
    compute_placement::PoolConfig::parse(&written).unwrap();

    init("fedcba9876543210fedcba9876543210")
        .assert()
        .success()
        .stdout(predicate::str::contains("left unchanged"));
    assert_eq!(std::fs::read_to_string(&pool).unwrap(), written);

    init("not-an-account")
        .arg("--provider")
        .arg("other")
        .assert()
        .failure()
        .stderr(predicate::str::contains("account_id"));
}
