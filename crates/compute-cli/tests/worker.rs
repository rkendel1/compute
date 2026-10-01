//! `compute worker github-actions run`, end to end: the real binary, the real
//! shell runtime, a fake runner release, and a local stand-in for GitHub's
//! API. No GitHub credential or network is involved.

#[path = "support/runtimes.rs"]
mod runtimes;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

const CREDENTIAL: &str = "ghp_CREDENTIAL0123456789-do-not-leak";
const TOKEN: &str = "AREGTOKEN-0123456789-do-not-leak";

/// Answer one request with `status` and `body`, keeping what was received.
fn github(status: u16, body: String) -> (String, Arc<Mutex<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let received = Arc::new(Mutex::new(String::new()));
    let sink = received.clone();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buffer = [0u8; 8192];
            let read = stream.read(&mut buffer).unwrap_or(0);
            *sink.lock().unwrap() = String::from_utf8_lossy(&buffer[..read]).into_owned();
            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (url, received)
}

struct Runner {
    root: tempfile::TempDir,
    archive: PathBuf,
    sha256: String,
}

fn runner(run_body: &str) -> Runner {
    let root = tempfile::tempdir().unwrap();
    let tree = root.path().join("tree");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(
        tree.join("config.sh"),
        "#!/bin/sh\necho configured \"$@\"\nexit 0\n",
    )
    .unwrap();
    std::fs::write(
        tree.join("run.sh"),
        format!(
            "#!/bin/sh\nmkdir -p _diag\n\
             echo 'Running job: build' >> _diag/Runner_1.log\n\
             echo 'Job build completed with result: Succeeded' >> _diag/Runner_1.log\n{run_body}\n"
        ),
    )
    .unwrap();
    for script in ["config.sh", "run.sh"] {
        use std::os::unix::fs::PermissionsExt;
        let path = tree.join(script);
        let mut mode = std::fs::metadata(&path).unwrap().permissions();
        mode.set_mode(0o755);
        std::fs::set_permissions(&path, mode).unwrap();
    }
    let archive = root.path().join("runner.tar.gz");
    assert!(
        Command::new("tar")
            .arg("czf")
            .arg(&archive)
            .arg("-C")
            .arg(&tree)
            .arg(".")
            .status()
            .unwrap()
            .success()
    );
    let sha256 = format!("{:x}", Sha256::digest(std::fs::read(&archive).unwrap()));
    Runner {
        root,
        archive,
        sha256,
    }
}

fn run(runner: &Runner, api: &str, extra: &[&str], credential: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_compute"));
    runtimes::with_fixture_runtimes(&mut command);
    command.env_remove("GITHUB_TOKEN");
    if let Some(credential) = credential {
        command.env("GITHUB_TOKEN", credential);
    }
    command
        .args([
            "worker",
            "github-actions",
            "run",
            "--repository",
            "rkendel1/compute",
            "--runner-version",
            "2.331.0",
            "--runner-sha256",
            &runner.sha256,
            "--download-url",
            &format!("file://{}", runner.archive.display()),
            "--api-url",
            api,
            "--name",
            "cli-test",
            "--label",
            "compute",
            "--timeout",
            "60s",
            "--json",
        ])
        .args(extra)
        .output()
        .unwrap()
}

fn stdout_json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "not JSON ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

/// A failure before the runner ran is an error: the envelope is the last line
/// of stderr and stdout stays empty.
fn stderr_envelope(output: &Output) -> serde_json::Value {
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    serde_json::from_str(stderr.trim_end().lines().last().unwrap_or_default())
        .unwrap_or_else(|error| panic!("not JSON ({error}): {stderr}"))
}

fn assert_no_secret(label: &str, bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    assert!(
        !text.contains(TOKEN) && !text.contains(CREDENTIAL),
        "{label} leaked a secret: {text}"
    );
}

