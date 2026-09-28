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

impl Server {
    fn stop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

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

/// `compute serve` is controlled only by the control planes it trusts:
/// no credential, a wrong one, or one it revoked is refused; another
/// control plane's valid credential reaches none of this one's sessions;
/// a restart keeps who owns what; and the only open mode is named.
#[test]
fn a_target_is_controlled_only_by_the_control_planes_it_trusts() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().to_path_buf();
    let cli = Cli { root: root.clone() };

    // A target that trusts nobody does not start.
    let refused = std::process::Command::new(env!("CARGO_BIN_EXE_compute"))
        .args(["serve", "--listen", "127.0.0.1:0", "--credentials"])
        .arg(root.join("nobody.json"))
        .output()
        .unwrap();
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("compute target credential issue")
            && stderr.contains("--insecure-unauthenticated"),
        "{stderr}"
    );

    let port = free_port();
    let a = targets::issue(&root, "node", "control-plane-a");
    let b = targets::issue(&root, "node", "control-plane-b");
    let wrong = root.join("wrong.token");
    std::fs::write(&wrong, "cmpt_tcred_0000000000000000_00\n").unwrap();
    let pool = |name: &str, token: Option<&std::path::Path>| {
        let path = root.join(format!("pool-{name}.toml"));
        std::fs::write(
            &path,
            format!(
                "[providers.node]\nkind = \"remote\"\nendpoint = \"http://127.0.0.1:{port}\"\n{}",
                token
                    .map(|token| format!("token_file = {:?}\n", token.display().to_string()))
                    .unwrap_or_default()
            ),
        )
        .unwrap();
        path.display().to_string()
    };
    let (pool_a, pool_b, pool_none, pool_wrong) = (
        pool("a", Some(&a.token_file)),
        pool("b", Some(&b.token_file)),
        pool("none", None),
        pool("wrong", Some(&wrong)),
    );
    let mut server = serve(&root, port);
    let capabilities = |pool: &str| {
        cli.run(&[
            "remote",
            "capabilities",
            "--provider",
            "node",
            "--json",
            "--pool-config",
            pool,
        ])
    };

    // 1–2. No credential, or a wrong one: refused, by structured kind.
    for pool in [&pool_none, &pool_wrong] {
        let output = capabilities(pool);
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("unauthorized:"), "{stderr}");
    }
    // 3. The control plane's credential: accepted, and the target says how
    // it authenticates.
    let output = capabilities(&pool_a);
    assert!(output.status.success(), "{output:?}");
    let advertised: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(advertised["authentication"], "credential");

    // 4. The authenticated control plane creates, lists, and runs in its
    // session.
    let created = cli.json(&[
        "session",
        "create",
        "--cpu",
        "1",
        "--memory",
        "64Mi",
        "--provider",
        "node",
        "--wait",
        "--json",
        "--pool-config",
        &pool_a,
    ]);
    let id = created["session"]["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let listed = cli.json(&["session", "list", "--json", "--pool-config", &pool_a]);
    assert!(listed.to_string().contains(&id), "{listed}");
    let output = cli.run(&[
        "session",
        "exec",
        &id,
        "--provider",
        "node",
        "--pool-config",
        &pool_a,
        "--",
        "sh",
        "-c",
        "echo mine",
    ]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "mine\n");

    // 5. Another control plane, with a valid credential, cannot inspect or
    // run in it: to it, the session does not exist.
    let listed = cli.json(&["session", "list", "--json", "--pool-config", &pool_b]);
    assert!(!listed.to_string().contains(&id), "{listed}");
    for arguments in [
        vec![
            "session",
            "info",
            &id,
            "--provider",
            "node",
            "--pool-config",
            &pool_b,
        ],
        vec![
            "session",
            "exec",
            &id,
            "--provider",
            "node",
            "--pool-config",
            &pool_b,
            "--",
            "true",
        ],
        vec![
            "session",
            "destroy",
            &id,
            "--provider",
            "node",
            "--pool-config",
            &pool_b,
        ],
    ] {
        let output = cli.run(&arguments);
        assert!(!output.status.success(), "{arguments:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("unknown_session"),
            "{arguments:?}: {stderr}"
        );
    }

    // 6. A restart keeps the relationship.
    server.stop();
    server = serve(&root, port);
    let info = |pool: &str| {
        cli.run(&[
            "session",
            "info",
            &id,
            "--provider",
            "node",
            "--json",
            "--pool-config",
            pool,
        ])
    };
    let output = info(&pool_a);
    assert!(output.status.success(), "{output:?}");
    let session: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(session["owner"], "control-plane:control-plane-a");

    // 7. A revoked credential cannot revive access; the control plane's
    // new credential keeps what it owns.
    let trusted = cli.json(&[
        "target",
        "credential",
        "list",
        "--credentials",
        &a.credentials.display().to_string(),
        "--json",
    ]);
    let credential_a = trusted
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["control_plane"] == "control-plane-a")
        .unwrap()["credential_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(!trusted.to_string().contains(&a.token()));
    let revoked = cli.json(&[
        "target",
        "credential",
        "revoke",
        &credential_a,
        "--credentials",
        &a.credentials.display().to_string(),
        "--json",
    ]);
    assert_eq!(revoked["status"], "revoked");
    let output = info(&pool_a);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unauthorized:") && stderr.contains("revoked"),
        "{stderr}"
    );
    let rotated = root.join("rotated.token");
    cli.ok(&[
        "target",
        "credential",
        "issue",
        "--credentials",
        &a.credentials.display().to_string(),
        "--control-plane",
        "control-plane-a",
        "--token-file",
        &rotated.display().to_string(),
    ]);
    let pool_rotated = pool("rotated", Some(&rotated));
    let output = info(&pool_rotated);
    assert!(output.status.success(), "{output:?}");
    cli.ok(&[
        "session",
        "destroy",
        &id,
        "--provider",
        "node",
        "--pool-config",
        &pool_rotated,
    ]);
    server.stop();

    // The one open mode is named, and says so.
    let insecure_port = free_port();
    let listen = format!("127.0.0.1:{insecure_port}");
    let mut insecure = Server(Some(
        std::process::Command::new(env!("CARGO_BIN_EXE_compute"))
            .args(["serve", "--listen", &listen, "--insecure-unauthenticated"])
            .arg("--job-store")
            .arg(root.join("insecure-jobs"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    ));
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(&listen).is_err() {
        assert!(Instant::now() < deadline, "compute serve did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    let open = root.join("pool-open.toml");
    std::fs::write(
        &open,
        format!("[providers.node]\nkind = \"remote\"\nendpoint = \"http://{listen}\"\n"),
    )
    .unwrap();
    let output = capabilities(&open.display().to_string());
    assert!(output.status.success(), "{output:?}");
    let advertised: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(advertised["authentication"], "insecure-unauthenticated");
    insecure.stop();
}
