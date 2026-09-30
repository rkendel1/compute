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
#[path = "support/targets.rs"]
mod targets;

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
    let credential = targets::issue(&root, "target-a", "control-plane");
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
            .arg("--credentials")
            .arg(&credential.credentials)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for_port(&target_listen);
    std::fs::write(
        root.join("compute-pool.toml"),
        format!(
            "[providers.target-a]\nkind = \"remote\"\nendpoint = \"http://{target_listen}\"\n{}",
            credential.pool_line()
        ),
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
        "Computer:    running (desired running; persistent) on target-a",
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
    let credential = targets::issue(&root, "target-a", "control-plane");
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
            .arg("--credentials")
            .arg(&credential.credentials)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for_port(&target_listen);
    std::fs::write(
        root.join("compute-pool.toml"),
        format!(
            "[providers.target-a]\nkind = \"remote\"\nendpoint = \"http://{target_listen}\"\n{}",
            credential.pool_line()
        ),
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

/// What the CLI says about a computer is what Compute last established
/// with its target: `environment status` never says running because the
/// environment wants it running. The target going away, coming back, and
/// losing its sessions are each shown as they are.
#[test]
fn the_cli_reports_observed_reality_not_desired_state() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().to_path_buf();
    let source = repository(&root);
    let target_listen = format!("127.0.0.1:{}", free_port());
    let credential = targets::issue(&root, "target-a", "control-plane");
    let serve = || {
        let mut serve = std::process::Command::new(env!("CARGO_BIN_EXE_compute"));
        runtimes::with_fixture_runtimes(&mut serve);
        let child = Serve(
            serve
                .args(["serve", "--listen", &target_listen, "--public-url"])
                .arg(format!("http://{target_listen}"))
                .arg("--job-store")
                .arg(root.join("target-jobs"))
                .arg("--session-store")
                .arg(root.join("target-sessions"))
                .arg("--credentials")
                .arg(&credential.credentials)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        wait_for_port(&target_listen);
        child
    };
    let mut target = Some(serve());
    std::fs::write(
        root.join("compute-pool.toml"),
        format!(
            "[providers.target-a]\nkind = \"remote\"\nendpoint = \"http://{target_listen}\"\n{}",
            credential.pool_line()
        ),
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
    let running = cli.computer_until("running", |view| {
        view["converged"] == true && view["reality"]["observed"] == "running"
    });
    let session = running["session_id"].as_str().unwrap().to_owned();
    let status = cli.ok(&["environment", "status", "myapp"]);
    assert!(
        status.contains("State: desired running, actual running, health healthy"),
        "{status}"
    );
    assert!(
        status.contains("Computer:    running (desired running;"),
        "{status}"
    );
    assert!(status.contains("Confirmed:"), "{status}");
    // Readiness: the CLI text and the API's JSON are one view.
    assert!(status.contains("Readiness:   ready"), "{status}");
    assert!(status.contains("✓ requirements"), "{status}");
    assert_eq!(running["readiness"]["state"], "ready");
    let environment = cli.json(&["environment", "status", "myapp", "--json"]);
    assert_eq!(environment["computer"]["readiness"]["state"], "ready");

    // The target goes away: unreachable, still wanted.
    drop(target.take());
    let unreachable = cli.computer_until("unreachable", |view| {
        view["reality"]["observed"] == "unreachable"
    });
    assert_eq!(unreachable["reality"]["desired"], "running");
    assert_eq!(unreachable["status"], "unreachable");
    let status = cli.ok(&["environment", "status", "myapp"]);
    assert!(
        status.contains("actual degraded, health unhealthy"),
        "{status}"
    );
    assert!(
        status.contains("Computer:    unreachable (desired running;"),
        "{status}"
    );
    assert!(status.contains("Compute keeps checking"), "{status}");
    assert!(status.contains("Readiness:   unavailable"), "{status}");
    assert_eq!(unreachable["readiness"]["state"], "unavailable");
    let listed = cli.json(&["environment", "list", "--json"]);
    let row = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "myapp")
        .unwrap();
    assert_eq!(row["reality"]["observed"], "unreachable");
    assert_eq!(row["computer"], "unreachable");
    let refused = cli.run(&["environment", "exec", "myapp", "--", "true"]);
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("runtime unavailable") && stderr.contains("unreachable"),
        "{stderr}"
    );

    // The target comes back with its stores: the same machine, running.
    target = Some(serve());
    let recovered =
        cli.computer_until("recovered", |view| view["reality"]["observed"] == "running");
    assert_eq!(recovered["session_id"], session.as_str());

    // The target comes back without its sessions: lost, still wanted.
    drop(target.take());
    cli.computer_until("unreachable again", |view| {
        view["reality"]["observed"] == "unreachable"
    });
    std::fs::remove_dir_all(root.join("target-sessions")).unwrap();
    target = Some(serve());
    let lost = cli.computer_until("lost", |view| view["reality"]["observed"] == "lost");
    assert_eq!(lost["reality"]["desired"], "running");
    assert_eq!(lost["failure"]["code"], "session_missing");
    let status = cli.ok(&["environment", "status", "myapp"]);
    assert!(
        status.contains("actual failed, health unhealthy"),
        "{status}"
    );
    assert!(
        status.contains("Computer:    lost (desired running;"),
        "{status}"
    );
    assert!(status.contains("replace the computer"), "{status}");
    let refused = cli.run(&["environment", "exec", "myapp", "--", "true"]);
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        !refused.status.success() && stderr.contains("lost"),
        "{stderr}"
    );
    // Reconcile asks again; the machine is still gone.
    let reconciled = cli.json(&["environment", "reconcile", "myapp", "--json"]);
    assert_eq!(reconciled["reality"]["observed"], "lost");

    // Replace: a new machine with the same contents.
    cli.json(&[
        "environment",
        "replace",
        "myapp",
        "--cpu",
        "1",
        "--memory",
        "64Mi",
        "--json",
    ]);
    let replaced = cli.computer_until("replaced", |view| {
        view["reality"]["observed"] == "running"
            && view["converged"] == true
            && view["session_id"] != session.as_str()
    });
    assert_eq!(replaced["desired"]["repositories"][0]["revision"], "v1");
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
    let events = cli.ok(&["events", "--environment", "myapp"]);
    for kind in [
        "computer.unreachable",
        "computer.recovered",
        "computer.lost",
        "computer.replacing",
    ] {
        assert!(events.contains(kind), "{kind} missing from\n{events}");
    }
    drop(target);
}

