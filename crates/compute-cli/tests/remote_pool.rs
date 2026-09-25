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
        self.stop();
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
    Server(Some(child))
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
    let mut server = start_server(temporary.path(), port);
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

    let runtimes = Command::cargo_bin("compute")
        .unwrap()
        .args(["runtimes", "--provider", "remote-dev", "--json"])
        .arg("--pool-config")
        .arg(&pool)
        .output()
        .unwrap();
    assert!(runtimes.status.success());
    let runtimes: serde_json::Value = serde_json::from_slice(&runtimes.stdout).unwrap();
    assert!(runtimes["platform"].as_str().is_some());
    assert!(
        runtimes["runtimes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|runtime| {
                runtime["lifecycle"].as_str().is_some()
                    && runtime["platform"] == runtimes["platform"]
            })
    );

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

    let placed = Command::cargo_bin("compute")
        .unwrap()
        .args([
            "run",
            wasm.to_str().unwrap(),
            "--provider",
            "remote-dev",
            "--json",
        ])
        .arg("--pool-config")
        .arg(&pool)
        .output()
        .unwrap();
    assert!(
        placed.status.success(),
        "{}",
        String::from_utf8_lossy(&placed.stderr)
    );
    let placed: serde_json::Value = serde_json::from_slice(&placed.stdout).unwrap();
    let placed_job = placed["job_id"].as_str().unwrap();
    assert_ne!(placed_job, placed["execution_id"]);
    assert_eq!(placed["placement"]["selection_mode"], "explicit");
    assert_eq!(placed["placement"]["selected"]["provider_id"], "remote-dev");
    assert_eq!(
        placed["receipt"]["placement"]["placement_id"],
        placed["placement"]["placement_id"]
    );
    remote(
        &pool,
        &["status", "--provider", "remote-dev", placed_job, "--json"],
    )
    .assert()
    .success();

    let run = remote(
        &pool,
        &[
            "run",
            "--provider",
            "remote-dev",
            wasm.to_str().unwrap(),
            "--json",
        ],
    )
    .output()
    .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let run: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();
    let job_id = run["job_id"].as_str().unwrap();
    let execution_id = run["execution_id"].as_str().unwrap();
    assert_ne!(job_id, execution_id);
    assert_eq!(run["status"], "completed");
    assert_eq!(run["exit_code"], 0);
    assert_eq!(run["receipt"]["execution_id"], execution_id);
    assert_eq!(run["receipt"]["provider_protocol"], "compute.remote@1");

    remote(
        &pool,
        &["run", "--provider", "remote-dev", wasm.to_str().unwrap()],
    )
    .assert()
    .success()
    .stderr(predicate::str::contains("job: job_"));

    remote(&pool, &["status", "--provider", "remote-dev", execution_id])
        .assert()
        .failure()
        .stderr(predicate::str::contains("malformed job identity"));

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
    let status = remote(
        &pool,
        &["status", "--provider", "remote-dev", job_id, "--json"],
    )
    .output()
    .unwrap();
    assert!(status.status.success());
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["job_id"], job_id);
    assert_eq!(status["execution_id"], execution_id);
    assert_eq!(status["status"], "succeeded");

    let job_store = temporary.path().join("jobs").join(job_id);
    for evidence in [
        "request.json",
        "status.json",
        "result.json",
        "receipt.json",
        "events.json",
    ] {
        assert!(job_store.join(evidence).is_file(), "missing {evidence}");
    }
    let stored_request: serde_json::Value =
        serde_json::from_slice(&std::fs::read(job_store.join("request.json")).unwrap()).unwrap();
    let stored_status: serde_json::Value =
        serde_json::from_slice(&std::fs::read(job_store.join("status.json")).unwrap()).unwrap();
    let stored_result: serde_json::Value =
        serde_json::from_slice(&std::fs::read(job_store.join("result.json")).unwrap()).unwrap();
    let stored_receipt: serde_json::Value =
        serde_json::from_slice(&std::fs::read(job_store.join("receipt.json")).unwrap()).unwrap();
    assert_eq!(stored_status["job_id"], job_id);
    assert_eq!(stored_status["execution_id"], execution_id);
    assert_eq!(stored_status["status"], "succeeded");
    assert!(stored_status["provider"].is_object());
    assert!(stored_status["request"]["workload_id"].is_string());
    assert!(stored_status["request"]["bundle_id"].is_string());
    assert!(stored_status["created_at"].is_string());
    assert!(stored_status["updated_at"].is_string());
    assert_eq!(
        stored_request["request"]["expected"]["workload_id"],
        stored_status["request"]["workload_id"]
    );
    assert_eq!(
        stored_request["request"]["expected"]["bundle_id"],
        stored_status["request"]["bundle_id"]
    );
    assert_eq!(stored_result["job_id"], job_id);
    assert_eq!(stored_result["result"]["execution_id"], execution_id);
    assert_eq!(stored_receipt["job_id"], job_id);
    assert_eq!(stored_receipt["receipt"]["execution_id"], execution_id);

    server.stop();
    server = start_server(temporary.path(), port);

    for operation in ["status", "result", "receipt", "artifacts"] {
        remote(
            &pool,
            &[operation, "--provider", "remote-dev", job_id, "--json"],
        )
        .assert()
        .success();
    }
    let result = remote(
        &pool,
        &["result", "--provider", "remote-dev", job_id, "--json"],
    )
    .output()
    .unwrap();
    let result: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(result["job_id"], job_id);
    assert_eq!(result["result"]["execution_id"], execution_id);
    assert_eq!(result["result"]["status"], "completed");
    assert_eq!(result["result"]["exit_code"], 0);
    let receipt = remote(
        &pool,
        &["receipt", "--provider", "remote-dev", job_id, "--json"],
    )
    .output()
    .unwrap();
    let receipt: serde_json::Value = serde_json::from_slice(&receipt.stdout).unwrap();
    assert_eq!(receipt["execution_id"], execution_id);
    assert_eq!(receipt["receipt_version"], "compute.receipt@1");
    assert_eq!(receipt["provider_protocol"], "compute.remote@1");
    assert_eq!(receipt["admission_status"], "admitted");
    assert_eq!(receipt["execution"]["status"], "completed");
    assert_eq!(receipt["execution"]["exit_code"], 0);

    remote(
        &pool,
        &["cancel", "--provider", "remote-dev", job_id, "--json"],
    )
    .assert()
    .success();

    drop(server);
}
