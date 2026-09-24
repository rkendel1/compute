//! Policy and admission certification for `compute certify`.
//!
//! Proves on the assembled distribution that positive admission leads to
//! execution with admission evidence in the receipt, and that negative
//! admission leads to no execution at all: locally, remotely, for jobs,
//! and through a provider pool.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use compute_core::{
    DependencyCapsule, PlatformIdentity, ProviderIdentity, RuntimeKind, WorkloadBundle,
    WorkloadDependencies,
};
use compute_placement::{
    AdmissionContext, CapabilityCache, DiscoveryMode, DispatchErrorCode, EvaluationStatus,
    PoolPolicy, ProviderConfig, ProviderKind, ProviderPool, RequirementOptions, SubmissionMode,
    dispatch, place,
};
use compute_policy::{EffectivePolicy, ExecutionContract, Policy, PolicySourceKind, ReasonKind};
use compute_provider::{
    ComputeProvider, LocalProvider, ProviderErrorKind, ProviderRequest, RemoteProvider,
    ServerConfig,
};

pub struct PolicyCertification {
    pub checks: Vec<(&'static str, Result<String, String>)>,
}

fn policy(json: serde_json::Value) -> Result<Policy, String> {
    Policy::from_json(json.to_string().as_bytes()).map_err(|error| error.to_string())
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

async fn serve(execution_policy: Option<Policy>) -> Result<Server, String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| error.to_string())?;
    let endpoint = format!(
        "http://{}",
        listener.local_addr().map_err(|error| error.to_string())?
    );
    let jobs = tempfile::tempdir().map_err(|error| error.to_string())?;
    let provider = Arc::new(
        LocalProvider::with_identity(ProviderIdentity::Remote {
            id: endpoint.clone(),
            endpoint: endpoint.clone(),
        })
        .with_execution_policy(execution_policy),
    );
    let mut config = ServerConfig::local(endpoint.clone());
    config.provider = provider.clone();
    config.job_store = jobs.path().to_path_buf();
    let handle = tokio::spawn(async move {
        let _ = compute_provider::serve_listener(listener, config).await;
    });
    Ok(Server {
        endpoint,
        provider,
        handle,
        _jobs: jobs,
    })
}

/// A runtime the bundle does not use, for runtime denial.
fn other_runtime(kind: RuntimeKind) -> RuntimeKind {
    if kind == RuntimeKind::Wasm {
        RuntimeKind::Shell
    } else {
        RuntimeKind::Wasm
    }
}

pub async fn certify(bundle_path: &Path) -> PolicyCertification {
    let bundle = match WorkloadBundle::read(bundle_path) {
        Ok(bundle) => bundle,
        Err(error) => {
            let error = error.to_string();
            return PolicyCertification {
                checks: [
                    "policy",
                    "admission",
                    "placement_policy",
                    "remote_job_policy",
                    "receipt_policy",
                ]
                .into_iter()
                .map(|name| (name, Err(error.clone())))
                .collect(),
            };
        }
    };
    PolicyCertification {
        checks: vec![
            ("policy", certify_policy_model()),
            ("admission", certify_admission(&bundle).await),
            ("placement_policy", certify_placement(&bundle).await),
            ("remote_job_policy", certify_jobs(&bundle).await),
            ("receipt_policy", certify_receipts(&bundle).await),
        ],
    }
}

/// Static validation fails closed; identity and composition are exact.
fn certify_policy_model() -> Result<String, String> {
    for document in [
        r#"{"version": 2}"#,
        r#"{"version": 1, "allowed_runtimes": ["cobol"]}"#,
        r#"{"version": 1, "limits": {"max_memory_bytes": 0}}"#,
        r#"{"version": 1, "limits": {"max_timeout_ms": -5}}"#,
        r#"{"version": 1, "allowed_distributions": ["sha256:short"]}"#,
        r#"{"version": 1, "allowed_networks": ["none", "none"]}"#,
        r#"{"version": 1, "defaults": {"isolation": "process"}, "minimum_isolation": "strict"}"#,
        r#"{"version": 1, "rules": "anything"}"#,
    ] {
        if Policy::from_json(document.as_bytes()).is_ok() {
            return Err(format!("invalid policy was accepted: {document}"));
        }
    }
    let first = policy(serde_json::json!({"version": 1, "allowed_runtimes": ["wasm", "python"]}))?;
    let reordered =
        policy(serde_json::json!({"allowed_runtimes": ["python", "wasm"], "version": 1}))?;
    let changed = policy(serde_json::json!({"version": 1, "allowed_runtimes": ["python"]}))?;
    if first.policy_id() != reordered.policy_id() || first.policy_id() == changed.policy_id() {
        return Err("policy identity is not canonical".into());
    }
    let composed = EffectivePolicy::compose(&[
        (PolicySourceKind::Server, first.clone()),
        (PolicySourceKind::Explicit, changed.clone()),
    ]);
    if composed.policy.allowed_runtimes != Some(vec![RuntimeKind::Python]) {
        return Err("composition widened a restriction".into());
    }
    Ok("unknown versions, invalid names, zero and negative limits, malformed identities, duplicates, contradictions, and unknown fields were rejected; identity is canonical; composition is an intersection".into())
}

