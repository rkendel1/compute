use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::Command;
use predicates::prelude::*;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_server(root: &std::path::Path, port: u16) -> Server {
    let listen = format!("127.0.0.1:{port}");
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_compute"))
        .args(["serve", "--listen", &listen, "--job-store"])
        .arg(root.join("jobs"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(&listen).is_err() {
        assert!(Instant::now() < deadline, "compute serve did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    Server(child)
}

fn remote(pool: &std::path::Path, arguments: &[&str]) -> Command {
    let mut command = Command::cargo_bin("compute").unwrap();
    command
        .arg("remote")
        .args(arguments)
        .arg("--pool-config")
        .arg(pool);
    command
}

#[test]
fn remote_commands_resolve_named_providers_from_the_caller_owned_pool() {
    let temporary = tempfile::tempdir().unwrap();
    let port = free_port();
    let _server = start_server(temporary.path(), port);
    let pool = temporary.path().join("compute-pool.toml");
    std::fs::write(
        &pool,
        format!(
            "[pool]\n\
             [providers.local]\n\
             kind = \"local\"\n\
             priority = 100\n\
             [providers.remote-dev]\n\
             kind = \"remote\"\n\
             endpoint = \"http://127.0.0.1:{port}\"\n\
             priority = 50\n"
        ),
    )
    .unwrap();

    remote(
        &pool,
        &["capabilities", "--provider", "remote-dev", "--json"],
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("compute.remote@1"));

    Command::cargo_bin("compute")
        .unwrap()
        .args(["remote", "health", "--provider", "remote-dev", "--json"])
        .env("COMPUTE_POOL_CONFIG", &pool)
        .assert()
        .success()
        .stdout(predicate::str::contains("compute.remote@1"));

    remote(&pool, &["capabilities", "--provider", "missing"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "provider missing is not configured in the caller-owned pool",
        ));

    let empty = temporary.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    Command::cargo_bin("compute")
        .unwrap()
        .current_dir(&empty)
        .args(["remote", "capabilities", "--provider", "remote-dev"])
        .env_remove("COMPUTE_POOL_CONFIG")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "provider remote-dev is not configured in the caller-owned pool",
        ))
        .stderr(predicate::str::contains("http:// endpoint").not());

    let wasm = temporary.path().join("empty.wasm");
    std::fs::write(
        &wasm,
        wat::parse_str(r#"(module (func (export "_start")))"#).unwrap(),
    )
    .unwrap();
    remote(
        &pool,
        &["run", "--provider", "local", wasm.to_str().unwrap()],
    )
    .assert()
    .failure()
    .stderr(predicate::str::contains(
        "provider local does not support the remote execution protocol",
    ));

    remote(
        &pool,
        &[
            "inspect",
            "--provider",
            "remote-dev",
            wasm.to_str().unwrap(),
            "--json",
        ],
    )
    .assert()
    .success();
    remote(
        &pool,
        &[
            "run",
            "--provider",
            "remote-dev",
            wasm.to_str().unwrap(),
            "--json",
        ],
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("compute.remote@1"));

    let submission = remote(
        &pool,
        &[
            "submit",
            "--provider",
            "remote-dev",
            wasm.to_str().unwrap(),
            "--json",
        ],
    )
    .output()
    .unwrap();
    assert!(
        submission.status.success(),
        "{}",
        String::from_utf8_lossy(&submission.stderr)
    );
    let submitted: serde_json::Value = serde_json::from_slice(&submission.stdout).unwrap();
    let job_id = submitted["job_id"].as_str().unwrap();

    remote(
        &pool,
        &[
            "wait",
            "--provider",
            "remote-dev",
            job_id,
            "--timeout",
            "10s",
            "--json",
        ],
    )
    .assert()
    .success();
    for operation in ["status", "result", "receipt", "artifacts"] {
        remote(
            &pool,
            &[operation, "--provider", "remote-dev", job_id, "--json"],
        )
        .assert()
        .success();
    }
    remote(
        &pool,
        &["cancel", "--provider", "remote-dev", job_id, "--json"],
    )
    .assert()
    .success();
}
