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

/// The provider key the launcher gives Rust Chip. It must reach the model endpoint and nowhere else.
const SENTINEL_KEY: &str = "sk-never-shown-to-anyone";

struct Mock {
    url: String,
    seen: Arc<Mutex<HashMap<String, Vec<String>>>>,
    /// The `Authorization` header of every request: the proof that the key really was sent to the model.
    auth: Arc<Mutex<Vec<String>>>,
}

fn marker_of(text: &str) -> Option<&'static str> {
    ["ALPHA", "BRAVO", "CHARLIE"]
        .into_iter()
        .find(|m| text.contains(m))
}

/// Answers `script[marker][n]` to the n-th request carrying `marker`. When `hold_first` is set, the
/// first answer for every marker is held that long, so the works stay in flight together long enough
/// for the test to observe it. (It cannot wait for the other work's request instead: the service
/// shares one model provider and FX holds it for the whole attempt, so two model calls are never in
/// flight at once. That is a property of chip-rs at the pinned commit, not of the Compute adapter.)
async fn mock(script: Script, hold_first: Option<Duration>) -> Mock {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let seen: Arc<Mutex<HashMap<String, Vec<String>>>> = Arc::default();
    let script = Arc::new(script);
    let state = seen.clone();
    let auth: Arc<Mutex<Vec<String>>> = Arc::default();
    let auth_state = auth.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let (script, state, auth) = (script.clone(), state.clone(), auth_state.clone());
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
                        let raw = String::from_utf8_lossy(&buf[..at]).to_string();
                        if let Some(line) = raw
                            .lines()
                            .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                        {
                            auth.lock()
                                .unwrap()
                                .push(line["authorization:".len()..].trim().to_string());
                        }
                        let head = raw.to_ascii_lowercase();
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
                if let (Some(hold), 0) = (hold_first, n) {
                    tokio::time::sleep(hold).await;
                }
                let reply = script[marker]
                    .get(n)
                    .cloned()
                    .unwrap_or_else(|| "{}".into());
                let id = format!("{}-{n}", marker.to_lowercase());
                let payload = json!({
                    "id": id,
                    "choices": [{"message": {"role": "assistant", "content": reply}}],
                    "usage": {"prompt_tokens": 5, "completion_tokens": 3},
                })
                .to_string();
                // Rust Chip's executor is FX, which asks for a stream; answer in the form that was
                // asked for, with the same scripted content and usage either way.
                let response = if body["stream"] == json!(true) {
                    let events = event_stream(&id, &reply);
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{events}",
                        events.len()
                    )
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                        payload.len()
                    )
                };
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    Mock { url, seen, auth }
}

/// One scripted reply as the OpenAI-style event stream FX reads: the content, a stop, the usage, the
/// terminator.
fn event_stream(id: &str, content: &str) -> String {
    let mut out = String::new();
    let mut event = |v: Value| out.push_str(&format!("data: {v}\n\n"));
    event(
        json!({"id": id, "choices": [{"index": 0, "delta": {"role": "assistant", "content": content}}]}),
    );
    event(json!({"id": id, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}));
    event(json!({"id": id, "choices": [], "usage": {"prompt_tokens": 5, "completion_tokens": 3}}));
    out.push_str("data: [DONE]\n\n");
    out
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
    _stdout: Option<BufReader<std::process::ChildStdout>>,
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
            "COMPUTE_RUST_CHIP_SESSION_MEMORY",
            TEST_SESSION_MEMORY.to_string(),
        )
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
        .env("CHIP_API_KEY", SENTINEL_KEY)
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
        _stdout: Some(stdout),
    }
}

