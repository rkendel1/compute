//! Session acceptance through the `compute` binary:
//!
//! ```text
//! compute pool:  local (in process, no sessions)   node (compute serve)
//! compute session create --cpu 1 --memory 64Mi --ttl 1h
//!   → placement rejects local (sessions_unsupported), selects node
//! compute session exec <id> -- …     (no provider named)
//! restart compute serve → the session and its evidence are still there
//! compute remote run still works, on the same durable job machinery
//! compute session destroy <id>       (the record remains)
//! ```

#[path = "support/runtimes.rs"]
mod runtimes;
#[path = "support/targets.rs"]
mod targets;

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

struct Server(Option<Child>);

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn serve(root: &std::path::Path, port: u16) -> Server {
    let listen = format!("127.0.0.1:{port}");
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_compute"));
    runtimes::with_fixture_runtimes(&mut command);
    let child = command
        .args(["serve", "--listen", &listen, "--public-url"])
        .arg(format!("http://{listen}"))
        .arg("--job-store")
        .arg(root.join("jobs"))
        .arg("--session-store")
        .arg(root.join("sessions"))
        .arg("--credentials")
        .arg(root.join("node-credentials.json"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(&listen).is_err() {
        assert!(Instant::now() < deadline, "compute serve did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    Server(Some(child))
}

struct Cli {
    root: std::path::PathBuf,
}

impl Cli {
    fn run(&self, arguments: &[&str]) -> Output {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_compute"));
        runtimes::with_fixture_runtimes(&mut command);
        // The pool is ./compute-pool.toml and the capability cache is under
        // ./.compute: the defaults, found from the working directory.
        command
            .current_dir(&self.root)
            .env_remove("COMPUTE_POOL_CONFIG")
            .env_remove("COMPUTE_CAPABILITY_CACHE")
            .args(arguments)
            .output()
            .unwrap()
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
}

#[test]
fn a_session_is_a_durable_computer_on_whatever_provider_placement_selects() {
    let temporary = tempfile::tempdir().unwrap();
    let port = free_port();
    let root = temporary.path().to_path_buf();
    let credential = targets::issue(&root, "node", "cli");
    std::fs::write(
        root.join("compute-pool.toml"),
        format!(
            "[providers.local]\nkind = \"local\"\npriority = 100\n\
             [providers.node]\nkind = \"remote\"\nendpoint = \"http://127.0.0.1:{port}\"\n{}",
            credential.pool_line()
        ),
    )
    .unwrap();
    std::fs::write(root.join("script.sh"), "printf 'one-shot'\n").unwrap();
    let cli = Cli { root: root.clone() };
    let server = serve(&root, port);

    // Placement evaluates the session like any workload: the in-process
    // local provider cannot host sessions, so the node is selected.
    let explained = cli.run(&[
        "session",
        "create",
        "--cpu",
        "1",
        "--memory",
        "64Mi",
        "--ttl",
        "1h",
        "--provider",
        "local",
        "--refresh",
    ]);
    assert_eq!(explained.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&explained.stderr).contains("sessions_unsupported"),
        "{}",
        String::from_utf8_lossy(&explained.stderr)
    );

    let created = cli.json(&[
        "session",
        "create",
        "--cpu",
        "1",
        "--memory",
        "64Mi",
        "--ttl",
        "1h",
        "--refresh",
        "--wait",
        "--json",
    ]);
    assert_eq!(created["provider_id"], "node");
    let session = &created["session"];
    let id = session["session_id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("ses_"));
    assert!(session["job_id"].as_str().unwrap().starts_with("job_"));
    assert!(
        session["execution_id"]
            .as_str()
            .unwrap()
            .starts_with("exec_")
    );
    assert_eq!(session["node_id"], "node");
    assert_eq!(session["status"], "ready");
    assert_eq!(session["resources"]["cpu_count"], 1);
    assert_eq!(session["resources"]["memory_bytes"], 64 * 1024 * 1024);
    assert_eq!(session["capabilities"]["exec"], true);
    assert!(session["expires_at"].is_string());
    assert_eq!(
        session["placement_id"], created["placement_id"],
        "the session carries its placement"
    );

    // No provider named: the pool finds the one that holds the session.
    let output = cli.run(&[
        "session",
        "exec",
        &id,
        "--",
        "sh",
        "-c",
        "echo hi > note; cat note",
    ]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "hi\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("job: job_") && stderr.contains("execution exec_"),
        "{stderr}"
    );
    // A failing command is the workload's failure, with its exit code.
    let failed = cli.run(&["session", "exec", &id, "--", "sh", "-c", "exit 7"]);
    assert_eq!(failed.status.code(), Some(7));
    let receipt = root.join("receipt.json");
    cli.ok(&[
        "session",
        "exec",
        &id,
        "--receipt",
        receipt.to_str().unwrap(),
        "--",
        "cat",
        "note",
    ]);
    cli.ok(&["receipt", "verify", receipt.to_str().unwrap()]);

    let info = cli.ok(&["session", "info", &id]);
    for line in [
        format!("Session:     {id}"),
        "Status:      ready".into(),
        "Node:        node".into(),
        format!("Job:         {}", session["job_id"].as_str().unwrap()),
        format!("Execution:   {}", session["execution_id"].as_str().unwrap()),
        "  CPU:        1".into(),
        "  Memory:     64 MiB".into(),
        "  exec                 yes".into(),
    ] {
        assert!(info.contains(&line), "missing {line:?} in\n{info}");
    }
    let logs = cli.ok(&["session", "logs", &id]);
    assert!(logs.contains("compute-session-ready"), "{logs}");
    assert!(logs.contains("hi\n"), "{logs}");

    // Restart the service: the session and its identities are unchanged.
    drop(server);
    let server = serve(&root, port);
    let after = cli.json(&["session", "info", &id, "--json"]);
    for field in [
        "session_id",
        "job_id",
        "execution_id",
        "node_id",
        "created_at",
        "expires_at",
    ] {
        assert_eq!(
            after[field], session[field],
            "{field} changed across a restart"
        );
    }
    assert_eq!(after["status"], "ready");
    assert_eq!(
        cli.ok(&["session", "exec", &id, "--", "cat", "note"]),
        "hi\n",
        "the same environment after the restart"
    );

    // One-shot remote execution is unchanged and uses the same jobs.
    let one_shot = cli.run(&[
        "remote",
        "run",
        "--provider",
        "node",
        "--network",
        "network",
        "script.sh",
    ]);
    assert!(one_shot.status.success(), "{one_shot:?}");
    assert_eq!(String::from_utf8_lossy(&one_shot.stdout), "one-shot");
    assert!(String::from_utf8_lossy(&one_shot.stderr).contains("job: job_"));

    let detached = cli.json(&["session", "exec", &id, "--detach", "--", "true"]);
    let job_id = detached["job_id"].as_str().unwrap();
    assert!(
        detached["execution_id"]
            .as_str()
            .unwrap()
            .starts_with("exec_")
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let status = cli.json(&["remote", "status", "--provider", "node", job_id]);
        if status["status"] == "succeeded" {
            assert_eq!(status["session_id"], id.as_str());
            assert_eq!(status["execution_id"], detached["execution_id"]);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "detached execution never finished"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let destroyed = cli.json(&["session", "destroy", &id, "--json"]);
    assert_eq!(destroyed["status"], "destroyed");
    let refused = cli.run(&["session", "exec", &id, "--", "true"]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("session_conflict"),
        "{refused:?}"
    );
    let listed = cli.json(&["session", "list", "--json"]);
    let entries = listed.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["provider_id"], "node");
    assert_eq!(entries[0]["session"]["status"], "destroyed");
    drop(server);
}
