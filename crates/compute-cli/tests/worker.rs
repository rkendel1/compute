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

/// A runner whose job ends with `result`, then exits 0 as a real ephemeral
/// runner does whatever the job's result.
fn runner_with_job(result: &str) -> Runner {
    runner(&format!(
        "echo '2026-10-01 00:00:05Z: Job build completed with result: {result}'\nexit 0"
    ))
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
            "#!/bin/sh\n\
             echo '2026-10-01 00:00:00Z: Running job: build'\n\
             {run_body}\n"
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
    build(runner, api, extra, credential).output().unwrap()
}

fn build(runner: &Runner, api: &str, extra: &[&str], credential: Option<&str>) -> Command {
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
            "--api-url",
            api,
            "--name",
            "cli-test",
            "--label",
            "compute",
            "--json",
        ])
        .args(extra);
    if !extra.contains(&"--timeout") {
        command.args(["--timeout", "60s"]);
    }
    if !extra.contains(&"--download-url") && !extra.contains(&"--archive-file") {
        command.args(["--archive-file", runner.archive.to_str().unwrap()]);
    }
    command
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
    let fake = runner_with_job("Succeeded");
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

/// Everything the process could have written lives under one root: its working
/// directory, `$HOME`, `$TMPDIR` and its runtime store.
struct Sandbox {
    root: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for directory in ["cwd", "home", "tmp", "store"] {
            std::fs::create_dir(root.path().join(directory)).unwrap();
        }
        Self { root }
    }

    fn confine(&self, command: &mut Command) {
        command
            .current_dir(self.root.path().join("cwd"))
            .env("HOME", self.root.path().join("home"))
            .env("TMPDIR", self.root.path().join("tmp"))
            .env("COMPUTE_RUNTIME_STORE", self.root.path().join("store"));
    }

    /// No file under the root contains a secret, and no staged workspace is
    /// left in `$TMPDIR`.
    fn assert_clean(&self, what: &str) {
        fn walk(directory: &Path, found: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(directory).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() && !path.is_symlink() {
                    walk(&path, found);
                } else {
                    found.push(path);
                }
            }
        }
        let mut files = vec![];
        walk(self.root.path(), &mut files);
        for file in &files {
            let bytes = std::fs::read(file).unwrap_or_default();
            let text = String::from_utf8_lossy(&bytes);
            // Skip the fake runner's own files, which hold neither.
            assert!(
                !text.contains(TOKEN) && !text.contains(CREDENTIAL),
                "{what}: {} holds a secret",
                file.display()
            );
        }
        let left = std::fs::read_dir(self.root.path().join("tmp"))
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        assert!(left.is_empty(), "{what}: workspaces left behind: {left:?}");
    }
}

fn run_sandboxed(sandbox: &Sandbox, fake: &Runner, api: &str, extra: &[&str]) -> Output {
    let mut command = build(fake, api, extra, Some(CREDENTIAL));
    sandbox.confine(&mut command);
    command.output().unwrap()
}

#[test]
fn nothing_secret_is_persisted_and_nothing_is_left_behind_whatever_the_outcome() {
    // A runner that prints both secrets, then ends in each way.
    let leaky = format!(
        "echo \"token=$ACTIONS_RUNNER_INPUT_TOKEN credential={CREDENTIAL}\"\n\
         echo \"token=$ACTIONS_RUNNER_INPUT_TOKEN\" >&2\n"
    );
    let cases: [(&str, String, i32); 3] = [
        ("success", format!("{leaky}exit 0"), 0),
        ("runner failure", format!("{leaky}exit 4"), 4),
        (
            "job failure",
            format!("{leaky}echo 'x: Job build completed with result: Failed'\nexit 0"),
            1,
        ),
    ];
    for (what, body, expected) in cases {
        let sandbox = Sandbox::new();
        let fake = runner(&body);
        let (api, _) = github(201, token_body());
        let output = run_sandboxed(&sandbox, &fake, &api, &[]);
        assert_eq!(output.status.code(), Some(expected), "{what}");
        assert_no_secret(what, &output.stdout);
        assert_no_secret(what, &output.stderr);
        sandbox.assert_clean(what);
    }
}

