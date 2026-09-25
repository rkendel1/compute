//! Release certification: zero-downtime releases against real services.
//!
//! A client keeps requesting a service's stable endpoint while releases
//! run. Across a successful release, a failed one, one that never becomes
//! ready, one that is rolled back, and a daemon restart in the middle of
//! one, every request is answered, and each answer comes from exactly one
//! revision.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use compute_core::{
    IsolationRequirement, NetworkPolicy, ResourceLimits, RuntimeKind, WORKLOAD_SPEC_VERSION,
    WorkloadBundle, WorkloadSpec,
};
use compute_environment::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn bundle(source: &str) -> Vec<u8> {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("main.py"), source).unwrap();
    let spec = WorkloadSpec {
        version: WORKLOAD_SPEC_VERSION.into(),
        runtime: RuntimeKind::Python,
        runtime_version: None,
        architecture: None,
        entrypoint: "main.py".into(),
        args: vec![],
        env: BTreeMap::new(),
        inputs: vec![],
        outputs: vec![],
        resources: ResourceLimits::default(),
        network: NetworkPolicy::Network,
        isolation: IsolationRequirement::default(),
        dependencies: None,
    };
    WorkloadBundle::create_from(spec, root.path())
        .unwrap()
        .to_bytes()
        .unwrap()
}

/// A threaded HTTP service that answers `version`. `prelude` runs before
/// it listens; `status` is the Python expression for each response code.
fn server(version: &str, prelude: &str, status: &str) -> String {
    format!(
        r#"import http.server, os, sys, time
{prelude}
served = [0]
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        served[0] += 1
        body = b"{version}"
        self.send_response({status})
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *args):
        pass
class S(http.server.ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True
S(("127.0.0.1", int(os.environ["PORT"])), H).serve_forever()
"#
    )
}

fn web(version: &str, prelude: &str) -> WorkloadDefinition {
    web_with(version, prelude, "200", http_readiness(10_000))
}

fn web_with(
    version: &str,
    prelude: &str,
    status: &str,
    readiness: Readiness,
) -> WorkloadDefinition {
    WorkloadDefinition {
        name: "web".into(),
        kind: WorkloadKind::Service,
        bundle: bundle(&server(version, prelude, status)),
        ports: vec![PortSpec {
            name: "http".into(),
            port: 8080,
        }],
        restart: RestartPolicy::Never,
        desired_state: DesiredState::Running,
        readiness: Some(readiness),
    }
}

fn http_readiness(timeout_ms: u64) -> Readiness {
    Readiness {
        check: ReadinessCheck::Http,
        port: None,
        path: Some("/health".into()),
        task: None,
        timeout_ms,
        interval_ms: 100,
    }
}

fn revision(label: &str, web: WorkloadDefinition) -> RevisionDefinition {
    RevisionDefinition {
        revision: label.into(),
        source: None,
        workloads: vec![web],
    }
}

fn port_windows(config: &mut DaemonConfig) {
    static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    let window = NEXT.fetch_add(1, Ordering::SeqCst) * 100;
    config.port_range = (21000 + window, 21099 + window);
    config.instance_port_range = (41000 + window, 41099 + window);
}

fn config(node: &std::path::Path, store: Arc<dyn compute_state::StateStore>) -> DaemonConfig {
    let artifacts = Arc::new(compute_state::StateArtifacts::new(
        compute_state::ControlState::new(store.clone()),
    ));
    let mut config = DaemonConfig::new(node, store, artifacts);
    config.restart_delay = Duration::from_millis(100);
    config.reconcile_interval = Duration::from_millis(200);
    config.switch_timeout = Duration::from_secs(2);
    config.drain_timeout = Duration::from_secs(30);
    port_windows(&mut config);
    config
}

async fn python() -> bool {
    let available = compute_runtime::Compute::new()
        .runtime(RuntimeKind::Python, None)
        .await
        .is_ok_and(|runtime| runtime.available);
    if !available {
        eprintln!("skipping: python is unavailable on this host");
    }
    available
}

