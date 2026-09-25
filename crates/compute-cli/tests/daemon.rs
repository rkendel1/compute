//! The CLI is a client of the Compute API: a detached daemon, operated
//! only through `compute environment|project|workload`.

use assert_cmd::Command;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn project_source(root: &std::path::Path) -> std::path::PathBuf {
    let source = root.join("app");
    std::fs::create_dir_all(source.join("migrate")).unwrap();
    std::fs::write(
        source.join("migrate/module.wasm"),
        wat::parse_str(r#"(module (func (export "_start")))"#).unwrap(),
    )
    .unwrap();
    std::fs::write(
        source.join("migrate/workload.json"),
        r#"{"version":"1","runtime":"wasm","entrypoint":"module.wasm","network":"none"}"#,
    )
    .unwrap();
    std::fs::write(
        source.join("compute.project.toml"),
        "[project]\nname = \"app\"\n\n[[workload]]\nname = \"migrate\"\nkind = \"task\"\nworkload = \"migrate/workload.json\"\n",
    )
    .unwrap();
    source
}

struct Daemon {
    endpoint: String,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = Command::cargo_bin("compute")
            .unwrap()
            .args(["stop", "--daemon", &self.endpoint])
            .env("COMPUTE_DAEMON_TOKEN", "secret")
            .output();
    }
}

fn compute(daemon: &Daemon, args: &[&str]) -> Command {
    let mut command = Command::cargo_bin("compute").unwrap();
    command.args(args).args(["--daemon", &daemon.endpoint]);
    command
}

fn json(mut command: impl std::borrow::BorrowMut<Command>) -> serde_json::Value {
    let command = command.borrow_mut();
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn cli_operates_environments_through_the_daemon_api() {
    let temporary = tempfile::tempdir().unwrap();
    let source = project_source(temporary.path());
    let listen = format!("127.0.0.1:{}", free_port());
    Command::cargo_bin("compute")
        .unwrap()
        .args(["start", "--detach", "--listen", &listen])
        .args(["--require-token-env", "COMPUTE_DAEMON_TOKEN", "--state-dir"])
        .arg(temporary.path().join("state"))
        .env("COMPUTE_DAEMON_TOKEN", "secret")
        .assert()
        .success();
    let daemon = Daemon {
        endpoint: format!("http://{listen}"),
    };

    // Mutations require the token; reads do not.
    compute(&daemon, &["environment", "create", "staging"])
        .env_remove("COMPUTE_DAEMON_TOKEN")
        .assert()
        .failure()
        .stderr(predicates::str::contains("unauthorized"));
    let created = json(
        compute(&daemon, &["environment", "create", "staging", "--json"])
            .env("COMPUTE_DAEMON_TOKEN", "secret"),
    );
    assert_eq!(created["name"], "staging");
    assert!(
        created["environment_id"]
            .as_str()
            .unwrap()
            .starts_with("env_")
    );

    let project = json(
        compute(
            &daemon,
            &[
                "project",
                "add",
                "app",
                "--environment",
                "staging",
                "--json",
            ],
        )
        .arg("--source")
        .arg(&source)
        .env("COMPUTE_DAEMON_TOKEN", "secret"),
    );
    assert!(project["project_id"].as_str().unwrap().starts_with("prj_"));

    let execution = json(
        compute(
            &daemon,
            &[
                "workload",
                "run",
                "app/migrate",
                "--environment",
                "staging",
                "--json",
            ],
        )
        .env("COMPUTE_DAEMON_TOKEN", "secret"),
    );
    assert_eq!(execution["status"], "completed");
    assert_eq!(execution["environment"], "staging");
    assert!(execution["receipt_id"].is_string());

    let recorded = json(compute(
        &daemon,
        &[
            "execution",
            execution["execution_id"].as_str().unwrap(),
            "--json",
        ],
    ));
    assert_eq!(recorded["execution_id"], execution["execution_id"]);

    // Inspection is deterministic JSON and needs no token.
    let first = json(compute(
        &daemon,
        &["environment", "inspect", "staging", "--json"],
    ));
    let second = json(compute(
        &daemon,
        &["environment", "inspect", "staging", "--json"],
    ));
    assert_eq!(first, second);
    assert_eq!(first["projects"][0]["name"], "app");
    let listed = json(compute(&daemon, &["environment", "list", "--json"]));
    assert_eq!(listed.as_array().unwrap().len(), 1);
    let status = json(compute(&daemon, &["status", "--json"]));
    assert_eq!(status["environments"], 1);
}
