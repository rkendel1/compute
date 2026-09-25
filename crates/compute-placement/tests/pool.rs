//! End-to-end placement against real providers: the local engine plus
//! several `compute.remote@1` servers with intentionally different,
//! operator-restricted capabilities.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use compute_core::{
    IsolationProfile, IsolationRequirement, NetworkPolicy, ProviderIdentity, ResourceLimits,
    RuntimeKind, SelectionMode, WORKLOAD_SPEC_VERSION, WorkloadBundle, WorkloadOutput,
    WorkloadSpec,
};
use compute_placement::{
    CapabilityCache, DiscoveryMode, DiscoveryStatus, DispatchErrorCode, EvaluationStatus,
    PlacementOutcome, PlacementRequirements, PoolPolicy, ProviderConfig, ProviderKind,
    ProviderPool, RequirementOptions, SubmissionMode, dispatch, place,
};
use compute_provider::{
    ComputeProvider, ExecuteResponse, InspectResponse, LocalProvider, ProviderCapabilities,
    ProviderError, ProviderHealth, ProviderPolicy, ProviderRequest, RemoteProvider, ServerConfig,
};

/// Wraps a provider and counts executions, to prove that nothing ran.
struct Counting {
    inner: LocalProvider,
    executions: Arc<AtomicUsize>,
}

#[async_trait]
impl ComputeProvider for Counting {
    fn identity(&self) -> ProviderIdentity {
        self.inner.identity()
    }
    async fn inspect(&self, request: ProviderRequest) -> Result<InspectResponse, ProviderError> {
        self.inner.inspect(request).await
    }
    async fn execute(&self, request: ProviderRequest) -> Result<ExecuteResponse, ProviderError> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        self.inner.execute(request).await
    }
    async fn capabilities(&self) -> Result<ProviderCapabilities, ProviderError> {
        self.inner.capabilities().await
    }
    async fn health(&self) -> Result<ProviderHealth, ProviderError> {
        self.inner.health().await
    }
    async fn resolve_runtime(
        &self,
        requirement: compute_core::ProviderRuntimeRequirement,
    ) -> Result<compute_core::RuntimeResolution, ProviderError> {
        self.inner.resolve_runtime(requirement).await
    }
    async fn prepare_runtime(
        &self,
        distribution: compute_core::RuntimeDistribution,
    ) -> Result<compute_core::RuntimePreparation, ProviderError> {
        self.inner.prepare_runtime(distribution).await
    }
    async fn runtime_status(
        &self,
        distribution: compute_core::RuntimeDistribution,
    ) -> Result<compute_core::RuntimeResolution, ProviderError> {
        self.inner.runtime_status(distribution).await
    }
}

/// Managed runtimes come from a host-backed fixture catalog: no download.
fn catalog() -> compute_provider::RuntimeCatalog {
    let directory = tempfile::tempdir().unwrap().keep();
    compute_provider::testing::host_fixture_catalog(&directory)
        .unwrap()
        .catalog
}

struct Server {
    endpoint: String,
    handle: tokio::task::JoinHandle<()>,
    _jobs: tempfile::TempDir,
}

async fn serve(policy: ProviderPolicy) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let jobs = tempfile::tempdir().unwrap();
    let mut config = ServerConfig::local(endpoint.clone());
    config.provider = Arc::new(
        LocalProvider::with_identity(ProviderIdentity::Remote {
            id: endpoint.clone(),
            endpoint: endpoint.clone(),
        })
        .with_policy(policy)
        .with_runtime_catalog(catalog()),
    );
    config.job_store = jobs.path().to_path_buf();
    let handle = tokio::spawn(async move {
        let _ = compute_provider::serve_listener(listener, config).await;
    });
    Server {
        endpoint,
        handle,
        _jobs: jobs,
    }
}

/// Serve a fixed HTTP response to every request.
async fn serve_raw(body: &'static str) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = [0; 8192];
            let _ = stream.read(&mut buffer).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    (endpoint, handle)
}

