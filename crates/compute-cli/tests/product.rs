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

/// A Compute daemon this test started: enough to stop and start it again
/// as the same node.
struct Node {
    name: String,
    url: String,
    token: String,
    /// The environment variable the pool reads its token from.
    token_env: String,
    listen: u16,
    endpoints: u16,
    instances: u16,
    offer: Option<String>,
}

struct Pool {
    root: tempfile::TempDir,
    catalog: PathBuf,
    daemons: Vec<Node>,
    servers: Vec<Child>,
    /// The daemon the `local` pool member uses, when there is one.
    local: Option<usize>,
}

impl Pool {
    fn empty() -> Self {
        let root = tempfile::tempdir().unwrap();
        let catalog = compute_provider::testing::host_fixture_catalog(&root.path().join("catalog"))
            .expect("fixture runtime catalog")
            .path;
        Self {
            root,
            catalog,
            daemons: vec![],
            servers: vec![],
            local: None,
        }
    }

    /// Two provider daemons and a jobs-only `compute serve`, which placement
    /// prefers (higher priority) whenever it qualifies.
    fn start() -> Self {
        let mut pool = Self::empty();
        let a = pool.start_daemon("a", TOKEN_A, None);
        let b = pool.start_daemon("b", TOKEN_B, None);
        let serve = pool.start_serve("jobs-only", None);
        pool.daemons[a].token_env = "PROVIDER_A_TOKEN".into();
        pool.daemons[b].token_env = "PROVIDER_B_TOKEN".into();
        pool.configure(&[
            ("jobs-only", Member::Remote(serve), 100),
            ("provider-a", Member::Daemon(a), 50),
            ("provider-b", Member::Daemon(b), 10),
        ]);
        pool
    }

    /// Start a daemon that requires `token`, offering `offer` (all modes
    /// when `None`). Its index in `daemons`.
    fn start_daemon(&mut self, name: &str, token: &str, offer: Option<&str>) -> usize {
        let listen = free_window(1);
        let node = Node {
            name: name.into(),
            url: format!("http://127.0.0.1:{listen}"),
            token: token.into(),
            token_env: format!("NODE_{}_TOKEN", name.to_ascii_uppercase().replace('-', "_")),
            listen,
            endpoints: free_window(4),
            instances: free_window(12),
            offer: offer.map(str::to_owned),
        };
        self.launch(&node);
        self.daemons.push(node);
        self.daemons.len() - 1
    }

    fn launch(&self, node: &Node) {
        let mut command = Command::new(BIN);
        command
            .args([
                "start",
                "--detach",
                "--listen",
                &format!("127.0.0.1:{}", node.listen),
            ])
            .arg("--state-dir")
            .arg(self.root.path().join(format!("node-{}", node.name)))
            .args([
                "--port-range",
                &format!("{}-{}", node.endpoints, node.endpoints + 3),
                "--instance-port-range",
                &format!("{}-{}", node.instances, node.instances + 11),
                "--reconcile-interval-ms",
                "500",
                "--require-token-env",
                "NODE_TOKEN",
            ]);
        if let Some(offer) = &node.offer {
            command.args(["--offer", offer]);
        }
        let output = command
            .env("NODE_TOKEN", &node.token)
            .env("COMPUTE_RUNTIME_CATALOG", &self.catalog)
            .env(
                "COMPUTE_RUNTIME_STORE",
                self.root.path().join(format!("store-{}", node.name)),
            )
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", text(&output));
    }

    /// Stop a daemon and start it again as the same node: the same state,
    /// address, and ports.
    fn restart_daemon(&self, index: usize) {
        let node = &self.daemons[index];
        let stopped = Command::new(BIN)
            .args(["stop", "--daemon", &node.url])
            .env("COMPUTE_DAEMON_TOKEN", &node.token)
            .output()
            .unwrap();
        assert!(stopped.status.success(), "{}", text(&stopped));
        wait_until("the daemon stops listening", || {
            TcpStream::connect(("127.0.0.1", node.listen)).is_err()
        });
        self.launch(node);
    }

