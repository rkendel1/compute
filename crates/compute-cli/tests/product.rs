//! Product acceptance: an application goes from source to a running,
//! reachable, versioned, recoverable deployment, and the developer never
//! says where it runs.
//!
//! ```text
//! compute pool:  jobs-only (compute serve)   provider-a   provider-b
//!                  runs workloads only        daemon       daemon
//! compute deploy hello-api
//!   → placement rejects jobs-only, selects provider-a
//!   → provider-a's daemon releases v1, returns the endpoint
//! v2 · rollback (v3) · stop · receipts that verify offline
//! ```
//!
//! Every provider is an ordinary Compute process on this machine; nothing
//! here depends on a container runtime, a VM, or the network. Managed
//! runtimes come from a host-backed fixture catalog. The provider daemons
//! require their operator credential: a remote deployment is never
//! unauthenticated.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_compute");
const TOKEN_A: &str = "provider-a-operator";
const TOKEN_B: &str = "provider-b-operator";

/// A port, and `width` ports after it, that nothing listens on now and
/// that no earlier call handed out.
fn free_window(width: u16) -> u16 {
    static TAKEN: std::sync::Mutex<Vec<(u16, u16)>> = std::sync::Mutex::new(Vec::new());
    let mut taken = TAKEN.lock().unwrap();
    for attempt in 0..500u32 {
        let base = 42_000 + ((std::process::id() * 7919 + attempt * 104_729) % 20_000) as u16;
        let end = base + width;
        if taken
            .iter()
            .any(|(start, stop)| base < *stop && *start < end)
        {
            continue;
        }
        let listeners = (base..end)
            .map(|port| TcpListener::bind(("127.0.0.1", port)))
            .collect::<Result<Vec<_>, _>>();
        if listeners.is_ok() {
            taken.push((base, end));
            return base;
        }
    }
    panic!("no free port window");
}

struct Pool {
    root: tempfile::TempDir,
    catalog: PathBuf,
    daemons: Vec<(String, String)>,
    serve: Option<Child>,
}

impl Pool {
    fn start() -> Self {
        let root = tempfile::tempdir().unwrap();
        let catalog = compute_provider::testing::host_fixture_catalog(&root.path().join("catalog"))
            .expect("fixture runtime catalog")
            .path;
        let mut pool = Self {
            root,
            catalog,
            daemons: vec![],
            serve: None,
        };
        let a = pool.start_daemon("a", TOKEN_A);
        let b = pool.start_daemon("b", TOKEN_B);
        let serve_port = free_window(1);
        let serve = Command::new(BIN)
            .args([
                "serve",
                "--listen",
                &format!("127.0.0.1:{serve_port}"),
                "--job-store",
            ])
            .arg(pool.root.path().join("jobs"))
            .env("COMPUTE_RUNTIME_CATALOG", &pool.catalog)
            .env(
                "COMPUTE_RUNTIME_STORE",
                pool.root.path().join("store-serve"),
            )
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        pool.serve = Some(serve);
        wait_until("compute serve answers", || {
            TcpStream::connect(("127.0.0.1", serve_port)).is_ok()
        });
        // Placement prefers higher priority: the jobs-only server first.
        std::fs::write(
            pool.config(),
            format!(
                r#"[providers.jobs-only]
kind = "remote"
endpoint = "http://127.0.0.1:{serve_port}"
priority = 100

[providers.provider-a]
kind = "remote"
endpoint = "{a}"
token_env = "PROVIDER_A_TOKEN"
priority = 50

[providers.provider-b]
kind = "remote"
endpoint = "{b}"
token_env = "PROVIDER_B_TOKEN"
priority = 10
"#
            ),
        )
        .unwrap();
        pool
    }

    fn start_daemon(&mut self, name: &str, token: &str) -> String {
        let listen = free_window(1);
        let endpoints = free_window(4);
        let instances = free_window(12);
        let url = format!("http://127.0.0.1:{listen}");
        let output = Command::new(BIN)
            .args([
                "start",
                "--detach",
                "--listen",
                &format!("127.0.0.1:{listen}"),
            ])
            .arg("--state-dir")
            .arg(self.root.path().join(format!("node-{name}")))
            .args([
                "--port-range",
                &format!("{endpoints}-{}", endpoints + 3),
                "--instance-port-range",
                &format!("{instances}-{}", instances + 11),
                "--reconcile-interval-ms",
                "500",
                "--require-token-env",
                "NODE_TOKEN",
            ])
            .env("NODE_TOKEN", token)
            .env("COMPUTE_RUNTIME_CATALOG", &self.catalog)
            .env(
                "COMPUTE_RUNTIME_STORE",
                self.root.path().join(format!("store-{name}")),
            )
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", text(&output));
        self.daemons.push((url.clone(), token.to_owned()));
        url
    }