fn remote(endpoint: &str, priority: i64) -> ProviderConfig {
    ProviderConfig {
        kind: ProviderKind::Remote,
        endpoint: Some(endpoint.into()),
        application_endpoint: None,
        priority,
        token_env: None,
    }
}

fn local(priority: i64) -> ProviderConfig {
    ProviderConfig {
        kind: ProviderKind::Local,
        endpoint: None,
        application_endpoint: None,
        priority,
        token_env: None,
    }
}

fn wasm_bundle(isolation: IsolationProfile) -> WorkloadBundle {
    let root = tempfile::tempdir().unwrap();
    let module = wat::parse_str(
        r#"(module
            (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (data (i32.const 8) "placed\n")
            (func (export "_start")
                (i32.store (i32.const 0) (i32.const 8))
                (i32.store (i32.const 4) (i32.const 7))
                (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 20)))))"#,
    )
    .unwrap();
    std::fs::write(root.path().join("main.wasm"), module).unwrap();
    let spec = WorkloadSpec {
        version: WORKLOAD_SPEC_VERSION.into(),
        runtime: RuntimeKind::Wasm,
        runtime_version: None,
        architecture: None,
        entrypoint: "main.wasm".into(),
        args: vec![],
        env: BTreeMap::new(),
        inputs: vec![],
        outputs: vec![],
        resources: ResourceLimits {
            wall_time: Some(Duration::from_secs(10)),
            ..ResourceLimits::default()
        },
        network: NetworkPolicy::None,
        isolation: IsolationRequirement {
            profile: isolation,
            host: Default::default(),
        },
        dependencies: None,
    };
    WorkloadBundle::create_from(spec, root.path()).unwrap()
}

fn shell_bundle() -> WorkloadBundle {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("main.sh"),
        "printf 'shell-placed'; printf ok > \"$COMPUTE_OUTPUT_DIR/result.txt\"",
    )
    .unwrap();
    let spec = WorkloadSpec {
        version: WORKLOAD_SPEC_VERSION.into(),
        runtime: RuntimeKind::Shell,
        runtime_version: None,
        architecture: None,
        entrypoint: "main.sh".into(),
        args: vec![],
        env: BTreeMap::new(),
        inputs: vec![],
        outputs: vec![WorkloadOutput {
            path: "result.txt".into(),
            required: true,
        }],
        resources: ResourceLimits::default(),
        network: NetworkPolicy::Network,
        isolation: IsolationRequirement::default(),
        dependencies: None,
    };
    WorkloadBundle::create_from(spec, root.path()).unwrap()
}

fn prepare(
    bundle: &WorkloadBundle,
    submission: SubmissionMode,
) -> (
    ProviderRequest,
    PlacementRequirements,
    compute_placement::AdmissionContext,
) {
    let mut request = ProviderRequest::bundle(bundle.to_bytes().unwrap());
    request.expected.workload_id = Some(bundle.workload_id().unwrap());
    request.expected.bundle_id = Some(bundle.bundle_id().unwrap());
    let size = serde_json::to_vec(&request).unwrap().len() as u64;
    let requirements = PlacementRequirements::from_bundle(
        bundle,
        size,
        submission,
        &RequirementOptions::default(),
    )
    .unwrap();
    let admission = compute_placement::AdmissionContext::new(
        &[],
        compute_policy::ExecutionContract::from_bundle(bundle, None).unwrap(),
    );
    (request, requirements, admission)
}

fn restricted(runtimes: &[RuntimeKind], isolation: &[IsolationProfile]) -> ProviderPolicy {
    ProviderPolicy {
        runtimes: Some(runtimes.iter().copied().collect()),
        isolation_profiles: Some(isolation.iter().copied().collect::<BTreeSet<_>>()),
        ..ProviderPolicy::default()
    }
}

struct Matrix {
    pool: ProviderPool,
    local_executions: Arc<AtomicUsize>,
    servers: Vec<Server>,
}