/// Positive admission executes; each negative dimension executes nothing.
async fn certify_admission(bundle: &WorkloadBundle) -> Result<String, String> {
    let kind = bundle.workload.runtime;
    let request = ProviderRequest::bundle(bundle.to_bytes().map_err(|e| e.to_string())?);
    let other = format!("sha256:{}", "0".repeat(64));
    let capsule_bundle = dependency_bundle()?;
    let cases = [
        (
            "runtime_denied",
            serde_json::json!({"version": 1, "allowed_runtimes": [other_runtime(kind)]}),
            &request,
        ),
        (
            "network_denied",
            serde_json::json!({"version": 1, "allowed_networks": [
                if bundle.workload.network == compute_core::NetworkPolicy::None { "network" } else { "none" }
            ]}),
            &request,
        ),
        (
            "isolation_below_minimum",
            serde_json::json!({"version": 1, "minimum_isolation": "strict"}),
            &request,
        ),
        (
            "memory_unbounded",
            serde_json::json!({"version": 1, "limits": {"max_memory_bytes": 1048576}}),
            &request,
        ),
        (
            "distribution_denied",
            serde_json::json!({"version": 1, "allowed_distributions": [other]}),
            &request,
        ),
        (
            "dependency_denied",
            serde_json::json!({"version": 1, "allowed_dependencies": [other]}),
            &capsule_bundle,
        ),
    ];
    for (code, document, request) in cases {
        let provider = LocalProvider::new().with_execution_policy(Some(policy(document)?));
        let error = provider
            .execute(request.clone())
            .await
            .err()
            .ok_or_else(|| format!("{code}: denied workload executed"))?;
        let evidence = error
            .admission
            .ok_or_else(|| format!("{code}: denial carried no evidence"))?;
        if error.kind != ProviderErrorKind::AdmissionDenied || !evidence.codes().contains(&code) {
            return Err(format!("{code}: unexpected denial {:?}", evidence.codes()));
        }
        if provider.executions_started() != 0 {
            return Err(format!("{code}: denied workload reached the runtime"));
        }
    }
    let provider = LocalProvider::new().with_execution_policy(Some(policy(serde_json::json!({
        "version": 1, "name": "certification-policy", "allowed_runtimes": [kind]
    }))?));
    let response = provider
        .execute(request)
        .await
        .map_err(|error| format!("admitted workload failed: {error}"))?;
    let receipt = response
        .result
        .receipt
        .ok_or("admitted execution omitted its receipt")?;
    receipt.verify().map_err(|error| error.to_string())?;
    if receipt.admission_status.as_deref() != Some("admitted")
        || receipt.admission_id != response.result.admission.map(|value| value.admission_id)
        || provider.executions_started() != 1
    {
        return Err("admitted execution evidence is inconsistent".into());
    }
    Ok("runtime, network, isolation, resource, distribution, and dependency denials executed nothing and carried evidence; the admitted execution's receipt binds its admission".into())
}

/// A small bundle with an embedded dependency capsule, for dependency
/// policy. It is only ever admitted or denied, never executed.
fn dependency_bundle() -> Result<ProviderRequest, String> {
    let root = tempfile::tempdir().map_err(|error| error.to_string())?;
    let payload = root.path().join("payload");
    std::fs::create_dir(&payload).map_err(|error| error.to_string())?;
    std::fs::write(payload.join("certified.py"), "VALUE = 1\n").map_err(|e| e.to_string())?;
    std::fs::write(root.path().join("main.py"), "import certified\n").map_err(|e| e.to_string())?;
    let capsule = DependencyCapsule::create(
        &payload,
        RuntimeKind::Python,
        Some("certification".into()),
        PlatformIdentity::current(),
        vec![],
        None,
    )
    .map_err(|error| error.to_string())?;
    let spec = compute_core::WorkloadSpec {
        version: "1".into(),
        runtime: RuntimeKind::Python,
        runtime_version: None,
        entrypoint: "main.py".into(),
        args: vec![],
        env: Default::default(),
        inputs: vec![],
        outputs: vec![],
        resources: Default::default(),
        network: compute_core::NetworkPolicy::Network,
        isolation: Default::default(),
        dependencies: Some(WorkloadDependencies {
            capsule: capsule.capsule_id().map_err(|error| error.to_string())?,
        }),
    };
    let bundle = WorkloadBundle::create_from_with_capsule(spec, root.path(), Some(capsule))
        .map_err(|error| error.to_string())?;
    Ok(ProviderRequest::bundle(
        bundle.to_bytes().map_err(|error| error.to_string())?,
    ))
}