/// A target and a controller whose pool names it, both separate processes.
fn controller_with_target(root: &Path) -> (Serve, Cli, String) {
    let target_listen = format!("127.0.0.1:{}", free_port());
    let credential = targets::issue(root, "target-a", "control-plane");
    let mut serve = std::process::Command::new(env!("CARGO_BIN_EXE_compute"));
    runtimes::with_fixture_runtimes(&mut serve);
    let target = Serve(
        serve
            .args(["serve", "--listen", &target_listen, "--public-url"])
            .arg(format!("http://{target_listen}"))
            .arg("--job-store")
            .arg(root.join("target-jobs"))
            .arg("--session-store")
            .arg(root.join("target-sessions"))
            .arg("--credentials")
            .arg(&credential.credentials)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for_port(&target_listen);
    std::fs::write(
        root.join("compute-pool.toml"),
        format!(
            "[providers.target-a]\nkind = \"remote\"\nendpoint = \"http://{target_listen}\"\n{}",
            credential.pool_line()
        ),
    )
    .unwrap();
    let listen = format!("127.0.0.1:{}", free_port());
    let cli = Cli {
        daemon: format!("http://{listen}"),
        root: root.to_path_buf(),
    };
    cli.start(&listen);
    (target, cli, listen)
}

/// Serves `/health` as ready, and records every start in `starts.log`.
const READY_SERVICE: &str = r#"
import http.server, os
with open("starts.log", "a") as log:
    log.write(f"{os.getpid()}\n")
class Health(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.end_headers()
    def log_message(self, *args):
        pass
http.server.HTTPServer(("127.0.0.1", int(os.environ["PORT"])), Health).serve_forever()
"#;

/// Readiness and restarts through the CLI, with the controller a process
/// of its own that is stopped and started: what one controller process
/// decided, the next reads back from control state.
#[test]
fn readiness_and_restarts_are_shown_and_survive_controller_process_restarts() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().to_path_buf();
    let (_target, cli, listen) = controller_with_target(&root);
    cli.ok(&[
        "environment",
        "create",
        "myapp",
        "--cpu",
        "1",
        "--memory",
        "64Mi",
        "--persistent",
    ]);
    let port = free_port().to_string();
    cli.ok(&[
        "environment",
        "process",
        "add",
        "myapp",
        "web",
        "--port",
        &port,
        "--ready-path",
        "/health",
        "--ready-deadline",
        "30",
        "--restart",
        "on-failure",
        "--",
        "python3",
        "-c",
        READY_SERVICE,
    ]);
    let web = |view: &Value| view["reality"]["processes"]["web"].clone();
    let ready = cli.computer_until("ready", |view| web(view)["process"] == "ready");
    assert_eq!(web(&ready)["desired"], "running");
    assert_eq!(web(&ready)["readiness"], "ready");
    assert_eq!(web(&ready)["restart_policy"], "on_failure");
    assert_eq!(web(&ready)["restarts"], 0);
    let pid = web(&ready)["pid"].as_u64().unwrap().to_string();

    // Killed behind Compute's back: a failure, restarted, shown.
    cli.ok(&["environment", "exec", "myapp", "--", "kill", &pid]);
    let restarted = cli.computer_until("the restart", |view| {
        web(view)["restarts"] == 1 && web(view)["process"] == "ready"
    });
    assert_eq!(web(&restarted)["last_failure"]["exit_code"], 143);
    let info = cli.ok(&["environment", "info", "myapp"]);
    for line in [
        "readiness: ready (GET /health expects 2xx; last: HTTP 200)",
        "restart: on_failure (0 in a row of at most 5)",
        "last failure: web exited with status 143 (exited,",
    ] {
        assert!(info.contains(line), "{line:?} missing from\n{info}");
    }
    let pid = web(&restarted)["pid"].as_u64().unwrap().to_string();

    // The controller process stops; the service exits while none runs.
    cli.stop();
    let killed = std::process::Command::new("kill")
        .arg(&pid)
        .status()
        .unwrap();
    assert!(killed.success());
    // A new controller process recovers it from the record: the count goes
    // on from where the last one left it.
    cli.start(&listen);
    let recovered = cli.computer_until("the recovery", |view| {
        web(view)["restarts"] == 2 && web(view)["process"] == "ready"
    });
    assert_ne!(web(&recovered)["pid"].as_u64().unwrap().to_string(), pid);
    let starts = cli.ok(&["environment", "exec", "myapp", "--", "cat", "starts.log"]);
    assert_eq!(
        starts.lines().count(),
        3,
        "one start per restart, no duplicate"
    );

    // Stopped on purpose: stays stopped across a controller restart.
    cli.ok(&["environment", "process", "stop", "myapp", "web"]);
    cli.computer_until("stopped", |view| web(view)["process"] == "stopped");
    cli.stop();
    cli.start(&listen);
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let view = cli.computer_until("the record", |_| true);
        assert_eq!(web(&view)["process"], "stopped", "{view:#}");
        assert_eq!(web(&view)["restarts"], 2);
        std::thread::sleep(Duration::from_millis(200));
    }
    let starts = cli.ok(&["environment", "exec", "myapp", "--", "cat", "starts.log"]);
    assert_eq!(starts.lines().count(), 3);
}