/// local (priority 10, everything), process (priority 100: shell and python,
/// process isolation only), strict (priority 50: wasm and shell, every
/// profile), node (priority 50: node and deno, strict only).
async fn matrix() -> Matrix {
    let process = serve(restricted(
        &[RuntimeKind::Shell, RuntimeKind::Python],
        &[IsolationProfile::Process],
    ))
    .await;
    let strict = serve(restricted(
        &[RuntimeKind::Wasm, RuntimeKind::Shell],
        &IsolationProfile::ALL,
    ))
    .await;
    let node = serve(restricted(
        &[RuntimeKind::Node, RuntimeKind::Deno],
        &[IsolationProfile::Strict],
    ))
    .await;
    let executions = Arc::new(AtomicUsize::new(0));
    let mut pool = ProviderPool::new(PoolPolicy::default());
    pool.add(
        "local",
        local(10),
        Arc::new(Counting {
            inner: LocalProvider::new().with_runtime_catalog(catalog()),
            executions: executions.clone(),
        }),
    )
    .unwrap();
    for (id, server, priority) in [
        ("process", &process, 100),
        ("strict", &strict, 50),
        ("node", &node, 50),
    ] {
        pool.add_remote(
            id,
            remote(&server.endpoint, priority),
            Arc::new(RemoteProvider::new(server.endpoint.clone())),
        )
        .unwrap();
    }
    Matrix {
        pool,
        local_executions: executions,
        servers: vec![process, strict, node],
    }
}