fn remote(endpoint: &str, priority: i64) -> ProviderConfig {
    ProviderConfig {
        kind: ProviderKind::Remote,
        endpoint: Some(endpoint.into()),
        priority,
        token_env: None,
    }
}

/// Capable but policy-denied providers are excluded; explicit selection
/// never bypasses policy; the selected provider's admission reproduces.
async fn certify_placement(bundle: &WorkloadBundle) -> Result<String, String> {
    let kind = bundle.workload.runtime;
    let locked = serve(Some(policy(serde_json::json!({
        "version": 1, "name": "locked", "allowed_runtimes": [other_runtime(kind)]
    }))?))
    .await?;
    let open = serve(None).await?;
    let mut pool = ProviderPool::new(PoolPolicy::default());
    for (id, server, priority) in [("locked", &locked, 100), ("open", &open, 10)] {
        pool.add_remote(
            id,
            remote(&server.endpoint, priority),
            Arc::new(RemoteProvider::new(server.endpoint.clone())),
        )
        .map_err(|error| error.to_string())?;
    }
    let mut cache = CapabilityCache::default();
    let records = pool
        .capabilities(&mut cache, DiscoveryMode::Refresh, None, Utc::now())
        .await;
    let mut request = ProviderRequest::bundle(bundle.to_bytes().map_err(|e| e.to_string())?);
    request.expected.workload_id = Some(bundle.workload_id().map_err(|e| e.to_string())?);
    request.expected.bundle_id = Some(bundle.bundle_id().map_err(|e| e.to_string())?);
    let size = serde_json::to_vec(&request)
        .map_err(|e| e.to_string())?
        .len() as u64;
    let requirements = compute_placement::PlacementRequirements::from_bundle(
        bundle,
        size,
        SubmissionMode::Synchronous,
        &RequirementOptions::default(),
    )
    .map_err(|error| error.to_string())?;
    let context = AdmissionContext::new(
        &[],
        ExecutionContract::from_bundle(bundle, None).map_err(|error| error.to_string())?,
    );
    let report = place(
        &pool.configs(),
        pool.policy(),
        &records,
        &requirements,
        &context,
        None,
    );
    let locked_eval = report
        .providers
        .iter()
        .find(|provider| provider.provider_id == "locked")
        .ok_or("locked provider was not evaluated")?;
    if locked_eval.status != EvaluationStatus::PolicyDenied
        || !locked_eval.reasons.is_empty()
        || !locked_eval
            .admission
            .as_ref()
            .is_some_and(|decision| decision.has(ReasonKind::Policy))
    {
        return Err("capable, policy-denied provider was not reported as policy_denied".into());
    }
    if report
        .selected
        .as_ref()
        .map(|selected| selected.provider_id.as_str())
        != Some("open")
    {
        return Err("placement did not select the admitted provider".into());
    }
    let response = dispatch::execute(&pool, &report, request.clone())
        .await
        .map_err(|error| error.to_string())?;
    report.verify_receipt(response.result.receipt.as_ref().ok_or("no receipt")?)?;
    let explicit = place(
        &pool.configs(),
        pool.policy(),
        &records,
        &requirements,
        &context,
        Some("locked"),
    );
    match dispatch::execute(&pool, &explicit, request.clone()).await {
        Err(error) if error.code == DispatchErrorCode::PlacementFailed => {}
        _ => return Err("explicit selection bypassed policy".into()),
    }
    match RemoteProvider::new(locked.endpoint.clone())
        .execute(request)
        .await
    {
        Err(error) if error.kind == ProviderErrorKind::AdmissionDenied => {}
        _ => return Err("provider admission did not refuse a bypassing request".into()),
    }
    if locked.provider.executions_started() != 0 {
        return Err("policy-denied provider reached its runtime".into());
    }
    Ok("capable providers denied by policy were excluded, explicit selection never bypassed policy, and the executed receipt reproduced the placement's admission".into())
}