    fn config(&self) -> PathBuf {
        self.root.path().join("compute-pool.toml")
    }

    /// `compute …` as a developer runs it: the pool is configured, and
    /// its credentials are in the environment.
    fn compute(&self, arguments: &[&str]) -> Output {
        Command::new(BIN)
            .args(arguments)
            .current_dir(self.root.path())
            .env("COMPUTE_POOL_CONFIG", self.config())
            .env(
                "COMPUTE_CAPABILITY_CACHE",
                self.root.path().join("cache.json"),
            )
            .env("PROVIDER_A_TOKEN", TOKEN_A)
            .env("PROVIDER_B_TOKEN", TOKEN_B)
            .env("COMPUTE_RUNTIME_CATALOG", &self.catalog)
            .output()
            .unwrap()
    }

    fn json(&self, arguments: &[&str]) -> Value {
        let output = self.compute(arguments);
        assert!(output.status.success(), "{arguments:?}: {}", text(&output));
        serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|error| panic!("{arguments:?}: {error}: {}", text(&output)))
    }

    /// The provider daemon's API, as an agent reads it.
    fn api(&self, daemon: usize, path: &str) -> Vec<u8> {
        let (url, token) = &self.daemons[daemon];
        let (status, body) = http_get(url, path, Some(token)).unwrap();
        assert_eq!(status, 200, "{path}: {}", String::from_utf8_lossy(&body));
        body
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        for (url, token) in &self.daemons {
            let _ = Command::new(BIN)
                .args(["stop", "--daemon", url])
                .env("COMPUTE_DAEMON_TOKEN", token)
                .output();
        }
        if let Some(mut serve) = self.serve.take() {
            let _ = serve.kill();
            let _ = serve.wait();
        }
    }
}

fn text(output: &Output) -> String {
    format!(
        "status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `GET path` over plain HTTP/1.1: the status and body.
fn http_get(url: &str, path: &str, token: Option<&str>) -> std::io::Result<(u16, Vec<u8>)> {
    let authority = url.trim_start_matches("http://").trim_end_matches('/');
    let mut stream = TcpStream::connect(authority)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let authorization = token
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\n{authorization}Connection: close\r\n\r\n"
    )?;
    let mut response = vec![];
    stream.read_to_end(&mut response)?;
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::other("malformed response"))?;
    let head = String::from_utf8_lossy(&response[..split]).into_owned();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| std::io::Error::other("malformed status"))?;
    let mut body = response[split + 4..].to_vec();
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        body = dechunk(&body);
    }
    Ok((status, body))
}

