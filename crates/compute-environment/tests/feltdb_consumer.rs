//! The controller as a FeltDB 0.11.8 consumer, against a real
//! `feltdb-server`, in process:
//!
//! ```sh
//! FELTDB_SERVER_BIN=/path/to/feltdb-server \
//!   cargo test -p compute-environment --test feltdb_consumer -- --ignored --test-threads 1
//! ```
//!
//! Boundedness is asserted from what FeltDB reports it executed (queries,
//! rows scanned, index use), never from latency. `benchmark` measures; set
//! `COMPUTE_CERTIFICATION_OUT` to a directory to keep its JSON.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use compute_core::{
    IsolationRequirement, NetworkPolicy, ResourceLimits, RuntimeKind, WORKLOAD_SPEC_VERSION,
    WorkloadBundle, WorkloadSpec,
};
use compute_environment::*;
use compute_state::{
    AccessReport, Batch, Collection, ControlState, ExecutionRecord, Query, StateStore,
};
use compute_state_feltdb::{FeltDbConfig, FeltDbState, ProvisionRequest, provision};
use serde_json::{Value, json};

// ---- A real FeltDB ---------------------------------------------------------

const MASTER_KEY: &str = "compute-consumer-certification";

struct Server {
    child: Child,
    url: String,
    port: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn binary() -> PathBuf {
    PathBuf::from(std::env::var("FELTDB_SERVER_BIN").expect("FELTDB_SERVER_BIN"))
}

fn create_key(data: &Path) -> String {
    let output = Command::new(binary())
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

fn start(data: &Path, port: u16) -> Server {
    let mut child = Command::new(binary())
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
    let port = url
        .trim_end_matches('/')
        .rsplit(':')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    Server { child, url, port }
}

struct Authority {
    server: Server,
    data: tempfile::TempDir,
    config: FeltDbConfig,
    revision_id: String,
}

async fn authority(tenant: &str) -> Authority {
    let data = tempfile::tempdir().unwrap();
    let token = create_key(data.path());
    let server = start(data.path(), 0);
    let provisioned = provision(ProvisionRequest {
        url: server.url.clone(),
        token: token.clone(),
        application_id: None,
        tenant_id: None,
        tenant_name: tenant.into(),
        environment: "production".into(),
        ca_certificate: None,
    })
    .await
    .unwrap();
    Authority {
        config: FeltDbConfig {
            url: server.url.clone(),
            token,
            application_id: provisioned.application_id,
            environment: "production".into(),
            ca_certificate: None,
        },
        revision_id: provisioned.revision_id,
        server,
        data,
    }
}

// ---- A controller on it ----------------------------------------------------

fn bundle(script: &str) -> Vec<u8> {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("main.sh"), script).unwrap();
    let spec = WorkloadSpec {
        version: WORKLOAD_SPEC_VERSION.into(),
        runtime: RuntimeKind::Shell,
        runtime_version: None,
        entrypoint: "main.sh".into(),
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

fn task(name: &str, bundle: &[u8]) -> ProjectDefinition {
    ProjectDefinition {
        name: name.into(),
        revision: "r1".into(),
        source: None,
        desired_state: DesiredState::Running,
        env: BTreeMap::new(),
        workloads: vec![WorkloadDefinition {
            name: "job".into(),
            kind: WorkloadKind::Task,
            bundle: bundle.to_vec(),
            ports: vec![],
            restart: RestartPolicy::Never,
            desired_state: DesiredState::Running,
            readiness: None,
        }],
    }
}

async fn controller(
    config: &FeltDbConfig,
    node: &Path,
    ports: u16,
) -> (Arc<Daemon>, Arc<FeltDbState>) {
    let (state, _) = FeltDbState::connect_or_defer(config.clone()).await.unwrap();
    let state = Arc::new(state);
    let artifacts = Arc::new(compute_state::StateArtifacts::new(ControlState::new(
        state.clone(),
    )));
    let mut daemon_config = DaemonConfig::new(node, state.clone(), artifacts);
    daemon_config.port_range = (ports, ports + 99);
    daemon_config.instance_port_range = (ports + 20000, ports + 20099);
    // The test drives every cycle.
    daemon_config.reconcile_interval = Duration::from_secs(3600);
    (Daemon::start(daemon_config).await.unwrap(), state)
}

fn access(state: &FeltDbState) -> AccessReport {
    state.access().unwrap()
}

fn difference(before: &AccessReport, after: &AccessReport) -> Value {
    json!({
        "queries": after.queries - before.queries,
        "indexed": after.indexed_queries - before.indexed_queries,
        "scanned": after.scanned_queries - before.scanned_queries,
        "rows_scanned": after.rows_scanned - before.rows_scanned,
        "rows_returned": after.rows_returned - before.rows_returned,
        "revision_reads": after.revision_reads - before.revision_reads,
        "transactions": after.transactions - before.transactions,
    })
}

fn authority_view(info: &ControllerInfo) -> AuthorityView {
    info.control_plane
        .authority
        .clone()
        .expect("the authority view")
}

// ---- Authority -------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires FELTDB_SERVER_BIN"]
async fn the_controller_keeps_authority_in_feltdb_through_an_outage() {
    let authority = authority("compute-controller-authority").await;
    let node = tempfile::tempdir().unwrap();
    let (daemon, state) = controller(&authority.config, node.path(), 27100).await;

    // Healthy.
    daemon
        .create_environment(EnvironmentDefinition {
            name: "prod".into(),
            desired_state: DesiredState::Running,
            env: BTreeMap::new(),
            policy: None,
            provider: None,
        })
        .await
        .unwrap();
    daemon
        .add_project("prod", task("jobs", &bundle("echo ran")))
        .await
        .unwrap();
    let run = daemon.run_task("prod", "jobs", "job").await.unwrap();
    let view = authority_view(&daemon.info().await);
    assert_eq!(view.state, "healthy");
    assert_eq!(view.certified_feltdb.as_deref(), Some("0.11.8"));
    assert_eq!(view.model_generation, compute_state::MODEL_GENERATION);
    assert!(view.last_durable_read.is_some() && view.last_durable_mutation.is_some());
    assert!(!view.snapshots.is_empty());

    // A quiet cycle reads revisions, not the universe.
    daemon.reconcile().await;
    let before = access(&state);
    daemon.reconcile().await;
    let quiet = difference(&before, &access(&state));
    assert_eq!(
        quiet["queries"], 0,
        "a quiet cycle runs no queries: {quiet}"
    );
    assert_eq!(quiet["revision_reads"], 2, "one per snapshot: {quiet}");
    let view = authority_view(&daemon.info().await);
    let reused = view.cache.reused
        + view
            .snapshots
            .iter()
            .map(|snapshot| snapshot.reused)
            .sum::<u64>();
    assert!(
        reused >= 2,
        "the working copy and snapshots were reused: {view:?}"
    );

    // A targeted change reads back what it wrote, by identity.
    let rolled = authority_view(&daemon.info().await).cache.rolled_forward;
    let before = access(&state);
    daemon
        .set_project_state("prod", "jobs", DesiredState::Stopped, false)
        .await
        .unwrap();
    let after = access(&state);
    let targeted = difference(&before, &after);
    eprintln!("project stop at the FeltDB boundary: {targeted}");
    // The next full cycle carries the controller's own commits forward:
    // desired state is not read again.
    let scans_before = access(&state).scans;
    daemon.reconcile().await;
    let scans_after = access(&state).scans;
    assert_eq!(
        scans_after.get("Environment[]"),
        scans_before.get("Environment[]"),
        "desired state was not rebuilt after this controller's own change"
    );
    assert!(authority_view(&daemon.info().await).cache.rolled_forward > rolled);

    // Another writer: the revision moves by a commit this controller did
    // not make, so the next periodic cycle rebuilds and sees it.
    let other = ControlState::new(Arc::new(
        FeltDbState::connect(authority.config.clone())
            .await
            .unwrap(),
    ));
    other
        .transaction(Batch::new().create(
            "env_foreign",
            &compute_state::EnvironmentRecord {
                name: "foreign".into(),
                desired_state: compute_state::DesiredState::Stopped,
                config: BTreeMap::new(),
                policy: None,
                provider: None,
                created_at: chrono::Utc::now(),
            },
        ))
        .await
        .unwrap();
    daemon.reconcile().await;
    assert_eq!(
        daemon.environment("foreign").await.unwrap().name,
        "foreign",
        "periodic reconciliation sees other writers"
    );

    // Outage.
    let port = authority.server.port;
    drop(authority.server);
    daemon.reconcile().await;
    let view = authority_view(&daemon.info().await);
    assert_eq!(view.state, "degraded_control_plane");
    assert_eq!(
        view.cache.freshness, "stale",
        "stale state is never current"
    );
    assert!(view.degraded_since.is_some());
    // Reads are served from the last snapshot of durable state.
    let environment = daemon
        .environment("prod")
        .await
        .expect("served from the last read");
    assert_eq!(environment.name, "prod");
    // Mutations are refused: durable authority is unavailable.
    let refused = daemon
        .create_environment(EnvironmentDefinition {
            name: "during".into(),
            desired_state: DesiredState::Running,
            env: BTreeMap::new(),
            policy: None,
            provider: None,
        })
        .await
        .unwrap_err();
    assert_eq!(refused.kind(), "state_unavailable", "{refused}");
    // An execution this controller ran is served from its own evidence,
    // marked stale; one it never saw is not fabricated.
    assert_eq!(
        daemon
            .execution(&run.record.execution_id)
            .await
            .unwrap()
            .record
            .execution_id,
        run.record.execution_id
    );
    assert!(daemon.execution("exe_never_ran").await.is_err());

    // Recovery: the same authority returns.
    let _server = start(authority.data.path(), port);
    let builds_before = authority_view(&daemon.info().await)
        .snapshots
        .iter()
        .map(|snapshot| snapshot.builds)
        .sum::<u64>();
    daemon.reconcile().await;
    let view = authority_view(&daemon.info().await);
    assert_eq!(view.state, "healthy", "{view:?}");
    let recovery = view
        .last_recovery
        .clone()
        .expect("the recovery is recorded");
    assert!(recovery.outage_seconds >= 0.0);
    assert!(
        view.snapshots
            .iter()
            .map(|snapshot| snapshot.builds)
            .sum::<u64>()
            > builds_before,
        "state derived before the outage was rebuilt, not reused"
    );
    let events = daemon
        .events(EventFilter {
            limit: Some(1000),
            ..EventFilter::default()
        })
        .await
        .unwrap();
    assert!(events.iter().any(|event| event.kind == "feltdb.recovered"));
    // Mutations resume, and state is exactly durable state.
    daemon
        .create_environment(EnvironmentDefinition {
            name: "after".into(),
            desired_state: DesiredState::Running,
            env: BTreeMap::new(),
            policy: None,
            provider: None,
        })
        .await
        .unwrap();
    assert!(
        daemon.environment("during").await.is_err(),
        "nothing was written during the outage"
    );
    assert_scans_are_expected(&state);
    assert_eq!(
        daemon
            .execution(&run.record.execution_id)
            .await
            .unwrap()
            .record
            .execution_id,
        run.record.execution_id
    );
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires FELTDB_SERVER_BIN"]
async fn concurrent_operations_keep_every_record_and_event() {
    let authority = authority("compute-controller-concurrency").await;
    let node = tempfile::tempdir().unwrap();
    let (daemon, _state) = controller(&authority.config, node.path(), 27300).await;
    daemon
        .create_environment(EnvironmentDefinition {
            name: "prod".into(),
            desired_state: DesiredState::Running,
            env: BTreeMap::new(),
            policy: None,
            provider: None,
        })
        .await
        .unwrap();
    daemon
        .add_project("prod", task("jobs", &bundle("echo ran")))
        .await
        .unwrap();
    let mut runs = vec![];
    for _ in 0..40 {
        let daemon = daemon.clone();
        runs.push(tokio::spawn(async move {
            daemon.run_task("prod", "jobs", "job").await
        }));
    }
    let mut ids = BTreeSet::new();
    for run in runs {
        ids.insert(run.await.unwrap().unwrap().record.execution_id);
    }
    assert_eq!(ids.len(), 40);
    let durable = daemon.executions("prod", "jobs", 1000).await.unwrap();
    assert_eq!(
        durable
            .iter()
            .map(|record| record.execution_id.clone())
            .collect::<BTreeSet<_>>(),
        ids,
        "every execution record is durable"
    );
    assert_eq!(
        daemon.receipts("prod", "jobs", 1000).await.unwrap().len(),
        40
    );
    let events = daemon
        .events(EventFilter {
            limit: Some(1000),
            ..EventFilter::default()
        })
        .await
        .unwrap();
    let sequences = events
        .iter()
        .map(|event| event.sequence)
        .collect::<BTreeSet<_>>();
    assert_eq!(sequences.len(), events.len(), "event sequences are unique");
    let finished = events
        .iter()
        .filter(|event| {
            event
                .execution_id
                .as_ref()
                .is_some_and(|id| ids.contains(id))
        })
        .map(|event| event.execution_id.clone().unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(finished, ids, "every execution has its events");
    assert_scans_are_expected(&_state);
    daemon.shutdown().await;
}

/// The only queries FeltDB may answer by scanning on the controller's
/// paths: the whole-collection sources of its two snapshots (desired and
/// observed state, bounded by what the control plane runs), the credential
/// cache, and the latest event sequence (FeltDB 0.11.8's planner uses no
/// ordered index). Anything else is a new unbounded access path.
fn assert_scans_are_expected(state: &FeltDbState) {
    let allowed = [
        "Environment[]",
        "Project[]",
        "EnvironmentProject[]",
        "Workload[]",
        "WorkloadInstance[]",
        "TrafficAssignment[]",
        "Domain[]",
        "DnsRecord[]",
        "Certificate[]",
        "WorkloadStatus[]",
        "OperatorCredential[]",
        "Event[] by sequence desc",
    ];
    let unexpected = access(state)
        .scans
        .into_keys()
        .filter(|shape| !allowed.contains(&shape.as_str()))
        .collect::<Vec<_>>();
    assert!(
        unexpected.is_empty(),
        "queries FeltDB answered by scanning: {unexpected:?}"
    );
}

// ---- Benchmarks ------------------------------------------------------------

#[derive(Default)]
struct Samples {
    rows: Vec<Value>,
}

impl Samples {
    /// Time `n` runs of `operation`, with what FeltDB did per run.
    async fn measure<F, Fut>(
        &mut self,
        state: &FeltDbState,
        name: &str,
        scope: &str,
        pattern: &str,
        n: usize,
        mut operation: F,
    ) where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let before = access(state);
        let mut times = vec![];
        for _ in 0..n {
            let started = Instant::now();
            operation().await;
            times.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        let after = access(state);
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let at = |quantile: f64| times[((times.len() - 1) as f64 * quantile).round() as usize];
        let per = |total: u64| (total as f64 / n as f64 * 10.0).round() / 10.0;
        let row = json!({
            "operation": name,
            "scope": scope,
            "pattern": pattern,
            "n": n,
            "p50_ms": (at(0.5) * 100.0).round() / 100.0,
            "p95_ms": (at(0.95) * 100.0).round() / 100.0,
            "max_ms": (at(1.0) * 100.0).round() / 100.0,
            "feltdb_queries_per_call": per(after.queries - before.queries),
            "feltdb_revision_reads_per_call": per(after.revision_reads - before.revision_reads),
            "feltdb_transactions_per_call": per(after.transactions - before.transactions),
            "feltdb_rows_scanned_per_call": per(after.rows_scanned - before.rows_scanned),
            "feltdb_rows_returned_per_call": per(after.rows_returned - before.rows_returned),
        });
        eprintln!("{row}");
        self.rows.push(row);
    }
}

/// A query exactly as Compute sent it before this change: `_id` and all,
/// straight to FeltDB, bypassing the adapter's planner.
async fn legacy_query(authority: &Authority, query: Value) -> usize {
    let response = reqwest::Client::new()
        .post(format!("{}/v1/query", authority.config.url))
        .bearer_auth(&authority.config.token)
        .header("FeltDB-Protocol", "1")
        .json(&json!({
            "application_id": authority.config.application_id,
            "environment": "production",
            "revision_id": authority.revision_id,
            "query": query,
        }))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    response.json::<Value>().await.unwrap()["records"]
        .as_array()
        .map_or(0, Vec::len)
}

fn host() -> Value {
    let read = |path: &str| std::fs::read_to_string(path).unwrap_or_default();
    let cpuinfo = read("/proc/cpuinfo");
    let meminfo = read("/proc/meminfo");
    json!({
        "os": format!("{} {}", std::env::consts::OS, read("/proc/sys/kernel/osrelease").trim()),
        "cpu": cpuinfo.lines().find(|line| line.starts_with("model name")).and_then(|line| line.split(':').nth(1)).map(str::trim),
        "cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "memory": meminfo.lines().next().map(|line| line.split_whitespace().skip(1).collect::<Vec<_>>().join(" ")),
        "build": if cfg!(debug_assertions) { "debug" } else { "release" },
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires FELTDB_SERVER_BIN; measures"]
async fn benchmark() {
    let samples_per = std::env::var("COMPUTE_BENCH_SAMPLES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(30usize);
    let projects = 40usize;
    let history = 4000usize;
    let authority = authority("compute-benchmark").await;
    let node = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let (daemon, state) = controller(&authority.config, node.path(), 27500).await;
    let startup_empty_ms = started.elapsed().as_secs_f64() * 1000.0;
    let control = ControlState::new(state.clone());
    let mut results = Samples::default();

    // Desired state: one environment, `projects` task projects.
    daemon
        .create_environment(EnvironmentDefinition {
            name: "prod".into(),
            desired_state: DesiredState::Running,
            env: BTreeMap::new(),
            policy: None,
            provider: None,
        })
        .await
        .unwrap();
    let bundle = bundle("echo done");
    for index in 0..projects {
        daemon
            .add_project("prod", task(&format!("p{index}"), &bundle))
            .await
            .unwrap();
    }
    // History: executions of every project, as months of operation leave.
    let environment_id = daemon
        .environment("prod")
        .await
        .unwrap()
        .environment_id
        .clone();
    let mut batch = Batch::new();
    for index in 0..history {
        let project = index % projects;
        let record: ExecutionRecord = serde_json::from_value(json!({
            "execution_id": format!("bench_{index}"),
            "environment_id": environment_id,
            "environment": "prod",
            "project_id": compute_state::ids::project(&format!("p{project}")),
            "project": format!("p{project}"),
            "workload_id": format!("wl_bench_{project}"),
            "workload": "job",
            "kind": "task",
            "status": "completed",
            "started_at": chrono::Utc::now(),
        }))
        .unwrap();
        batch = batch.create(
            &compute_state::ids::execution(&format!("bench_{index}")),
            &record,
        );
        if batch.len() == 200 {
            control
                .transaction(std::mem::take(&mut batch))
                .await
                .unwrap();
        }
    }
    control.transaction(batch).await.unwrap();
    let executions_total = control
        .query::<ExecutionRecord>(Query::all(Collection::Execution).limit(100_000))
        .await
        .unwrap()
        .len();
    let n = samples_per;
    let target = compute_state::ids::execution(&format!("bench_{}", history / 2));
    let project_id = compute_state::ids::project("p7");

    // Early: placement's capability cache is not re-discovered after its
    // TTL (a pre-existing placement issue), so adds are measured first.
    let mut index = projects;
    results
        .measure(
            &state,
            "add project/service",
            "end-to-end",
            "after",
            n.min(10),
            || {
                index += 1;
                let daemon = daemon.clone();
                let definition = task(&format!("p{index}"), &bundle);
                async move {
                    daemon.add_project("prod", definition).await.unwrap();
                }
            },
        )
        .await;
    // Reads.
    results
        .measure(
            &state,
            "single indexed lookup",
            "feltdb-only",
            "after",
            n,
            || async {
                assert!(
                    control
                        .get::<ExecutionRecord>(&target)
                        .await
                        .unwrap()
                        .is_some()
                );
            },
        )
        .await;
    results
        .measure(
            &state,
            "single indexed lookup",
            "feltdb-only",
            "before (_id scan)",
            n,
            || async {
                let found = legacy_query(
                    &authority,
                    json!({ "collection": "Execution", "limit": 1,
                        "filter": { "operator": "eq", "field": "_id", "value": target } }),
                )
                .await;
                assert_eq!(found, 1);
            },
        )
        .await;
    results
        .measure(
            &state,
            "bounded list (a project's 20 newest executions)",
            "feltdb-only",
            "after",
            n,
            || async {
                let recent = control
                    .query::<ExecutionRecord>(
                        Query::all(Collection::Execution)
                            .eq("project_id", project_id.clone())
                            .eq("environment_id", environment_id.clone())
                            .descending("started_at")
                            .limit(20),
                    )
                    .await
                    .unwrap();
                assert_eq!(recent.len(), 20);
            },
        )
        .await;
    results
        .measure(
            &state,
            "bounded list (a project's 20 newest executions)",
            "feltdb-only",
            "before (whole collection, filtered in Compute)",
            n.min(10),
            || async {
                let mut all = control.list::<ExecutionRecord>().await.unwrap();
                all.retain(|record| record.value.project_id == project_id);
                all.sort_by(|left, right| right.value.started_at.cmp(&left.value.started_at));
                all.truncate(20);
                assert_eq!(all.len(), 20);
            },
        )
        .await;
    results
        .measure(
            &state,
            "execution lookup",
            "end-to-end",
            "after",
            n,
            || async {
                daemon
                    .execution(&format!("bench_{}", history / 3))
                    .await
                    .unwrap();
            },
        )
        .await;
    results
        .measure(
            &state,
            "executions view (project, 20)",
            "end-to-end",
            "after",
            n,
            || async {
                assert_eq!(daemon.executions("prod", "p7", 20).await.unwrap().len(), 20);
            },
        )
        .await;
    results
        .measure(
            &state,
            "environment view",
            "end-to-end",
            "after",
            n,
            || async {
                daemon.environment("prod").await.unwrap();
            },
        )
        .await;
    results
        .measure(&state, "project view", "end-to-end", "after", n, || async {
            daemon.project("prod", "p3").await.unwrap();
        })
        .await;
    let deployment_id = daemon
        .deployments(Some("prod".into()), Some("p5".into()), Some(1))
        .await
        .unwrap()[0]
        .deployment_id
        .clone();
    results
        .measure(
            &state,
            "release lookup",
            "end-to-end",
            "after",
            n,
            || async {
                daemon.deployment(&deployment_id).await.unwrap();
            },
        )
        .await;

    // Snapshots.
    use compute_state::{SnapshotDefinition, SnapshotSource};
    let small = control
        .snapshot(SnapshotDefinition::new(
            "bench.small",
            vec![SnapshotSource::filtered(
                Query::all(Collection::Project).one_of("name", ["p1", "p2"]),
            )],
        ))
        .unwrap();
    results
        .measure(
            &state,
            "small bounded snapshot (2 records, build)",
            "feltdb-only",
            "after",
            n,
            || async {
                small.refresh(true).await.unwrap();
            },
        )
        .await;
    let large = control
        .snapshot(SnapshotDefinition::new(
            "bench.large",
            vec![SnapshotSource::all(Collection::Execution)],
        ))
        .unwrap();
    results
        .measure(
            &state,
            "large bounded snapshot (every execution, build)",
            "feltdb-only",
            "after",
            n.min(10),
            || async {
                large.refresh(true).await.unwrap();
            },
        )
        .await;
    let desired = control.snapshot(desired_snapshot_definition()).unwrap();
    results
        .measure(
            &state,
            "multi-source coherent snapshot (desired state, build)",
            "feltdb-only",
            "after",
            n,
            || async {
                desired.refresh(true).await.unwrap();
            },
        )
        .await;
    results
        .measure(
            &state,
            "multi-source coherent snapshot (desired state, unchanged: reuse)",
            "feltdb-only",
            "after",
            n,
            || async {
                desired.refresh(false).await.unwrap();
            },
        )
        .await;
    results
        .measure(
            &state,
            "desired-state load",
            "feltdb-only",
            "before (independent whole reads, _id gets)",
            n,
            || async {
                legacy_load_desired(&authority, &control).await;
            },
        )
        .await;

    // Mutations.
    let mut counter = 0u64;
    results
        .measure(
            &state,
            "single durable mutation",
            "feltdb-only",
            "after",
            n,
            || {
                counter += 1;
                let id = format!("aud_bench_{counter}");
                let control = control.clone();
                async move {
                    let record = compute_state::AuditRecord {
                        request_id: id.clone(),
                        operator_id: "bench".into(),
                        credential_id: None,
                        operation: "bench".into(),
                        resource: "bench".into(),
                        resource_id: None,
                        result: "allowed".into(),
                        status: 200,
                        error_kind: None,
                        detail: serde_json::Map::new(),
                        at: chrono::Utc::now(),
                    };
                    control
                        .transaction(Batch::new().create(&id, &record))
                        .await
                        .unwrap();
                }
            },
        )
        .await;
    let mut counter = 0u64;
    results
        .measure(
            &state,
            "mutation + event (environment create)",
            "end-to-end",
            "after",
            n,
            || {
                counter += 1;
                let daemon = daemon.clone();
                async move {
                    daemon
                        .create_environment(EnvironmentDefinition {
                            name: format!("bench{counter}"),
                            desired_state: DesiredState::Stopped,
                            env: BTreeMap::new(),
                            policy: None,
                            provider: None,
                        })
                        .await
                        .unwrap();
                }
            },
        )
        .await;
    let mut stopped = false;
    results
        .measure(
            &state,
            "project update (stop/start)",
            "end-to-end",
            "after",
            n,
            || {
                stopped = !stopped;
                let daemon = daemon.clone();
                let desired = if stopped {
                    DesiredState::Stopped
                } else {
                    DesiredState::Running
                };
                async move {
                    daemon
                        .set_project_state("prod", "p9", desired, false)
                        .await
                        .unwrap();
                }
            },
        )
        .await;
    let mut revision = 0u64;
    results
        .measure(
            &state,
            "release update (register, deploy, complete)",
            "end-to-end",
            "after",
            n.min(10),
            || {
                revision += 1;
                let daemon = daemon.clone();
                let bundle = bundle.clone();
                async move {
                    let mut definition = task("p11", &bundle);
                    definition.revision = format!("r-bench-{revision}");
                    let registered = daemon
                        .register_revision("p11", definition.revision_definition())
                        .await
                        .unwrap();
                    let deployment = daemon
                        .deploy(DeployRequest {
                            project: "p11".into(),
                            environment: "prod".into(),
                            revision: Some(registered.revision_id),
                            config: Some(BTreeMap::new()),
                            desired_state: Some(DesiredState::Running),
                        })
                        .await
                        .unwrap();
                    daemon
                        .await_release(&deployment.deployment_id, Duration::from_secs(60))
                        .await
                        .unwrap();
                }
            },
        )
        .await;

    // Controller.
    results
        .measure(
            &state,
            "reconciliation (full cycle, nothing changed)",
            "end-to-end",
            "after",
            n,
            || async {
                daemon.reconcile().await;
            },
        )
        .await;
    daemon.shutdown().await;
    let node = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let (restarted, _) = controller(&authority.config, node.path(), 27700).await;
    let startup_loaded_ms = started.elapsed().as_secs_f64() * 1000.0;
    let info = restarted.info().await;
    restarted.shutdown().await;

    let report = json!({
        "format": "compute.feltdb-consumer-benchmark@1",
        "measured_at": chrono::Utc::now(),
        "feltdb": {
            "certified": compute_state_feltdb::CERTIFIED_FELTDB_VERSION,
            "server": info.control_plane.authority.as_ref().and_then(|view| view.access.as_ref()).and_then(|access| access.server_version.clone()),
            "transport": "feltdb-server on this host, HTTP over loopback",
        },
        "model_generation": compute_state::MODEL_GENERATION,
        "host": host(),
        "dataset": {
            "projects": projects,
            "executions": executions_total,
        },
        "samples": n,
        "controller": {
            "startup_empty_ms": startup_empty_ms,
            "startup_with_state_ms": startup_loaded_ms,
            "last_reconcile": info.reconcile.last,
        },
        "scopes": {
            "feltdb-only": "the adapter's request(s) to FeltDB and back; Compute does no work but encode and decode",
            "end-to-end": "a controller operation: Compute work plus its FeltDB requests; the requests per call are listed",
            "compute-only": "not measured separately: end-to-end minus FeltDB is not isolated here",
        },
        "results": results.rows,
    });
    eprintln!("{}", serde_json::to_string_pretty(&report).unwrap());
    if let Ok(directory) = std::env::var("COMPUTE_CERTIFICATION_OUT") {
        std::fs::write(
            Path::new(&directory).join("feltdb-consumer-benchmark.json"),
            serde_json::to_string_pretty(&report).unwrap(),
        )
        .unwrap();
    }
}

/// The controller's desired-state snapshot, as the daemon defines it.
fn desired_snapshot_definition() -> compute_state::SnapshotDefinition {
    compute_environment::desired_snapshot()
}

/// Desired state as Compute read it before this change: every collection
/// whole and independently, the in-flight releases by status, then each
/// named deployment and revision through an `_id` filter (a scan in FeltDB).
async fn legacy_load_desired(authority: &Authority, control: &ControlState) {
    let lists = [
        Collection::Environment,
        Collection::Project,
        Collection::EnvironmentProject,
        Collection::Workload,
        Collection::WorkloadInstance,
        Collection::TrafficAssignment,
        Collection::Domain,
        Collection::DnsRecord,
        Collection::Certificate,
    ];
    let mut reads = tokio::task::JoinSet::new();
    for collection in lists {
        let store = control.store().clone();
        reads.spawn(async move { store.query(&Query::all(collection)).await.unwrap() });
    }
    let mut named = BTreeSet::new();
    while let Some(records) = reads.join_next().await {
        for record in records.unwrap() {
            if let Some(Value::String(id)) = record.value.get("deployment_id") {
                named.insert(id.clone());
            }
        }
    }
    for status in [
        "pending",
        "starting",
        "ready",
        "network_ready",
        "switching",
        "active",
        "draining",
    ] {
        control
            .store()
            .query(&Query::all(Collection::Deployment).eq("status", status))
            .await
            .unwrap();
    }
    let mut gets = vec![];
    for id in named {
        gets.push(legacy_query(
            authority,
            json!({ "collection": "Deployment", "limit": 1,
                    "filter": { "operator": "eq", "field": "_id", "value": id } }),
        ));
    }
    for get in gets {
        get.await;
    }
}