/// One HTTP/1.0 request to a local port: the body, or why there was none.
async fn get(port: u16) -> Result<String, String> {
    let exchange = async {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .map_err(|error| format!("connect: {error}"))?;
        stream
            .write_all(b"GET / HTTP/1.0\r\n\r\n")
            .await
            .map_err(|error| format!("write: {error}"))?;
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .await
            .map_err(|error| format!("read: {error}"))?;
        let (head, body) = response
            .split_once("\r\n\r\n")
            .ok_or_else(|| format!("no response: {response:?}"))?;
        if !head.starts_with("HTTP/1.0 200") && !head.starts_with("HTTP/1.1 200") {
            return Err(format!(
                "status: {}",
                head.lines().next().unwrap_or_default()
            ));
        }
        Ok(body.to_string())
    };
    tokio::time::timeout(Duration::from_secs(5), exchange)
        .await
        .map_err(|_| "timed out".to_string())?
}

/// Clients requesting an endpoint until stopped.
struct Load {
    stop: Arc<AtomicBool>,
    tasks: Vec<tokio::task::JoinHandle<LoadReport>>,
}

#[derive(Default)]
struct LoadReport {
    requests: u64,
    failures: Vec<String>,
    answers: BTreeMap<String, u64>,
}

impl Load {
    fn start(port: u16, clients: usize) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let tasks = (0..clients)
            .map(|_| {
                let stop = stop.clone();
                tokio::spawn(async move {
                    let mut report = LoadReport::default();
                    while !stop.load(Ordering::SeqCst) {
                        report.requests += 1;
                        match get(port).await {
                            Ok(body) => *report.answers.entry(body).or_default() += 1,
                            Err(error) => report.failures.push(error),
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    report
                })
            })
            .collect();
        Self { stop, tasks }
    }

    async fn stop(self) -> LoadReport {
        self.stop.store(true, Ordering::SeqCst);
        let mut report = LoadReport::default();
        for task in self.tasks {
            let LoadReport {
                requests,
                failures,
                answers,
            } = task.await.unwrap();
            report.requests += requests;
            report.failures.extend(failures);
            for (answer, count) in answers {
                *report.answers.entry(answer).or_default() += count;
            }
        }
        report
    }
}

async fn settle(daemon: &Daemon, deployment_id: &str) -> DeploymentView {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let view = daemon.deployment(deployment_id).await.unwrap();
        if view.record.status.is_terminal() {
            return view;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{deployment_id} never settled: {:?}",
            view.record
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn release(daemon: &Arc<Daemon>, label: &str, web: WorkloadDefinition) -> DeploymentView {
    daemon
        .register_revision("site", revision(label, web))
        .await
        .unwrap();
    daemon
        .deploy(DeployRequest {
            project: "site".into(),
            environment: "production".into(),
            revision: Some(label.into()),
            config: Some(BTreeMap::from([(
                "API_TOKEN".to_string(),
                "s3cr3t-token-value".to_string(),
            )])),
            desired_state: None,
        })
        .await
        .unwrap()
}

/// A daemon serving `site` v1 in production, and its endpoint port.
async fn serving_v1(daemon: &Arc<Daemon>) -> u16 {
    daemon
        .create_environment(EnvironmentDefinition {
            name: "production".into(),
            desired_state: DesiredState::Running,
            env: BTreeMap::new(),
            policy: None,
            provider: None,
        })
        .await
        .unwrap();
    let first = release(daemon, "v1", web("v1", "")).await;
    let first = settle(daemon, &first.deployment_id).await;
    assert_eq!(
        first.record.status,
        DeploymentStatus::Complete,
        "{:?}",
        first.record
    );
    assert_eq!(first.record.version, 1);
    let endpoint = daemon
        .workload("production", "site", "web")
        .await
        .unwrap()
        .ports[0]
        .host;
    assert_eq!(get(endpoint).await.unwrap(), "v1");
    endpoint
}

fn kinds(events: &[EventRecord]) -> Vec<String> {
    events.iter().map(|event| event.kind.clone()).collect()
}

async fn events_of(daemon: &Daemon, deployment_id: &str) -> Vec<EventRecord> {
    daemon
        .events(EventFilter {
            deployment_id: Some(deployment_id.into()),
            limit: Some(1000),
            ..EventFilter::default()
        })
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_release_moves_traffic_without_dropping_a_request() {
    if !python().await {
        return;
    }
    let node = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(config(
        node.path(),
        Arc::new(compute_state_memory::MemoryState::new()),
    ))
    .await
    .unwrap();
    let endpoint = serving_v1(&daemon).await;
    let load = Load::start(endpoint, 4);
    tokio::time::sleep(Duration::from_millis(300)).await;

    // v2 takes a moment to start: v1 serves meanwhile.
    let started = release(&daemon, "v2", web("v2", "time.sleep(1)")).await;
    let settled = settle(&daemon, &started.deployment_id).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let report = load.stop().await;

    let record = &settled.record;
    assert_eq!(record.status, DeploymentStatus::Complete, "{record:?}");
    assert_eq!(record.version, 2);
    assert!(
        report.failures.is_empty(),
        "{} of {} requests failed during the release: {:?}",
        report.failures.len(),
        report.requests,
        &report.failures[..report.failures.len().min(5)]
    );
    assert!(
        report.answers.get("v1").copied().unwrap_or(0) > 0,
        "{:?}",
        report.answers
    );
    assert!(
        report.answers.get("v2").copied().unwrap_or(0) > 0,
        "{:?}",
        report.answers
    );
    assert_eq!(report.answers.len(), 2, "only v1 and v2 answered");
    assert_eq!(get(endpoint).await.unwrap(), "v2");
    assert_eq!(
        daemon
            .workload("production", "site", "web")
            .await
            .unwrap()
            .ports[0]
            .host,
        endpoint,
        "the endpoint is stable across releases"
    );

    // The durable evidence of every step.
    assert_eq!(record.old_revision.as_deref(), Some("v1"));
    assert!(record.readiness_result.is_some());
    assert!(record.network_result.is_some());
    let switch = record.traffic_switch_result.as_ref().unwrap();
    assert_eq!(switch["endpoints"][0]["host_port"], endpoint);
    assert!(switch["verified_at"].is_string());
    let kinds = kinds(&events_of(&daemon, &started.deployment_id).await);
    let order = [
        "deployment.started",
        "deployment.admitted",
        "deployment.placed",
        "instance.ready",
        "deployment.ready",
        "deployment.switched",
        "network.route.switched",
        "deployment.draining",
        "deployment.completed",
    ];
    let positions = order
        .iter()
        .map(|kind| {
            kinds
                .iter()
                .position(|candidate| candidate == kind)
                .unwrap_or_else(|| panic!("{kind} missing from {kinds:?}"))
        })
        .collect::<Vec<_>>();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "{kinds:?}"
    );

    // The replaced instance drained and stopped; only v2 runs.
    let previous = daemon
        .deployment(record.previous.as_deref().unwrap())
        .await
        .unwrap();
    assert!(
        previous
            .instances
            .iter()
            .all(|instance| instance.record.state == InstanceState::Stopped),
        "{:?}",
        previous.instances
    );
    let current = daemon.deployment(&started.deployment_id).await.unwrap();
    assert_eq!(current.instances.len(), 1);
    assert_eq!(current.instances[0].record.state, InstanceState::Serving);

    // The receipt records what was released, and nothing secret.
    let receipt = daemon
        .deployment_receipt_document(&started.deployment_id)
        .await
        .unwrap();
    assert_eq!(receipt["format"], "compute.deployment-receipt@1");
    assert_eq!(receipt["deployment_version"], 2);
    assert_eq!(receipt["application"]["name"], "site");
    assert_eq!(receipt["revision"], "v2");
    assert_eq!(receipt["old_revision"], "v1");
    assert_eq!(receipt["status"], "complete");
    assert!(receipt["config_digest"].is_string());
    assert!(
        !receipt.to_string().contains("s3cr3t-token-value"),
        "configuration values never reach a receipt"
    );
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_failed_release_leaves_the_current_revision_serving() {
    if !python().await {
        return;
    }
    let node = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(config(
        node.path(),
        Arc::new(compute_state_memory::MemoryState::new()),
    ))
    .await
    .unwrap();
    let endpoint = serving_v1(&daemon).await;
    let load = Load::start(endpoint, 2);

    // v2 crashes on start.
    let started = release(&daemon, "v2", web("v2", "sys.exit(3)")).await;
    let settled = settle(&daemon, &started.deployment_id).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let report = load.stop().await;
    assert_eq!(settled.record.status, DeploymentStatus::Failed);
    let failure = settled.record.failure.clone().unwrap();
    assert!(failure.contains("failed before it was ready"), "{failure}");
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(
        report.answers.keys().collect::<Vec<_>>(),
        vec!["v1"],
        "only v1 ever answered"
    );
    assert_eq!(
        daemon.project("production", "site").await.unwrap().revision,
        "v1"
    );
    assert!(
        settled
            .instances
            .iter()
            .all(|instance| instance.record.state == InstanceState::Failed)
    );
    assert!(
        kinds(&events_of(&daemon, &started.deployment_id).await)
            .contains(&"deployment.failed".to_string())
    );
    let receipt = daemon
        .deployment_receipt_document(&started.deployment_id)
        .await
        .unwrap();
    assert_eq!(receipt["status"], "failed");
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_release_that_never_becomes_ready_times_out() {
    if !python().await {
        return;
    }
    let node = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(config(
        node.path(),
        Arc::new(compute_state_memory::MemoryState::new()),
    ))
    .await
    .unwrap();
    let endpoint = serving_v1(&daemon).await;
    let load = Load::start(endpoint, 2);

    // v2 runs but never listens.
    let never = web_with("v2", "time.sleep(3600)", "200", http_readiness(1500));
    let started = release(&daemon, "v2", never).await;
    let settled = settle(&daemon, &started.deployment_id).await;
    let report = load.stop().await;
    assert_eq!(settled.record.status, DeploymentStatus::Failed);
    let failure = settled.record.failure.unwrap();
    assert!(failure.contains("readiness timed out"), "{failure}");
    assert!(failure.contains("GET /health"), "{failure}");
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(report.answers.keys().collect::<Vec<_>>(), vec!["v1"]);
    // The candidate is stopped.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let view = daemon.deployment(&started.deployment_id).await.unwrap();
        if view
            .instances
            .iter()
            .all(|instance| instance.actual_state != Some(ActualState::Running))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{:?}",
            view.instances
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn draining_waits_for_open_connections() {
    if !python().await {
        return;
    }
    let node = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(config(
        node.path(),
        Arc::new(compute_state_memory::MemoryState::new()),
    ))
    .await
    .unwrap();
    let endpoint = serving_v1(&daemon).await;

    // A client connects to v1 and has not sent its request yet.
    let mut held = tokio::net::TcpStream::connect(("127.0.0.1", endpoint))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let started = release(&daemon, "v2", web("v2", "")).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let status = daemon
            .deployment(&started.deployment_id)
            .await
            .unwrap()
            .record
            .status;
        if status == DeploymentStatus::Draining {
            break;
        }
        assert!(!status.is_terminal(), "{status:?}");
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // New connections reach v2 while v1 finishes what it has.
    assert_eq!(get(endpoint).await.unwrap(), "v2");
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        daemon
            .deployment(&started.deployment_id)
            .await
            .unwrap()
            .record
            .status,
        DeploymentStatus::Draining,
        "draining waits for the open connection"
    );
    held.write_all(b"GET / HTTP/1.0\r\n\r\n").await.unwrap();
    let mut response = String::new();
    held.read_to_string(&mut response).await.unwrap();
    assert!(
        response.ends_with("v1"),
        "the old instance finished its request: {response}"
    );
    drop(held);
    let settled = settle(&daemon, &started.deployment_id).await;
    assert_eq!(settled.record.status, DeploymentStatus::Complete);
    let stopped = events_of(&daemon, record_previous(&settled)).await;
    let drained = stopped
        .iter()
        .find(|event| event.kind == "instance.stopped")
        .unwrap();
    assert_eq!(drained.data["open_connections"], 0);
    daemon.shutdown().await;
}

fn record_previous(view: &DeploymentView) -> &str {
    view.record.previous.as_deref().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_release_failing_verification_after_the_switch_rolls_back() {
    if !python().await {
        return;
    }
    let node = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(config(
        node.path(),
        Arc::new(compute_state_memory::MemoryState::new()),
    ))
    .await
    .unwrap();
    let endpoint = serving_v1(&daemon).await;

    // v2 answers its readiness check, then only errors.
    let flaky = web_with(
        "v2",
        "",
        "200 if served[0] <= 1 else 500",
        http_readiness(10_000),
    );
    let started = release(&daemon, "v2", flaky).await;
    let settled = settle(&daemon, &started.deployment_id).await;
    assert_eq!(
        settled.record.status,
        DeploymentStatus::RolledBack,
        "{:?}",
        settled.record
    );
    assert!(settled.record.rollback_reason.is_some());
    assert_eq!(get(endpoint).await.unwrap(), "v1", "traffic is back on v1");
    let project = daemon.project("production", "site").await.unwrap();
    assert_eq!(project.revision, "v1");
    assert!(
        kinds(&events_of(&daemon, &started.deployment_id).await)
            .contains(&"deployment.rolled_back".to_string())
    );
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn an_operator_rolls_a_release_back_to_the_revision_it_replaced() {
    if !python().await {
        return;
    }
    let node = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(config(
        node.path(),
        Arc::new(compute_state_memory::MemoryState::new()),
    ))
    .await
    .unwrap();
    let endpoint = serving_v1(&daemon).await;
    let v2 = release(&daemon, "v2", web("v2", "")).await;
    let v2 = settle(&daemon, &v2.deployment_id).await;
    assert_eq!(v2.record.status, DeploymentStatus::Complete);
    assert_eq!(get(endpoint).await.unwrap(), "v2");

    let load = Load::start(endpoint, 2);
    let back = daemon.rollback(&v2.deployment_id).await.unwrap();
    assert_ne!(back.deployment_id, v2.deployment_id, "a new release");
    let back = settle(&daemon, &back.deployment_id).await;
    let report = load.stop().await;
    assert_eq!(back.record.status, DeploymentStatus::Complete);
    assert_eq!(back.record.revision, "v1");
    assert_eq!(back.record.version, 3);
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(get(endpoint).await.unwrap(), "v1");

    // Rolling back a release in flight abandons it.
    let v3 = release(&daemon, "v3", web("v3", "time.sleep(3600)")).await;
    let abandoned = daemon.rollback(&v3.deployment_id).await.unwrap();
    assert_eq!(abandoned.record.status, DeploymentStatus::Failed);
    assert_eq!(abandoned.record.version, 4);
    assert_eq!(get(endpoint).await.unwrap(), "v1");
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_release_resumes_from_control_state_after_a_restart() {
    if !python().await {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let store: Arc<dyn compute_state::StateStore> = Arc::new(
        compute_state_file::FileState::open(root.path().join("control-state.json")).unwrap(),
    );
    let mut first_config = config(&root.path().join("node"), store.clone());
    let windows = (first_config.port_range, first_config.instance_port_range);
    first_config.drain_timeout = Duration::from_secs(30);
    let first = Daemon::start(first_config).await.unwrap();
    let endpoint = serving_v1(&first).await;

    // v2 needs a few seconds before it is ready; the daemon stops meanwhile.
    let started = release(&first, "v2", web("v2", "time.sleep(2)")).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while first
        .deployment(&started.deployment_id)
        .await
        .unwrap()
        .record
        .status
        != DeploymentStatus::Starting
    {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    first.shutdown().await;
    drop(first);

    // A new daemon reads the release from control state and finishes it.
    let mut second_config = config(&root.path().join("node"), store.clone());
    (second_config.port_range, second_config.instance_port_range) = windows;
    let second = Daemon::start(second_config).await.unwrap();
    let settled = settle(&second, &started.deployment_id).await;
    assert_eq!(
        settled.record.status,
        DeploymentStatus::Complete,
        "{:?}",
        settled.record
    );
    assert_eq!(get(endpoint).await.unwrap(), "v2");
    assert_eq!(
        second
            .workload("production", "site", "web")
            .await
            .unwrap()
            .ports[0]
            .host,
        endpoint
    );
    let kinds = kinds(&events_of(&second, &started.deployment_id).await);
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| *kind == "deployment.switched")
            .count(),
        1,
        "the switch happened exactly once: {kinds:?}"
    );
    second.shutdown().await;
}
