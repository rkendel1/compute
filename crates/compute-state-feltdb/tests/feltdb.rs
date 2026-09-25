//! Certify the FeltDB backend against a real FeltDB authority.
//!
//! Run with a `feltdb-server` binary:
//!
//! ```sh
//! FELTDB_SERVER_BIN=/path/to/feltdb-server \
//!   cargo test -p compute-state-feltdb --test feltdb -- --ignored
//! ```

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

use compute_state::StateStore;
use compute_state_feltdb::{FeltDbConfig, FeltDbState, ProvisionRequest, provision};

mod common;
use common::*;

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
        ca_certificate: None,
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
            ca_certificate: None,
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
        ca_certificate: None,
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
        ca_certificate: None,
    };
    let first = compute_state_feltdb::provision_manifest(request(), &older.to_string())
        .await
        .expect("provision the older model");
    let config = FeltDbConfig {
        url: server.url.clone(),
        token: token.clone(),
        application_id: first.application_id.clone(),
        environment: "production".into(),
        ca_certificate: None,
    };
    // `connect` refuses the older model; the writes below stand in for
    // the older controller that used it.
    assert!(FeltDbState::connect(config.clone()).await.is_err());
    let state = ControlState::new(Arc::new(FeltDbState::new(config.clone()).unwrap()));
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

/// A TLS terminator in front of the FeltDB authority, with a certificate
/// from a throwaway private CA.
struct Tls {
    child: Child,
    port: u16,
    ca: Vec<u8>,
}