fn dechunk(mut body: &[u8]) -> Vec<u8> {
    let mut out = vec![];
    while let Some(end) = body.windows(2).position(|window| window == b"\r\n") {
        let size =
            usize::from_str_radix(String::from_utf8_lossy(&body[..end]).trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        out.extend_from_slice(&body[end + 2..end + 2 + size]);
        body = &body[end + 2 + size + 2..];
    }
    out
}

fn fetch(endpoint: &str) -> Option<String> {
    http_get(endpoint, "/", None)
        .ok()
        .filter(|(status, _)| *status == 200)
        .map(|(_, body)| String::from_utf8_lossy(&body).trim().to_owned())
}

fn version(history: &Value, version: u64) -> Value {
    history
        .as_array()
        .unwrap()
        .iter()
        .find(|deployment| deployment["version"] == version)
        .unwrap_or_else(|| panic!("no v{version} in {history:#}"))
        .clone()
}

fn set_greeting(application: &Path, greeting: &str) {
    let main = application.join("main.py");
    let source = std::fs::read_to_string(&main).unwrap();
    let start = source.find("b'").unwrap() + 2;
    let end = start + source[start..].find("\\n'").unwrap();
    std::fs::write(
        &main,
        format!("{}{greeting}{}", &source[..start], &source[end..]),
    )
    .unwrap();
}

#[test]
fn an_application_moves_from_source_to_a_placed_versioned_verifiable_deployment() {
    let pool = Pool::start();
    let application = pool.root.path().join("hello-api");
    let app = application.to_str().unwrap();
    let init = pool.json(&["init", app, "--runtime", "python", "--json"]);
    assert_eq!(init["application"]["name"], "hello-api");

    // v1: placement chooses; the provider's daemon releases and returns
    // the endpoint.
    let v1 = pool.json(&["deploy", app, "--json"]);
    assert_eq!(v1["provider"], "provider-a", "{v1:#}");
    assert_eq!(v1["version"], 1);
    assert_eq!(v1["status"], "running");
    assert_eq!(v1["application"]["name"], "hello-api");
    let endpoint = v1["endpoint"].as_str().unwrap().to_owned();
    assert!(endpoint.starts_with("http://127.0.0.1:"), "{endpoint}");
    assert_eq!(v1["deployment"]["runtime"], "python");
    // Placement evidence: the jobs-only provider was rejected for this
    // deployment, and provider-a proved it can host it.
    let placement = &v1["deployment"]["placement"];
    assert_eq!(placement["provider_id"], "provider-a");
    assert_eq!(placement["selection_mode"], "pool");
    wait_until("v1 answers", || {
        fetch(&endpoint).as_deref() == Some("Hello from Compute")
    });

    let status = pool.json(&["status", app, "--json"]);
    assert_eq!(status["status"], "running");
    assert_eq!(status["version"], 1);
    assert_eq!(status["provider"], "provider-a");
    assert_eq!(status["endpoint"], endpoint.as_str());
    let logs = pool.json(&["logs", app, "--json"]);
    let output = format!("{}{}", logs["stdout"], logs["stderr"]);
    assert!(output.contains("GET / HTTP"), "{output}");
    let first = version(&pool.json(&["history", app, "--json"]), 1);
    assert!(first["receipt"].as_str().is_some(), "{first:#}");

    // v2 replaces v1 without dropping a request.
    set_greeting(&application, "Hello v2");
    let serving = Arc::new(AtomicBool::new(true));
    let watcher = {
        let serving = serving.clone();
        let endpoint = endpoint.clone();
        std::thread::spawn(move || {
            let mut seen = vec![];
            while serving.load(Ordering::SeqCst) {
                seen.push(fetch(&endpoint));
                std::thread::sleep(Duration::from_millis(20));
            }
            seen
        })
    };
    let v2 = pool.json(&["deploy", app, "--json"]);
    std::thread::sleep(Duration::from_millis(200));
    serving.store(false, Ordering::SeqCst);
    let seen = watcher.join().unwrap();
    assert_eq!(v2["version"], 2);
    assert_eq!(v2["endpoint"], endpoint.as_str(), "the endpoint is stable");
    assert!(
        seen.iter()
            .all(|body| matches!(body.as_deref(), Some("Hello from Compute" | "Hello v2"))),
        "every request during the release was answered: {seen:?}"
    );
    assert_eq!(seen.first().unwrap().as_deref(), Some("Hello from Compute"));
    assert_eq!(fetch(&endpoint).as_deref(), Some("Hello v2"));
    let history = pool.json(&["history", app, "--json"]);
    assert_eq!(version(&history, 2)["state"], "active");
    assert_eq!(version(&history, 1)["state"], "superseded");

    // Rollback is the next version, with v1's code; v1 is not edited.
    let v3 = pool.json(&["rollback", app, "1", "--json"]);
    assert_eq!(v3["version"], 3);
    assert_eq!(v3["deployment"]["rollback_of"], 1);
    assert_eq!(v3["endpoint"], endpoint.as_str());
    assert_eq!(fetch(&endpoint).as_deref(), Some("Hello from Compute"));
    let history = pool.json(&["history", app, "--json"]);
    assert_eq!(version(&history, 3)["state"], "active");
    assert_eq!(version(&history, 3)["rollback_of"], 1);
    assert_eq!(version(&history, 2)["state"], "superseded");
    assert_eq!(
        version(&history, 1)["deployment_id"],
        first["deployment_id"]
    );
    assert_eq!(version(&history, 1)["created_at"], first["created_at"]);

    // The application lives where it was placed: deploy does not move it.
    let moved = pool.compute(&["deploy", app, "--provider", "provider-b"]);
    assert!(!moved.status.success());
    assert!(
        String::from_utf8_lossy(&moved.stderr).contains("deployed on provider-a"),
        "{}",
        text(&moved)
    );

    // Requirements no provider meets fail before anything is deployed,
    // with what each provider lacks.
    let elsewhere = pool.root.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::copy(application.join("main.py"), elsewhere.join("main.py")).unwrap();
    let other = if cfg!(target_arch = "aarch64") {
        "x86_64"
    } else {
        "aarch64"
    };
    std::fs::write(
        elsewhere.join("compute.toml"),
        std::fs::read_to_string(application.join("compute.toml"))
            .unwrap()
            .replace("name = \"hello-api\"", "name = \"elsewhere\"")
            .replace(
                "[runtime]",
                &format!("[runtime]\narchitecture = \"{other}\""),
            ),
    )
    .unwrap();
    let refused = pool.compute(&["deploy", elsewhere.to_str().unwrap()]);
    assert!(!refused.status.success());
    let explanation = String::from_utf8_lossy(&refused.stderr);
    assert!(
        explanation.contains("No compatible provider found for elsewhere"),
        "{explanation}"
    );
    assert!(
        explanation.contains(&format!("architecture: {other}")),
        "{explanation}"
    );
    assert!(
        explanation.contains("does not host application deployments"),
        "{explanation}"
    );
    let applications: Value = serde_json::from_slice(&pool.api(0, "/applications")).unwrap();
    assert_eq!(
        applications.as_array().unwrap().len(),
        1,
        "nothing was deployed"
    );

    // Stop: the application stops serving; its history remains.
    let stopped = pool.json(&["stop", app, "--json"]);
    assert_eq!(stopped["status"], "stopped");
    wait_until("the endpoint stops serving", || fetch(&endpoint).is_none());
    let history = pool.json(&["history", app, "--json"]);
    assert_eq!(history.as_array().unwrap().len(), 3);
    assert_eq!(version(&history, 3)["state"], "stopped");

    // Evidence: application → deployment → execution → receipt, fetched
    // through the API and verified offline.
    let v3_id = version(&history, 3)["deployment_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut deployment = Value::Null;
    wait_until("v3's execution receipt is bound", || {
        deployment = serde_json::from_slice(
            &pool.api(0, &format!("/applications/hello-api/deployments/{v3_id}")),
        )
        .unwrap();
        deployment["execution_receipts"]
            .as_array()
            .is_some_and(|receipts| !receipts.is_empty())
    });
    let receipt_id = deployment["execution_receipts"][0].as_str().unwrap();
    let stored = pool.api(0, &format!("/receipts/{receipt_id}"));
    let file = pool.root.path().join("receipt.json");
    std::fs::write(&file, &stored).unwrap();
    let verified = pool.compute(&["receipt", "verify", file.to_str().unwrap()]);
    assert!(verified.status.success(), "{}", text(&verified));
    let receipt: Value = serde_json::from_slice(&stored).unwrap();
    assert_eq!(receipt["receipt_hash"], receipt_id);
    assert_eq!(receipt["application"]["name"], "hello-api");
    assert_eq!(receipt["application"]["id"], v1["application"]["id"]);
    assert_eq!(receipt["scope"]["deployment_id"], v3_id.as_str());
    assert!(receipt["provider"].is_object(), "{receipt:#}");
    assert!(
        receipt["placement"]["placement_id"].is_string(),
        "{receipt:#}"
    );
    assert!(receipt["admission_id"].is_string(), "{receipt:#}");
    assert!(
        receipt["runtime"]["distribution_id"].is_string(),
        "{receipt:#}"
    );
    let deployment_receipt: Value =
        serde_json::from_slice(&pool.api(0, &format!("/deployments/{v3_id}/receipt"))).unwrap();
    assert_eq!(deployment_receipt["application"]["name"], "hello-api");
    assert_eq!(deployment_receipt["deployment_version"], 3);
    assert!(
        deployment_receipt["execution_receipts"]
            .as_array()
            .unwrap()
            .contains(&Value::String(receipt_id.into())),
        "{deployment_receipt:#}"
    );
    let v1_receipt: Value = serde_json::from_slice(&pool.api(
        0,
        &format!(
            "/deployments/{}/receipt",
            first["deployment_id"].as_str().unwrap()
        ),
    ))
    .unwrap();
    assert_eq!(
        v1_receipt["workloads"][0]["pool_placement"]["provider_id"],
        "provider-a"
    );
    // The application is not on provider-b.
    let (status, _) =
        http_get(&pool.daemons[1].0, "/applications/hello-api", Some(TOKEN_B)).unwrap();
    assert_eq!(status, 404);
    // Remote deployment needs the provider's credential.
    let (status, _) = http_get(&pool.daemons[0].0, "/applications", None).unwrap();
    assert_eq!(status, 401);
}
