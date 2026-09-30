//! Readiness and restart policy for the processes a computer runs (G-DEP-2).
//!
//! A durable process in a computer has a desired state, an observed state,
//! an HTTP readiness contract when it serves, and a bounded restart policy.
//! Every decision is recorded in control state before it acts, so a
//! controller that restarts carries on from the record: it neither loses a
//! restart nor makes one twice. These tests run against a real target
//! (`compute serve` hosting workspace sessions) and a deterministic HTTP
//! service; a controller restart is a new controller reading the same
//! control state back from disk.

mod common;
#[path = "common/target.rs"]
mod target;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use compute_core::{
    ComputerLifecycle, ComputerRequirements, EnvironmentContents, HttpReadiness, NetworkPolicy,
    ProcessDesired, ProcessKind, ProcessRestartPolicy, ProcessSpec, ProcessState,
};
use compute_environment::*;
use compute_state::StateStore;
use compute_state_file::FileState;
use compute_state_memory::MemoryState;
use target::*;

/// The service: it records every start in `starts.log`, then, by mode,
/// serves `/health` as ready (`ready`), ready while `ready.flag` exists
/// (`flag`), never ready (`never`), or exits with a status after a moment
/// (`exit<N>`) or at once (`fail`).
const SERVICE: &str = r#"
import http.server, os, sys, time
mode = sys.argv[1]
with open("starts.log", "a") as log:
    log.write(f"{os.getpid()}\n")
if mode == "fail":
    sys.exit(1)
if mode.startswith("exit"):
    time.sleep(1.5)
    sys.exit(int(mode[4:]))
class Health(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        ready = mode == "ready" or (mode == "flag" and os.path.exists("ready.flag"))
        self.send_response(200 if ready else 503)
        self.end_headers()
    def log_message(self, *args):
        pass
http.server.HTTPServer(("127.0.0.1", int(os.environ["PORT"])), Health).serve_forever()
"#;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn service(
    mode: &str,
    readiness: Option<HttpReadiness>,
    policy: ProcessRestartPolicy,
    max_restarts: u32,
) -> ProcessSpec {
    ProcessSpec {
        name: "web".into(),
        kind: ProcessKind::Service,
        runtime: None,
        command: vec!["python3".into(), "-c".into(), SERVICE.into(), mode.into()],
        repository: None,
        env: BTreeMap::new(),
        desired: ProcessDesired::Running,
        port: Some(free_port()),
        restart: 0,
        readiness,
        restart_policy: policy,
        max_restarts,
    }
}

fn health(deadline_seconds: u64) -> Option<HttpReadiness> {
    Some(HttpReadiness {
        request_timeout_seconds: 1,
        deadline_seconds,
        ..HttpReadiness::path("/health")
    })
}

async fn computer(daemon: &Arc<Daemon>, name: &str, process: ProcessSpec) {
    daemon
        .create_computer_environment(
            ComputerEnvironmentDefinition {
                name: name.into(),
                desired_state: DesiredState::Running,
                env: BTreeMap::new(),
                policy: None,
                computer: ComputerRequest {
                    lifecycle: ComputerLifecycle::Persistent,
                    requirements: requirements(1),
                    target: None,
                    ttl_seconds: None,
                },
                contents: EnvironmentContents {
                    processes: vec![process],
                    ..Default::default()
                },
                recipe: None,
            },
            "alice",
        )
        .await
        .unwrap();
}

fn requirements(cpu: u32) -> ComputerRequirements {
    ComputerRequirements {
        cpu_count: Some(cpu),
        memory_bytes: Some(64 << 20),
        network: NetworkPolicy::Network,
        ..Default::default()
    }
}