/// Checkpoint and fork through the CLI: the same verified workspace state,
/// durable in one and a new environment in the other.
#[test]
fn a_checkpoint_and_a_fork_carry_the_same_verified_workspace_from_the_cli() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().to_path_buf();
    let (_target, cli, _listen) = controller_with_target(&root);
    cli.ok(&[
        "environment",
        "create",
        "myapp",
        "--cpu",
        "1",
        "--memory",
        "64Mi",
        "--persistent",
    ]);
    cli.computer_until("running", |view| view["reality"]["observed"] == "running");
    cli.ok(&[
        "environment",
        "exec",
        "myapp",
        "--",
        "sh",
        "-c",
        "mkdir -p data && printf hello > data/notes.txt",
    ]);
    let workspace = cli.json(&["environment", "workspace", "verify", "myapp", "--json"]);
    let digest = workspace["digest"].as_str().unwrap().to_owned();

    let text = cli.ok(&["environment", "checkpoint", "myapp"]);
    assert!(text.contains("Checkpoint ckp_"), "{text}");
    assert!(text.contains(&digest), "{text}");
    assert!(text.contains("compute.checkpoint@1"), "{text}");
    assert!(text.contains("Verified:  true"), "{text}");
    let captured = cli.json(&["environment", "checkpoint", "myapp", "--json"]);
    assert_eq!(
        captured["existing"], true,
        "the same state, the same checkpoint"
    );
    let id = captured["checkpoint_id"].as_str().unwrap().to_owned();
    assert_eq!(captured["workspace"], digest.as_str());

    let listed = cli.json(&["environment", "checkpoints", "myapp", "--json"]);
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["checkpoint_id"], id.as_str());
    let shown = cli.json(&["environment", "checkpoints", "myapp", &id, "--json"]);
    assert_eq!(shown["valid"], true, "{shown}");
    assert_eq!(shown["workspace_digest"], digest.as_str());
    let text = cli.ok(&["environment", "checkpoints", "myapp", &id]);
    assert!(text.contains("Valid:     true"), "{text}");
    let refused = cli.run(&["environment", "checkpoints", "myapp", "ckp_missing"]);
    assert!(!refused.status.success());

    // A fork carries the same state into a new environment.
    let forked = cli.json(&["environment", "fork", "myapp", "copy", "--json"]);
    assert_eq!(forked["workspace"], digest.as_str());
    assert_eq!(forked["workspace_verified"], true);
    assert_eq!(forked["environment"], "copy");
    let origin = cli.json(&["environment", "computer", "myapp", "--json"]);
    assert_ne!(
        forked["computer"]["environment_id"],
        origin["environment_id"]
    );
    assert_ne!(forked["computer"]["session_id"], origin["session_id"]);
    assert_eq!(
        cli.ok(&["environment", "exec", "copy", "--", "cat", "data/notes.txt"]),
        "hello"
    );

    // A restore carries the checkpoint's state into another new environment,
    // independent of the fork.
    let text = cli.ok(&["environment", "restore", &id, "revived"]);
    assert!(
        text.contains(&format!("Restored checkpoint {id} into revived")),
        "{text}"
    );
    assert!(text.contains(&digest), "{text}");
    assert!(
        text.contains("verified inside the new computer: true"),
        "{text}"
    );
    let restored = cli.json(&["environment", "restore", &id, "revived-two", "--json"]);
    assert_eq!(restored["checkpoint_id"], id.as_str());
    assert_eq!(restored["workspace"], digest.as_str());
    assert_eq!(restored["workspace_verified"], true);
    assert_ne!(restored["environment_id"], origin["environment_id"]);
    assert_ne!(restored["computer"]["session_id"], origin["session_id"]);
    assert_eq!(
        cli.ok(&[
            "environment",
            "exec",
            "revived-two",
            "--",
            "cat",
            "data/notes.txt"
        ]),
        "hello"
    );
    let refused = cli.run(&["environment", "restore", &id, "revived"]);
    assert!(!refused.status.success(), "a name in use is refused");
    let refused = cli.run(&["environment", "restore", "ckp_missing", "ghost"]);
    assert!(!refused.status.success());
}

