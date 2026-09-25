use std::collections::BTreeMap;
use std::fs;
use std::net::TcpListener as StdTcpListener;
use std::sync::Arc;
use std::time::Duration;

use compute_core::{
    IsolationRequirement, NetworkPolicy, ResourceLimits, RuntimeKind, WORKLOAD_SPEC_VERSION,
    WorkloadBundle, WorkloadOutput, WorkloadSpec,
};
use compute_provider::{
    ComputeProvider, LocalProvider, ProviderAuthorizer, ProviderError, ProviderErrorKind,
    ProviderOperation, ProviderRequest, RemoteProvider, ServerConfig,
};

struct DenyAll;

#[async_trait::async_trait]
impl ProviderAuthorizer for DenyAll {
    async fn authorize(&self, _: ProviderOperation, _: Option<&str>) -> Result<(), ProviderError> {
        Err(ProviderError::new(
            ProviderErrorKind::Unauthorized,
            "request denied",
        ))
    }
}

fn fixture() -> (tempfile::TempDir, Vec<u8>) {
    fixture_with_script(
        b"printf 'provider-ok'; printf 'artifact-ok' > \"$COMPUTE_OUTPUT_DIR/result.txt\"",
    )
}

fn fixture_with_script(script: &[u8]) -> (tempfile::TempDir, Vec<u8>) {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("main.sh"), script).unwrap();
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
    let bytes = WorkloadBundle::create_from(spec, root.path())
        .unwrap()
        .to_bytes()
        .unwrap();
    (root, bytes)
}

