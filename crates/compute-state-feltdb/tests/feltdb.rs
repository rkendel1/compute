//! Certify the FeltDB backend against a real FeltDB authority.
//!
//! Run with a `feltdb-server` binary:
//!
//! ```sh
//! FELTDB_SERVER_BIN=/path/to/feltdb-server \
//!   cargo test -p compute-state-feltdb --test feltdb -- --ignored
//! ```

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

use compute_state::StateStore;
use compute_state_feltdb::{FeltDbConfig, FeltDbState, ProvisionRequest, provision};

const MASTER_KEY: &str = "compute-state-certification";

struct Server {
    child: Child,
    url: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn binary() -> PathBuf {
    PathBuf::from(
        std::env::var("FELTDB_SERVER_BIN")
            .expect("FELTDB_SERVER_BIN must name a feltdb-server binary"),
    )
}

fn create_key(data: &Path) -> String {
    let output = Command::new(binary())
        .args(["keys", "create", "--keys"])
        .arg(data.join("keys.json"))
        .args(["--name", "compute", "--namespace", "compute", "--scope"])
        .arg(
            "state:read,state:write,events:read,application:read,application:write,\
             application:revision:read,application:revision:create,application:revision:promote,\
             application:environment:read,application:environment:write",
        )
        .env("FELTDB_MASTER_KEY", MASTER_KEY)
        .output()
        .expect("feltdb-server runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .find(|word| word.starts_with("fdb_live_"))
        .expect("a key")
        .to_string()
}

fn start(data: &Path) -> Server {
    let mut child = Command::new(binary())
        .args([
            "--host",
            "127.0.0.1",
            "--port",
            "0",
            "--namespace",
            "compute",
            "--auth",
        ])
        .arg("--data")
        .arg(data.join("state.log"))
        .arg("--keys")
        .arg(data.join("keys.json"))
        .arg("--audit")
        .arg(data.join("audit.log"))
        .env("FELTDB_MASTER_KEY", MASTER_KEY)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("feltdb-server starts");
    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();
    let url = loop {
        let line = lines
            .next()
            .expect("feltdb-server reports readiness")
            .unwrap();
        if let Some(index) = line.find("http://") {
            break line[index..].split_whitespace().next().unwrap().to_string();
        }
    };
    // Keep draining output so the server never blocks on a full pipe.
    std::thread::spawn(move || for _ in lines {});
    Server { child, url }
}

#[tokio::test]
#[ignore = "requires FELTDB_SERVER_BIN"]
async fn feltdb_is_a_conforming_durable_authority() {
    let data = tempfile_dir();
    let token = create_key(&data);
    let server = start(&data);

    let provisioned = provision(ProvisionRequest {
        url: server.url.clone(),
        token: token.clone(),
        application_id: None,
        tenant_id: None,
        tenant_name: "compute-certification".into(),
        environment: "production".into(),
    })
    .await
    .expect("provision the Compute model");
    assert!(provisioned.changed);

    // Provisioning again is a no-op: the same tenant, application, and
    // revision, whether or not the application is named.
    for application_id in [None, Some(provisioned.application_id.clone())] {
        let again = provision(ProvisionRequest {
            url: server.url.clone(),
            token: token.clone(),
            application_id,
            tenant_id: None,
            tenant_name: "compute-certification".into(),
            environment: "production".into(),
        })
        .await
        .expect("provision again");
        assert!(
            !again.changed,
            "an application on the current model is unchanged"
        );
        assert_eq!(again.tenant_id, provisioned.tenant_id);
        assert_eq!(again.application_id, provisioned.application_id);
        assert_eq!(again.revision_id, provisioned.revision_id);
    }

    let config = FeltDbConfig {
        url: server.url.clone(),
        token: token.clone(),
        application_id: provisioned.application_id.clone(),
        environment: "production".into(),
    };
    let store = Arc::new(FeltDbState::connect(config.clone()).await.expect("connect"));
    assert_eq!(store.backend().kind, "feltdb");
    compute_state::conformance::check(store, None).await;

    // Durability: the authority restarts and the state is still there.
    let before = count_events(&config).await;
    assert!(before > 0);
    drop(server);
    let server = start(&data);
    let config = FeltDbConfig {
        url: server.url.clone(),
        ..config
    };
    assert_eq!(
        count_events(&config).await,
        before,
        "state survives a FeltDB restart"
    );
    let reopened = Arc::new(FeltDbState::connect(config.clone()).await.unwrap());
    compute_state::conformance::check(reopened, None).await;

    let _ = std::fs::remove_dir_all(data);
}

#[tokio::test]
#[ignore = "requires FELTDB_SERVER_BIN"]
async fn an_older_compute_model_is_upgraded_in_place() {
    use compute_state::{Batch, ControlState, EnvironmentRecord, WorkloadStatusRecord};

    let data = tempfile_dir();
    let token = create_key(&data);
    let server = start(&data);
    // The previous model: everything except WorkloadStatus.
    let mut older: serde_json::Value =
        serde_json::from_str(compute_state_feltdb::COMPUTE_MANIFEST).unwrap();
    for key in ["collections", "policies"] {
        older[key]
            .as_array_mut()
            .unwrap()
            .retain(|item| item["name"] != "WorkloadStatus");
    }
    older["indexes"]
        .as_array_mut()
        .unwrap()
        .retain(|index| index["collection"] != "WorkloadStatus");
    let request = || ProvisionRequest {
        url: server.url.clone(),
        token: token.clone(),
        application_id: None,
        tenant_id: None,
        tenant_name: "compute-upgrade".into(),
        environment: "production".into(),
    };
    let first = compute_state_feltdb::provision_manifest(request(), &older.to_string())
        .await
        .expect("provision the older model");
    let config = FeltDbConfig {
        url: server.url.clone(),
        token: token.clone(),
        application_id: first.application_id.clone(),
        environment: "production".into(),
    };
    let state = ControlState::new(Arc::new(
        FeltDbState::connect(config.clone()).await.unwrap(),
    ));
    let environment = EnvironmentRecord {
        name: "production".into(),
        desired_state: compute_state::DesiredState::Running,
        config: Default::default(),
        policy: None,
        provider: None,
        created_at: chrono::Utc::now(),
    };
    state
        .transaction(Batch::new().create("env_upgrade", &environment))
        .await
        .unwrap();
    let status = WorkloadStatusRecord {
        workload_id: "wl_upgrade".into(),
        environment: "production".into(),
        project: "attn".into(),
        workload: "api".into(),
        actual_state: "running".into(),
        health: "healthy".into(),
        deployment_id: None,
        execution_id: None,
        restarts: 0,
        error: None,
        observed_by: "daemon_upgrade".into(),
        observed_at: chrono::Utc::now(),
    };
    assert!(
        state
            .transaction(Batch::new().create("ws_upgrade", &status))
            .await
            .is_err(),
        "the older model has no WorkloadStatus"
    );

    let upgraded = provision(request()).await.expect("upgrade");
    assert!(upgraded.changed);
    assert_eq!(
        upgraded.application_id, first.application_id,
        "upgraded in place"
    );
    assert_ne!(upgraded.revision_id, first.revision_id);
    let state = ControlState::new(Arc::new(FeltDbState::connect(config).await.unwrap()));
    assert_eq!(
        state
            .get::<EnvironmentRecord>("env_upgrade")
            .await
            .unwrap()
            .unwrap()
            .value,
        environment,
        "existing state survives the upgrade"
    );
    state
        .transaction(Batch::new().create("ws_upgrade", &status))
        .await
        .expect("the new collection is usable");
    assert!(!provision(request()).await.unwrap().changed);
    drop(server);
    let _ = std::fs::remove_dir_all(data);
}

async fn count_events(config: &FeltDbConfig) -> usize {
    let store = FeltDbState::connect(config.clone()).await.unwrap();
    store
        .query(&compute_state::Query::all(compute_state::Collection::Event))
        .await
        .unwrap()
        .len()
}

fn tempfile_dir() -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "compute-state-feltdb-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    directory
}