/// Configuration through the CLI: imported from files, shown without values,
/// and never printed anywhere an operator looks.
#[test]
fn env_files_are_imported_through_the_cli_and_no_output_shows_a_secret() {
    const SECRET: &str = "sk_live_51NotARealKeyJustATest";
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().to_path_buf();
    let (_target, cli, _listen) = controller_with_target(&root);
    cli.ok(&[
        "environment",
        "create",
        "myapp",
        "--cpu",
        "1",
        "--memory",
        "64Mi",
        "--persistent",
    ]);
    cli.computer_until("running", |view| view["reality"]["observed"] == "running");
    std::fs::write(
        root.join(".env"),
        format!("DATABASE_URL=postgres://u:p@db/app\nSTRIPE_SECRET_KEY={SECRET}\nPORT=3000\nAPP_MODE=test\n"),
    )
    .unwrap();
    std::fs::write(root.join(".env.local"), "APP_MODE=local\n").unwrap();

    let text = cli.ok(&[
        "environment",
        "config",
        "import",
        "myapp",
        ".env",
        ".env.local",
    ]);
    assert!(text.contains("Imported 3 variables into myapp"), "{text}");
    assert!(
        text.contains("STRIPE_SECRET_KEY") && text.contains("secret"),
        "{text}"
    );
    assert!(
        text.contains("PORT") && text.contains("skipped: reserved"),
        "{text}"
    );
    assert!(
        text.contains("defined in more than one file") && text.contains("APP_MODE"),
        "{text}"
    );
    assert!(
        !text.contains(SECRET) && !text.contains("postgres://"),
        "{text}"
    );

    let shown = cli.ok(&["environment", "config", "myapp"]);
    assert!(shown.contains("configuration generation"), "{shown}");
    assert!(
        shown.contains("DATABASE_URL") && shown.contains("configured") && shown.contains("secret"),
        "{shown}"
    );
    assert!(
        shown.contains("APP_MODE") && shown.contains("= local"),
        "a public value is shown: {shown}"
    );
    assert!(
        !shown.contains(SECRET) && !shown.contains("postgres://"),
        "{shown}"
    );
    let json = cli.json(&["environment", "config", "myapp", "--json"]);
    assert_eq!(json["variables"].as_array().unwrap().len(), 3);
    let stripe = json["variables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|variable| variable["name"] == "STRIPE_SECRET_KEY")
        .unwrap();
    assert_eq!(
        (
            stripe["configured"].clone(),
            stripe["sensitive"].clone(),
            stripe["source"].clone()
        ),
        (true.into(), true.into(), ".env".into())
    );
    assert!(stripe.get("value").is_none());

    // The same secret is in no other output an operator can ask for.
    for arguments in [
        vec!["environment", "computer", "myapp", "--json"],
        vec!["environment", "status", "myapp"],
        vec!["events", "--environment", "myapp"],
        vec!["environment", "list", "--json"],
    ] {
        let output = cli.ok(&arguments);
        assert!(
            !output.contains(SECRET) && !output.contains("postgres://"),
            "{arguments:?}:\n{output}"
        );
    }

    // A malformed file is refused whole, without echoing it.
    std::fs::write(root.join("bad.env"), format!("GOOD=1\n{SECRET}\n")).unwrap();
    let refused = cli.run(&["environment", "config", "import", "myapp", "bad.env"]);
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("line 2") && !stderr.contains(SECRET),
        "{stderr}"
    );
    assert!(!cli.ok(&["environment", "config", "myapp"]).contains("GOOD"));

    // Set and remove, and name what may be shown.
    cli.ok(&[
        "environment",
        "config",
        "myapp",
        "--set",
        "REGION=eu",
        "--set",
        "SALT_LEVEL=7",
        "--secret",
        "SALT_LEVEL",
        "--unset",
        "APP_MODE",
    ]);
    let shown = cli.ok(&["environment", "config", "myapp"]);
    assert!(
        shown.contains("REGION") && shown.contains("= eu") && shown.contains("cli"),
        "{shown}"
    );
    assert!(
        shown.contains("SALT_LEVEL") && !shown.contains("= 7"),
        "{shown}"
    );
    assert!(!shown.contains("APP_MODE"), "{shown}");

    // Discovery reads names from the workspace, never values.
    cli.ok(&[
        "environment",
        "exec",
        "myapp",
        "--",
        "sh",
        "-c",
        "printf 'A_KEY=\\nB_MODE=1\\n' > .env.example",
    ]);
    let discovered = cli.ok(&["environment", "config", "discover", "myapp"]);
    assert!(
        discovered.contains(".env.example (requirements)"),
        "{discovered}"
    );
    assert!(
        discovered.contains("A_KEY") && discovered.contains("missing"),
        "{discovered}"
    );
    let json = cli.json(&["environment", "config", "discover", "myapp", "--json"]);
    assert_eq!(json["files"][0]["kind"], "requirements");
}