impl Drop for Tls {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

const TLS_PROXY: &str = r#"
import socket, ssl, sys, threading
target, cert, key = int(sys.argv[1]), sys.argv[2], sys.argv[3]
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(cert, key)
server = socket.socket()
server.bind(("127.0.0.1", 0))
server.listen()
print(server.getsockname()[1], flush=True)
def pipe(source, sink):
    try:
        while True:
            data = source.recv(65536)
            if not data:
                break
            sink.sendall(data)
    except Exception:
        pass
    for end in (source, sink):
        try:
            end.shutdown(socket.SHUT_RDWR)
        except Exception:
            pass
def handle(client):
    try:
        secure = context.wrap_socket(client, server_side=True)
    except Exception:
        client.close()
        return
    upstream = socket.create_connection(("127.0.0.1", target))
    threading.Thread(target=pipe, args=(secure, upstream), daemon=True).start()
    pipe(upstream, secure)
while True:
    client, _ = server.accept()
    threading.Thread(target=handle, args=(client,), daemon=True).start()
"#;

fn tls_in_front_of(url: &str, directory: &Path) -> Tls {
    let openssl = |args: &[&str]| {
        let output = Command::new("openssl")
            .args(args)
            .current_dir(directory)
            .output()
            .expect("openssl runs");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    openssl(&[
        "req",
        "-x509",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-days",
        "2",
        "-subj",
        "/CN=Compute test CA",
        "-keyout",
        "ca.key",
        "-out",
        "ca.pem",
        "-addext",
        "basicConstraints=critical,CA:TRUE",
        "-addext",
        "keyUsage=critical,keyCertSign,cRLSign",
    ]);
    openssl(&[
        "req",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-subj",
        "/CN=localhost",
        "-keyout",
        "server.key",
        "-out",
        "server.csr",
    ]);
    std::fs::write(
        directory.join("server.ext"),
        "subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\nbasicConstraints=CA:FALSE\n",
    )
    .unwrap();
    openssl(&[
        "x509",
        "-req",
        "-in",
        "server.csr",
        "-CA",
        "ca.pem",
        "-CAkey",
        "ca.key",
        "-CAcreateserial",
        "-days",
        "2",
        "-extfile",
        "server.ext",
        "-out",
        "server.pem",
    ]);
    let target = url
        .rsplit(':')
        .next()
        .unwrap()
        .trim_end_matches('/')
        .to_string();
    let mut child = Command::new("python3")
        .arg("-c")
        .arg(TLS_PROXY)
        .arg(&target)
        .arg(directory.join("server.pem"))
        .arg(directory.join("server.key"))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("python3 runs");
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    Tls {
        child,
        port: line.trim().parse().unwrap(),
        ca: std::fs::read(directory.join("ca.pem")).unwrap(),
    }
}

#[tokio::test]
#[ignore = "requires FELTDB_SERVER_BIN, openssl, and python3"]
async fn compute_speaks_to_feltdb_over_verified_https() {
    use compute_state::{Batch, ControlState, EnvironmentRecord};

    let data = tempfile_dir();
    let token = create_key(&data);
    let server = start(&data);
    let tls = tls_in_front_of(&server.url, &data);
    let https = format!("https://localhost:{}", tls.port);

    let provisioned = provision(ProvisionRequest {
        url: https.clone(),
        token: token.clone(),
        application_id: None,
        tenant_id: None,
        tenant_name: "compute-tls".into(),
        environment: "production".into(),
        ca_certificate: Some(tls.ca.clone()),
    })
    .await
    .expect("provision over HTTPS");
    let config = FeltDbConfig {
        url: https.clone(),
        token: token.clone(),
        application_id: provisioned.application_id.clone(),
        environment: "production".into(),
        ca_certificate: Some(tls.ca.clone()),
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
        .transaction(Batch::new().create("env_tls", &environment))
        .await
        .expect("a transaction over HTTPS");
    assert_eq!(
        state
            .get::<EnvironmentRecord>("env_tls")
            .await
            .unwrap()
            .unwrap()
            .value,
        environment
    );

    // Verification is not optional: without the CA, the same endpoint is
    // refused.
    let untrusted = FeltDbState::connect(FeltDbConfig {
        ca_certificate: None,
        ..config
    })
    .await;
    assert!(
        matches!(untrusted, Err(compute_state::StateError::Unavailable(_))),
        "an unverified certificate is refused"
    );
    drop(tls);
    drop(server);
    let _ = std::fs::remove_dir_all(data);
}

/// Wire level: a replayed transaction ID returns the original result and
/// applies nothing twice.
#[tokio::test]
#[ignore = "requires FELTDB_SERVER_BIN"]
async fn a_replayed_transaction_id_applies_once() {
    let data = tempfile_dir();
    let token = create_key(&data);
    let server = start(&data);
    let provisioned = provision(ProvisionRequest {
        url: server.url.clone(),
        token: token.clone(),
        application_id: None,
        tenant_id: None,
        tenant_name: "compute-replay".into(),
        environment: "production".into(),
        ca_certificate: None,
    })
    .await
    .unwrap();
    let http = reqwest::Client::new();
    let body = serde_json::json!({
        "application_id": provisioned.application_id,
        "environment": "production",
        "revision_id": provisioned.revision_id,
        "transaction": {
            "transaction_id": "replay-1",
            "tenant_id": "", "application_id": "", "revision_id": "", "schema_version": 0,
            "authorization": { "subject": "", "tenant_id": "", "application_id": "", "revision_id": "", "capabilities": [] },
            "operations": [{
                "kind": "insert", "collection": "Project", "id": "prj_replay",
                "value": { "name": "replay", "created_at": "2026-09-25T00:00:00Z" },
            }],
        },
    });
    let mut results = vec![];
    for _ in 0..2 {
        let response = http
            .post(format!("{}/v1/transactions", server.url))
            .bearer_auth(&token)
            .header("FeltDB-Protocol", "1")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "{}",
            response.text().await.unwrap()
        );
        results.push(response.json::<serde_json::Value>().await.unwrap());
    }
    assert_eq!(results[0]["duplicate"], false);
    assert_eq!(results[1]["duplicate"], true, "the replay is recognized");
    assert_eq!(results[0]["commit_revision"], results[1]["commit_revision"]);
    let state = FeltDbState::connect(FeltDbConfig {
        url: server.url.clone(),
        token,
        application_id: provisioned.application_id,
        environment: "production".into(),
        ca_certificate: None,
    })
    .await
    .unwrap();
    let projects = state
        .query(&compute_state::Query::all(
            compute_state::Collection::Project,
        ))
        .await
        .unwrap();
    assert_eq!(projects.len(), 1, "applied once");
    drop(server);
    let _ = std::fs::remove_dir_all(data);
}
