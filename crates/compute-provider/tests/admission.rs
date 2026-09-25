//! Remote admission: client → provider → capability → admission →
//! execution → job → receipt, and denials that never reach a runtime.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use compute_core::{
    DependencyCapsule, IsolationRequirement, NetworkPolicy, PlatformIdentity, ProviderIdentity,
    ResourceLimits, RuntimeKind, WORKLOAD_SPEC_VERSION, WorkloadBundle, WorkloadDependencies,
    WorkloadSpec,
};
use compute_provider::{
    ComputeProvider, LocalProvider, Policy, ProviderErrorKind, ProviderPolicy, ProviderRequest,
    RemoteProvider, ServerConfig,
};

fn shell(script: &str, edit: impl FnOnce(&mut WorkloadSpec)) -> Vec<u8> {
    shell_with_capsule(script, None, edit)
}

fn shell_with_capsule(
    script: &str,
    capsule: Option<DependencyCapsule>,
    edit: impl FnOnce(&mut WorkloadSpec),
) -> Vec<u8> {
    workload(RuntimeKind::Shell, "main.sh", script, capsule, edit)
}

fn workload(
    runtime: RuntimeKind,
    entrypoint: &str,
    script: &str,
    capsule: Option<DependencyCapsule>,
    edit: impl FnOnce(&mut WorkloadSpec),
) -> Vec<u8> {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(entrypoint), script).unwrap();
    let mut spec = WorkloadSpec {
        version: WORKLOAD_SPEC_VERSION.into(),
        runtime,
        runtime_version: None,
        architecture: None,
        entrypoint: entrypoint.into(),
        args: vec![],
        env: BTreeMap::new(),
        inputs: vec![],
        outputs: vec![],
        resources: ResourceLimits::default(),
        network: NetworkPolicy::Network,
        isolation: IsolationRequirement::default(),
        dependencies: capsule.as_ref().map(|capsule| WorkloadDependencies {
            capsule: capsule.capsule_id().unwrap(),
        }),
    };
    edit(&mut spec);
    WorkloadBundle::create_from_with_capsule(spec, root.path(), capsule)
        .unwrap()
        .to_bytes()
        .unwrap()
}

fn policy(json: serde_json::Value) -> Policy {
    Policy::from_json(json.to_string().as_bytes()).unwrap()
}

struct Server {
    endpoint: String,
    provider: Arc<LocalProvider>,
    handle: tokio::task::JoinHandle<()>,
    _jobs: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn serve(execution_policy: Option<Policy>, max_concurrent_jobs: usize) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let jobs = tempfile::tempdir().unwrap();
    let provider = Arc::new(
        LocalProvider::with_identity(ProviderIdentity::Remote {
            id: endpoint.clone(),
            endpoint: endpoint.clone(),
        })
        .with_policy(ProviderPolicy::default())
        .with_execution_policy(execution_policy),
    );
    let mut config = ServerConfig::local(endpoint.clone());
    config.provider = provider.clone();
    config.job_store = jobs.path().to_path_buf();
    config.max_concurrent_jobs = max_concurrent_jobs;
    let handle = tokio::spawn(async move {
        let _ = compute_provider::serve_listener(listener, config).await;
    });
    Server {
        endpoint,
        provider,
        handle,
        _jobs: jobs,
    }
}

