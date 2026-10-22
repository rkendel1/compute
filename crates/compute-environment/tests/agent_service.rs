//! The Work page's agent: goals reach the operator's agent service and its
//! answers come back unchanged. The service here is a stand-in on loopback;
//! this test is the only one in its binary, so it alone sets the environment.

mod common;

use std::sync::{Arc, Mutex};

use compute_environment::*;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A stand-in for `compute-stack-agent serve`: records each request line and
/// body, answers `/v1/work` like the real one.
async fn stand_in(seen: Arc<Mutex<Vec<String>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 8192];
                let read = stream.read(&mut buffer).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                let line = request.lines().next().unwrap_or_default().to_owned();
                let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
                seen.lock().unwrap().push(format!("{line} {body}"));
                let (status, answer) = if line.starts_with("POST /v1/work ") {
                    (202, json!({"work_id": "work_1"}))
                } else if line.contains("/events") {
                    (200, json!({"events": [{"kind": "work.started"}]}))
                } else if line.contains("/cancel") {
                    (200, json!({"status": "cancelled"}))
                } else if line.contains("/v1/work/work_missing") {
                    (404, json!({"error": "unknown work"}))
                } else {
                    (200, json!({"status": "running"}))
                };
                let answer = answer.to_string();
                let _ = stream
                    .write_all(
                        format!(
                            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}",
                            answer.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            });
        }
    });
    format!("http://{address}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goals_reach_the_agent_service_and_answers_come_back_unchanged() {
    let node = tempfile::tempdir().unwrap();
    let store = Arc::new(compute_state_memory::MemoryState::new());
    let artifacts = Arc::new(compute_state::StateArtifacts::new(
        compute_state::ControlState::new(store.clone()),
    ));
    let daemon = Daemon::start({
        let mut config = DaemonConfig::new(node.path(), store, artifacts);
        config.provider = Arc::new(common::provider());
        config
    })
    .await
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(api::serve(listener, daemon.clone(), None));
    let client = client::DaemonClient::new(&endpoint).unwrap();

    // Nothing listening: the answer says so and says what to do, not a 500.
    // SAFETY: the only test in this binary, so nothing else reads the environment.
    unsafe { std::env::set_var(agent_service::URL_ENV, "http://127.0.0.1:9") };
    let status: Value = client.get("/agent").await.unwrap();
    assert_eq!(status["reachable"], false);
    assert!(
        status["error"].as_str().unwrap().contains("serve"),
        "{status}"
    );
    let error = client
        .post::<_, Value>("/agent/work", Some(&json!({"goal": "x"})))
        .await
        .expect_err("no service");
    assert_eq!(error.kind(), "runtime_unavailable");

    // The service holds no credential of Compute's and is never reached off loopback.
    unsafe { std::env::set_var(agent_service::URL_ENV, "http://example.com:8765") };
    let status: Value = client.get("/agent").await.unwrap();
    assert_eq!(status["reachable"], false);
    assert!(
        status["error"].as_str().unwrap().contains("loopback"),
        "{status}"
    );

    let seen = Arc::new(Mutex::new(vec![]));
    unsafe { std::env::set_var(agent_service::URL_ENV, stand_in(seen.clone()).await) };

    let status: Value = client.get("/agent").await.unwrap();
    assert_eq!(status["reachable"], true, "{status}");

    // Only the goal is passed on, and the service's answer is the answer.
    let submitted: Value = client
        .post(
            "/agent/work",
            Some(&json!({"goal": "  add a health check  ", "token": "secret"})),
        )
        .await
        .unwrap();
    assert_eq!(submitted["work_id"], "work_1");
    let work: Value = client.get("/agent/work/work_1").await.unwrap();
    assert_eq!(work["status"], "running");
    let events: Value = client.get("/agent/work/work_1/events").await.unwrap();
    assert_eq!(events["events"][0]["kind"], "work.started");
    let cancelled: Value = client
        .post::<(), Value>("/agent/work/work_1/cancel", None)
        .await
        .unwrap();
    assert_eq!(cancelled["status"], "cancelled");
    let missing = client
        .get::<Value>("/agent/work/work_missing")
        .await
        .expect_err("the service's 404 is relayed");
    assert!(missing.message().contains("unknown work"), "{missing}");

    let seen = seen.lock().unwrap().join("\n");
    assert!(
        seen.contains(r#"POST /v1/work HTTP/1.1 {"goal":"add a health check"}"#),
        "{seen}"
    );
    assert!(!seen.contains("secret"), "{seen}");

    // An empty goal never leaves Compute.
    let error = client
        .post::<_, Value>("/agent/work", Some(&json!({"goal": "   "})))
        .await
        .expect_err("a goal is required");
    assert_eq!(error.kind(), "invalid");

    daemon.shutdown().await;
}
