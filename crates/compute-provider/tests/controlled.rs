use std::collections::BTreeMap;
use std::time::Duration;

use compute_core::{
    ExecutionControl, ExecutionStatus, IsolationRequirement, NetworkPolicy, ReceiptScope,
    ResourceLimits, RuntimeKind, WORKLOAD_SPEC_VERSION, WorkloadBundle, WorkloadSpec,
};
use compute_provider::{ComputeProvider, LocalProvider, ProviderRequest};

fn service(script: &str) -> Vec<u8> {
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
        resources: ResourceLimits {
            stdout_bytes: Some(4096),
            stderr_bytes: Some(4096),
            ..ResourceLimits::default()
        },
        network: NetworkPolicy::Network,
        isolation: IsolationRequirement::default(),
        dependencies: None,
    };
    WorkloadBundle::create_from(spec, root.path())
        .unwrap()
        .to_bytes()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn controlled_services_log_live_cancel_and_bind_scope() {
    let logs = tempfile::tempdir().unwrap();
    let provider = std::sync::Arc::new(LocalProvider::new());
    let mut request = ProviderRequest::bundle(service(
        "echo started; sh -c 'while :; do sleep 0.1; done' & while :; do echo tick; sleep 0.1; done",
    ));
    request.execution.scope = Some(ReceiptScope {
        environment_id: "env_test".into(),
        environment: "preprod".into(),
        project_id: "prj_test".into(),
        project: "authboundry".into(),
        revision: "abc123".into(),
        workload_id: "wl_test".into(),
        workload: "api".into(),
        workload_kind: "service".into(),
    });
    let admission = provider.admit(request.clone()).await.unwrap();
    assert!(admission.decision.admitted);
    let control = ExecutionControl::new().with_log_directory(logs.path());
    let running = {
        let provider = provider.clone();
        let control = control.clone();
        tokio::spawn(async move {
            provider
                .execute_controlled(request, admission, &control)
                .await
        })
    };
    // Output is observable while the service runs.
    let stdout = logs.path().join("stdout.log");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if std::fs::read_to_string(&stdout).is_ok_and(|text| text.contains("tick")) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "no live output");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!running.is_finished(), "a service keeps running");
    control.cancel();
    let response = tokio::time::timeout(Duration::from_secs(10), running)
        .await
        .expect("cancellation stops the service")
        .unwrap()
        .unwrap();
    assert_eq!(response.result.status, ExecutionStatus::Cancelled);
    assert_eq!(
        response.result.error.as_ref().unwrap().kind,
        compute_core::ExecutionErrorKind::Cancelled
    );
    assert!(response.result.stdout.text.starts_with("started"));
    let receipt = response.result.receipt.unwrap();
    receipt.verify().unwrap();
    let scope = receipt.scope.unwrap();
    assert_eq!(scope.environment, "preprod");
    assert_eq!(scope.workload_kind, "service");
    assert_eq!(receipt.admission_status.as_deref(), Some("admitted"));

    // The whole process group is gone: the log stops growing.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let size = std::fs::metadata(&stdout).unwrap().len();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(std::fs::metadata(&stdout).unwrap().len(), size);
}
