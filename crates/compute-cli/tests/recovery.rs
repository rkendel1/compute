//! Process-level recovery: kill the Compute controller, start another, and
//! the workloads are still what durable control state says they should be.
//!
//! "Compute's memory can disappear. The desired state cannot." With the
//! node's supervisor, a controller's death does not even interrupt them:
//! services keep running and endpoints keep answering, and the next
//! controller reattaches.
//!
//! The FeltDB variant needs a feltdb-server binary:
//!
//! ```sh
//! FELTDB_SERVER_BIN=/path/to/feltdb-server \
//!   cargo test -p compute-cli --test recovery -- --ignored
//! ```

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SERVICE: &str = r#"import http.server, os
body = ("revision=%s pid=%d" % (os.environ["REVISION"], os.getpid())).encode()
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *args):
        pass
http.server.HTTPServer(("127.0.0.1", int(os.environ["PORT"])), Handler).serve_forever()
"#;

#[path = "support/runtimes.rs"]
mod runtimes;

fn compute() -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin("compute"));
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

/// A project whose `api` service answers with its revision and PID.
fn project(root: &Path) -> PathBuf {
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
    source
}

struct Cli {
    endpoint: String,
    env: Vec<(String, String)>,
    /// This test's own port windows: tests run their daemons concurrently.
    window: u16,
}

/// A test that fails must not leave its detached daemon (and its services)
/// running.
impl Drop for Cli {
    fn drop(&mut self) {
        self.stop();
    }
}

fn window() -> u16 {
    static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    23000 + NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst) * 100
}