async fn wait_for_terminal(client: &RemoteProvider, job_id: &str) -> compute_core::ExecutionJob {
    loop {
        let status = client.job_status(job_id).await.unwrap();
        if status.status.is_terminal() {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn local_provider_hashes_plans_and_binds_receipts() {
    let (_root, bytes) = fixture();
    let provider = LocalProvider::new();
    let request = ProviderRequest::bundle(bytes);
    let mut correlated = request.clone();
    correlated.execution.execution_request_id = Some("correlation-only".into());
    assert_eq!(
        request.request_hash().unwrap(),
        correlated.request_hash().unwrap()
    );
    let first = provider.inspect(request.clone()).await.unwrap();
    let second = provider.inspect(request.clone()).await.unwrap();
    assert_eq!(first.request_hash, second.request_hash);
    let response = provider.execute(request).await.unwrap();
    assert_eq!(response.result.stdout.text, "provider-ok");
    assert_eq!(
        response.result.provider.as_ref(),
        Some(&provider.identity())
    );
    let receipt = response.result.receipt.unwrap();
    assert_eq!(receipt.provider.as_ref(), Some(&provider.identity()));
    receipt.verify().unwrap();
}

#[tokio::test]
async fn modified_bundle_fails_before_execution() {
    let (_root, mut bytes) = fixture();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    let error = LocalProvider::new()
        .inspect(ProviderRequest::bundle(bytes))
        .await
        .unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::ArtifactInvalid);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_transport_is_semantically_equivalent() {
    let (_root, bytes) = fixture();
    let socket = StdTcpListener::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    drop(socket);
    let endpoint = format!("http://{addr}");
    let server_endpoint = endpoint.clone();
    let job_store = tempfile::tempdir().unwrap();
    let job_store_path = job_store.path().to_path_buf();
    let server = tokio::spawn(async move {
        let mut config = ServerConfig::local(server_endpoint);
        config.job_store = job_store_path;
        let _ = compute_provider::serve(addr, config).await;
    });
    tokio::time::sleep(Duration::from_millis(25)).await;

    let request = ProviderRequest::bundle(bytes);
    let local = LocalProvider::new().execute(request.clone()).await.unwrap();
    let remote_provider = RemoteProvider::new(endpoint);
    assert!(remote_provider.health().await.unwrap().healthy);
    let remote = remote_provider.execute(request).await.unwrap();
    assert_eq!(local.result.stdout, remote.result.stdout);
    assert_eq!(local.result.stderr, remote.result.stderr);
    assert_eq!(local.result.exit_code, remote.result.exit_code);
    assert_eq!(local.result.status, remote.result.status);
    assert_ne!(local.result.provider, remote.result.provider);
    remote.result.receipt.unwrap().verify().unwrap();
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_authorization_is_injectable_and_fails_closed() {
    let socket = StdTcpListener::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    drop(socket);
    let endpoint = format!("http://{addr}");
    let server_endpoint = endpoint.clone();
    let job_store = tempfile::tempdir().unwrap();
    let job_store_path = job_store.path().to_path_buf();
    let server = tokio::spawn(async move {
        let mut config = ServerConfig::local(server_endpoint);
        config.authorizer = Arc::new(DenyAll);
        config.job_store = job_store_path;
        let _ = compute_provider::serve(addr, config).await;
    });
    tokio::time::sleep(Duration::from_millis(25)).await;
    let error = RemoteProvider::new(endpoint).health().await.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Unauthorized);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn durable_jobs_are_idempotent_owned_verifiable_and_restart_safe() {
    let (_root, bytes) = fixture();
    let store = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let endpoint = format!("http://{addr}");
    let mut config = ServerConfig::local(endpoint.clone());
    config.job_store = store.path().to_path_buf();
    config.max_concurrent_jobs = 1;
    let server = tokio::spawn(async move {
        let _ = compute_provider::serve_listener(listener, config).await;
    });

    let client = RemoteProvider::new(endpoint.clone()).with_bearer_token("owner-a");
    let malformed = client.job_status("../../etc/passwd").await.unwrap_err();
    assert_eq!(malformed.kind, ProviderErrorKind::UnknownJob);
    let mut request = ProviderRequest::bundle(bytes);
    request.execution.execution_request_id = Some("request-one".into());
    let submitted = client
        .submit(request.clone(), Some("stable-key"))
        .await
        .unwrap();
    let duplicate = client
        .submit(request.clone(), Some("stable-key"))
        .await
        .unwrap();
    assert_eq!(submitted.job_id, duplicate.job_id);
    let mut different = request.clone();
    different.expected.distribution_id = Some(compute_core::sha256_identity(b"different"));
    let conflict = client
        .submit(different, Some("stable-key"))
        .await
        .unwrap_err();
    assert_eq!(conflict.kind, ProviderErrorKind::IdempotencyConflict);

    let foreign = RemoteProvider::new(endpoint).with_bearer_token("owner-b");
    let denied = foreign.job_status(&submitted.job_id.0).await.unwrap_err();
    assert_eq!(denied.kind, ProviderErrorKind::Unauthorized);

    let terminal = loop {
        let status = client.job_status(&submitted.job_id.0).await.unwrap();
        if status.status.is_terminal() {
            break status;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(terminal.status, compute_core::JobStatus::Succeeded);
    let result = client.job_result(&submitted.job_id.0).await.unwrap();
    assert_eq!(result.result.stdout.text, "provider-ok");
    let receipt = client.job_receipt(&submitted.job_id.0).await.unwrap();
    receipt.receipt.verify().unwrap();
    let artifacts = client.job_artifacts(&submitted.job_id.0).await.unwrap();
    assert_eq!(artifacts.artifacts.len(), 1);
    assert_eq!(artifacts.artifacts[0].data, b"artifact-ok");
    let events = client.job_events(&submitted.job_id.0).await.unwrap();
    assert!(
        events
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence)
    );
    assert_eq!(
        events.last().map(|event| event.event_type.as_str()),
        Some("terminal")
    );

    server.abort();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let restart_addr = listener.local_addr().unwrap();
    let restart_endpoint = format!("http://{restart_addr}");
    let mut restart_config = ServerConfig::local(restart_endpoint.clone());
    restart_config.job_store = store.path().to_path_buf();
    let restarted = tokio::spawn(async move {
        let _ = compute_provider::serve_listener(listener, restart_config).await;
    });
    let restarted_client = RemoteProvider::new(restart_endpoint).with_bearer_token("owner-a");
    let recovered = restarted_client
        .job_status(&submitted.job_id.0)
        .await
        .unwrap();
    assert_eq!(recovered.status, compute_core::JobStatus::Succeeded);
    restarted_client
        .job_receipt(&submitted.job_id.0)
        .await
        .unwrap();

    let request_path = store.path().join(&submitted.job_id.0).join("request.json");
    let original_request = fs::read(&request_path).unwrap();
    let mut modified_request: serde_json::Value =
        serde_json::from_slice(&original_request).unwrap();
    modified_request["request"]["execution"]["isolation"] = serde_json::json!("strict");
    fs::write(
        &request_path,
        serde_json::to_vec_pretty(&modified_request).unwrap(),
    )
    .unwrap();
    let tampered = restarted_client
        .job_status(&submitted.job_id.0)
        .await
        .unwrap_err();
    assert_eq!(tampered.kind, ProviderErrorKind::EvidenceInvalid);
    fs::write(&request_path, original_request).unwrap();

    let result_path = store.path().join(&submitted.job_id.0).join("result.json");
    let original = fs::read(&result_path).unwrap();
    let mut modified: serde_json::Value = serde_json::from_slice(&original).unwrap();
    modified["result"]["stdout"]["text"] = serde_json::json!("tampered");
    fs::write(&result_path, serde_json::to_vec_pretty(&modified).unwrap()).unwrap();
    let tampered = restarted_client
        .job_result(&submitted.job_id.0)
        .await
        .unwrap_err();
    assert_eq!(tampered.kind, ProviderErrorKind::EvidenceInvalid);
    fs::write(&result_path, original).unwrap();

    let receipt_path = store.path().join(&submitted.job_id.0).join("receipt.json");
    let original_receipt = fs::read(&receipt_path).unwrap();
    let mut modified_receipt: serde_json::Value =
        serde_json::from_slice(&original_receipt).unwrap();
    modified_receipt["receipt"]["receipt_hash"] = serde_json::json!(
        "sha256:0000000000000000000000000000000000000000000000000000000000000000"
    );
    fs::write(
        &receipt_path,
        serde_json::to_vec_pretty(&modified_receipt).unwrap(),
    )
    .unwrap();
    let tampered = restarted_client
        .job_receipt(&submitted.job_id.0)
        .await
        .unwrap_err();
    assert_eq!(tampered.kind, ProviderErrorKind::EvidenceInvalid);
    fs::write(&receipt_path, original_receipt).unwrap();

    let digest = artifacts.artifacts[0].digest.trim_start_matches("sha256:");
    fs::write(
        store
            .path()
            .join(&submitted.job_id.0)
            .join("artifacts")
            .join(digest),
        b"tampered",
    )
    .unwrap();
    let tampered = restarted_client
        .job_artifacts(&submitted.job_id.0)
        .await
        .unwrap_err();
    assert_eq!(tampered.kind, ProviderErrorKind::EvidenceInvalid);
    restarted.abort();

    let job_directory = store.path().join(&submitted.job_id.0);
    let status_path = job_directory.join("status.json");
    let mut interrupted: serde_json::Value =
        serde_json::from_slice(&fs::read(&status_path).unwrap()).unwrap();
    interrupted["status"] = serde_json::json!("running");
    interrupted["execution_id"] = serde_json::Value::Null;
    interrupted["result_digest"] = serde_json::Value::Null;
    fs::write(
        &status_path,
        serde_json::to_vec_pretty(&interrupted).unwrap(),
    )
    .unwrap();
    let _ = fs::remove_file(job_directory.join("result.json"));
    let _ = fs::remove_file(job_directory.join("receipt.json"));
    let _ = fs::remove_dir_all(job_directory.join("artifacts"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let mut config = ServerConfig::local(endpoint.clone());
    config.job_store = store.path().to_path_buf();
    let recovery_server = tokio::spawn(async move {
        let _ = compute_provider::serve_listener(listener, config).await;
    });
    let recovery_client = RemoteProvider::new(endpoint).with_bearer_token("owner-a");
    let interrupted = recovery_client
        .job_status(&submitted.job_id.0)
        .await
        .unwrap();
    assert_eq!(interrupted.status, compute_core::JobStatus::Failed);
    assert!(
        interrupted
            .failure
            .as_deref()
            .is_some_and(|failure| failure.contains("provider_interrupted"))
    );
    recovery_server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_jobs_cancel_truthfully_and_terminal_jobs_expire() {
    let (_root, bytes) = fixture_with_script(
        b"sleep 0.25; printf 'provider-ok'; printf 'artifact-ok' > \"$COMPUTE_OUTPUT_DIR/result.txt\"",
    );
    let store = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let mut config = ServerConfig::local(endpoint.clone());
    config.job_store = store.path().to_path_buf();
    config.max_concurrent_jobs = 1;
    config.job_retention = Duration::from_millis(150);
    let server = tokio::spawn(async move {
        let _ = compute_provider::serve_listener(listener, config).await;
    });
    let client = RemoteProvider::new(endpoint.clone()).with_bearer_token("queue-owner");
    let first = client
        .submit(ProviderRequest::bundle(bytes.clone()), None)
        .await
        .unwrap();
    loop {
        if client.job_status(&first.job_id.0).await.unwrap().status
            == compute_core::JobStatus::Running
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let running_cancel = client.cancel_job(&first.job_id.0).await.unwrap();
    assert_eq!(running_cancel.status, compute_core::JobStatus::Running);
    assert!(running_cancel.cancellation.requested);
    assert!(!running_cancel.cancellation.effective);
    assert_eq!(
        running_cancel.cancellation.phase.as_deref(),
        Some("execution_not_interruptible")
    );
    let second = client
        .submit(ProviderRequest::bundle(bytes), None)
        .await
        .unwrap();
    loop {
        if client.job_status(&second.job_id.0).await.unwrap().status
            == compute_core::JobStatus::Queued
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let cancelled = client.cancel_job(&second.job_id.0).await.unwrap();
    assert_eq!(cancelled.status, compute_core::JobStatus::Cancelled);
    assert!(cancelled.cancellation.requested);
    assert!(cancelled.cancellation.effective);
    assert_eq!(
        cancelled.cancellation.phase.as_deref(),
        Some("before_execution")
    );

    loop {
        let status = client.job_status(&first.job_id.0).await.unwrap();
        if status.status.is_terminal() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(175)).await;
    let expired = client.job_status(&first.job_id.0).await.unwrap_err();
    assert_eq!(expired.kind, ProviderErrorKind::JobExpired);
    let foreign = RemoteProvider::new(endpoint).with_bearer_token("foreign-owner");
    let hidden = foreign.job_status(&first.job_id.0).await.unwrap_err();
    assert_eq!(hidden.kind, ProviderErrorKind::Unauthorized);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn asynchronous_failures_and_timeouts_agree_with_execution_evidence() {
    let store = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let mut config = ServerConfig::local(endpoint.clone());
    config.job_store = store.path().to_path_buf();
    let server = tokio::spawn(async move {
        let _ = compute_provider::serve_listener(listener, config).await;
    });
    let client = RemoteProvider::new(endpoint);

    let (_root, failed_bytes) = fixture_with_script(b"exit 7");
    let failed = client
        .submit(ProviderRequest::bundle(failed_bytes), None)
        .await
        .unwrap();
    let failed_status = wait_for_terminal(&client, &failed.job_id.0).await;
    assert_eq!(failed_status.status, compute_core::JobStatus::Failed);
    let failed_result = client.job_result(&failed.job_id.0).await.unwrap();
    assert_eq!(failed_result.status, compute_core::JobStatus::Failed);
    assert_eq!(
        failed_result.result.status,
        compute_core::ExecutionStatus::Completed
    );
    assert_eq!(failed_result.result.exit_code, Some(7));
    failed_result.result.receipt.unwrap().verify().unwrap();

    let (_root, timeout_bytes) = fixture_with_script(b"sleep 1");
    let mut timeout_bundle = WorkloadBundle::from_bytes(&timeout_bytes).unwrap();
    timeout_bundle.workload.resources.wall_time = Some(Duration::from_millis(30));
    let timed_out = client
        .submit(
            ProviderRequest::bundle(timeout_bundle.to_bytes().unwrap()),
            None,
        )
        .await
        .unwrap();
    let timeout_status = wait_for_terminal(&client, &timed_out.job_id.0).await;
    assert_eq!(timeout_status.status, compute_core::JobStatus::TimedOut);
    let timeout_result = client.job_result(&timed_out.job_id.0).await.unwrap();
    assert_eq!(timeout_result.status, compute_core::JobStatus::TimedOut);
    assert_eq!(
        timeout_result.result.status,
        compute_core::ExecutionStatus::TimedOut
    );
    timeout_result.result.receipt.unwrap().verify().unwrap();
    server.abort();
}
