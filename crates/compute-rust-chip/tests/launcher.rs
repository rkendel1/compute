//! The `compute-rust-chip serve` launcher path, as Compute-configured runs it: a real Compute
//! target, Rust Chip's real runtime service with its real model provider (Rust FX over HTTP),
//! two concurrent works with conflicting edits, real Git, real PAX, real Cargo.
//!
//! The model is a local mock HTTP endpoint answering with a fixed script per work (the mock is the
//! only substitution; there is no live model on the build machine). The mock also records every
//! request it receives, which is exactly what a model would have been shown.

mod support;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::*;

type Script = HashMap<&'static str, Vec<String>>;

struct Mock {
    url: String,
    seen: Arc<Mutex<HashMap<String, Vec<String>>>>,
}

fn marker_of(text: &str) -> Option<&'static str> {
    ["ALPHA", "BRAVO", "CHARLIE"]
        .into_iter()
        .find(|m| text.contains(m))
}

/// Answers `script[marker][n]` to the n-th request carrying `marker`. When `barrier` is set, the
/// first answer for every marker is held until all markers have asked: if the works were not
/// running at the same time, nothing would ever be answered.
async fn mock(script: Script, barrier: bool) -> Mock {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let seen: Arc<Mutex<HashMap<String, Vec<String>>>> = Arc::default();
    let expected = script.len();
    let script = Arc::new(script);
    let state = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let (script, state) = (script.clone(), state.clone());
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let (head_end, length) = loop {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..at]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|l| {
                                l.strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        break (at + 4, length);
                    }
                };
                while buf.len() < head_end + length {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let body: Value = serde_json::from_slice(&buf[head_end..]).unwrap();
                let text: String = body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|m| m["content"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                let marker = marker_of(&text).expect("a request that names no work");
                let n = {
                    let mut seen = state.lock().unwrap();
                    let list = seen.entry(marker.to_string()).or_default();
                    list.push(text);
                    list.len() - 1
                };
                if barrier && n == 0 {
                    let deadline = Instant::now() + Duration::from_secs(60);
                    while state.lock().unwrap().len() < expected && Instant::now() < deadline {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
                let reply = script[marker]
                    .get(n)
                    .cloned()
                    .unwrap_or_else(|| "{}".into());
                let payload = json!({
                    "id": format!("{}-{n}", marker.to_lowercase()),
                    "choices": [{"message": {"role": "assistant", "content": reply}}],
                    "usage": {"prompt_tokens": 5, "completion_tokens": 3},
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    Mock { url, seen }
}

fn write(path: &str, content: &str) -> String {
    json!({"decision": "request_capability", "capability": "project.write",
           "inputs": {"path": path, "content": content}})
    .to_string()
}

fn read(path: &str) -> String {
    json!({"decision": "request_capability", "capability": "project.read", "inputs": {"path": path}})
        .to_string()
}

fn pax_test() -> String {
    json!({"decision": "request_capability", "capability": "pax.test"}).to_string()
}

struct Service {
    child: Child,
    addr: String,
    _stdout: BufReader<std::process::ChildStdout>,
}

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn token_file(target: &Target, tag: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "compute-rust-chip-token-{}-{tag}",
        std::process::id()
    ));
    std::fs::write(&path, &target.token).unwrap();
    path
}

fn launcher(target: &Target, source: &std::path::Path, model: &str, tag: &str) -> Command {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let mut c = Command::new(worker());
    c.arg("serve")
        // What Compute-configured supplies:
        .env("COMPUTE_RUST_CHIP_TARGET", &target.endpoint)
        .env(
            "COMPUTE_RUST_CHIP_TARGET_TOKEN_FILE",
            token_file(target, tag),
        )
        .env("COMPUTE_RUST_CHIP_PROJECT", source)
        .env("COMPUTE_RUST_CHIP_WORKER", worker())
        .env(
            "COMPUTE_RUST_CHIP_COMMAND_PATH",
            format!("{home}/.cargo/bin:/usr/local/bin:/usr/bin:/bin"),
        )
        .env("COMPUTE_RUST_CHIP_CARGO_HOME", format!("{home}/.cargo"))
        .env("COMPUTE_RUST_CHIP_RUSTUP_HOME", format!("{home}/.rustup"))
        // Rust Chip's own model configuration, untouched by Compute:
        .env("CHIP_PROVIDER", "openai-compatible")
        .env("CHIP_MODEL", "mock-model")
        .env("CHIP_ENDPOINT", model)
        .env("CHIP_API_KEY", "sk-never-shown-to-anyone")
        .env_remove("PAX_BIN");
    c
}

fn start(mut c: Command, args: &[&str]) -> Service {
    let mut child = c
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let addr = line
        .trim()
        .strip_prefix("Chip Runtime Service listening on http://")
        .unwrap_or_else(|| panic!("unexpected first line {line:?}"))
        .to_string();
    Service {
        child,
        addr,
        _stdout: stdout,
    }
}

fn http(addr: &str, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
    let body = body.unwrap_or("");
    let mut stream = TcpStream::connect(addr).unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    stream.read_to_string(&mut out).unwrap();
    let (head, body) = out.split_once("\r\n\r\n").unwrap();
    (
        head.split(' ').nth(1).unwrap().parse().unwrap(),
        serde_json::from_str(body).unwrap_or(Value::Null),
    )
}

fn submit(addr: &str, goal: &str) -> String {
    let (status, body) = http(
        addr,
        "POST",
        "/v1/work",
        Some(&json!({"goal": goal}).to_string()),
    );
    assert_eq!(status, 202, "{body}");
    body["work_id"].as_str().unwrap().to_string()
}

fn wait_done(addr: &str, id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(240);
    loop {
        let (_, body) = http(addr, "GET", &format!("/v1/work/{id}"), None);
        if body["status"] != "running" && body["status"] != "queued" {
            return body;
        }
        assert!(Instant::now() < deadline, "work did not finish: {body}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn kinds(addr: &str, id: &str) -> Vec<String> {
    let (_, body) = http(addr, "GET", &format!("/v1/work/{id}/events"), None);
    body["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn two_concurrent_rust_chip_works_edit_the_same_file_in_two_compute_environments() {
    if !pax_installed() {
        eprintln!("SKIPPED: PAX is not installed");
        return;
    }
    let target = Target::start();
    let source = project("collision", true);
    let script: Script = HashMap::from([
        (
            "ALPHA",
            vec![write("README.md", "ALPHA\n"), read("README.md"), pax_test()],
        ),
        (
            "BRAVO",
            vec![write("README.md", "BRAVO\n"), read("README.md"), pax_test()],
        ),
    ]);
    // Neither work is answered until both have asked: they must be running at the same time.
    let model = mock(script, true).await;
    let (url, endpoint, tgt_token) = (
        model.url.clone(),
        target.endpoint.clone(),
        target.token.clone(),
    );
    let src = source.clone();
    let seen = model.seen.clone();

    let results = tokio::task::spawn_blocking(move || {
        let _ = (&endpoint, &tgt_token);
        let service = start(
            launcher_for(&endpoint, &tgt_token, &src, &url),
            &["--port", "0"],
        );
        let addr = service.addr.clone();
        let (_, metrics) = http(&addr, "GET", "/v1/metrics", None);
        assert_eq!(
            metrics["max_concurrent_work"], 2,
            "the Compute provider's isolation capacity"
        );
        let a = submit(
            &addr,
            "ALPHA: put the word ALPHA in README.md, then verify.",
        );
        let b = submit(
            &addr,
            "BRAVO: put the word BRAVO in README.md, then verify.",
        );
        let (da, db) = (wait_done(&addr, &a), wait_done(&addr, &b));
        let (ka, kb) = (kinds(&addr, &a), kinds(&addr, &b));
        let (_, final_metrics) = http(&addr, "GET", "/v1/metrics", None);
        (a, b, da, db, ka, kb, final_metrics)
    })
    .await
    .unwrap();
    let (a, b, da, db, ka, kb, metrics) = results;

    // Both independently verified from PAX's own observation, run for real inside Compute.
    for done in [&da, &db] {
        assert_eq!(done["status"], "completed", "{done}");
        assert_eq!(done["result"]["verified"], true);
        assert_eq!(done["result"]["audit"]["clean"], true);
        assert_eq!(done["result"]["pax"]["last_status"], "passed");
    }
    // Distinct environments, with distinct opaque identities.
    let (ea, eb) = (
        da["environment_id"].as_str().unwrap(),
        db["environment_id"].as_str().unwrap(),
    );
    assert_ne!(ea, eb);
    assert!(ea.starts_with("env_") && eb.starts_with("env_"));
    assert_ne!(a, b);
    // Each work's trajectory is its own and complete.
    for kinds in [&ka, &kb] {
        assert_eq!(kinds.iter().filter(|k| *k == "ExecutionStarted").count(), 3);
        assert_eq!(
            kinds.iter().filter(|k| *k == "ObservationRecorded").count(),
            3
        );
        assert_eq!(kinds.last().map(String::as_str), Some("WorkCompleted"));
    }
    // What each model was shown: its own file content, never the other's, and no host path or
    // Compute identity.
    let seen = seen.lock().unwrap();
    let (alpha, bravo) = (seen["ALPHA"].join("\n"), seen["BRAVO"].join("\n"));
    // Each model read back its own edit (the file's content appears in the observation it was
    // shown), and was never shown the other work's goal, edit or observations.
    let edit = |text: &str, word: &str| {
        text.contains(&format!("{word}\\n")) || text.contains(&format!("{word}\n"))
    };
    assert!(
        !edit(&seen["ALPHA"][0], "ALPHA") && edit(&seen["ALPHA"][2], "ALPHA"),
        "A read its own edit"
    );
    assert!(
        !edit(&seen["BRAVO"][0], "BRAVO") && edit(&seen["BRAVO"][2], "BRAVO"),
        "B read its own edit"
    );
    assert!(!alpha.contains("BRAVO"), "A's model was shown B's work");
    assert!(!bravo.contains("ALPHA"), "B's model was shown A's work");
    for text in [&alpha, &bravo] {
        assert!(
            !text.contains(&source.display().to_string()),
            "the model saw the project's host path"
        );
        assert!(
            !text.contains("env_") && !text.contains("ses_") && !text.contains("wks_"),
            "a Compute identity reached the model"
        );
        assert!(
            !text.contains("compute-rust-chip"),
            "the worker's location reached the model"
        );
    }
    // The source project was never touched.
    assert_eq!(
        std::fs::read_to_string(source.join("README.md")).unwrap(),
        "baseline\n"
    );
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&source)
        .output()
        .unwrap();
    assert!(status.stdout.is_empty(), "the source repository changed");
    assert_eq!(metrics["completed_work"], 2);
    assert_eq!(metrics["failed_work"], 0);
}

fn launcher_for(endpoint: &str, token: &str, source: &std::path::Path, model: &str) -> Command {
    // The same launch as `launcher`, without needing the Target value on this thread.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let token_path = std::env::temp_dir().join(format!(
        "compute-rust-chip-token-{}-{}",
        std::process::id(),
        &token[..8.min(token.len())]
    ));
    std::fs::write(&token_path, token).unwrap();
    let mut c = Command::new(worker());
    c.arg("serve")
        .env("COMPUTE_RUST_CHIP_TARGET", endpoint)
        .env("COMPUTE_RUST_CHIP_TARGET_TOKEN_FILE", token_path)
        .env("COMPUTE_RUST_CHIP_PROJECT", source)
        .env("COMPUTE_RUST_CHIP_WORKER", worker())
        .env(
            "COMPUTE_RUST_CHIP_COMMAND_PATH",
            format!("{home}/.cargo/bin:/usr/local/bin:/usr/bin:/bin"),
        )
        .env("COMPUTE_RUST_CHIP_CARGO_HOME", format!("{home}/.cargo"))
        .env("COMPUTE_RUST_CHIP_RUSTUP_HOME", format!("{home}/.rustup"))
        .env("CHIP_PROVIDER", "openai-compatible")
        .env("CHIP_MODEL", "mock-model")
        .env("CHIP_ENDPOINT", model)
        .env("CHIP_API_KEY", "sk-never-shown-to-anyone")
        .env_remove("PAX_BIN");
    c
}

#[test]
fn the_launcher_refuses_more_concurrency_than_the_provider_can_isolate_and_needs_configuration() {
    let target = Target::start();
    let source = project("launcher-limits", true);
    // More work at once than isolated Compute sessions the provider declared.
    let out = launcher(&target, &source, "http://127.0.0.1:1", "limits")
        .env("COMPUTE_RUST_CHIP_MAX_ENVIRONMENTS", "2")
        .args(["--port", "0", "--max-concurrent-work", "3"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("requires isolated environments"));
    assert!(out.stdout.is_empty());
    // No target configured: nothing is run, and nothing falls back to the local machine.
    let out = launcher(&target, &source, "http://127.0.0.1:1", "limits2")
        .env_remove("COMPUTE_RUST_CHIP_TARGET")
        .args(["--port", "0"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("COMPUTE_RUST_CHIP_TARGET is not set"));
    assert!(out.stdout.is_empty());
}