impl Cli {
    fn run(&self, args: &[&str]) -> serde_json::Value {
        let output = compute()
            .args(args)
            .args(["--daemon", &self.endpoint, "--json"])
            .envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "compute {args:?} failed: {}{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "compute {args:?} printed no JSON ({error}): {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }

    fn start(&self, node: &Path, extra: &[&str]) {
        let listen = self.endpoint.trim_start_matches("http://");
        let status = compute()
            .args([
                "start",
                "--detach",
                "--listen",
                listen,
                "--reconcile-interval-ms",
                "300",
                "--port-range",
                &format!("{}-{}", self.window, self.window + 99),
                "--instance-port-range",
                &format!("{}-{}", self.window + 20000, self.window + 20099),
            ])
            .arg("--state-dir")
            .arg(node)
            .args(extra)
            .envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .status()
            .unwrap();
        assert!(status.success(), "the daemon started");
    }

    fn daemon_pid(&self) -> i32 {
        self.run(&["status"])["pid"].as_i64().unwrap() as i32
    }

    /// SIGKILL the daemon: no shutdown, no cleanup.
    fn kill(&self) {
        self.signal(libc::SIGKILL);
    }

    /// What a service manager sends to restart the controller.
    fn terminate(&self) {
        self.signal(libc::SIGTERM);
    }

    fn signal(&self, signal: i32) {
        let pid = self.daemon_pid();
        // SAFETY: signalling the daemon process this test started.
        assert_eq!(unsafe { libc::kill(pid, signal) }, 0);
        let deadline = Instant::now() + Duration::from_secs(10);
        // SAFETY: probing for existence with signal 0.
        while unsafe { libc::kill(pid, 0) } == 0 {
            assert!(Instant::now() < deadline, "the daemon did not die");
            std::thread::sleep(Duration::from_millis(50));
        }
        // Its listener is gone; wait for the port to be free.
        let deadline = Instant::now() + Duration::from_secs(10);
        while TcpStream::connect(self.endpoint.trim_start_matches("http://")).is_ok() {
            assert!(Instant::now() < deadline, "the daemon's port stayed open");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn stop(&self) {
        let _ = compute()
            .args(["stop", "--daemon", &self.endpoint])
            .envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .output();
    }

    fn host_port(&self, environment: &str) -> u16 {
        let project = self.run(&["project", "status", "app", "--environment", environment]);
        project["workloads"][0]["ports"][0]["host"]
            .as_u64()
            .unwrap() as u16
    }
}

/// GET / on a service port, waiting until it answers.
fn answer(port: u16) -> String {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            if stream.write_all(b"GET / HTTP/1.0\r\n\r\n").is_ok() {
                let mut response = String::new();
                if stream.read_to_string(&mut response).is_ok()
                    && let Some(body) = response.split("\r\n\r\n").nth(1)
                    && !body.is_empty()
                {
                    return body.to_string();
                }
            }
        }
        assert!(Instant::now() < deadline, "port {port} never answered");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn pid_of(answer: &str) -> i32 {
    answer
        .split_whitespace()
        .find_map(|word| word.strip_prefix("pid="))
        .unwrap()
        .parse()
        .unwrap()
}

/// Running: it exists and is not a zombie waiting for a parent to reap it
/// (orphans reparented to a container's init may never be reaped).
fn alive(pid: i32) -> bool {
    // SAFETY: probing for existence with signal 0.
    if unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rfind(')')
                .and_then(|end| stat[end + 2..].split_whitespace().next().map(str::to_owned))
        })
        .is_none_or(|state| state != "Z")
}

/// Wait until the service answers from a process other than `old`.
fn replaced(port: u16, old: i32) -> String {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let body = answer(port);
        if pid_of(&body) != old {
            return body;
        }
        assert!(Instant::now() < deadline, "the service was never replaced");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Probe an endpoint every few milliseconds until told to stop, counting
/// answers that fail and collecting the PIDs that answered.
struct Prober {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: std::thread::JoinHandle<(u64, u64, std::collections::BTreeSet<i32>)>,
}

impl Prober {
    fn start(port: u16) -> Self {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let handle = std::thread::spawn(move || {
            let (mut ok, mut failed) = (0_u64, 0_u64);
            let mut pids = std::collections::BTreeSet::new();
            while !flag.load(std::sync::atomic::Ordering::SeqCst) {
                let answered =
                    TcpStream::connect(("127.0.0.1", port))
                        .ok()
                        .and_then(|mut stream| {
                            stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
                            stream.write_all(b"GET / HTTP/1.0\r\n\r\n").ok()?;
                            let mut response = String::new();
                            stream.read_to_string(&mut response).ok()?;
                            let body = response.split("\r\n\r\n").nth(1)?.to_string();
                            (!body.is_empty()).then_some(body)
                        });
                match answered {
                    Some(body) => {
                        ok += 1;
                        pids.insert(pid_of(&body));
                    }
                    None => failed += 1,
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            (ok, failed, pids)
        });
        Self { stop, handle }
    }

    fn finish(self) -> (u64, u64, std::collections::BTreeSet<i32>) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        self.handle.join().unwrap()
    }
}

/// The data-plane invariant: a controller that is SIGKILLed and restarted
/// does not interrupt a running application. The process survives, the
/// endpoint answers throughout, and nothing is redeployed or restarted.
#[test]
fn a_killed_controller_leaves_its_workloads_serving() {
    if !python_available() {
        eprintln!("skipping: python3 is unavailable");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let source = project(root.path());
    let cli = Cli {
        endpoint: format!("http://127.0.0.1:{}", free_port()),
        env: vec![],
        window: window(),
    };
    let node = root.path().join("node");
    cli.start(&node, &[]);
    cli.run(&["environment", "create", "production"]);
    let deployment = cli.run(&[
        "deploy",
        "app",
        "--environment",
        "production",
        "--source",
        source.to_str().unwrap(),
        "--revision",
        "abc123",
        "--set",
        "REVISION=abc123",
        "--wait",
    ]);
    assert_eq!(deployment["status"], "complete", "{deployment}");
    let port = cli.host_port("production");
    let before = answer(port);
    let service_pid = pid_of(&before);
    let info = cli.run(&["node", "info"]);
    assert_eq!(info["data_plane"]["independent"], true, "{info}");
    let supervisor = info["data_plane"]["info"]["pid"].as_i64().unwrap() as i32;
    let deployments_before = cli.run(&["deployment", "list"]).as_array().unwrap().len();

    let prober = Prober::start(port);
    std::thread::sleep(Duration::from_millis(300));
    let killed_at = Instant::now();
    cli.kill();
    // No controller: the application still answers, from the same process.
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(pid_of(&answer(port)), service_pid);
    assert!(alive(supervisor), "the supervisor outlived the controller");
    cli.start(&node, &[]);
    let restarted_in = killed_at.elapsed();
    std::thread::sleep(Duration::from_millis(1500));
    let (ok, failed, pids) = prober.finish();

    assert_eq!(
        failed,
        0,
        "{failed} of {} requests failed across the restart",
        ok + failed
    );
    assert!(
        ok > 100,
        "the endpoint was probed throughout ({ok} answers)"
    );
    assert_eq!(
        pids,
        [service_pid].into(),
        "one process served every request"
    );
    assert!(alive(service_pid), "the service was not restarted");
    // Reattached, not restarted, not redeployed.
    let info = cli.run(&["node", "info"]);
    assert_eq!(
        info["data_plane"]["info"]["pid"].as_i64().unwrap() as i32,
        supervisor
    );
    assert_eq!(
        info["data_plane"]["recovery"]["reattached"],
        serde_json::json!(["production/app/api"]),
        "{info}"
    );
    let workload = cli.run(&["project", "status", "app", "--environment", "production"]);
    assert_eq!(
        workload["workloads"][0]["actual_state"], "running",
        "{workload}"
    );
    assert_eq!(
        cli.run(&["deployment", "list"]).as_array().unwrap().len(),
        deployments_before,
        "no deployment was needed"
    );
    let events = cli.run(&["events", "--limit", "200"]);
    let kinds = events
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["kind"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(
        kinds.contains(&"workload.reattached".to_string()),
        "{kinds:?}"
    );
    assert!(kinds.contains(&"controller.ready".to_string()), "{kinds:?}");
    assert!(
        kinds.contains(&"reconcile.finished".to_string()),
        "{kinds:?}"
    );
    eprintln!(
        "controller restart: {ok} requests, {failed} failed, controller back after {restarted_in:?}"
    );
    // A full stop stops everything, supervisor included.
    cli.stop();
    let deadline = Instant::now() + Duration::from_secs(20);
    while alive(service_pid) || alive(supervisor) {
        assert!(Instant::now() < deadline, "stop left processes running");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A controller stopped with --keep-workloads leaves the application
/// serving; the next one reattaches.
#[test]
fn a_controller_stopped_for_an_upgrade_keeps_its_workloads() {
    if !python_available() {
        eprintln!("skipping: python3 is unavailable");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let source = project(root.path());
    let cli = Cli {
        endpoint: format!("http://127.0.0.1:{}", free_port()),
        env: vec![],
        window: window(),
    };
    let node = root.path().join("node");
    cli.start(&node, &[]);
    cli.run(&["environment", "create", "production"]);
    let deployment = cli.run(&[
        "deploy",
        "app",
        "--environment",
        "production",
        "--source",
        source.to_str().unwrap(),
        "--revision",
        "v1",
        "--set",
        "REVISION=v1",
        "--wait",
    ]);
    assert_eq!(deployment["status"], "complete", "{deployment}");
    let port = cli.host_port("production");
    let service_pid = pid_of(&answer(port));
    let prober = Prober::start(port);
    let stopped = cli.run(&["stop", "--keep-workloads"]);
    assert_eq!(stopped["workloads_kept"], true);
    std::thread::sleep(Duration::from_millis(500));
    cli.start(&node, &[]);
    std::thread::sleep(Duration::from_millis(500));
    // A service manager's restart (SIGTERM) keeps them too.
    cli.terminate();
    std::thread::sleep(Duration::from_millis(300));
    assert!(alive(service_pid), "SIGTERM stopped the workload");
    cli.start(&node, &[]);
    std::thread::sleep(Duration::from_millis(500));
    let (ok, failed, pids) = prober.finish();
    assert_eq!(failed, 0, "{failed} of {} failed", ok + failed);
    assert_eq!(pids, [service_pid].into());
    let info = cli.run(&["node", "info"]);
    assert_eq!(
        info["data_plane"]["recovery"]["reattached"],
        serde_json::json!(["production/app/api"]),
        "{info}"
    );
    cli.stop();
}

/// A supervisor that dies takes its workloads' supervision with it. The
/// controller replaces it; the new supervisor stops what the old one left
/// running (never a second copy) and restores the endpoints; the
/// controller reports the workloads orphaned and starts them again.
/// A workload that crashes on its own is restarted by its restart policy.
#[test]
fn a_lost_supervisor_is_replaced_and_its_orphans_are_cleaned_up() {
    if !python_available() {
        eprintln!("skipping: python3 is unavailable");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let source = project(root.path());
    let cli = Cli {
        endpoint: format!("http://127.0.0.1:{}", free_port()),
        env: vec![],
        window: window(),
    };
    let node = root.path().join("node");
    cli.start(&node, &[]);
    cli.run(&["environment", "create", "production"]);
    let deployment = cli.run(&[
        "deploy",
        "app",
        "--environment",
        "production",
        "--source",
        source.to_str().unwrap(),
        "--revision",
        "v1",
        "--set",
        "REVISION=v1",
        "--wait",
    ]);
    assert_eq!(deployment["status"], "complete", "{deployment}");
    let port = cli.host_port("production");
    let first = pid_of(&answer(port));

    // The workload crashes: its restart policy brings it back.
    // SAFETY: killing the service process this test started.
    assert_eq!(unsafe { libc::kill(first, libc::SIGKILL) }, 0);
    let second = pid_of(&replaced(port, first));
    assert_ne!(second, first);

    // The supervisor dies.
    let supervisor = cli.run(&["node", "info"])["data_plane"]["info"]["pid"]
        .as_i64()
        .unwrap() as i32;
    // SAFETY: killing the supervisor process this test started.
    assert_eq!(unsafe { libc::kill(supervisor, libc::SIGKILL) }, 0);
    let third = pid_of(&replaced(port, second));
    assert!(
        !alive(second),
        "the orphan was stopped, not left beside its replacement"
    );
    let info = cli.run(&["node", "info"]);
    let replacement = info["data_plane"]["info"]["pid"].as_i64().unwrap() as i32;
    assert_ne!(replacement, supervisor, "{info}");
    assert!(alive(third));
    let events = cli.run(&["events", "--limit", "200"]);
    let kinds = events
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["kind"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    for kind in [
        "data_plane.restarted",
        "workload.orphaned",
        "workload.restarted",
    ] {
        assert!(
            kinds.contains(&kind.to_string()),
            "{kind} missing: {kinds:?}"
        );
    }
    cli.stop();
    let deadline = Instant::now() + Duration::from_secs(20);
    while alive(third) || alive(replacement) {
        assert!(Instant::now() < deadline, "stop left processes running");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A workload that ends while no controller is running keeps its outcome
/// on the supervisor; the next controller records it as evidence — the
/// execution record and its receipt — exactly once.
#[test]
fn an_execution_that_ends_while_the_controller_is_down_is_recorded_after_recovery() {
    if !python_available() {
        eprintln!("skipping: python3 is unavailable");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("batch");
    std::fs::create_dir_all(source.join("job")).unwrap();
    std::fs::write(
        source.join("job/main.py"),
        "import sys, time\nprint('working', flush=True)\ntime.sleep(3)\nprint('done', flush=True)\nsys.exit(3)\n",
    )
    .unwrap();
    std::fs::write(
        source.join("job/workload.json"),
        r#"{"version":"1","runtime":"python","entrypoint":"main.py","network":"network"}"#,
    )
    .unwrap();
    std::fs::write(
        source.join("compute.project.toml"),
        "[project]\nname = \"batch\"\n\n[[workload]]\nname = \"job\"\nkind = \"service\"\nworkload = \"job/workload.json\"\nrestart = \"never\"\nreadiness = { check = \"process\" }\n",
    )
    .unwrap();
    let cli = Cli {
        endpoint: format!("http://127.0.0.1:{}", free_port()),
        env: vec![],
        window: window(),
    };
    let node = root.path().join("node");
    cli.start(&node, &[]);
    cli.run(&["environment", "create", "production"]);
    let deployment = cli.run(&[
        "deploy",
        "batch",
        "--environment",
        "production",
        "--source",
        source.to_str().unwrap(),
        "--revision",
        "v1",
        "--wait",
    ]);
    assert_eq!(deployment["status"], "complete", "{deployment}");
    cli.kill();
    // It ends while no controller runs.
    std::thread::sleep(Duration::from_secs(4));
    cli.start(&node, &[]);
    let info = cli.run(&["node", "info"]);
    assert_eq!(
        info["data_plane"]["recovery"]["collected"],
        serde_json::json!(["production/batch/job"]),
        "{info}"
    );
    let executions = cli.run(&[
        "project",
        "executions",
        "batch",
        "--environment",
        "production",
    ]);
    let executions = executions.as_array().unwrap();
    assert_eq!(executions.len(), 1, "recorded exactly once: {executions:?}");
    assert_eq!(executions[0]["exit_code"], 3);
    assert!(executions[0]["receipt_id"].is_string());
    // A second restart does not record it again.
    cli.kill();
    cli.start(&node, &[]);
    let again = cli.run(&[
        "project",
        "executions",
        "batch",
        "--environment",
        "production",
    ]);
    assert_eq!(again.as_array().unwrap().len(), 1);
    cli.stop();
}

/// Without a supervisor, workloads share the controller's fate: a killed
/// controller's orphans are reaped, never duplicated.
#[test]
fn an_in_process_data_plane_is_reaped_not_duplicated() {
    if !python_available() {
        eprintln!("skipping: python3 is unavailable");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let source = project(root.path());
    let cli = Cli {
        endpoint: format!("http://127.0.0.1:{}", free_port()),
        env: vec![],
        window: window(),
    };
    let node = root.path().join("node");
    cli.start(&node, &["--data-plane", "in-process"]);
    cli.run(&["environment", "create", "production"]);
    let deployment = cli.run(&[
        "deploy",
        "app",
        "--environment",
        "production",
        "--source",
        source.to_str().unwrap(),
        "--revision",
        "abc123",
        "--set",
        "REVISION=abc123",
        "--wait",
    ]);
    assert_eq!(deployment["status"], "complete", "{deployment}");
    let port = cli.host_port("production");
    let before = answer(port);
    assert!(before.starts_with("revision=abc123"), "{before}");

    cli.kill();
    // The service outlived its daemon.
    assert!(alive(pid_of(&before)), "the orphan is still running");

    cli.start(&node, &["--data-plane", "in-process"]);
    let after = replaced(port, pid_of(&before));
    assert!(after.starts_with("revision=abc123"), "{after}");
    assert!(
        !alive(pid_of(&before)),
        "the orphan was reaped, not duplicated"
    );
    assert_eq!(cli.host_port("production"), port, "the same host port");
    cli.stop();
}

/// A daemon killed in the middle of a release: the next daemon reloads the
/// release from control state, inspects the node, and finishes it — one
/// switch, no duplicated instance, the endpoint unchanged.
#[test]
fn a_release_interrupted_by_a_killed_daemon_completes_after_restart() {
    if !python_available() {
        eprintln!("skipping: python3 is unavailable");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let source = project(root.path());
    let cli = Cli {
        endpoint: format!("http://127.0.0.1:{}", free_port()),
        env: vec![],
        window: window(),
    };
    let node = root.path().join("node");
    cli.start(&node, &[]);
    cli.run(&["environment", "create", "production"]);
    let first = cli.run(&[
        "deploy",
        "app",
        "--environment",
        "production",
        "--source",
        source.to_str().unwrap(),
        "--revision",
        "v1",
        "--set",
        "REVISION=v1",
        "--wait",
    ]);
    assert_eq!(first["status"], "complete", "{first}");
    let port = cli.host_port("production");
    let v1 = answer(port);
    assert!(v1.starts_with("revision=v1"), "{v1}");

    // v2 takes seconds to become ready.
    std::fs::write(
        source.join("api/main.py"),
        format!("import time\ntime.sleep(4)\n{SERVICE}"),
    )
    .unwrap();
    let second = cli.run(&[
        "deploy",
        "app",
        "--environment",
        "production",
        "--source",
        source.to_str().unwrap(),
        "--revision",
        "v2",
        "--set",
        "REVISION=v2",
    ]);
    let release = second["deployment_id"].as_str().unwrap().to_string();
    assert_eq!(second["status"], "starting", "{second}");

    cli.kill();
    cli.start(&node, &[]);
    let deadline = Instant::now() + Duration::from_secs(90);
    let settled = loop {
        let view = cli.run(&["deployment", "inspect", &release]);
        let status = view["status"].as_str().unwrap().to_string();
        if status == "complete" || status == "failed" || status == "rolled_back" {
            break view;
        }
        assert!(
            Instant::now() < deadline,
            "the release never finished: {view}"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(settled["status"], "complete", "{settled}");
    let v2 = answer(port);
    assert!(v2.starts_with("revision=v2"), "{v2}");
    assert_eq!(
        cli.host_port("production"),
        port,
        "the endpoint is unchanged"
    );
    // The drained instance's process ends once the supervisor has stopped
    // it, moments after the release records it stopped.
    let deadline = Instant::now() + Duration::from_secs(15);
    while alive(pid_of(&v1)) {
        assert!(Instant::now() < deadline, "v1 was never stopped");
        std::thread::sleep(Duration::from_millis(50));
    }
    let instances = settled["instances"].as_array().unwrap();
    assert_eq!(instances.len(), 1, "{settled}");
    assert_eq!(instances[0]["state"], "serving");
    let events = cli.run(&["events", "--deployment", &release]);
    let switches = events
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "deployment.switched")
        .count();
    assert_eq!(switches, 1, "{events}");
    let receipt = cli.run(&["deployment", "receipt", &release]);
    assert_eq!(receipt["format"], "compute.deployment-receipt@1");
    assert!(!receipt.to_string().contains("REVISION=v2"));
    cli.stop();
}

// ---- Managed FeltDB -------------------------------------------------------

const MASTER_KEY: &str = "compute-recovery-certification";

struct FeltDb {
    child: Child,
    url: String,
}

impl Drop for FeltDb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn feltdb_binary() -> PathBuf {
    PathBuf::from(std::env::var("FELTDB_SERVER_BIN").expect("FELTDB_SERVER_BIN"))
}

fn feltdb_key(data: &Path) -> String {
    let output = Command::new(feltdb_binary())
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
        .unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .find(|word| word.starts_with("fdb_live_"))
        .unwrap()
        .to_string()
}

fn feltdb_start(data: &Path) -> FeltDb {
    feltdb_start_on(data, 0)
}

fn feltdb_start_on(data: &Path, port: u16) -> FeltDb {
    let mut child = Command::new(feltdb_binary())
        .args(["--host", "127.0.0.1", "--port", &port.to_string()])
        .args(["--namespace", "compute", "--auth"])
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
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let url = loop {
        let line = lines.next().unwrap().unwrap();
        if let Some(index) = line.find("http://") {
            break line[index..].split_whitespace().next().unwrap().to_string();
        }
    };
    std::thread::spawn(move || for _ in lines {});
    FeltDb { child, url }
}

#[test]
#[ignore = "requires FELTDB_SERVER_BIN"]
fn managed_feltdb_is_the_durable_authority() {
    assert!(python_available(), "python3 is required");
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("feltdb");
    std::fs::create_dir_all(&data).unwrap();
    let token = feltdb_key(&data);
    let feltdb = feltdb_start(&data);
    let env = vec![("COMPUTE_FELTDB_TOKEN".to_string(), token)];

    // Provision the control model; provisioning twice changes nothing.
    let provision = |args: &[&str]| {
        let output = compute()
            .args([
                "control-plane",
                "provision",
                "--feltdb-url",
                &feltdb.url,
                "--json",
            ])
            .args(args)
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let first = provision(&[]);
    let second = provision(&[]);
    assert_eq!(first["application_id"], second["application_id"]);
    assert_eq!(second["changed"], false);
    let application = first["application_id"].as_str().unwrap().to_string();
    let config = root.path().join("compute.toml");
    std::fs::write(
        &config,
        format!(
            "[state]\nbackend = \"feltdb\"\n\n[state.feltdb]\nurl = \"{}\"\napplication = \"{application}\"\n",
            feltdb.url
        ),
    )
    .unwrap();
    let config_arg = ["--config", config.to_str().unwrap()];

    let source = project(root.path());
    let cli = Cli {
        endpoint: format!("http://127.0.0.1:{}", free_port()),
        env: env.clone(),
        window: window(),
    };
    let node = root.path().join("node-1");
    cli.start(&node, &config_arg);
    assert_eq!(cli.run(&["status"])["state"]["kind"], "feltdb");
    for environment in ["preprod", "production"] {
        cli.run(&["environment", "create", environment]);
    }
    // Deploy to preprod, verify, promote the same revision.
    let preprod = cli.run(&[
        "deploy",
        "app",
        "--environment",
        "preprod",
        "--source",
        source.to_str().unwrap(),
        "--revision",
        "abc123",
        "--set",
        "REVISION=abc123",
        "--wait",
    ]);
    assert_eq!(preprod["status"], "complete", "{preprod}");
    assert!(answer(cli.host_port("preprod")).starts_with("revision=abc123"));
    let production = cli.run(&[
        "promote",
        "app",
        "--from",
        "preprod",
        "--to",
        "production",
        "--set",
        "REVISION=abc123",
        "--wait",
    ]);
    assert_eq!(production["status"], "complete", "{production}");
    assert_eq!(production["revision_digest"], preprod["revision_digest"]);
    assert_eq!(production["promoted_from"], preprod["deployment_id"]);
    let port = cli.host_port("production");
    let before = answer(port);

    // The controller is killed on its node; its replacement reattaches
    // to the same running service.
    cli.kill();
    cli.start(&node, &config_arg);
    let after = answer(port);
    assert!(after.starts_with("revision=abc123"));
    assert_eq!(pid_of(&after), pid_of(&before), "reattached, not restarted");

    // The node is lost entirely: a new node restores everything from
    // Managed FeltDB, bundles included.
    cli.stop();
    std::thread::sleep(Duration::from_millis(500));
    std::fs::remove_dir_all(&node).unwrap();
    let fresh = root.path().join("node-2");
    cli.start(&fresh, &config_arg);
    assert!(answer(cli.host_port("production")).starts_with("revision=abc123"));
    let events = cli.run(&["events", "--project", "app", "--limit", "500"]);
    let kinds = events
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["kind"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    for expected in [
        "deployment.started",
        "deployment.admitted",
        "deployment.completed",
        "deployment.promoted",
    ] {
        assert!(
            kinds.contains(&expected.to_string()),
            "{expected}: {kinds:?}"
        );
    }

    // FeltDB unavailable, and the operator requires it at start: Compute
    // refuses to start rather than diverge.
    cli.stop();
    let feltdb_port = feltdb
        .url
        .rsplit(':')
        .next()
        .unwrap()
        .trim_end_matches('/')
        .parse::<u16>()
        .unwrap();
    drop(feltdb);
    let listen = format!("127.0.0.1:{}", free_port());
    let refused = compute()
        .args([
            "start",
            "--listen",
            &listen,
            "--require-state-at-start",
            "--config",
            config.to_str().unwrap(),
        ])
        .arg("--state-dir")
        .arg(root.path().join("node-3"))
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .unwrap();
    assert!(!refused.status.success(), "no control state, no daemon");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("refusing to start"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(
        !root.path().join("node-3/control-state.json").exists(),
        "no local fallback"
    );

    // By default it starts in degraded_control_plane: it answers, refuses
    // changes with state_unavailable, and never writes local state.
    let degraded = root.path().join("node-4");
    cli.start(&degraded, &config_arg);
    let info = cli.run(&["node", "info"]);
    assert_eq!(
        info["control_plane"]["mode"], "degraded_control_plane",
        "{info}"
    );
    let change = compute()
        .args(["environment", "create", "while-degraded"])
        .args(["--daemon", &cli.endpoint, "--json"])
        .output()
        .unwrap();
    assert!(!change.status.success());
    assert!(
        String::from_utf8_lossy(&change.stderr).contains("unavailable"),
        "{}",
        String::from_utf8_lossy(&change.stderr)
    );
    assert!(
        !degraded.join("control-state.json").exists(),
        "no local fallback"
    );
    // FeltDB comes back: the controller recovers, reconciles, and
    // restores what desired state says should run.
    let _feltdb = feltdb_start_on(&data, feltdb_port);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let info = cli.run(&["node", "info"]);
        if info["control_plane"]["mode"] == "normal" {
            break;
        }
        assert!(Instant::now() < deadline, "never recovered: {info}");
        std::thread::sleep(Duration::from_millis(200));
    }
    cli.run(&["environment", "create", "after-recovery"]);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let port = cli.run(&["project", "status", "app", "--environment", "production"]);
        if port["workloads"][0]["actual_state"] == "running" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "production was not restored: {port}"
        );
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(answer(cli.host_port("production")).starts_with("revision=abc123"));
    let kinds = cli.run(&["events", "--limit", "500"]);
    let kinds = kinds
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["kind"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(kinds.contains(&"feltdb.recovered".to_string()), "{kinds:?}");
    cli.stop();
}