async fn discover(pool: &ProviderPool) -> Vec<compute_placement::DiscoveryRecord> {
    let mut cache = CapabilityCache::default();
    pool.capabilities(&mut cache, DiscoveryMode::Refresh, None, Utc::now())
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn placement_selects_only_providers_that_satisfy_each_workload() {
    let matrix = matrix().await;
    let records = discover(&matrix.pool).await;
    assert!(
        records
            .iter()
            .all(|record| record.status == DiscoveryStatus::Discovered)
    );

    // Strict WASM: the priority-100 process provider cannot satisfy strict
    // isolation and must never win. Auto prefers the eligible local provider
    // over the higher-priority remote provider.
    let mut bundle = wasm_bundle(IsolationProfile::Strict);
    bundle.workload.resources.cpu_count = Some(1);
    bundle.workload.resources.memory_required_bytes = Some(64 * 1024 * 1024);
    bundle.workload.resources.disk_bytes = Some(1024 * 1024);
    let (request, requirements, admission) = prepare(&bundle, SubmissionMode::Synchronous);
    let report = place(
        &matrix.pool.configs(),
        matrix.pool.policy(),
        &records,
        &requirements,
        &admission,
        None,
    );
    assert_eq!(report.compatible_providers, ["strict", "local"]);
    let process = report
        .providers
        .iter()
        .find(|p| p.provider_id == "process")
        .unwrap();
    assert_eq!(process.status, EvaluationStatus::Incompatible);
    let codes = process
        .reasons
        .iter()
        .map(|r| r.code.as_str())
        .collect::<Vec<_>>();
    assert!(
        codes.contains(&"runtime_unsupported".to_string()),
        "{codes:?}"
    );
    assert!(
        codes.contains(&"isolation_unsupported".to_string()),
        "{codes:?}"
    );
    let node = report
        .providers
        .iter()
        .find(|p| p.provider_id == "node")
        .unwrap();
    assert_eq!(node.status, EvaluationStatus::Incompatible);
    let selected = report.selected.clone().unwrap();
    assert_eq!(selected.provider_id, "local");

    let response = dispatch::execute(&matrix.pool, &report, request)
        .await
        .unwrap();
    assert_eq!(response.result.stdout.text, "placed\n");
    let receipt = response.result.receipt.unwrap();
    receipt.verify().unwrap();
    report.verify_receipt(&receipt).unwrap();
    let placement = receipt.placement.unwrap();
    assert_eq!(placement.placement_id, report.placement_id);
    assert_eq!(placement.provider_id, "local");
    assert_eq!(placement.selection_mode, SelectionMode::Pool);
    assert_eq!(placement.provider_protocol, "compute.local@1");
    assert_eq!(placement.requested_resources.cpu_count, 1);
    assert_eq!(placement.requested_resources.memory_bytes, 64 * 1024 * 1024);
    assert_eq!(placement.allocated_resources.disk_bytes, 1024 * 1024);
    assert!(placement.provider_resources.available.cpu_count >= 1);
    assert!(placement.execution_platform.is_some());
    assert_eq!(
        receipt.provider,
        Some(ProviderIdentity::Local { id: "local".into() })
    );
    assert_eq!(receipt.isolation.effective, IsolationProfile::Strict);
    assert_eq!(matrix.local_executions.load(Ordering::SeqCst), 1);

    // Process-isolated shell: auto still prefers the eligible local provider.
    let bundle = shell_bundle();
    let (request, requirements, admission) = prepare(&bundle, SubmissionMode::Synchronous);
    let report = place(
        &matrix.pool.configs(),
        matrix.pool.policy(),
        &records,
        &requirements,
        &admission,
        None,
    );
    assert_eq!(report.compatible_providers, ["process", "strict", "local"]);
    let response = dispatch::execute(&matrix.pool, &report, request)
        .await
        .unwrap();
    assert_eq!(response.result.stdout.text, "shell-placed");
    let receipt = response.result.receipt.unwrap();
    assert_eq!(receipt.placement.as_ref().unwrap().provider_id, "local");
    report.verify_receipt(&receipt).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_provider_executes_there_or_fails_without_fallback() {
    let matrix = matrix().await;
    let bundle = wasm_bundle(IsolationProfile::Strict);
    let (request, requirements, admission) = prepare(&bundle, SubmissionMode::Synchronous);

    let mut cache = CapabilityCache::default();
    let only = matrix
        .pool
        .capabilities(
            &mut cache,
            DiscoveryMode::Refresh,
            Some("process"),
            Utc::now(),
        )
        .await;
    assert_eq!(only.len(), 1);
    let report = place(
        &matrix.pool.configs(),
        matrix.pool.policy(),
        &only,
        &requirements,
        &admission,
        Some("process"),
    );
    assert_eq!(report.outcome, PlacementOutcome::PlacementFailed);
    assert_eq!(
        report.failure.as_ref().unwrap().code,
        "explicit_provider_incompatible"
    );
    let error = dispatch::execute(&matrix.pool, &report, request.clone())
        .await
        .unwrap_err();
    assert_eq!(error.code, DispatchErrorCode::PlacementFailed);
    assert!(!error.retried);
    assert_eq!(matrix.local_executions.load(Ordering::SeqCst), 0);

    let only = matrix
        .pool
        .capabilities(
            &mut cache,
            DiscoveryMode::Refresh,
            Some("local"),
            Utc::now(),
        )
        .await;
    let report = place(
        &matrix.pool.configs(),
        matrix.pool.policy(),
        &only,
        &requirements,
        &admission,
        Some("local"),
    );
    let response = dispatch::execute(&matrix.pool, &report, request)
        .await
        .unwrap();
    let placement = response
        .result
        .receipt
        .as_ref()
        .unwrap()
        .placement
        .clone()
        .unwrap();
    assert_eq!(placement.selection_mode, SelectionMode::Explicit);
    assert_eq!(placement.provider_id, "local");
    assert_eq!(placement.provider_protocol, "compute.local@1");
    assert_eq!(matrix.local_executions.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pool_submission_places_jobs_on_job_capable_providers() {
    let matrix = matrix().await;
    let records = discover(&matrix.pool).await;
    let bundle = wasm_bundle(IsolationProfile::Process);
    let (request, requirements, admission) = prepare(&bundle, SubmissionMode::Job);
    let report = place(
        &matrix.pool.configs(),
        matrix.pool.policy(),
        &records,
        &requirements,
        &admission,
        None,
    );
    let local = report
        .providers
        .iter()
        .find(|p| p.provider_id == "local")
        .unwrap();
    assert_eq!(local.reasons[0].code.as_str(), "jobs_unsupported");
    assert_eq!(report.selected.as_ref().unwrap().provider_id, "strict");
    let submitted = dispatch::submit(&matrix.pool, &report, request, None)
        .await
        .unwrap();
    assert_eq!(submitted.provider_id, "strict");
    let client = RemoteProvider::new(submitted.endpoint.clone().unwrap());
    let job_id = submitted.job.job_id.0.clone();
    let job = loop {
        let job = client.job_status(&job_id).await.unwrap();
        if job.status.is_terminal() {
            break job;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let durable_placement = job
        .placement
        .as_ref()
        .expect("job preserves placement policy");
    assert_eq!(durable_placement.policy.mode, "auto");
    assert_eq!(
        durable_placement
            .candidates
            .iter()
            .filter(|candidate| candidate.selected)
            .count(),
        1
    );
    let result = client.job_result(&job_id).await.unwrap();
    assert_eq!(result.result.stdout.text, "placed\n");
    let receipt = client.job_receipt(&job_id).await.unwrap().receipt;
    report.verify_receipt(&receipt).unwrap();
    assert_eq!(
        receipt.placement.unwrap().placement_id,
        submitted.placement_id
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failure_certification() {
    let matrix = matrix().await;
    let (malformed, malformed_handle) = serve_raw("{\"not\": \"capabilities\"}").await;
    let contradictory_body: &'static str = {
        let mut capabilities = LocalProvider::new().capabilities().await.unwrap();
        capabilities.protocol = "compute.remote@1".into();
        capabilities.provider = ProviderIdentity::Remote {
            id: "http://liar".into(),
            endpoint: "http://liar".into(),
        };
        capabilities.inventory.runtimes[0]
            .capabilities
            .isolation
            .memory_enforcement = !capabilities.inventory.runtimes[0]
            .capabilities
            .memory_limit
            .supported;
        Box::leak(
            serde_json::to_string(&capabilities)
                .unwrap()
                .into_boxed_str(),
        )
    };
    let (contradictory, contradictory_handle) = serve_raw(contradictory_body).await;
    let vanished = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };

    let mut pool = ProviderPool::new(PoolPolicy::default());
    for (id, endpoint) in [
        ("malformed", malformed.as_str()),
        ("contradictory", contradictory.as_str()),
        ("vanished", vanished.as_str()),
    ] {
        pool.add_remote(
            id,
            remote(endpoint, 1000),
            Arc::new(RemoteProvider::new(endpoint)),
        )
        .unwrap();
    }
    let strict_endpoint = matrix.servers[1].endpoint.clone();
    pool.add_remote(
        "strict",
        remote(&strict_endpoint, 1),
        Arc::new(RemoteProvider::new(strict_endpoint.clone())),
    )
    .unwrap();

    let records = discover(&pool).await;
    let status = |id: &str| records.iter().find(|r| r.provider_id == id).unwrap().status;
    assert_eq!(status("malformed"), DiscoveryStatus::Invalid);
    assert_eq!(status("contradictory"), DiscoveryStatus::Invalid);
    assert_eq!(status("vanished"), DiscoveryStatus::Unavailable);
    assert_eq!(status("strict"), DiscoveryStatus::Discovered);

    // Malformed, contradictory, and vanished providers are excluded; the
    // placement is still structured and deterministic.
    let bundle = wasm_bundle(IsolationProfile::Strict);
    let (request, requirements, admission) = prepare(&bundle, SubmissionMode::Synchronous);
    let report = place(
        &pool.configs(),
        pool.policy(),
        &records,
        &requirements,
        &admission,
        None,
    );
    assert_eq!(report.compatible_providers, ["strict"]);
    let evaluation = |id: &str| {
        report
            .providers
            .iter()
            .find(|p| p.provider_id == id)
            .unwrap()
            .status
    };
    assert_eq!(
        evaluation("malformed"),
        EvaluationStatus::CapabilitiesInvalid
    );
    assert_eq!(
        evaluation("contradictory"),
        EvaluationStatus::CapabilitiesInvalid
    );
    assert_eq!(
        evaluation("vanished"),
        EvaluationStatus::ProviderUnavailable
    );

    // All providers incompatible: structured failure, nothing executes.
    let (_, ruby) = {
        let mut requirements = requirements.clone();
        requirements.runtime.kind = RuntimeKind::Ruby;
        ((), requirements)
    };
    let failed = place(
        &pool.configs(),
        pool.policy(),
        &records,
        &ruby,
        &admission,
        None,
    );
    assert_eq!(
        failed.failure.as_ref().unwrap().code,
        "no_compatible_provider"
    );
    let error = dispatch::execute(&pool, &failed, request.clone())
        .await
        .unwrap_err();
    assert_eq!(error.code, DispatchErrorCode::PlacementFailed);

    // Stale capabilities become unknown rather than assumed valid.
    let mut cache = CapabilityCache::default();
    let now = Utc::now();
    pool.capabilities(&mut cache, DiscoveryMode::Refresh, Some("strict"), now)
        .await;
    let later = now + chrono::Duration::seconds(pool.policy().capability_ttl_seconds as i64 + 1);
    let stale = pool
        .capabilities(
            &mut cache,
            DiscoveryMode::PreferCache,
            Some("strict"),
            later,
        )
        .await;
    assert_eq!(stale[0].status, DiscoveryStatus::Stale);
    let report = place(
        &pool.configs(),
        pool.policy(),
        &stale,
        &requirements,
        &admission,
        None,
    );
    assert_eq!(report.outcome, PlacementOutcome::PlacementFailed);
    assert_eq!(
        report.providers[0].status,
        EvaluationStatus::CapabilitiesUnknown
    );
    let fresh = pool
        .capabilities(&mut cache, DiscoveryMode::PreferCache, Some("strict"), now)
        .await;
    assert_eq!(fresh[0].status, DiscoveryStatus::Cached);

    // Provider unavailable after selection: the error is distinct from
    // incompatibility, and the workload is not redirected.
    let report = place(
        &pool.configs(),
        pool.policy(),
        &records,
        &requirements,
        &admission,
        None,
    );
    assert_eq!(report.selected.as_ref().unwrap().provider_id, "strict");
    matrix.servers[1].handle.abort();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let error = dispatch::execute(&pool, &report, request)
        .await
        .unwrap_err();
    assert_eq!(error.code, DispatchErrorCode::ProviderUnavailable);
    assert_eq!(error.provider_id.as_deref(), Some("strict"));
    assert_eq!(error.placement_id, report.placement_id);
    assert!(!error.retried);
    assert_eq!(matrix.local_executions.load(Ordering::SeqCst), 0);

    malformed_handle.abort();
    contradictory_handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restricted_provider_enforces_what_it_withholds() {
    let matrix = matrix().await;
    // Bypass placement and send strict WASM straight to the process-only
    // provider: it must refuse rather than execute weaker.
    let bundle = wasm_bundle(IsolationProfile::Strict);
    let request = ProviderRequest::bundle(bundle.to_bytes().unwrap());
    let error = RemoteProvider::new(matrix.servers[0].endpoint.clone())
        .execute(request)
        .await
        .unwrap_err();
    // Refused at admission, with the refusal attributed to capability.
    assert_eq!(
        error.kind,
        compute_provider::ProviderErrorKind::AdmissionDenied
    );
    let decision = error.admission.expect("denials carry evidence");
    assert!(!decision.admitted);
    assert!(decision.has(compute_policy::ReasonKind::Capability));
    assert!(!decision.has(compute_policy::ReasonKind::Policy));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_discovery_and_placement_is_deterministic() {
    let matrix = matrix().await;
    let bundle = wasm_bundle(IsolationProfile::Strict);
    let (_, requirements, admission) = prepare(&bundle, SubmissionMode::Synchronous);
    let first = place(
        &matrix.pool.configs(),
        matrix.pool.policy(),
        &discover(&matrix.pool).await,
        &requirements,
        &admission,
        None,
    );
    for _ in 0..3 {
        let again = place(
            &matrix.pool.configs(),
            matrix.pool.policy(),
            &discover(&matrix.pool).await,
            &requirements,
            &admission,
            None,
        );
        assert_eq!(again.placement_id, first.placement_id);
        assert_eq!(
            again
                .selected
                .as_ref()
                .map(|selected| &selected.provider_id),
            first
                .selected
                .as_ref()
                .map(|selected| &selected.provider_id)
        );
        assert_eq!(again.explanation, first.explanation);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_denied_providers_are_excluded_and_never_execute() {
    let locked_policy = compute_policy::Policy::from_json(
        br#"{"version": 1, "name": "locked", "allowed_runtimes": ["shell"]}"#,
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let jobs = tempfile::tempdir().unwrap();
    let locked = Arc::new(
        LocalProvider::with_identity(ProviderIdentity::Remote {
            id: endpoint.clone(),
            endpoint: endpoint.clone(),
        })
        .with_execution_policy(Some(locked_policy.clone()))
        .with_runtime_catalog(catalog()),
    );
    let mut config = ServerConfig::local(endpoint.clone());
    config.provider = locked.clone();
    config.job_store = jobs.path().to_path_buf();
    let handle = tokio::spawn(async move {
        let _ = compute_provider::serve_listener(listener, config).await;
    });
    let open = serve(ProviderPolicy::default()).await;

    let mut pool = ProviderPool::new(PoolPolicy::default());
    pool.add_remote(
        "locked",
        remote(&endpoint, 100),
        Arc::new(RemoteProvider::new(endpoint.clone())),
    )
    .unwrap();
    pool.add_remote(
        "open",
        remote(&open.endpoint, 10),
        Arc::new(RemoteProvider::new(open.endpoint.clone())),
    )
    .unwrap();
    let records = discover(&pool).await;
    let locked_record = records.iter().find(|r| r.provider_id == "locked").unwrap();
    assert_eq!(
        locked_record
            .descriptor
            .as_ref()
            .unwrap()
            .policy
            .as_ref()
            .map(|p| p.policy_id()),
        Some(locked_policy.policy_id())
    );

    let bundle = wasm_bundle(IsolationProfile::Process);
    let (request, requirements, admission) = prepare(&bundle, SubmissionMode::Synchronous);
    let report = place(
        &pool.configs(),
        pool.policy(),
        &records,
        &requirements,
        &admission,
        None,
    );
    let locked_eval = report
        .providers
        .iter()
        .find(|p| p.provider_id == "locked")
        .unwrap();
    assert_eq!(locked_eval.status, EvaluationStatus::PolicyDenied);
    assert!(locked_eval.reasons.is_empty(), "capable");
    assert_eq!(
        locked_eval.admission.as_ref().unwrap().codes(),
        ["runtime_denied"]
    );
    assert_eq!(report.selected.as_ref().unwrap().provider_id, "open");
    let response = dispatch::execute(&pool, &report, request.clone())
        .await
        .unwrap();
    report
        .verify_receipt(response.result.receipt.as_ref().unwrap())
        .unwrap();

    // Explicit selection of the denied provider fails without execution.
    let explicit = place(
        &pool.configs(),
        pool.policy(),
        &records,
        &requirements,
        &admission,
        Some("locked"),
    );
    assert_eq!(
        explicit.failure.as_ref().unwrap().code,
        "explicit_provider_denied"
    );
    let error = dispatch::execute(&pool, &explicit, request.clone())
        .await
        .unwrap_err();
    assert_eq!(error.code, DispatchErrorCode::PlacementFailed);

    // Even bypassing placement, the provider's own admission refuses, with
    // evidence, before any runtime starts.
    let bypass = RemoteProvider::new(endpoint.clone())
        .execute(request)
        .await
        .unwrap_err();
    assert_eq!(
        bypass.kind,
        compute_provider::ProviderErrorKind::AdmissionDenied
    );
    assert_eq!(bypass.admission.unwrap().codes(), ["runtime_denied"]);
    assert_eq!(locked.executions_started(), 0);
    handle.abort();
}
