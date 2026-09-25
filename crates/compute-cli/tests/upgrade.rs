//! Upgrading the Compute controller on a node without touching its
//! workloads: success, rollback, a new build that fails, and one that
//! hangs — each under continuous traffic to a running service.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const SERVICE: &str = r#"import http.server, os
body = ("pid=%d" % os.getpid()).encode()
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *args):
        pass
http.server.ThreadingHTTPServer(("127.0.0.1", int(os.environ["PORT"])), Handler).serve_forever()
"#;

#[path = "support/runtimes.rs"]
mod runtimes;

fn compute_binary() -> PathBuf {
    assert_cmd::cargo::cargo_bin("compute")
}

fn compute_command() -> Command {
    let mut command = Command::new(compute_binary());
    runtimes::with_fixture_runtimes(&mut command);
    command
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn python_available() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

struct Node {
    endpoint: String,
    state: PathBuf,
    window: u16,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = compute_command()
            .args(["stop", "--daemon", &self.endpoint])
            .output();
    }
}

impl Node {
    fn run(&self, args: &[&str]) -> serde_json::Value {
        let output = compute_command()
            .args(args)
            .args(["--daemon", &self.endpoint, "--json"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "compute {args:?}: {}{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn try_run(&self, args: &[&str]) -> (bool, String) {
        let output = compute_command()
            .args(args)
            .args(["--daemon", &self.endpoint, "--json"])
            .output()
            .unwrap();
        (
            output.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
        )
    }
}

fn start(root: &Path, window: u16) -> Node {
    let port = free_port();
    let node = Node {
        endpoint: format!("http://127.0.0.1:{port}"),
        state: root.join("node"),
        window,
    };
    let status = compute_command()
        .args([
            "start",
            "--detach",
            "--listen",
            &format!("127.0.0.1:{port}"),
        ])
        .args(["--reconcile-interval-ms", "300"])
        .args([
            "--port-range",
            &format!("{}-{}", node.window, node.window + 99),
            "--instance-port-range",
            &format!("{}-{}", node.window + 20000, node.window + 20099),
        ])
        .arg("--state-dir")
        .arg(&node.state)
        .status()
        .unwrap();
    assert!(status.success());
    node
}

fn deploy(node: &Node, root: &Path) -> u16 {
    let source = root.join("app");
    std::fs::create_dir_all(source.join("api")).unwrap();
    std::fs::write(source.join("api/main.py"), SERVICE).unwrap();
    std::fs::write(
        source.join("api/workload.json"),
        r#"{"version":"1","runtime":"python","entrypoint":"main.py","network":"network"}"#,
    )
    .unwrap();
    std::fs::write(
        source.join("compute.project.toml"),
        "[project]\nname = \"app\"\n\n[[workload]]\nname = \"api\"\nkind = \"service\"\nworkload = \"api/workload.json\"\nports = [{ name = \"http\", port = 8000 }]\n",
    )
    .unwrap();
    node.run(&["environment", "create", "production"]);
    let deployment = node.run(&[
        "deploy",
        "app",
        "--environment",
        "production",
        "--source",
        source.to_str().unwrap(),
        "--revision",
        "v1",
        "--wait",
    ]);
    assert_eq!(deployment["status"], "complete", "{deployment}");
    let project = node.run(&["project", "status", "app", "--environment", "production"]);
    project["workloads"][0]["ports"][0]["host"]
        .as_u64()
        .unwrap() as u16
}

fn get(port: u16) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    stream.write_all(b"GET / HTTP/1.0\r\n\r\n").ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    let body = response.split("\r\n\r\n").nth(1)?.to_string();
    (!body.is_empty()).then_some(body)
}

/// Requests every 10 ms until stopped: (answered, failed, distinct bodies).
fn probe(
    port: u16,
) -> (
    Arc<AtomicBool>,
    std::thread::JoinHandle<(u64, u64, std::collections::BTreeSet<String>)>,
) {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let handle = std::thread::spawn(move || {
        let (mut ok, mut failed) = (0, 0);
        let mut bodies = std::collections::BTreeSet::new();
        while !flag.load(Ordering::SeqCst) {
            match get(port) {
                Some(body) => {
                    ok += 1;
                    bodies.insert(body);
                }
                None => failed += 1,
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        (ok, failed, bodies)
    });
    (stop, handle)
}

/// A build that differs from this one only in its bytes: a new build ID.
fn rebuilt(root: &Path, name: &str) -> PathBuf {
    let path = root.join(name);
    let mut bytes = std::fs::read(compute_binary()).unwrap();
    bytes.extend_from_slice(format!("\ncompute-upgrade-test-{name}\n").as_bytes());
    std::fs::write(&path, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// An artifact that answers `version --json` like a compatible Compute,
/// then misbehaves when started.
fn impostor(root: &Path, name: &str, start: &str) -> PathBuf {
    let version = compute_command()
        .args(["version", "--json"])
        .output()
        .unwrap()
        .stdout;
    let path = root.join(name);
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nif [ \"$1\" = version ]; then cat <<'JSON'\n{}\nJSON\nexit 0; fi\n{start}\n",
            String::from_utf8_lossy(&version).trim()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

fn build_id(node: &Node) -> String {
    node.run(&["node", "info"])["controller"]["build_id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn the_controller_upgrades_and_rolls_back_under_traffic_without_touching_workloads() {
    if !python_available() {
        eprintln!("skipping: python3 is unavailable");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let node = start(root.path(), 24000);
    let port = deploy(&node, root.path());
    let original = build_id(&node);
    let service = get(port).unwrap();
    let artifact = rebuilt(root.path(), "compute-next");
    let (stop, prober) = probe(port);
    std::thread::sleep(Duration::from_millis(300));

    let started = Instant::now();
    let upgraded = node.run(&["node", "upgrade", artifact.to_str().unwrap()]);
    let upgrade_seconds = started.elapsed().as_secs_f64();
    assert_eq!(upgraded["status"], "completed", "{upgraded}");
    let next = build_id(&node);
    assert_ne!(next, original);
    assert_eq!(upgraded["to"]["build_id"], next);
    assert_eq!(upgraded["units"].as_array().unwrap().len(), 1);

    let started = Instant::now();
    let rolled = node.run(&["node", "rollback"]);
    let rollback_seconds = started.elapsed().as_secs_f64();
    assert_eq!(rolled["status"], "completed", "{rolled}");
    assert_eq!(build_id(&node), original, "the previous build runs again");
    std::thread::sleep(Duration::from_millis(300));
    stop.store(true, Ordering::SeqCst);
    let (ok, failed, bodies) = prober.join().unwrap();
    assert_eq!(failed, 0, "{failed} of {} requests failed", ok + failed);
    assert_eq!(bodies, [service].into(), "one process served everything");
    let events = node.run(&["events", "--limit", "300"]);
    let kinds = events
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["kind"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    for kind in [
        "upgrade.started",
        "upgrade.ready",
        "upgrade.completed",
        "workload.reattached",
    ] {
        assert!(kinds.contains(&kind.to_string()), "{kind}: {kinds:?}");
    }
    // Both builds are kept on the node.
    assert_eq!(
        std::fs::read_dir(node.state.join("binaries"))
            .unwrap()
            .count(),
        2
    );
    eprintln!(
        "upgrade {upgrade_seconds:.2}s, rollback {rollback_seconds:.2}s, {ok} requests, {failed} failed"
    );
}

#[test]
fn a_new_build_that_fails_or_hangs_is_rolled_back() {
    if !python_available() {
        eprintln!("skipping: python3 is unavailable");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let node = start(root.path(), 24200);
    let port = deploy(&node, root.path());
    let original = build_id(&node);
    let service = get(port).unwrap();
    let (stop, prober) = probe(port);

    // Starts and dies at once: the health check fails.
    let crashing = impostor(root.path(), "compute-crash", "exit 1");
    let (succeeded, output) = node.try_run(&["node", "upgrade", crashing.to_str().unwrap()]);
    assert!(!succeeded, "{output}");
    assert!(output.contains("upgrade_failed"), "{output}");
    let record = node.run(&["node", "upgrade-status"]);
    assert_eq!(record["status"], "rolled_back", "{record}");
    assert!(
        record["reason"].as_str().unwrap().contains("exited"),
        "{record}"
    );
    assert_eq!(build_id(&node), original);

    // Starts and never becomes ready: the deadline passes.
    let hanging = impostor(root.path(), "compute-hang", "sleep 600");
    let (succeeded, output) = node.try_run(&[
        "node",
        "upgrade",
        hanging.to_str().unwrap(),
        "--timeout",
        "5",
    ]);
    assert!(!succeeded, "{output}");
    let record = node.run(&["node", "upgrade-status"]);
    assert_eq!(record["status"], "rolled_back", "{record}");
    assert!(
        record["reason"].as_str().unwrap().contains("not ready"),
        "{record}"
    );
    assert_eq!(build_id(&node), original);

    // A digest that does not match is refused before anything stops.
    let other = rebuilt(root.path(), "compute-other");
    let (succeeded, output) = node.try_run(&[
        "node",
        "upgrade",
        other.to_str().unwrap(),
        "--sha256",
        "0000000000000000000000000000000000000000000000000000000000000000",
    ]);
    assert!(!succeeded && output.contains("upgrade failed"), "{output}");

    stop.store(true, Ordering::SeqCst);
    let (ok, failed, bodies) = prober.join().unwrap();
    assert_eq!(failed, 0, "{failed} of {} requests failed", ok + failed);
    assert_eq!(bodies, [service].into());
    let events = node.run(&["events", "--limit", "300"]);
    let kinds = events
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["kind"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(kinds.contains(&"upgrade.failed".to_string()), "{kinds:?}");
    assert!(
        kinds.contains(&"upgrade.rolled_back".to_string()),
        "{kinds:?}"
    );
}
