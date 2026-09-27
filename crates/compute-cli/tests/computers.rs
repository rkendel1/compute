//! Acceptance through the `compute` binary: a persistent environment on a
//! computer, changed in place, across a controller restart.
//!
//! ```text
//! compute serve            a target that hosts computers
//! compute start            the controller, its pool naming that target
//! compute environment create myapp --cpu 1 --memory 64Mi --persistent
//! compute environment repo add / process add      → the computer changes
//! compute environment repo update --revision v2   → it changes in place
//! compute stop; compute start                     → it is still there
//! compute environment destroy myapp               → its record remains
//! ```

#[path = "support/runtimes.rs"]
mod runtimes;

use std::path::{Path, PathBuf};
use std::process::{Child, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_for_port(address: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while std::net::TcpStream::connect(address).is_err() {
        assert!(Instant::now() < deadline, "{address} never listened");
        std::thread::sleep(Duration::from_millis(20));
    }
}

struct Serve(Child);

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Cli {
    daemon: String,
    root: PathBuf,
}

impl Cli {
    fn command(&self, arguments: &[&str]) -> std::process::Command {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_compute"));
        runtimes::with_fixture_runtimes(&mut command);
        command
            .current_dir(&self.root)
            .args(arguments)
            .env("COMPUTE_DAEMON", &self.daemon)
            .env("COMPUTE_DAEMON_TOKEN", "secret");
        command
    }

    fn run(&self, arguments: &[&str]) -> Output {
        self.command(arguments).output().unwrap()
    }

    fn ok(&self, arguments: &[&str]) -> String {
        let output = self.run(arguments);
        assert!(
            output.status.success(),
            "compute {arguments:?} failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn json(&self, arguments: &[&str]) -> Value {
        serde_json::from_str(&self.ok(arguments)).unwrap()
    }

    fn start(&self, listen: &str) {
        let pool = self.root.join("compute-pool.toml");
        let state = self.root.join("state");
        let output = self
            .command(&["start", "--detach", "--listen", listen])
            .args(["--require-token-env", "COMPUTE_DAEMON_TOKEN"])
            .args(["--reconcile-interval-ms", "200", "--state-dir"])
            .arg(&state)
            .arg("--pool-config")
            .arg(&pool)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        wait_for_port(listen);
    }

    fn stop(&self) {
        let _ = self.run(&["stop"]);
    }

    fn computer_until(&self, what: &str, wanted: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut last = Value::Null;
        loop {
            let output = self.run(&["environment", "computer", "myapp", "--json"]);
            if output.status.success() {
                last = serde_json::from_slice(&output.stdout).unwrap();
                if wanted(&last) {
                    return last;
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {last:#}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Cli {
    fn drop(&mut self) {
        self.stop();
    }
}

fn repository(root: &Path) -> PathBuf {
    let source = root.join("app");
    std::fs::create_dir_all(&source).unwrap();
    let git = |arguments: &[&str]| {
        let output = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(arguments)
            .current_dir(&source)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    };
    git(&["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(
        source.join("serve.sh"),
        "cat VERSION > \"$COMPUTE_SESSION_WORKSPACE/running-version\"\nexec sleep 600\n",
    )
    .unwrap();
    for version in ["v1", "v2"] {
        std::fs::write(source.join("VERSION"), version).unwrap();
        git(&["add", "."]);
        git(&["commit", "--quiet", "-m", version]);
        git(&["tag", version]);
    }
    source
}

#[test]
fn a_computer_is_created_once_and_changed_in_place_from_the_cli() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().to_path_buf();
    let source = repository(&root);
    let target_listen = format!("127.0.0.1:{}", free_port());
    let mut serve = std::process::Command::new(env!("CARGO_BIN_EXE_compute"));
    runtimes::with_fixture_runtimes(&mut serve);
    let _target = Serve(
        serve
            .args(["serve", "--listen", &target_listen, "--public-url"])
            .arg(format!("http://{target_listen}"))
            .arg("--job-store")
            .arg(root.join("target-jobs"))
            .arg("--session-store")
            .arg(root.join("target-sessions"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for_port(&target_listen);
    std::fs::write(
        root.join("compute-pool.toml"),
        format!("[providers.target-a]\nkind = \"remote\"\nendpoint = \"http://{target_listen}\"\n"),
    )
    .unwrap();
    let listen = format!("127.0.0.1:{}", free_port());
    let cli = Cli {
        daemon: format!("http://{listen}"),
        root: root.clone(),
    };
    cli.start(&listen);

    // The targets the controller can place computers on.
    let targets = cli.json(&["target", "list", "--json"]);
    let target = targets
        .as_array()
        .unwrap()
        .iter()
        .find(|target| target["target_id"] == "target-a")
        .unwrap();
    assert_eq!(target["hosts_computers"], true);

    // Describe the computer; Compute decides where it runs.
    let created = cli.json(&[
        "environment",
        "create",
        "myapp",
        "--cpu",
        "1",
        "--memory",
        "64Mi",
        "--persistent",
        "--json",
    ]);
    assert_eq!(created["computer"]["lifecycle"], "persistent");
    let url = source.display().to_string();
    cli.ok(&[
        "environment",
        "repo",
        "add",
        "myapp",
        "app",
        "--url",
        &url,
        "--revision",
        "v1",
    ]);
    cli.ok(&[
        "environment",
        "process",
        "add",
        "myapp",
        "api",
        "--repository",
        "app",
        "--",
        "sh",
        "serve.sh",
    ]);
    let running = cli.computer_until("the application", |view| {
        view["converged"] == true && view["desired"]["generation"] == 3
    });
    assert_eq!(running["target"], "target-a");
    let session = running["session_id"].as_str().unwrap().to_owned();
    assert_eq!(
        cli.ok(&[
            "environment",
            "exec",
            "myapp",
            "--",
            "cat",
            "running-version"
        ]),
        "v1"
    );

    // A new revision: the same computer, changed in place.
    cli.ok(&[
        "environment",
        "repo",
        "update",
        "myapp",
        "app",
        "--url",
        &url,
        "--revision",
        "v2",
    ]);
    let changed = cli.computer_until("the new revision", |view| {
        view["converged"] == true && view["desired"]["generation"] == 4
    });
    assert_eq!(changed["session_id"], session.as_str(), "no redeployment");
    assert_eq!(
        cli.ok(&[
            "environment",
            "exec",
            "myapp",
            "--",
            "cat",
            "running-version"
        ]),
        "v2"
    );
    let info = cli.ok(&["environment", "info", "myapp"]);
    for line in [
        "Computer:    running (persistent) on target-a",
        "Repositories",
        "Processes",
        "api",
    ] {
        assert!(info.contains(line), "{line:?} missing from\n{info}");
    }
    // A failing command is the command's failure, with its exit code.
    let failed = cli.run(&["environment", "exec", "myapp", "--", "sh", "-c", "exit 4"]);
    assert_eq!(failed.status.code(), Some(4));

    // The controller restarts; the computer and what it holds do not.
    cli.stop();
    cli.start(&listen);
    let after = cli.computer_until("the computer after a restart", |view| {
        view["converged"] == true
    });
    assert_eq!(after["session_id"], session.as_str());
    assert_eq!(
        cli.ok(&[
            "environment",
            "exec",
            "myapp",
            "--",
            "cat",
            "running-version"
        ]),
        "v2"
    );

    // Destroyed: the machine goes, the record stays.
    cli.ok(&["environment", "destroy", "myapp"]);
    let destroyed = cli.computer_until("destruction", |view| view["status"] == "destroyed");
    assert_eq!(
        destroyed["observed"]["repositories"]["app"]["revision"],
        "v2"
    );
    let refused = cli.run(&["environment", "process", "stop", "myapp", "api"]);
    assert!(!refused.status.success());
}

/// Deployment and interactive work through the CLI, against the same
/// environment: `compute deploy` on an environment with a computer is a
/// release, reconciled in place; build, test, and project commands run in
/// the computer; a work session enters and leaves it.
#[test]
fn deploy_is_a_release_and_work_runs_in_the_computer() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().to_path_buf();
    let source = repository(&root);
    let target_listen = format!("127.0.0.1:{}", free_port());
    let mut serve = std::process::Command::new(env!("CARGO_BIN_EXE_compute"));
    runtimes::with_fixture_runtimes(&mut serve);
    let _target = Serve(
        serve
            .args(["serve", "--listen", &target_listen, "--public-url"])
            .arg(format!("http://{target_listen}"))
            .arg("--job-store")
            .arg(root.join("target-jobs"))
            .arg("--session-store")
            .arg(root.join("target-sessions"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for_port(&target_listen);
    std::fs::write(
        root.join("compute-pool.toml"),
        format!("[providers.target-a]\nkind = \"remote\"\nendpoint = \"http://{target_listen}\"\n"),
    )
    .unwrap();
    let listen = format!("127.0.0.1:{}", free_port());
    let cli = Cli {
        daemon: format!("http://{listen}"),
        root: root.clone(),
    };
    cli.start(&listen);

    cli.json(&[
        "environment",
        "create",
        "myapp",
        "--cpu",
        "1",
        "--memory",
        "64Mi",
        "--persistent",
        "--json",
    ]);
    let url = source.display().to_string();
    cli.ok(&[
        "environment",
        "repo",
        "add",
        "myapp",
        "app",
        "--url",
        &url,
        "--revision",
        "v1",
    ]);
    cli.ok(&[
        "environment",
        "project",
        "add",
        "myapp",
        "app",
        "--repository",
        "app",
        "--build",
        "cat VERSION > BUILT",
        "--test",
        "grep -q v BUILT",
        "--command",
        "where=pwd",
    ]);
    cli.ok(&[
        "environment",
        "service",
        "add",
        "myapp",
        "api",
        "--repository",
        "app",
        "--port",
        "18556",
        "--",
        "sh",
        "serve.sh",
    ]);
    let running = cli.computer_until("the first build", |view| {
        view["converged"] == true
            && view["observed"]["builds"]["app"]["evidence"]["outcome"] == "succeeded"
    });
    let resource = running["machine"]["resource"].clone();
    assert!(resource.is_string());

    // Work runs inside the computer: the target's session store.
    let built = cli.ok(&["environment", "build", "myapp"]);
    assert!(built.is_empty(), "{built}");
    cli.ok(&["environment", "test", "myapp", "app"]);
    let directory = cli.ok(&["environment", "run", "myapp", "app", "where"]);
    assert!(
        directory
            .trim()
            .starts_with(&root.join("target-sessions").display().to_string()),
        "{directory}"
    );
    assert_eq!(
        cli.ok(&[
            "environment",
            "exec",
            "myapp",
            "--",
            "cat",
            "repos/app/BUILT"
        ]),
        "v1"
    );

    // `compute deploy` to this environment is a release: the same machine.
    let deployed = cli.ok(&[
        "deploy",
        "app",
        "--environment",
        "myapp",
        "--revision",
        "v2",
    ]);
    assert!(deployed.contains("on the same machine"), "{deployed}");
    assert_eq!(
        cli.ok(&[
            "environment",
            "exec",
            "myapp",
            "--",
            "cat",
            "running-version"
        ]),
        "v2"
    );
    let after = cli.computer_until("the release", |view| view["converged"] == true);
    assert_eq!(after["machine"]["resource"], resource);
    let refused = cli.run(&["deploy", "app", "--environment", "myapp"]);
    assert!(!refused.status.success(), "a release names its revision");

    // A work session enters the environment and leaves it running.
    let session = cli.json(&["session", "open", "myapp", "--json"]);
    assert_eq!(session["kind"], "attached");
    let id = session["session_id"].as_str().unwrap().to_owned();
    let listed = cli.json(&["session", "opened", "--environment", "myapp", "--json"]);
    assert_eq!(listed[0]["session_id"], id.as_str());
    let closed = cli.json(&["session", "close", &id, "--json"]);
    assert_eq!(closed["status"], "closed");
    let still = cli.computer_until("the environment after the session", |view| {
        view["status"] == "running"
    });
    assert_eq!(still["machine"]["resource"], resource);

    // The lifetime changes in place.
    cli.ok(&[
        "environment",
        "lifetime",
        "myapp",
        "--temporary",
        "--ttl",
        "1h",
    ]);
    let temporary = cli.computer_until("a temporary lifetime", |view| {
        view["lifecycle"] == "ephemeral"
    });
    assert_eq!(temporary["machine"]["resource"], resource);
}