#[test]
fn a_failed_job_is_a_failure_even_though_the_runner_exits_zero() {
    let fake = runner_with_job("Failed");
    let (api, _) = github(201, token_body());
    let output = run(&fake, &api, &[], Some(CREDENTIAL));
    assert_eq!(output.status.code(), Some(1));
    let document = stdout_json(&output);
    assert_eq!(document["ok"], false);
    assert_eq!(document["error"]["code"], "job_failed");
    let report = &document["data"]["report"];
    assert_eq!(report["exit_code"], 0, "the runner itself exited cleanly");
    assert_eq!(report["job"]["result"], "Failed");
    assert_eq!(report["job"]["name"], "build");
}

#[test]
fn a_timeout_is_reported_cleaned_up_and_leaves_no_runner_process() {
    let sandbox = Sandbox::new();
    let pid_file = sandbox.root.path().join("pid");
    let fake = runner(&format!(
        "sleep 300 &\necho $! > {}\nwait",
        pid_file.display()
    ));
    let (api, _) = github(201, token_body());
    let output = {
        let mut command = build(&fake, &api, &["--timeout", "3s"], Some(CREDENTIAL));
        sandbox.confine(&mut command);
        command.output().unwrap()
    };
    assert_eq!(output.status.code(), Some(1));
    let document = stdout_json(&output);
    assert_eq!(document["ok"], false);
    assert_eq!(document["error"]["code"], "runner_failed");
    assert_eq!(document["data"]["report"]["status"], "timed_out");
    assert_eq!(document["data"]["report"]["stage"], "run");
    assert_eq!(
        document["data"]["report"]["cleanup"]["workspace_removed"],
        true
    );
    let pid: u32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        !alive(pid),
        "the runner's descendant {pid} outlived the timeout"
    );
    assert_no_secret("stdout", &output.stdout);
    sandbox.assert_clean("timeout");
}

#[test]
fn a_signal_cancels_the_run_and_cleans_up() {
    let sandbox = Sandbox::new();
    let pid_file = sandbox.root.path().join("pid");
    let fake = runner(&format!(
        "sleep 300 &\necho $! > {}\nwait",
        pid_file.display()
    ));
    let (api, _) = github(201, token_body());
    let mut command = build(&fake, &api, &["--timeout", "120s"], Some(CREDENTIAL));
    sandbox.confine(&mut command);
    let child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    for _ in 0..200 {
        if pid_file.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(pid_file.exists(), "the runner started");
    let descendant: u32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let document = stdout_json(&output);
    assert_eq!(document["data"]["report"]["status"], "cancelled");
    assert_eq!(
        document["data"]["report"]["cleanup"]["workspace_removed"],
        true
    );
    assert!(
        !alive(descendant),
        "the descendant {descendant} outlived the cancellation"
    );
    assert_no_secret("stdout", &output.stdout);
    assert_no_secret("stderr", &output.stderr);
    sandbox.assert_clean("cancellation");
}

fn alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .and_then(|rest| rest.split_whitespace().next())
            .is_some_and(|state| state != "Z"),
        Err(_) => false,
    }
}

#[test]
fn the_download_must_be_https_and_the_archive_file_absolute_and_real() {
    let fake = runner_with_job("Succeeded");
    for extra in [
        vec!["--download-url", "http://example.com/runner.tar.gz"],
        vec!["--download-url", "file:///etc/passwd"],
        vec!["--download-url", "ftp://example.com/runner.tar.gz"],
        vec!["--archive-file", "relative.tar.gz"],
        vec!["--archive-file", "/does/not/exist.tar.gz"],
    ] {
        let output = run(&fake, "http://127.0.0.1:1", &extra, Some(CREDENTIAL));
        assert_eq!(output.status.code(), Some(1), "{extra:?}");
        assert_eq!(
            stderr_envelope(&output)["error"]["code"],
            "invalid_runner_spec",
            "{extra:?}"
        );
    }
}