fn token_body() -> String {
    format!(r#"{{"token":"{TOKEN}","expires_at":"2030-01-01T00:00:00Z"}}"#)
}

#[test]
fn one_ephemeral_job_is_run_reported_and_cleaned_up_without_leaking_secrets() {
    let fake = runner("exit 0");
    let (api, received) = github(201, token_body());
    let receipt = fake.root.path().join("receipt.json");
    let output = run(
        &fake,
        &api,
        &["--receipt", receipt.to_str().unwrap()],
        Some(CREDENTIAL),
    );
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let document = stdout_json(&output);
    assert_eq!(document["ok"], true);
    assert_eq!(document["command"], "compute worker github-actions run");
    assert_eq!(document["exit_code"], 0);
    assert!(document["error"].is_null());
    let report = &document["data"]["report"];
    assert_eq!(report["repository"], "rkendel1/compute");
    assert_eq!(report["runner"]["version"], "2.331.0");
    assert_eq!(report["runner"]["name"], "cli-test");
    assert_eq!(report["runner"]["ephemeral"], true);
    assert_eq!(report["status"], "completed");
    assert_eq!(report["exit_code"], 0);
    assert_eq!(report["job"]["name"], "build");
    assert_eq!(report["job"]["result"], "Succeeded");
    assert_eq!(report["cleanup"]["workspace_removed"], true);
    assert!(report["started_at"].is_string() && report["finished_at"].is_string());
    assert!(report["receipt_hash"].is_string());

    // The registration token was requested with the credential...
    let request = received.lock().unwrap().to_lowercase();
    assert!(request.contains("/repos/rkendel1/compute/actions/runners/registration-token"));
    assert!(request.contains(&format!(
        "authorization: bearer {}",
        CREDENTIAL.to_lowercase()
    )));

    // ...and appears nowhere a person or a tool could read it afterwards.
    assert_no_secret("stdout", &output.stdout);
    assert_no_secret("stderr", &output.stderr);
    let receipt = std::fs::read(&receipt).expect("--receipt wrote Compute's receipt");
    assert_no_secret("receipt", &receipt);
    let receipt: serde_json::Value = serde_json::from_slice(&receipt).unwrap();
    assert_eq!(receipt["execution"]["status"], "completed");
}

#[test]
fn a_missing_credential_is_a_coded_failure_before_anything_runs() {
    let fake = runner("exit 0");
    let output = run(&fake, "http://127.0.0.1:1", &[], None);
    assert_eq!(output.status.code(), Some(1));
    let document = stderr_envelope(&output);
    assert_eq!(document["ok"], false);
    assert_eq!(document["error"]["code"], "missing_credential");
    assert!(
        document["error"]["message"]
            .as_str()
            .unwrap()
            .contains("$GITHUB_TOKEN")
    );
}

#[test]
fn an_invalid_repository_is_a_coded_failure() {
    let fake = runner("exit 0");
    let mut command = Command::new(env!("CARGO_BIN_EXE_compute"));
    runtimes::with_fixture_runtimes(&mut command);
    let output = command
        .env("GITHUB_TOKEN", CREDENTIAL)
        .args([
            "worker",
            "github-actions",
            "run",
            "--repository",
            "not a repo",
            "--runner-version",
            "2.331.0",
            "--runner-sha256",
            &fake.sha256,
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr_envelope(&output)["error"]["code"],
        "invalid_repository"
    );
    assert_no_secret("stderr", &output.stderr);
}

#[test]
fn an_unknown_repository_is_a_coded_failure_that_does_not_echo_the_credential() {
    let fake = runner("exit 0");
    let (api, _) = github(404, r#"{"message":"Not Found"}"#.into());
    let output = run(&fake, &api, &[], Some(CREDENTIAL));
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr_envelope(&output)["error"]["code"],
        "repository_not_found"
    );
    assert_no_secret("stderr", &output.stderr);
}

#[test]
fn a_failing_runner_is_reported_and_exits_with_its_status() {
    let fake = runner("exit 3");
    let (api, _) = github(201, token_body());
    let output = run(&fake, &api, &[], Some(CREDENTIAL));
    assert_eq!(output.status.code(), Some(3));
    let document = stdout_json(&output);
    assert_eq!(document["ok"], false);
    assert_eq!(document["exit_code"], 3);
    assert_eq!(document["error"]["code"], "runner_failed");
    assert_eq!(document["data"]["report"]["exit_code"], 3);
    assert_eq!(
        document["data"]["report"]["cleanup"]["workspace_removed"],
        true
    );
    assert_no_secret("stdout", &output.stdout);
}

#[test]
fn a_wrong_checksum_never_runs_the_archive() {
    let fake = runner("exit 0");
    let (api, _) = github(201, token_body());
    let wrong = Runner {
        root: tempfile::tempdir().unwrap(),
        archive: fake.archive.clone(),
        sha256: "0".repeat(64),
    };
    let output = run(&wrong, &api, &[], Some(CREDENTIAL));
    assert_eq!(output.status.code(), Some(71));
    let document = stdout_json(&output);
    assert_eq!(document["data"]["report"]["stage"], "verify");
    let _ = &fake.root;
}

#[test]
fn the_shipped_recipe_is_valid_names_no_repository_and_holds_no_credential() {
    let path: &Path = &Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/recipes/github-actions-runner.json");
    let text = std::fs::read_to_string(path).unwrap();
    let spec: compute_core::RecipeSpec = serde_json::from_str(&text).unwrap();
    assert!(spec.problems().is_empty(), "{:?}", spec.problems());
    assert_eq!(spec.lifecycle, compute_core::ComputerLifecycle::Ephemeral);
    for forbidden in [
        "token",
        "secret",
        "password",
        "ghp_",
        "github_pat_",
        "rkendel1",
    ] {
        assert!(
            !text.to_lowercase().contains(forbidden),
            "the recipe mentions {forbidden}"
        );
    }
    // And the CLI accepts it as a recipe, without a controller.
    let mut command = Command::new(env!("CARGO_BIN_EXE_compute"));
    runtimes::with_fixture_runtimes(&mut command);
    let output = command
        .env("COMPUTE_DAEMON", "http://127.0.0.1:1")
        .args(["worker", "github-actions", "run", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("--recipe-file"));
}