/// As [`start`], keeping everything the service writes to stdout and stderr: its log.
fn start_logged(mut c: Command, args: &[&str]) -> (Service, Arc<Mutex<Vec<u8>>>) {
    let mut child = c
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    log.lock().unwrap().extend_from_slice(line.as_bytes());
    let addr = line
        .trim()
        .strip_prefix("Chip Runtime Service listening on http://")
        .unwrap_or_else(|| panic!("unexpected first line {line:?}"))
        .to_string();
    let drain = |mut from: Box<dyn Read + Send>, into: Arc<Mutex<Vec<u8>>>| {
        std::thread::spawn(move || {
            let mut chunk = [0_u8; 4096];
            while let Ok(n) = from.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                into.lock().unwrap().extend_from_slice(&chunk[..n]);
            }
        });
    };
    drain(Box::new(stdout), log.clone());
    drain(Box::new(child.stderr.take().unwrap()), log.clone());
    (
        Service {
            child,
            addr,
            _stdout: None,
        },
        log,
    )
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
    // Each work's first answer is held, so both are in flight at once and the test can see it.
    let model = mock(script, Some(Duration::from_millis(1500))).await;
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
        // Both are admitted and running at the same moment, each in its own Compute environment.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let running = |id: &str| {
                http(&addr, "GET", &format!("/v1/work/{id}"), None).1["status"] == "running"
            };
            if running(&a) && running(&b) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the two works were never running together"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
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
    // One file per launch: tokens share a fixed prefix, so a name derived from the token would
    // let concurrent tests overwrite each other's credential.
    static LAUNCHES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let token_path = std::env::temp_dir().join(format!(
        "compute-rust-chip-token-{}-launch-{}",
        std::process::id(),
        LAUNCHES.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    std::fs::write(&token_path, token).unwrap();
    let mut c = Command::new(worker());
    c.arg("serve")
        .env("COMPUTE_RUST_CHIP_TARGET", endpoint)
        .env("COMPUTE_RUST_CHIP_TARGET_TOKEN_FILE", token_path)
        .env("COMPUTE_RUST_CHIP_PROJECT", source)
        .env("COMPUTE_RUST_CHIP_WORKER", worker())
        .env(
            "COMPUTE_RUST_CHIP_SESSION_MEMORY",
            TEST_SESSION_MEMORY.to_string(),
        )
        .env(
            "COMPUTE_RUST_CHIP_COMMAND_PATH",
            format!("{home}/.cargo/bin:/usr/local/bin:/usr/bin:/bin"),
        )
        .env("COMPUTE_RUST_CHIP_CARGO_HOME", format!("{home}/.cargo"))
        .env("COMPUTE_RUST_CHIP_RUSTUP_HOME", format!("{home}/.rustup"))
        .env("CHIP_PROVIDER", "openai-compatible")
        .env("CHIP_MODEL", "mock-model")
        .env("CHIP_ENDPOINT", model)
        .env("CHIP_API_KEY", SENTINEL_KEY)
        .env_remove("PAX_BIN");
    c
}

#[test]
fn the_launcher_refuses_more_concurrency_than_the_provider_can_isolate_and_needs_configuration() {
    let target = Target::start();
    let source = project("launcher-limits", true);
    // The model endpoint must be shaped the way the FX executor requires (`.../chat/completions`):
    // the service checks the model provider before it checks capacity, so a malformed endpoint
    // ends the launch with exit 3 and the capacity check is never reached. Nothing listens here.
    const UNREACHABLE_MODEL: &str = "http://127.0.0.1:1/v1/chat/completions";
    // More work at once than isolated Compute sessions the provider declared.
    let out = launcher(&target, &source, UNREACHABLE_MODEL, "limits")
        .env("COMPUTE_RUST_CHIP_MAX_ENVIRONMENTS", "2")
        .args(["--port", "0", "--max-concurrent-work", "3"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("requires isolated environments"));
    assert!(out.stdout.is_empty());
    // No target configured: nothing is run, and nothing falls back to the local machine.
    let out = launcher(&target, &source, UNREACHABLE_MODEL, "limits2")
        .env_remove("COMPUTE_RUST_CHIP_TARGET")
        .args(["--port", "0"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("COMPUTE_RUST_CHIP_TARGET is not set"));
    assert!(out.stdout.is_empty());
}

#[test]
fn a_model_endpoint_the_fx_executor_cannot_use_ends_the_launch_before_anything_runs() {
    let target = Target::start();
    let source = project("launcher-endpoint", true);
    let out = launcher(&target, &source, "http://127.0.0.1:1", "endpoint")
        .args(["--port", "0"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("endpoint ending in /chat/completions")
            && stderr.contains("nothing was run"),
        "{stderr}"
    );
    assert!(out.stdout.is_empty());
}

/// Every file under `dir`, as bytes, with its path.
fn files_under(dir: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                out.push((path, bytes));
            }
        }
    }
    out
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

/// A value in every command environment Compute receives, so the worker's recorded environment
/// is known to be the one the launcher configured.
const COMMAND_ENVIRONMENT_MARKER: &str = "/g8-command-environment-marker";

/// A worker that records the environment it was started with in `dump`, then runs the real one.
/// It stands where Rust Chip's worker stands inside each Compute session.
fn recording_worker(dir: &std::path::Path, dump: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("recording-worker");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nenv >> '{}'\nexec '{}' \"$@\"\n",
            dump.display(),
            worker()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// G8 (compute#95): the provider key reaches the model endpoint and nowhere else. It runs on the
/// real integration path (the launcher, Rust Chip's service, FX, Compute sessions and the worker)
/// and needs no PAX: the work's verdict is irrelevant here, only where the key went.
#[tokio::test(flavor = "multi_thread")]
async fn the_provider_key_reaches_the_model_and_nothing_compute_stores_or_the_service_reports() {
    let target = Target::start();
    let source = project("key-nondisclosure", true);
    let script: Script = HashMap::from([(
        "ALPHA",
        vec![write("README.md", "ALPHA\n"), read("README.md")],
    )]);
    let model = mock(script, None).await;
    let scratch = tempfile::tempdir().unwrap();
    let dump = scratch.path().join("worker-environment");
    let recorder = recording_worker(scratch.path(), &dump);
    let (url, endpoint, tgt_token, src) = (
        model.url.clone(),
        target.endpoint.clone(),
        target.token.clone(),
        source.clone(),
    );
    let (done, reported, log) = tokio::task::spawn_blocking(move || {
        let mut launch = launcher_for(&endpoint, &tgt_token, &src, &url);
        launch.env("COMPUTE_RUST_CHIP_WORKER", &recorder);
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        launch.env(
            "COMPUTE_RUST_CHIP_COMMAND_PATH",
            format!("{home}/.cargo/bin:/usr/local/bin:/usr/bin:/bin:{COMMAND_ENVIRONMENT_MARKER}"),
        );
        let (service, log) = start_logged(launch, &["--port", "0"]);
        let id = submit(
            &service.addr,
            "ALPHA: put the word ALPHA in README.md, then verify.",
        );
        let done = wait_done(&service.addr, &id);
        let (_, work) = http(&service.addr, "GET", &format!("/v1/work/{id}"), None);
        let (_, events) = http(&service.addr, "GET", &format!("/v1/work/{id}/events"), None);
        let (_, metrics) = http(&service.addr, "GET", "/v1/metrics", None);
        drop(service);
        // Give the log readers a moment to drain what the service wrote before it stopped.
        std::thread::sleep(Duration::from_millis(200));
        let log = log.lock().unwrap().clone();
        (done, format!("{work}\n{events}\n{metrics}"), log)
    })
    .await
    .unwrap();
    assert!(
        done["status"] != "running" && done["status"] != "queued",
        "the work did not end"
    );

    // Positive controls, so each absence below is not vacuous: the key really was sent to the
    // model, the worker really ran in Compute sessions, and the records really hold command
    // environments.
    let sent = model.auth.lock().unwrap().clone();
    assert!(
        !sent.is_empty() && sent.iter().all(|h| h == &format!("Bearer {SENTINEL_KEY}")),
        "the model endpoint did not receive the configured key as a bearer credential"
    );
    let stored = files_under(target.store_dir());
    assert!(
        stored
            .iter()
            .any(|(_, bytes)| contains(bytes, "capability-exec")),
        "no job records were found in the target's stores: the check below would prove nothing"
    );
    let worker_environment = std::fs::read(&dump).unwrap_or_default();
    assert!(
        contains(&worker_environment, COMMAND_ENVIRONMENT_MARKER),
        "the worker's environment was not recorded, so it cannot show the key was not forwarded"
    );
    assert!(!log.is_empty(), "the service wrote no log at all");
    // compute#91: the no-persistence limitation is said where an operator sees it.
    assert!(
        contains(&log, "works are not persisted"),
        "the service did not warn that works are not persisted"
    );

    // Absence: what the service reports, what it logged, what the model was shown, and everything
    // the Compute target stored durably: events, session data, job records and receipts, and the
    // command environment of every session command (the worker's environment). The sentinel is
    // never printed in a failure message.
    assert!(
        !done.to_string().contains(SENTINEL_KEY) && !reported.contains(SENTINEL_KEY),
        "the key is in the work, its events, or the metrics"
    );
    assert!(
        !contains(&log, SENTINEL_KEY),
        "the key is in the service's log"
    );
    assert!(
        !contains(&worker_environment, SENTINEL_KEY),
        "the key was forwarded into the session worker's environment"
    );
    let shown = model
        .seen
        .lock()
        .unwrap()
        .values()
        .flatten()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !shown.contains(SENTINEL_KEY),
        "the key was shown to the model"
    );
    for (path, bytes) in &stored {
        assert!(
            !contains(bytes, SENTINEL_KEY),
            "the key is in a Compute record: {}",
            path.display()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_models_own_claim_of_success_does_not_complete_a_work_without_the_required_evidence() {
    if !pax_installed() {
        eprintln!("SKIPPED: PAX is not installed");
        return;
    }
    let target = Target::start();
    let source = project("claim-without-evidence", true);
    // The model asks for nothing and declares victory at once.
    let script: Script = HashMap::from([(
        "ALPHA",
        vec![json!({"decision": "complete", "summary": "all done, trust me"}).to_string()],
    )]);
    let model = mock(script, None).await;
    let (url, endpoint, tgt_token, src) = (
        model.url.clone(),
        target.endpoint.clone(),
        target.token.clone(),
        source.clone(),
    );
    let done = tokio::task::spawn_blocking(move || {
        let service = start(
            launcher_for(&endpoint, &tgt_token, &src, &url),
            &["--port", "0"],
        );
        let id = submit(
            &service.addr,
            "ALPHA: put the word ALPHA in README.md, then verify.",
        );
        wait_done(&service.addr, &id)
    })
    .await
    .unwrap();
    assert_ne!(
        done["status"], "completed",
        "a claim without evidence completed the work"
    );
    assert_eq!(done["result"]["verified"], false);
    assert_eq!(done["result"]["pax_executions"], 0);
    assert_eq!(done["result"]["audit"]["false_completions"], 0);
    // Nothing changed in the project either.
    assert_eq!(
        std::fs::read_to_string(source.join("README.md")).unwrap(),
        "baseline\n"
    );
}

/// G3 (compute#90): Compute asks Rust Chip to cancel through Chip's own endpoint, and reports
/// what Chip established: a queued work never ran; a running work is asked to stop (advisory: it
/// is not reported cancelled); a finished work is unchanged; asking again changes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn compute_cancels_through_chips_endpoint_and_reports_what_stopped() {
    use compute_rust_chip::cancel::{ChipCancellation, request};
    if !pax_installed() {
        eprintln!("SKIPPED: PAX is not installed");
        return;
    }
    let target = Target::start();
    let source = project("cancel", true);
    // ALPHA's first model reply is held, so it is cancelled during a provider call; BETA waits
    // in the queue behind it, so it is cancelled before any provider call.
    let script: Script = HashMap::from([
        ("ALPHA", vec![write("README.md", "ALPHA\n"), pax_test()]),
        ("BETA", vec![write("README.md", "BETA\n"), pax_test()]),
    ]);
    let model = mock(script, Some(Duration::from_secs(2))).await;
    let (url, endpoint, tgt_token, src) = (
        model.url.clone(),
        target.endpoint.clone(),
        target.token.clone(),
        source.clone(),
    );
    let service = tokio::task::spawn_blocking(move || {
        start(
            launcher_for(&endpoint, &tgt_token, &src, &url),
            &["--port", "0", "--max-concurrent-work", "1"],
        )
    })
    .await
    .unwrap();
    let addr = service.addr.clone();
    let (alpha, beta) = {
        let addr = addr.clone();
        tokio::task::spawn_blocking(move || {
            (
                submit(
                    &addr,
                    "ALPHA: put the word ALPHA in README.md, then verify.",
                ),
                submit(&addr, "BETA: put the word BETA in README.md, then verify."),
            )
        })
        .await
        .unwrap()
    };

    // Cancel ALPHA while its first model call is in flight (the mock holds the reply).
    let deadline = Instant::now() + Duration::from_secs(60);
    while model
        .seen
        .lock()
        .unwrap()
        .get("ALPHA")
        .is_none_or(Vec::is_empty)
    {
        assert!(Instant::now() < deadline, "ALPHA never called the model");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        request(&addr, &beta).await.unwrap(),
        ChipCancellation::Cancelled
    );
    assert_eq!(
        request(&addr, &alpha).await.unwrap(),
        ChipCancellation::Requested
    );
    assert_eq!(
        request(&addr, "work_nope").await.unwrap(),
        ChipCancellation::Unknown
    );

    let (alpha_done, beta_done) = {
        let addr = addr.clone();
        let (alpha, beta) = (alpha.clone(), beta.clone());
        tokio::task::spawn_blocking(move || (wait_done(&addr, &alpha), wait_done(&addr, &beta)))
            .await
            .unwrap()
    };
    assert_eq!(beta_done["status"], "cancelled");
    assert!(
        model
            .seen
            .lock()
            .unwrap()
            .get("BETA")
            .is_none_or(Vec::is_empty),
        "a work cancelled in the queue reached the model"
    );
    // Requested is not cancelled: Chip reports however the running work actually ended.
    assert_eq!(alpha_done["cancellation_requested"], true);
    assert_ne!(alpha_done["status"], "running");

    // After completion, and repeatedly: nothing changes.
    for _ in 0..2 {
        assert_eq!(
            request(&addr, &alpha).await.unwrap(),
            ChipCancellation::AlreadyFinished
        );
    }
    drop(service);
}