async fn wait(client: &RemoteProvider, job_id: &str) -> compute_core::ExecutionJob {
    loop {
        let job = client.job_status(job_id).await.unwrap();
        if job.status.is_terminal() {
            return job;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admitted_execution_binds_policy_and_admission_everywhere() {
    let production = policy(serde_json::json!({
        "version": 1, "name": "production-policy",
        "allowed_runtimes": ["shell", "wasm"],
        "limits": {"max_timeout_ms": 60000}
    }));
    let server = serve(Some(production.clone()), 2).await;
    let client = RemoteProvider::new(server.endpoint.clone());
    let bytes = shell("printf admitted", |spec| {
        spec.resources.wall_time = Some(Duration::from_secs(10));
    });
    let request = ProviderRequest::bundle(bytes);

    let admission = client.admit(request.clone()).await.unwrap();
    assert!(
        admission.decision.admitted,
        "{:?}",
        admission.decision.reasons
    );
    assert_eq!(admission.decision.policy_id, admission.policy.policy_id);
    assert!(
        admission
            .policy
            .sources
            .iter()
            .any(|source| source.policy_id == production.policy_id())
    );

    let response = client.execute(request.clone()).await.unwrap();
    assert_eq!(response.result.stdout.text, "admitted");
    let executed = response.result.admission.clone().unwrap();
    assert_eq!(executed.admission_id, admission.decision.admission_id);
    assert_eq!(executed.policy_id, admission.policy.policy_id);
    assert_eq!(executed.admission_status, "admitted");
    let receipt = response.result.receipt.unwrap();
    receipt.verify().unwrap();
    assert_eq!(
        receipt.policy_id.as_deref(),
        Some(executed.policy_id.as_str())
    );
    assert_eq!(
        receipt.admission_id.as_deref(),
        Some(executed.admission_id.as_str())
    );
    assert_eq!(receipt.admission_status.as_deref(), Some("admitted"));

    // Jobs persist the admission and execute under it.
    let submission = client.submit(request, None).await.unwrap();
    let job = wait(&client, &submission.job_id.0).await;
    assert_eq!(job.status, compute_core::JobStatus::Succeeded);
    assert_eq!(job.admission.as_ref(), Some(&executed));
    let job_receipt = client
        .job_receipt(&submission.job_id.0)
        .await
        .unwrap()
        .receipt;
    assert_eq!(job_receipt.admission_id, receipt.admission_id);
    assert_eq!(job_receipt.policy_id, receipt.policy_id);

    // Tampered evidence fails verification.
    let mut tampered = job_receipt.clone();
    tampered.admission_status = Some("denied".into());
    tampered.seal().unwrap();
    assert!(tampered.verify().is_err());
    let mut partial = job_receipt;
    partial.admission_id = None;
    partial.seal().unwrap();
    assert!(partial.verify().is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_denial_is_explained_and_never_reaches_a_runtime() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("placed.py"), "VALUE = 'packaged'\n").unwrap();
    let capsule = DependencyCapsule::create(
        root.path(),
        RuntimeKind::Python,
        Some("3.13.1".into()),
        PlatformIdentity::current(),
        vec![],
        None,
    )
    .unwrap();
    let other = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
    let cases: Vec<(&str, serde_json::Value, Vec<u8>)> = vec![
        (
            "runtime_denied",
            serde_json::json!({"version": 1, "allowed_runtimes": ["wasm"]}),
            shell("printf ran", |_| {}),
        ),
        (
            "network_denied",
            serde_json::json!({"version": 1, "allowed_networks": ["none"]}),
            shell("printf ran", |_| {}),
        ),
        (
            "isolation_below_minimum",
            serde_json::json!({"version": 1, "minimum_isolation": "strict"}),
            shell("printf ran", |_| {}),
        ),
        (
            "timeout_unbounded",
            serde_json::json!({"version": 1, "limits": {"max_timeout_ms": 1000}}),
            shell("printf ran", |_| {}),
        ),
        (
            "memory_unbounded",
            serde_json::json!({"version": 1, "limits": {"max_memory_bytes": 1048576}}),
            shell("printf ran", |_| {}),
        ),
        (
            "distribution_denied",
            serde_json::json!({"version": 1, "allowed_distributions": [other]}),
            shell("printf ran", |_| {}),
        ),
        (
            "dependency_denied",
            serde_json::json!({"version": 1, "allowed_dependencies": [other]}),
            workload(
                RuntimeKind::Python,
                "main.py",
                "print('ran')",
                Some(capsule.clone()),
                |_| {},
            ),
        ),
    ];
    for (code, document, bytes) in cases {
        let server = serve(Some(policy(document)), 2).await;
        let client = RemoteProvider::new(server.endpoint.clone());
        let request = ProviderRequest::bundle(bytes);

        let decision = client.admit(request.clone()).await.unwrap().decision;
        assert!(!decision.admitted, "{code}");
        assert!(
            decision.codes().contains(&code),
            "{code}: {:?}",
            decision.codes()
        );

        let error = client.execute(request.clone()).await.unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::AdmissionDenied, "{code}");
        let evidence = error.admission.expect("denial carries evidence");
        assert_eq!(evidence.admission_id, decision.admission_id, "{code}");

        let submitted = client.submit(request, None).await.unwrap();
        let job = wait(&client, &submitted.job_id.0).await;
        assert_eq!(job.status, compute_core::JobStatus::Rejected, "{code}: job");
        assert!(
            job.failure
                .as_deref()
                .is_some_and(|failure| failure.contains("admission_denied")),
            "{code}: {:?}",
            job.failure
        );

        assert_eq!(
            server.provider.executions_started(),
            0,
            "{code}: reached a runtime"
        );
        let health = client.health().await.unwrap();
        assert_eq!(health.executions_started, Some(0));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn caller_policy_can_only_restrict() {
    let server = serve(None, 2).await;
    let client = RemoteProvider::new(server.endpoint.clone());
    let mut request = ProviderRequest::bundle(shell("printf ran", |_| {}));
    request.execution.policy = Some(policy(serde_json::json!({
        "version": 1, "allowed_networks": ["none"]
    })));
    let error = client.execute(request.clone()).await.unwrap_err();
    assert_eq!(error.admission.unwrap().codes(), ["network_denied"]);
    // A caller policy is part of the request identity.
    let mut plain = request.clone();
    plain.execution.policy = None;
    assert_ne!(
        plain.request_hash().unwrap(),
        request.request_hash().unwrap()
    );
    // An invalid caller policy is rejected before admission.
    let mut invalid = request;
    invalid.execution.policy = Some(Policy {
        version: 2,
        ..Policy::unrestricted()
    });
    assert_eq!(
        client.execute(invalid).await.unwrap_err().kind,
        ProviderErrorKind::PolicyRejected
    );
    assert_eq!(server.provider.executions_started(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_changes_never_mutate_an_admitted_execution() {
    let original = policy(
        serde_json::json!({"version": 1, "name": "original", "allowed_runtimes": ["shell"]}),
    );
    let replacement = policy(
        serde_json::json!({"version": 1, "name": "replacement", "allowed_runtimes": ["wasm"]}),
    );

    // Library level: an admission keeps its snapshot.
    let provider = LocalProvider::new().with_execution_policy(Some(original.clone()));
    let request = ProviderRequest::bundle(shell("printf ran", |_| {}));
    let admission = provider.admit(request.clone()).await.unwrap();
    assert!(admission.decision.admitted);
    provider.set_execution_policy(Some(replacement.clone()));
    let response = provider
        .execute_admitted(request.clone(), admission.clone())
        .await
        .unwrap();
    let receipt = response.result.receipt.unwrap();
    assert_eq!(
        receipt.policy_id.as_ref(),
        Some(&admission.policy.policy_id)
    );
    receipt.verify().unwrap();
    // A new execution evaluates the new policy.
    let denied = provider.execute(request.clone()).await.unwrap_err();
    let evidence = denied.admission.unwrap();
    assert_ne!(evidence.policy_id, admission.policy.policy_id);
    assert_eq!(evidence.codes(), ["runtime_denied"]);
    // Forged or altered decisions are refused.
    let mut forged = admission.clone();
    forged.decision.admission_id = format!("sha256:{}", "f".repeat(64));
    assert_eq!(
        provider
            .execute_admitted(request.clone(), forged)
            .await
            .unwrap_err()
            .kind,
        ProviderErrorKind::PolicyRejected
    );
    let mut swapped = admission.clone();
    swapped.policy.policy = replacement.clone();
    assert!(provider.execute_admitted(request, swapped).await.is_err());

    // Server level: admission happens only after an atomic reservation. A
    // capacity-waiting job has not been admitted yet and observes the policy
    // in force when it eventually reserves.
    let server = serve(Some(original.clone()), 1).await;
    let client = RemoteProvider::new(server.endpoint.clone());
    let blocker = client
        .submit(ProviderRequest::bundle(shell("sleep 1", |_| {})), None)
        .await
        .unwrap();
    let queued_request = ProviderRequest::bundle(shell("printf queued", |_| {}));
    let queued = client.submit(queued_request.clone(), None).await.unwrap();
    let waiting = client.job_status(&queued.job_id.0).await.unwrap();
    assert!(waiting.admission.is_none());
    server.provider.set_execution_policy(Some(replacement));
    let job = wait(&client, &queued.job_id.0).await;
    wait(&client, &blocker.job_id.0).await;
    assert_eq!(job.status, compute_core::JobStatus::Rejected);
    assert!(
        job.failure
            .as_deref()
            .is_some_and(|failure| failure.contains("admission_denied"))
    );
    // New submissions also evaluate the new policy after reservation.
    let submitted = client.submit(queued_request, None).await.unwrap();
    let denied = wait(&client, &submitted.job_id.0).await;
    assert_eq!(denied.status, compute_core::JobStatus::Rejected);
}