async fn web_where(
    daemon: &Arc<Daemon>,
    name: &str,
    what: &str,
    seconds: u64,
    wanted: impl Fn(&ProcessReality) -> bool,
) -> ComputerView {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let mut last = None;
    loop {
        if let Ok(view) = daemon.computer(name).await {
            if view.reality.processes.get("web").is_some_and(&wanted) {
                return view;
            }
            last = Some(view.reality);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}; last saw {last:#?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Hold for `seconds`, asserting `invariant` of the process all along.
async fn holds(
    daemon: &Arc<Daemon>,
    name: &str,
    seconds: u64,
    invariant: impl Fn(&ProcessReality) -> bool,
) -> ProcessReality {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        let view = daemon.computer(name).await.unwrap();
        let web = view.reality.processes["web"].clone();
        assert!(invariant(&web), "{web:#?}");
        if tokio::time::Instant::now() >= deadline {
            return web;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Run a command in the computer's workspace, on the host, and return its
/// output. These tests inspect and disturb environments whose declared
/// process is unready, restarting, or failed: those are not ready, so no
/// workload is admitted to them, and the test reaches the machine's
/// directory directly instead of asking Compute to run work in it.
async fn run(daemon: &Arc<Daemon>, name: &str, command: &[&str]) -> String {
    let resource = daemon
        .computer(name)
        .await
        .unwrap()
        .machine
        .unwrap()
        .resource
        .unwrap();
    let output = std::process::Command::new(command[0])
        .args(&command[1..])
        .current_dir(workspace_of(&resource))
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// How many times the service has started in this computer.
async fn starts(daemon: &Arc<Daemon>, name: &str) -> usize {
    run(
        daemon,
        name,
        &["sh", "-c", "cat starts.log 2>/dev/null || true"],
    )
    .await
    .lines()
    .count()
}

async fn events(daemon: &Arc<Daemon>, name: &str) -> Vec<(String, serde_json::Value)> {
    daemon
        .events(EventFilter {
            environment: Some(name.into()),
            limit: Some(1000),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_iter()
        .map(|event| (event.kind, event.data))
        .collect()
}

fn memory() -> Arc<dyn StateStore> {
    Arc::new(MemoryState::new())
}

/// Control state on disk: each controller opens it anew, so what one
/// controller wrote is what the next reads, and nothing else.
fn on_disk(path: &Path) -> Arc<dyn StateStore> {
    Arc::new(FileState::open(path).unwrap())
}

// ---- 1, 2: readiness -------------------------------------------------------

/// A started process is `starting` until a request made inside the computer
/// is answered as ready; then `ready`; and `unready` once it stops being.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_process_is_ready_only_when_its_readiness_request_answers() {
    let target = Target::start();
    let (daemon, _node) = start_daemon(memory(), Some(pool(&target))).await;
    computer(
        &daemon,
        "readiness",
        service("flag", health(60), ProcessRestartPolicy::OnFailure, 5),
    )
    .await;
    // Its child process started: that is not ready.
    web_where(&daemon, "readiness", "the process to start", 60, |web| {
        web.process == "starting" && web.readiness.as_deref() == Some("starting")
    })
    .await;
    holds(&daemon, "readiness", 2, |web| web.process == "starting").await;
    // Ready once it answers 2xx.
    run(&daemon, "readiness", &["touch", "ready.flag"]).await;
    let view = web_where(&daemon, "readiness", "ready", 30, |web| {
        web.process == "ready"
    })
    .await;
    let web = &view.reality.processes["web"];
    assert_eq!(web.desired, "running");
    assert_eq!(web.readiness_detail.as_deref(), Some("HTTP 200"));
    assert_eq!(web.restarts, 0);
    // No longer answering as ready: unready, still running, not restarted.
    run(&daemon, "readiness", &["rm", "ready.flag"]).await;
    let view = web_where(&daemon, "readiness", "unready", 30, |web| {
        web.process == "unready"
    })
    .await;
    let web = &view.reality.processes["web"];
    assert_eq!(web.readiness_detail.as_deref(), Some("HTTP 503"));
    assert_eq!(view.observed.processes["web"].state, ProcessState::Running);
    assert_eq!(web.restarts, 0);
    let kinds = events(&daemon, "readiness").await;
    let kinds = kinds
        .iter()
        .map(|(kind, _)| kind.as_str())
        .collect::<Vec<_>>();
    assert!(kinds.contains(&"process.ready"), "{kinds:?}");
    assert!(kinds.contains(&"process.unready"), "{kinds:?}");
    daemon.shutdown().await;
}

// ---- 3, 11: a readiness deadline, its evidence, and on_failure ------------

/// A process that never becomes ready is a failure at its deadline, with
/// durable evidence (the probe job, its event); `on_failure` restarts it,
/// bounded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missed_readiness_deadline_is_evidenced_and_restarts_on_failure() {
    let target = Target::start();
    let (daemon, _node) = start_daemon(memory(), Some(pool(&target))).await;
    computer(
        &daemon,
        "deadline",
        service("never", health(2), ProcessRestartPolicy::OnFailure, 1),
    )
    .await;
    // It is never reported ready; at its deadline it fails, and is
    // restarted once.
    let view = web_where(&daemon, "deadline", "a restart", 60, |web| {
        web.restarts == 1
    })
    .await;
    let web = &view.reality.processes["web"];
    assert_ne!(web.process, "ready");
    let failure = web.last_failure.clone().expect("the failure is recorded");
    assert_eq!(failure.reason, "readiness_timeout");
    assert!(
        failure.message.contains("not ready within 2s"),
        "{}",
        failure.message
    );
    assert!(failure.message.contains("HTTP 503"), "{}", failure.message);
    assert!(
        failure.decision.starts_with("restart 1 of 1"),
        "{}",
        failure.decision
    );
    // The evidence is the probe job, which the target has.
    let job = daemon
        .computer_job("deadline", "alice", &failure.evidence.job_id)
        .await
        .expect("the target has the probe job");
    assert!(job.result.is_some());
    // Its second deadline: bounded, no more restarts, and it says so.
    let view = web_where(&daemon, "deadline", "the bound", 60, |web| {
        web.last_failure
            .as_ref()
            .is_some_and(|failure| failure.decision.contains("1 restarts in a row"))
    })
    .await;
    let web = &view.reality.processes["web"];
    assert_eq!(web.process, "unready", "never reported healthy");
    assert_eq!(web.next_restart_at, None);
    let events = events(&daemon, "deadline").await;
    let failed = events
        .iter()
        .filter(|(kind, _)| kind == "process.failed")
        .collect::<Vec<_>>();
    assert_eq!(failed.len(), 2, "{events:#?}");
    assert_eq!(failed[0].1["reason"], "readiness_timeout");
    assert!(
        failed[0].1["job_id"]
            .as_str()
            .is_some_and(|job| !job.is_empty())
    );
    assert_eq!(
        events
            .iter()
            .filter(|(kind, _)| kind == "process.restarting")
            .count(),
        1
    );
    holds(&daemon, "deadline", 3, |web| web.restarts == 1).await;
    assert_eq!(starts(&daemon, "deadline").await, 2);
    daemon.shutdown().await;
}

// ---- 4, 5: never and always -----------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn never_leaves_an_exited_process_and_always_restarts_it() {
    let target = Target::start();
    let (daemon, _node) = start_daemon(memory(), Some(pool(&target))).await;
    computer(
        &daemon,
        "never",
        service("exit3", None, ProcessRestartPolicy::Never, 5),
    )
    .await;
    computer(
        &daemon,
        "always",
        service("exit0", None, ProcessRestartPolicy::Always, 5),
    )
    .await;
    // never: it exits, and stays exited, with why.
    let view = web_where(&daemon, "never", "the exit", 60, |web| {
        web.process == "exited"
    })
    .await;
    let failure = view.reality.processes["web"].last_failure.clone().unwrap();
    assert_eq!(failure.exit_code, Some(3));
    assert!(
        failure.decision.contains("restart policy never"),
        "{}",
        failure.decision
    );
    holds(&daemon, "never", 4, |web| {
        web.process == "exited" && web.restarts == 0
    })
    .await;
    assert_eq!(starts(&daemon, "never").await, 1);
    // always: even a clean exit is restarted.
    let view = web_where(&daemon, "always", "a restart", 60, |web| web.restarts >= 1).await;
    let failure = view.reality.processes["web"].last_failure.clone().unwrap();
    assert_eq!(failure.exit_code, Some(0));
    // A restart is counted when it is recorded, before its start job runs:
    // wait for the job to have started it again.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while starts(&daemon, "always").await < 2 {
        assert!(tokio::time::Instant::now() < deadline, "never restarted");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    daemon.shutdown().await;
}

// ---- 6: stop wins over always ----------------------------------------------

/// A process stopped by its desired state stays stopped under `always`,
/// and across a controller restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_explicit_stop_is_never_undone_by_a_restart_policy() {
    let target = Target::start();
    let state = tempfile::tempdir().unwrap();
    let path = state.path().join("control-state.json");
    let (daemon, _node) = start_daemon(on_disk(&path), Some(pool(&target))).await;
    computer(
        &daemon,
        "stop",
        service("ready", health(30), ProcessRestartPolicy::Always, 5),
    )
    .await;
    web_where(&daemon, "stop", "ready", 60, |web| web.process == "ready").await;
    daemon
        .set_process("stop", "alice", "web", ProcessDesired::Stopped)
        .await
        .unwrap();
    web_where(&daemon, "stop", "stopped", 60, |web| {
        web.process == "stopped"
    })
    .await;
    holds(&daemon, "stop", 3, |web| {
        web.desired == "stopped" && web.process == "stopped" && web.restarts == 0
    })
    .await;
    daemon.shutdown().await;
    // A new controller reads the same record: still stopped.
    let (daemon, _node) = start_daemon(on_disk(&path), Some(pool(&target))).await;
    web_where(&daemon, "stop", "the record", 60, |web| {
        web.process == "stopped"
    })
    .await;
    holds(&daemon, "stop", 3, |web| {
        web.process == "stopped" && web.restarts == 0
    })
    .await;
    assert_eq!(starts(&daemon, "stop").await, 1);
    daemon.shutdown().await;
}

// ---- 7, 8, 9: controller restarts --------------------------------------------

/// Restart authority is in control state: a new controller neither
/// duplicates a healthy process nor forgets its restarts, and recovers one
/// that exited while no controller ran.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restarts_are_durable_across_controller_restarts_and_never_duplicated() {
    let target = Target::start();
    let state = tempfile::tempdir().unwrap();
    let path = state.path().join("control-state.json");
    let (first, _node) = start_daemon(on_disk(&path), Some(pool(&target))).await;
    computer(
        &first,
        "durable",
        service("ready", health(30), ProcessRestartPolicy::Always, 5),
    )
    .await;
    let view = web_where(&first, "durable", "ready", 60, |web| web.process == "ready").await;
    let pid = view.reality.processes["web"].pid.unwrap();
    // It is killed behind Compute's back: restarted, counted.
    run(&first, "durable", &["kill", &pid.to_string()]).await;
    let view = web_where(&first, "durable", "the restart", 60, |web| {
        web.restarts == 1 && web.process == "ready"
    })
    .await;
    let failure = view.reality.processes["web"].last_failure.clone().unwrap();
    assert_eq!(failure.exit_code, Some(143));
    let pid = view.reality.processes["web"].pid.unwrap();
    first.shutdown().await;
    drop(first);

    // 9: a new controller finds it running: the same process, not a second.
    let (second, _node) = start_daemon(on_disk(&path), Some(pool(&target))).await;
    web_where(&second, "durable", "the record", 60, |web| {
        web.process == "ready"
    })
    .await;
    let web = holds(&second, "durable", 3, |web| {
        web.pid == Some(pid) && web.restarts == 1 && web.process == "ready"
    })
    .await;
    assert_eq!(web.restarts, 1, "7: the count survived");
    assert_eq!(starts(&second, "durable").await, 2, "no duplicate start");
    second.shutdown().await;
    drop(second);

    // 8: it exits while no controller runs; the next one recovers it.
    let killed = std::process::Command::new("kill")
        .arg(pid.to_string())
        .status()
        .unwrap();
    assert!(killed.success());
    let (third, _node) = start_daemon(on_disk(&path), Some(pool(&target))).await;
    let view = web_where(&third, "durable", "the recovery", 60, |web| {
        web.restarts == 2 && web.process == "ready"
    })
    .await;
    assert_ne!(view.reality.processes["web"].pid, Some(pid));
    assert_eq!(starts(&third, "durable").await, 3);
    let restarting = events(&third, "durable")
        .await
        .into_iter()
        .filter(|(kind, _)| kind == "process.restarting")
        .map(|(_, data)| data["restarts"].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(restarting, [1, 2], "each restart recorded once");
    third.shutdown().await;
}

// ---- 10: fencing -------------------------------------------------------------

/// A restart decided for a machine that has been replaced never runs on
/// it: every restart is recorded, fenced, against the session of the
/// record it was decided on, and a replacement starts its count afresh.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replaced_machine_is_never_restarted_into() {
    let target = Target::start();
    let (daemon, _node) = start_daemon(memory(), Some(pool(&target))).await;
    computer(
        &daemon,
        "fenced",
        service("exit1", None, ProcessRestartPolicy::Always, 1),
    )
    .await;
    // On the first machine: one restart, then the bound.
    let settled = |web: &ProcessReality| {
        web.restarts == 1 && web.process == "exited" && web.next_restart_at.is_none()
    };
    let old = web_where(&daemon, "fenced", "the first machine's bound", 60, settled).await;
    let old_session = old.session_id.clone().unwrap();
    daemon
        .replace_computer("fenced", "alice", requirements(2))
        .await
        .unwrap();
    // On the replacement: its own count, from zero, to the same bound.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    let new = loop {
        let view = daemon.computer("fenced").await.unwrap();
        if view
            .session_id
            .as_ref()
            .is_some_and(|session| *session != old_session)
            && view.reality.processes.get("web").is_some_and(settled)
        {
            break view;
        }
        assert!(tokio::time::Instant::now() < deadline, "{view:#?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let new_session = new.session_id.clone().unwrap();
    holds(&daemon, "fenced", 3, |web| web.restarts == 1).await;
    let events = events(&daemon, "fenced").await;
    let replacing = events
        .iter()
        .position(|(kind, _)| kind == "computer.replacing")
        .expect("the replacement is recorded");
    let restarts_in = |events: &[(String, serde_json::Value)], session: &str| {
        events
            .iter()
            .filter(|(kind, data)| kind == "process.restarting" && data["session_id"] == session)
            .count()
    };
    assert_eq!(
        restarts_in(&events[..replacing], &old_session),
        1,
        "{events:#?}"
    );
    assert_eq!(
        restarts_in(&events[replacing..], &old_session),
        0,
        "{events:#?}"
    );
    assert_eq!(
        restarts_in(&events[replacing..], &new_session),
        1,
        "{events:#?}"
    );
    daemon.shutdown().await;
}

// ---- 12: bounded -------------------------------------------------------------

/// A process that can never start is restarted at most `max_restarts`
/// times in a row, with a growing backoff, then left failed with why.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crash_loop_is_bounded() {
    let target = Target::start();
    let (daemon, _node) = start_daemon(memory(), Some(pool(&target))).await;
    computer(
        &daemon,
        "loop",
        service("fail", None, ProcessRestartPolicy::Always, 2),
    )
    .await;
    let view = web_where(&daemon, "loop", "the bound", 60, |web| {
        web.restarts == 2 && web.next_restart_at.is_none() && web.process == "failed"
    })
    .await;
    let failure = view.reality.processes["web"].last_failure.clone().unwrap();
    assert_eq!(failure.reason, "start_failed");
    assert_eq!(failure.exit_code, Some(1));
    assert!(
        failure.decision.contains("2 restarts in a row"),
        "{}",
        failure.decision
    );
    holds(&daemon, "loop", 5, |web| {
        web.restarts == 2 && web.process == "failed"
    })
    .await;
    assert_eq!(starts(&daemon, "loop").await, 3);
    // Someone asks: it is tried again, its count kept.
    daemon.reconcile_computer("loop", "alice").await.unwrap();
    web_where(&daemon, "loop", "the retry", 60, |web| web.restarts == 4).await;
    daemon.shutdown().await;
}