    /// Start `compute serve` offering `offer` (its default when `None`).
    fn start_serve(&mut self, name: &str, offer: Option<&str>) -> String {
        let port = free_window(1);
        let mut command = Command::new(BIN);
        command
            .args(["serve", "--listen", &format!("127.0.0.1:{port}")])
            .arg("--job-store")
            .arg(self.root.path().join(format!("jobs-{name}")));
        if let Some(offer) = offer {
            command.args(["--offer", offer]);
        }
        let child = command
            .env("COMPUTE_RUNTIME_CATALOG", &self.catalog)
            .env(
                "COMPUTE_RUNTIME_STORE",
                self.root.path().join(format!("store-{name}")),
            )
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.servers.push(child);
        wait_until("compute serve answers", || {
            TcpStream::connect(("127.0.0.1", port)).is_ok()
        });
        format!("http://127.0.0.1:{port}")
    }

    /// Write the pool: each member's ID, what it is, and its priority.
    fn configure(&mut self, members: &[(&str, Member, u32)]) {
        let mut config = String::new();
        self.local = None;
        for (id, member, priority) in members {
            match member {
                Member::Local(daemon) => {
                    self.local = Some(*daemon);
                    config.push_str(&format!(
                        "[providers.{id}]\nkind = \"local\"\npriority = {priority}\n\n"
                    ));
                }
                Member::Daemon(daemon) => {
                    let node = &self.daemons[*daemon];
                    config.push_str(&format!(
                        "[providers.{id}]\nkind = \"remote\"\nendpoint = \"{}\"\ntoken_env = \"{}\"\npriority = {priority}\n\n",
                        node.url, node.token_env
                    ));
                }
                Member::Remote(endpoint) => config.push_str(&format!(
                    "[providers.{id}]\nkind = \"remote\"\nendpoint = \"{endpoint}\"\npriority = {priority}\n\n"
                )),
            }
        }
        std::fs::write(self.config(), config).unwrap();
    }

    fn config(&self) -> PathBuf {
        self.root.path().join("compute-pool.toml")
    }

    /// `compute …` as a developer runs it: the pool is configured, and
    /// its credentials are in the environment.
    fn compute(&self, arguments: &[&str]) -> Output {
        let mut command = Command::new(BIN);
        command
            .args(arguments)
            .current_dir(self.root.path())
            .env("COMPUTE_POOL_CONFIG", self.config())
            .env(
                "COMPUTE_CAPABILITY_CACHE",
                self.root.path().join("cache.json"),
            )
            .env("COMPUTE_RUNTIME_CATALOG", &self.catalog)
            .env("COMPUTE_RUNTIME_STORE", self.root.path().join("store-cli"))
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost");
        for node in &self.daemons {
            command.env(&node.token_env, &node.token);
        }
        if let Some(local) = self.local {
            let node = &self.daemons[local];
            command
                .env("COMPUTE_DAEMON", &node.url)
                .env("COMPUTE_DAEMON_TOKEN", &node.token);
        }
        command.output().unwrap()
    }

    fn json(&self, arguments: &[&str]) -> Value {
        let output = self.compute(arguments);
        assert!(output.status.success(), "{arguments:?}: {}", text(&output));
        serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|error| panic!("{arguments:?}: {error}: {}", text(&output)))
    }

    /// `compute …` that must fail: its stderr.
    fn refused(&self, arguments: &[&str]) -> String {
        let output = self.compute(arguments);
        assert!(!output.status.success(), "{arguments:?}: {}", text(&output));
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    /// The provider daemon's API, as an agent reads it.
    fn api(&self, daemon: usize, path: &str) -> Vec<u8> {
        let node = &self.daemons[daemon];
        let (status, body) = http_get(&node.url, path, Some(&node.token)).unwrap();
        assert_eq!(status, 200, "{path}: {}", String::from_utf8_lossy(&body));
        body
    }
}

enum Member {
    /// The `local` member, backed by this daemon.
    Local(usize),
    /// A daemon, with its credential.
    Daemon(usize),
    /// A `compute serve` endpoint.
    Remote(String),
}