/// Jobs are created only from admitted decisions and persist them.
async fn certify_jobs(bundle: &WorkloadBundle) -> Result<String, String> {
    let kind = bundle.workload.runtime;
    let locked = serve(Some(policy(serde_json::json!({
        "version": 1, "allowed_runtimes": [other_runtime(kind)]
    }))?))
    .await?;
    let open = serve(None).await?;
    let request = ProviderRequest::bundle(bundle.to_bytes().map_err(|e| e.to_string())?);
    match RemoteProvider::new(locked.endpoint.clone())
        .submit(request.clone(), None)
        .await
    {
        Err(error)
            if error.kind == ProviderErrorKind::AdmissionDenied && error.admission.is_some() => {}
        _ => return Err("a denied request became a job".into()),
    }
    let client = RemoteProvider::new(open.endpoint.clone());
    let submission = client
        .submit(request, None)
        .await
        .map_err(|error| error.to_string())?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let job = loop {
        let job = client
            .job_status(&submission.job_id.0)
            .await
            .map_err(|error| error.to_string())?;
        if job.status.is_terminal() {
            break job;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("admitted job did not finish".into());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    let admission = job.admission.ok_or("job did not persist its admission")?;
    let receipt = client
        .job_receipt(&submission.job_id.0)
        .await
        .map_err(|error| error.to_string())?
        .receipt;
    if receipt.admission_id.as_ref() != Some(&admission.admission_id)
        || receipt.policy_id.as_ref() != Some(&admission.policy_id)
        || locked.provider.executions_started() != 0
    {
        return Err("job admission evidence is inconsistent".into());
    }
    Ok("denied submissions created no job and ran nothing; admitted jobs persisted policy_id and admission_id into their receipts".into())
}

/// Receipts bind admission; tampering and policy changes cannot alter an
/// admitted execution.
async fn certify_receipts(bundle: &WorkloadBundle) -> Result<String, String> {
    let kind = bundle.workload.runtime;
    let original =
        policy(serde_json::json!({"version": 1, "name": "original", "allowed_runtimes": [kind]}))?;
    let replacement = policy(
        serde_json::json!({"version": 1, "name": "replacement", "allowed_runtimes": [other_runtime(kind)]}),
    )?;
    let provider = LocalProvider::new().with_execution_policy(Some(original));
    let request = ProviderRequest::bundle(bundle.to_bytes().map_err(|e| e.to_string())?);
    let admission = provider
        .admit(request.clone())
        .await
        .map_err(|error| error.to_string())?;
    if !admission.decision.admitted {
        return Err("original policy did not admit".into());
    }
    provider.set_execution_policy(Some(replacement));
    let response = provider
        .execute_admitted(request.clone(), admission.clone())
        .await
        .map_err(|error| format!("admitted execution changed with policy: {error}"))?;
    let receipt = response.result.receipt.ok_or("no receipt")?;
    receipt.verify().map_err(|error| error.to_string())?;
    if receipt.policy_id.as_ref() != Some(&admission.policy.policy_id) {
        return Err("receipt does not carry the admitted policy snapshot".into());
    }
    match provider.execute(request.clone()).await {
        Err(error) if error.kind == ProviderErrorKind::AdmissionDenied => {}
        _ => return Err("a new execution did not evaluate the new policy".into()),
    }
    let mut forged = admission;
    forged.decision.admission_id = format!("sha256:{}", "f".repeat(64));
    if provider.execute_admitted(request, forged).await.is_ok() {
        return Err("a forged admission was executed".into());
    }
    let mut tampered = receipt;
    tampered.admission_status = Some("denied".into());
    let _ = tampered.seal();
    if tampered.verify().is_ok() {
        return Err("a receipt with a non-admitted status verified".into());
    }
    Ok("receipts bind policy_id, admission_id, and admission_status; a policy change did not alter an admitted execution; forged admissions and tampered receipts were rejected".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn policy_certification_passes_for_a_wasm_bundle() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("main.wasm"),
            wat::parse_str(r#"(module (func (export "_start")))"#).unwrap(),
        )
        .unwrap();
        let spec = compute_core::WorkloadSpec {
            version: "1".into(),
            runtime: RuntimeKind::Wasm,
            runtime_version: None,
            entrypoint: "main.wasm".into(),
            args: vec![],
            env: Default::default(),
            inputs: vec![],
            outputs: vec![],
            resources: Default::default(),
            network: compute_core::NetworkPolicy::None,
            isolation: Default::default(),
            dependencies: None,
        };
        let path = root.path().join("certified.compute");
        WorkloadBundle::create_from(spec, root.path())
            .unwrap()
            .write(&path)
            .unwrap();
        for (name, result) in certify(&path).await.checks {
            assert!(result.is_ok(), "{name}: {result:?}");
        }
    }
}