impl Drop for Pool {
    fn drop(&mut self) {
        for node in &self.daemons {
            let _ = Command::new(BIN)
                .args(["stop", "--daemon", &node.url])
                .env("COMPUTE_DAEMON_TOKEN", &node.token)
                .output();
        }
        for server in &mut self.servers {
            let _ = server.kill();
            let _ = server.wait();
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

    // The provider restarts: its daemon stops and starts again as the
    // same node. Reconciliation brings the application back at the same
    // endpoint, with the same identity and version.
    pool.restart_daemon(0);
    wait_until("the application serves after the provider restarts", || {
        fetch(&endpoint).as_deref() == Some("Hello from Compute")
    });
    let after = pool.json(&["application", "status", "hello-api", "--json"]);
    assert_eq!(after["application_id"], v1["application_id"]);
    assert_eq!(after["version"], 3);
    assert_eq!(after["status"], "running");
    assert_eq!(after["endpoint"], endpoint.as_str());
    assert_eq!(after["provider"], "provider-a");
    // Operations take the application's name, as an agent has it.
    let by_name = pool.json(&["application", "history", "hello-api", "--json"]);
    assert_eq!(by_name.as_array().unwrap().len(), 3);

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
    let (status, _) = http_get(
        &pool.daemons[1].url,
        "/applications/hello-api",
        Some(TOKEN_B),
    )
    .unwrap();
    assert_eq!(status, 404);
    // Remote deployment needs the provider's credential.
    let (status, _) = http_get(&pool.daemons[0].url, "/applications", None).unwrap();
    assert_eq!(status, 401);

    // The application restarts: deploying after a stop serves again, as
    // the next version, at the same endpoint.
    let restarted = pool.json(&["application", "deploy", app, "--json"]);
    assert_eq!(restarted["version"], 4);
    assert_eq!(restarted["status"], "running");
    assert_eq!(restarted["endpoint"], endpoint.as_str());
    wait_until("the restarted application answers", || {
        fetch(&endpoint).as_deref() == Some("Hello v2")
    });
}

/// A portable application artifact: packed once, described without a
/// provider, served over HTTP, and deployed by URL. The selected provider
/// fetches it and verifies it is the artifact placement evaluated, and a
/// deployment that lacks the configuration the artifact requires is
/// refused before anything runs.
#[test]
fn a_portable_artifact_is_deployed_by_url_and_fetched_by_its_provider() {
    let pool = Pool::start();
    let application = pool.root.path().join("hello-http");
    let app = application.to_str().unwrap();
    pool.json(&["init", app, "--runtime", "python", "--json"]);
    let manifest = application.join("compute.toml");
    std::fs::write(
        &manifest,
        std::fs::read_to_string(&manifest).unwrap().replace(
            "port = 3000\n",
            "port = 3000\nversion = \"1.2.0\"\nrequired_env = [\"GREETING\"]\ncapabilities = [\"http.hello\"]\n",
        ),
    )
    .unwrap();
    let file = pool.root.path().join("hello-http.capp");
    let packed = pool.json(&[
        "application",
        "pack",
        app,
        "--output",
        file.to_str().unwrap(),
        "--json",
    ]);
    let artifact_id = packed["artifact_id"].as_str().unwrap().to_owned();
    assert_eq!(
        packed["manifest"]["format"],
        "compute.application-artifact@1"
    );
    assert_eq!(packed["manifest"]["version"], "1.2.0");
    assert_eq!(packed["manifest"]["env"]["required"][0], "GREETING");

    // Packing is deterministic: the same source is the same artifact.
    let again = pool.root.path().join("again.capp");
    pool.json(&[
        "application",
        "pack",
        app,
        "--output",
        again.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(
        std::fs::read(&file).unwrap(),
        std::fs::read(&again).unwrap()
    );

    let url = serve_file(std::fs::read(&file).unwrap(), "/artifacts/hello-http.capp");
    let info = pool.json(&["application", "info", &url, "--json"]);
    assert_eq!(info["artifact_id"], artifact_id.as_str());
    assert_eq!(info["manifest"]["application"]["name"], "hello-http");
    assert_eq!(info["manifest"]["runtime"]["name"], "python");

    // Its environment contract: without GREETING, nothing is deployed.
    let refusal = pool.refused(&["deploy", &url, "--provider", "provider-b"]);
    assert!(
        refusal.contains("requires configuration it was not given: GREETING"),
        "{refusal}"
    );
    let (status, _) = http_get(
        &pool.daemons[1].url,
        "/applications/hello-http",
        Some(TOKEN_B),
    )
    .unwrap();
    assert_eq!(status, 404, "nothing was deployed");

    let deployed = pool.json(&[
        "application",
        "deploy",
        &url,
        "--provider",
        "provider-b",
        "--set",
        "GREETING=hello",
        "--json",
    ]);
    assert_eq!(deployed["provider"], "provider-b");
    assert_eq!(deployed["version"], 1);
    assert_eq!(deployed["status"], "running");
    // The provider fetched the artifact from the URL, and it is the one
    // that was packed.
    assert_eq!(deployed["artifact"]["artifact_id"], artifact_id.as_str());
    assert_eq!(deployed["artifact"]["url"], url.as_str());
    assert_eq!(deployed["artifact"]["version"], "1.2.0");
    assert_eq!(deployed["artifact"]["capabilities"][0], "http.hello");
    let endpoint = deployed["endpoint"].as_str().unwrap().to_owned();
    wait_until("the artifact's application answers", || {
        fetch(&endpoint).as_deref() == Some("Hello from Compute")
    });
    let receipt: Value = serde_json::from_slice(&pool.api(
        1,
        &format!(
            "/deployments/{}/receipt",
            deployed["deployment_id"].as_str().unwrap()
        ),
    ))
    .unwrap();
    assert_eq!(
        receipt["workloads"][0]["application_artifact"]["artifact_id"],
        artifact_id.as_str()
    );
}

/// Serve `bytes` at `path` over HTTP on a free port, for as long as the
/// test runs: its URL.
fn serve_file(bytes: Vec<u8>, path: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}{path}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = [0u8; 4096];
            let read = stream.read(&mut request).unwrap_or(0);
            let head = String::from_utf8_lossy(&request[..read]);
            let found = head.starts_with(&format!("GET {path} "));
            let (status, body): (&str, &[u8]) = if found {
                ("200 OK", &bytes)
            } else {
                ("404 Not Found", b"")
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(body);
        }
    });
    url
}

/// Every provider type crossed with every submission mode. A provider
/// accepts exactly the execution modes it offers; any other combination is
/// rejected by placement, before anything executes.
///
/// ```text
///                     run   job   deployment
/// local               yes   no    yes   (this machine: in process, and its daemon)
/// daemon              yes   yes   yes   (compute start)
/// deployment-only     no    no    yes   (compute start --offer deployments)
/// jobs-only           no    yes   no    (compute serve --offer jobs)
/// ```
#[test]
fn every_provider_accepts_exactly_the_execution_modes_it_offers() {
    let mut pool = Pool::empty();
    let local = pool.start_daemon("local-node", "local-operator", None);
    let daemon = pool.start_daemon("daemon", "daemon-operator", None);
    let deployment_only = pool.start_daemon(
        "deployment-only",
        "deployment-operator",
        Some("deployments"),
    );
    let jobs_only = pool.start_serve("jobs-only", Some("jobs"));
    // Priorities rank the providers that do not offer a mode first, so a
    // provider that cannot take a submission is never chosen by accident.
    pool.configure(&[
        ("local", Member::Local(local), 1),
        ("daemon", Member::Daemon(daemon), 10),
        ("deployment-only", Member::Daemon(deployment_only), 100),
        ("jobs-only", Member::Remote(jobs_only), 50),
    ]);
    let job = pool.root.path().join("job.py");
    std::fs::write(&job, "print('ran on a provider')\n").unwrap();
    let job = job.to_str().unwrap();

    let nothing_ran = |provider: &str, code: &str, refusal: &str| {
        assert!(
            refusal.contains(&format!("provider {provider}: incompatible"))
                && refusal.contains(code)
                && refusal.contains("nothing was executed"),
            "{provider}: {refusal}"
        );
    };
    for (provider, run, jobs, deploys) in [
        ("local", true, false, true),
        ("daemon", true, true, true),
        ("deployment-only", false, false, true),
        ("jobs-only", false, true, false),
    ] {
        let receipt = pool.root.path().join(format!("run-{provider}.json"));
        let run_arguments = [
            "pool",
            "run",
            job,
            "--network",
            "network",
            "--provider",
            provider,
            "--receipt",
            receipt.to_str().unwrap(),
            "--json",
        ];
        if run {
            let ran = pool.json(&run_arguments);
            assert_eq!(ran["status"], "completed", "{provider}: {ran:#}");
            assert_eq!(ran["stdout"]["text"], "ran on a provider\n");
            assert_eq!(ran["placement"]["selected"]["provider_id"], provider);
            // The run's receipt verifies offline.
            let verified = pool.compute(&["receipt", "verify", receipt.to_str().unwrap()]);
            assert!(verified.status.success(), "{}", text(&verified));
        } else {
            nothing_ran(provider, "run_unsupported", &pool.refused(&run_arguments));
        }
        let submit_arguments = [
            "pool",
            "submit",
            job,
            "--network",
            "network",
            "--provider",
            provider,
            "--json",
        ];
        if jobs {
            let submitted = pool.json(&submit_arguments);
            assert_eq!(submitted["provider_id"], provider, "{submitted:#}");
            assert!(submitted["job_id"].as_str().unwrap().starts_with("job_"));
        } else {
            nothing_ran(
                provider,
                "jobs_unsupported",
                &pool.refused(&submit_arguments),
            );
        }
        let application = pool.root.path().join(format!("app-{provider}"));
        let app = application.to_str().unwrap();
        pool.json(&["init", app, "--runtime", "python", "--json"]);
        let deploy_arguments = ["deploy", app, "--provider", provider, "--json"];
        if deploys {
            let deployed = pool.json(&deploy_arguments);
            assert_eq!(deployed["provider"], provider, "{deployed:#}");
            assert_eq!(deployed["status"], "running");
            assert_eq!(deployed["version"], 1);
            for field in ["application_id", "deployment_id", "runtime"] {
                assert!(deployed[field].is_string(), "{field}: {deployed:#}");
            }
            // The deployment receipt is issued once the release completes.
            wait_until("the deployment receipt is issued", || {
                let history = pool.json(&[
                    "application",
                    "history",
                    &format!("app-{provider}"),
                    "--json",
                ]);
                version(&history, 1)["receipt"].is_string()
            });
            assert!(deployed["artifact"]["artifact_id"].is_string());
            let endpoint = deployed["endpoint"].as_str().unwrap().to_owned();
            wait_until("the deployment answers", || {
                fetch(&endpoint).as_deref() == Some("Hello from Compute")
            });
        } else {
            let refusal = pool.refused(&deploy_arguments);
            assert!(
                refusal.contains("No compatible provider found")
                    && refusal.contains("does not host application deployments"),
                "{provider}: {refusal}"
            );
        }
    }
    // Nothing was deployed where deployment was refused.
    let (status, _) = http_get(
        &pool.daemons[deployment_only].url,
        "/applications/app-jobs-only",
        Some("deployment-operator"),
    )
    .unwrap();
    assert_eq!(status, 404);

    // With no provider named, placement sends each submission to the
    // highest-priority provider that offers its mode. A daemon in a run
    // pool runs workloads; it no longer fails them at dispatch. (`auto`
    // prefers this machine, so the pool here is the remote providers.)
    let jobs_only = pool.start_serve("jobs-only-2", Some("jobs"));
    pool.configure(&[
        ("daemon", Member::Daemon(daemon), 10),
        ("deployment-only", Member::Daemon(deployment_only), 100),
        ("jobs-only", Member::Remote(jobs_only), 50),
    ]);
    let ran = pool.json(&["pool", "run", job, "--network", "network", "--json"]);
    assert_eq!(ran["placement"]["selected"]["provider_id"], "daemon");
    assert_eq!(ran["status"], "completed");
    let incompatible = ran["placement"]["incompatible_providers"].to_string();
    assert!(
        incompatible.contains("deployment-only") && incompatible.contains("jobs-only"),
        "{incompatible}"
    );
    let submitted = pool.json(&["pool", "submit", job, "--network", "network", "--json"]);
    assert!(
        ["daemon", "jobs-only"].contains(&submitted["provider_id"].as_str().unwrap()),
        "{submitted:#}"
    );
    let application = pool.root.path().join("placed");
    let app = application.to_str().unwrap();
    pool.json(&["init", app, "--runtime", "python", "--json"]);
    let deployed = pool.json(&["deploy", app, "--json"]);
    assert!(
        ["daemon", "deployment-only"].contains(&deployed["provider"].as_str().unwrap()),
        "{deployed:#}"
    );
}
