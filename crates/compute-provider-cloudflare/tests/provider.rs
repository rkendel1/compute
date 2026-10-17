//! The Cloudflare provider against a deterministic local fake of
//! Cloudflare's API and workers.dev. No account, credential, or network.

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use compute_core::{
    ExecutionControl, ExecutionErrorKind, ExecutionStatus, InputSource, IsolationProfile,
    IsolationRequirement, NetworkPolicy, ProviderIdentity, ResourceLimits, RuntimeKind,
    WORKLOAD_SPEC_VERSION, WorkloadBundle, WorkloadInput, WorkloadSpec,
};
use compute_provider::{ComputeProvider, Policy, ProviderErrorKind, ProviderRequest};
use compute_provider_cloudflare::{
    CloudflareProvider, DeletionResult, DeploymentAction, Invocation, InvocationOutcome,
};
use support::{ACCOUNT, FakeCloudflare, Mode, TOKEN, modules};

fn bundle(module: &[u8], edit: impl FnOnce(&mut WorkloadSpec)) -> Vec<u8> {
    bundle_as(RuntimeKind::Wasm, "main.wasm", module, edit)
}

fn bundle_as(
    runtime: RuntimeKind,
    entrypoint: &str,
    content: &[u8],
    edit: impl FnOnce(&mut WorkloadSpec),
) -> Vec<u8> {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(entrypoint), content).unwrap();
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
        network: NetworkPolicy::None,
        isolation: IsolationRequirement::default(),
        dependencies: None,
    };
    edit(&mut spec);
    WorkloadBundle::create_from(spec, root.path())
        .unwrap()
        .to_bytes()
        .unwrap()
}

async fn deployed(fake: &FakeCloudflare, module: &[u8]) -> CloudflareProvider {
    let provider = fake.provider();
    provider.deploy(module).await.unwrap();
    provider
}

// ── registration, capabilities, configuration ──────────────────────────

#[tokio::test]
async fn capabilities_come_from_the_profile_without_a_credential_or_a_call() {
    let fake = FakeCloudflare::start().await;
    let provider = fake.provider_with(fake.settings(), None);
    let capabilities = provider.capabilities().await.unwrap();
    assert_eq!(capabilities.protocol, "compute.cloudflare@1");
    assert_eq!(
        capabilities.provider,
        ProviderIdentity::Cloudflare {
            id: "edge".into(),
            account_id: ACCOUNT.into()
        }
    );
    let modes = capabilities.execution_modes();
    assert!(modes.run && !modes.jobs && !modes.deployments && !modes.sessions);
    assert_eq!(capabilities.network_policies, [NetworkPolicy::None]);
    assert!(
        !capabilities
            .isolation_profiles
            .contains(&IsolationProfile::Strict)
    );
    assert_eq!(capabilities.inventory.platform, "cloudflare-workers");
    let runtimes = &capabilities.inventory.runtimes;
    assert_eq!(runtimes.len(), 1);
    assert_eq!(runtimes[0].id, RuntimeKind::Wasm);
    assert!(!runtimes[0].capabilities.memory_limit.supported);
    assert!(!runtimes[0].capabilities.cpu_limit.supported);
    assert!(!runtimes[0].capabilities.artifacts.supported);
    assert_eq!(capabilities.max_memory_bytes, Some(128 * 1024 * 1024));
    // Discovery made no request; health says the account is unreachable.
    assert!(fake.requests().is_empty());
    assert!(!provider.health().await.unwrap().healthy);
}

#[tokio::test]
async fn health_reflects_whether_the_credential_reaches_the_account() {
    let fake = FakeCloudflare::start().await;
    assert!(fake.provider().health().await.unwrap().healthy);
    let wrong = fake.provider_with(fake.settings(), Some("not-the-token"));
    assert!(!wrong.health().await.unwrap().healthy);
}

// ── credentials and account configuration ─────────────────────────────

#[tokio::test]
async fn a_missing_credential_names_where_it_is_expected() {
    let fake = FakeCloudflare::start().await;
    let provider = fake.provider_with(fake.settings(), None);
    for error in [
        provider.validate_account().await.unwrap_err(),
        provider.deploy(&modules::hello()).await.unwrap_err(),
        provider.list().await.unwrap_err(),
    ] {
        assert_eq!(error.kind, ProviderErrorKind::Unauthorized);
        assert!(
            error.message.contains("CLOUDFLARE_API_TOKEN"),
            "{}",
            error.message
        );
    }
    assert!(
        fake.requests().is_empty(),
        "nothing is sent without a credential"
    );
}

#[tokio::test]
async fn a_refused_credential_is_unauthorized_and_never_echoed() {
    let fake = FakeCloudflare::start().await;
    let secret = "cf-wrong-token-should-not-leak";
    let provider = fake.provider_with(fake.settings(), Some(secret));
    let error = provider.validate_account().await.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Unauthorized);
    assert!(error.message.contains("code 10000"), "{}", error.message);
    assert!(error.message.contains("CLOUDFLARE_API_TOKEN"));
    assert!(!error.message.contains(secret));
    assert!(!format!("{provider:?}").contains(secret));
}

#[tokio::test]
async fn an_unknown_account_is_diagnosed_as_the_account() {
    let fake = FakeCloudflare::start().await;
    let mut settings = fake.settings();
    settings.account_id = "fedcba9876543210fedcba9876543210".into();
    let error = fake
        .provider_with(settings, Some(TOKEN))
        .validate_account()
        .await
        .unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Unauthorized);
    assert!(
        error.message.contains("check account_id"),
        "{}",
        error.message
    );
    assert!(error.message.contains("code 7003"));
}

#[tokio::test]
async fn an_account_without_a_workers_dev_subdomain_is_explicit() {
    let fake = FakeCloudflare::start().await;
    fake.state.lock().unwrap().subdomain = None;
    let error = fake.provider().validate_account().await.unwrap_err();
    assert!(error.message.contains("workers.dev"), "{}", error.message);
}

#[tokio::test]
async fn a_valid_account_reports_its_subdomain() {
    let fake = FakeCloudflare::start().await;
    let check = fake.provider().validate_account().await.unwrap();
    assert_eq!(check.workers_dev_subdomain, support::SUBDOMAIN);
    assert_eq!(check.account_id, ACCOUNT);
}

// ── deployment ─────────────────────────────────────────────────────────

#[tokio::test]
async fn deployment_is_explicit_idempotent_and_evidenced() {
    let fake = FakeCloudflare::start().await;
    let provider = fake.provider();
    let module = modules::hello();

    assert_eq!(
        provider.status(&module).await.unwrap().state,
        "not_deployed"
    );
    let first = provider.deploy(&module).await.unwrap();
    assert_eq!(first.action, DeploymentAction::Deployed);
    assert!(first.verify());
    assert_eq!(
        first.workload_digest,
        compute_core::sha256_identity(&module)
    );
    assert!(first.note.contains("does not prove"));
    assert_eq!(first.cloudflare_etag.as_deref(), Some("etag-1"));

    let again = provider.deploy(&module).await.unwrap();
    assert_eq!(again.action, DeploymentAction::AlreadyDeployed);
    assert_eq!(again.deployment_id, first.deployment_id);
    assert_eq!(again.worker, first.worker);
    assert_eq!(
        fake.uploads(),
        1,
        "an identical deployment is not uploaded again"
    );
    assert_eq!(fake.scripts(), [first.worker.clone()]);
    assert_eq!(provider.status(&module).await.unwrap().state, "deployed");
    assert_eq!(fake.invocations(), 0, "deploying runs nothing");
}

#[tokio::test]
async fn a_retried_deployment_never_creates_a_second_worker() {
    let fake = FakeCloudflare::start().await;
    fake.set_mode(Mode::UploadResponseLost);
    let provider = fake.provider();
    let module = modules::hello();
    let lost = provider.deploy(&module).await.unwrap_err();
    assert_eq!(lost.kind, ProviderErrorKind::ProviderUnavailable);
    let retried = provider.deploy(&module).await.unwrap();
    assert_eq!(retried.action, DeploymentAction::AlreadyDeployed);
    assert_eq!(fake.scripts().len(), 1);
    assert_eq!(fake.uploads(), 1);
}

#[tokio::test]
async fn deployment_failures_map_to_compute_errors() {
    for (mode, kind, mention) in [
        (
            Mode::ScriptLimit,
            ProviderErrorKind::QuotaExhausted,
            "code 10037",
        ),
        (
            Mode::ApiRateLimited,
            ProviderErrorKind::QuotaExhausted,
            "rate limit",
        ),
        (
            Mode::ApiDown,
            ProviderErrorKind::ProviderUnavailable,
            "HTTP 500",
        ),
    ] {
        let fake = FakeCloudflare::start().await;
        fake.set_mode(mode);
        let error = fake.provider().deploy(&modules::hello()).await.unwrap_err();
        assert_eq!(error.kind, kind, "{mode:?}: {}", error.message);
        assert!(
            error.message.contains(mention),
            "{mode:?}: {}",
            error.message
        );
        assert!(fake.scripts().is_empty());
    }
}

#[tokio::test]
async fn modules_beyond_the_profile_are_refused_before_anything_is_sent() {
    let fake = FakeCloudflare::start().await;
    let provider = fake.provider();
    let error = provider.deploy(b"#!/bin/sh\necho hi").await.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::ArtifactInvalid);

    let mut settings = fake.settings();
    settings.limits.max_module_bytes = 64;
    let small = fake.provider_with(settings, Some(TOKEN));
    let mut big = modules::hello();
    big.resize(4096, 0);
    let error = small.deploy(&big).await.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::CapabilityMismatch);
    assert!(error.message.contains("max_module_bytes"));
    assert!(fake.requests().is_empty());
}

// ── execution through the provider boundary ────────────────────────────

#[tokio::test]
async fn a_deployed_module_executes_with_a_verifiable_receipt() {
    let fake = FakeCloudflare::start().await;
    let module = modules::hello();
    let provider = fake.provider();
    let deployment = provider.deploy(&module).await.unwrap();

    let response = provider
        .execute(ProviderRequest::bundle(bundle(&module, |_| {})))
        .await
        .unwrap();
    let result = &response.result;
    assert_eq!(response.protocol, "compute.cloudflare@1");
    assert_eq!(result.status, ExecutionStatus::Completed);
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(result.stdout.text, "hello from cloudflare\n");
    assert_eq!(result.provider, Some(provider.identity()));
    assert!(result.admission.is_some());

    let receipt = result.receipt.as_ref().unwrap();
    receipt.verify().unwrap();
    assert_eq!(receipt.provider, Some(provider.identity()));
    assert_eq!(
        receipt.provider_protocol.as_deref(),
        Some("compute.cloudflare@1")
    );
    // The deployment is the distribution that executed; the module is the
    // executable.
    assert_eq!(receipt.distribution.id, deployment.deployment_id);
    assert_eq!(
        receipt.runtime.executable_identity,
        deployment.workload_digest
    );
    assert_eq!(receipt.execution.exit_code, Some(0));
    assert_eq!(receipt.admission_status.as_deref(), Some("admitted"));
    assert_eq!(fake.invocations(), 1);
}

#[tokio::test]
async fn execution_never_deploys() {
    let fake = FakeCloudflare::start().await;
    let error = fake
        .provider()
        .execute(ProviderRequest::bundle(bundle(&modules::hello(), |_| {})))
        .await
        .unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::RuntimeUnavailable);
    assert!(error.message.contains("not deployed"), "{}", error.message);
    assert!(error.message.contains("compute cloudflare deploy"));
    assert_eq!(fake.uploads(), 0);
    assert!(fake.scripts().is_empty());
}

#[tokio::test]
async fn workload_failures_are_results_not_provider_errors() {
    let fake = FakeCloudflare::start().await;

    let module = modules::exit3();
    let provider = deployed(&fake, &module).await;
    let result = provider
        .execute(ProviderRequest::bundle(bundle(&module, |_| {})))
        .await
        .unwrap()
        .result;
    // Like every Compute runtime: a non-zero exit is a completed execution.
    assert_eq!(result.status, ExecutionStatus::Completed);
    assert_eq!(result.exit_code, Some(3));
    assert_eq!(result.stderr.text, "failing\n");

    let module = modules::trap();
    let provider = deployed(&fake, &module).await;
    let result = provider
        .execute(ProviderRequest::bundle(bundle(&module, |_| {})))
        .await
        .unwrap()
        .result;
    assert_eq!(result.status, ExecutionStatus::Failed);
    let error = result.error.as_ref().unwrap();
    assert_eq!(error.kind, ExecutionErrorKind::Runtime);
    assert!(error.started);
    result.receipt.as_ref().unwrap().verify().unwrap();

    let module = modules::library();
    let provider = deployed(&fake, &module).await;
    let result = provider
        .execute(ProviderRequest::bundle(bundle(&module, |_| {})))
        .await
        .unwrap()
        .result;
    assert_eq!(result.status, ExecutionStatus::Failed);
    assert_eq!(
        result.error.as_ref().unwrap().kind,
        ExecutionErrorKind::Start
    );
    assert!(!result.error.as_ref().unwrap().started);
}

#[tokio::test]
async fn arguments_environment_and_stdin_reach_the_module() {
    let fake = FakeCloudflare::start().await;
    let show = modules::show();
    let provider = deployed(&fake, &show).await;
    let response = provider
        .execute(ProviderRequest::bundle(bundle(&show, |spec| {
            spec.args = vec!["--name".into(), "edge".into()];
            spec.env = BTreeMap::from([("GREETING".into(), "hi".into())]);
        })))
        .await
        .unwrap();
    assert_eq!(
        response.result.stdout.text,
        "workload.wasm\0--name\0edge\0GREETING=hi\0"
    );

    let echo = modules::echo_stdin();
    provider.deploy(&echo).await.unwrap();
    let (invoked, record) = provider
        .invoke(
            &echo,
            Invocation {
                stdin: b"from stdin".to_vec(),
                ..Invocation::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(invoked.stdout, b"from stdin");
    assert_eq!(record.outcome, InvocationOutcome::Exited);
    assert!(record.workload_identity_matched);
    assert!(record.verify());
}

#[tokio::test]
async fn output_beyond_the_limit_is_truncated_and_counted() {
    let fake = FakeCloudflare::start().await;
    let module = modules::hello();
    let provider = deployed(&fake, &module).await;
    let result = provider
        .execute(ProviderRequest::bundle(bundle(&module, |spec| {
            spec.resources.stdout_bytes = Some(5);
        })))
        .await
        .unwrap()
        .result;
    assert_eq!(result.stdout.text, "hello");
    assert!(result.stdout.truncated);
    assert_eq!(result.stdout.bytes, 22);
}

// ── timeouts, cancellation, quotas, limits ─────────────────────────────

#[tokio::test]
async fn a_timeout_is_a_timed_out_result_whose_outcome_is_unknown() {
    let fake = FakeCloudflare::start().await;
    let module = modules::hello();
    let provider = deployed(&fake, &module).await;
    fake.set_mode(Mode::Hang);
    let started = std::time::Instant::now();
    let result = provider
        .execute(ProviderRequest::bundle(bundle(&module, |spec| {
            spec.resources.wall_time = Some(Duration::from_millis(200));
        })))
        .await
        .unwrap()
        .result;
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(result.status, ExecutionStatus::TimedOut);
    let error = result.error.as_ref().unwrap();
    assert_eq!(error.kind, ExecutionErrorKind::Timeout);
    assert!(error.message.contains("unknown"));
    let receipt = result.receipt.as_ref().unwrap();
    receipt.verify().unwrap();
    assert_eq!(receipt.execution.status, ExecutionStatus::TimedOut);
}

#[tokio::test]
async fn cancellation_stops_waiting_and_says_so() {
    let fake = FakeCloudflare::start().await;
    let module = modules::hello();
    let provider = Arc::new(deployed(&fake, &module).await);
    fake.set_mode(Mode::Hang);
    let control = ExecutionControl::new().with_execution_id("exec_cancelled_1");
    let request = ProviderRequest::bundle(bundle(&module, |_| {}));
    let admission = provider.admit(request.clone()).await.unwrap();
    let canceller = control.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        canceller.cancel();
    });
    let result = provider
        .execute_admitted_controlled(request, admission, &control)
        .await
        .unwrap()
        .result;
    assert_eq!(result.status, ExecutionStatus::Cancelled);
    assert_eq!(result.execution_id, "exec_cancelled_1");
    assert_eq!(result.error.unwrap().kind, ExecutionErrorKind::Cancelled);
}

#[tokio::test]
async fn the_daily_quota_is_quota_exhausted_and_nothing_ran() {
    let fake = FakeCloudflare::start().await;
    let module = modules::hello();
    let provider = deployed(&fake, &module).await;
    fake.set_mode(Mode::DailyQuota);
    let error = provider
        .execute(ProviderRequest::bundle(bundle(&module, |_| {})))
        .await
        .unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::QuotaExhausted);
    assert!(error.message.contains("1027"), "{}", error.message);
    assert!(error.message.contains("100000 requests a day"));
    assert!(error.message.contains("nothing ran"));
}

#[tokio::test]
async fn the_cpu_limit_is_a_killed_execution() {
    let fake = FakeCloudflare::start().await;
    let module = modules::hello();
    let provider = deployed(&fake, &module).await;
    fake.set_mode(Mode::CpuLimit);
    let result = provider
        .execute(ProviderRequest::bundle(bundle(&module, |_| {})))
        .await
        .unwrap()
        .result;
    assert_eq!(result.status, ExecutionStatus::Killed);
    let error = result.error.unwrap();
    assert_eq!(error.kind, ExecutionErrorKind::ResourceLimit);
    assert!(error.message.contains("10 ms CPU"), "{}", error.message);
}

#[tokio::test]
async fn concurrent_invocations_beyond_the_limit_are_refused_not_queued() {
    let fake = FakeCloudflare::start().await;
    let module = modules::hello();
    let mut settings = fake.settings();
    settings.limits.max_concurrent_invocations = 1;
    let provider = Arc::new(fake.provider_with(settings, Some(TOKEN)));
    provider.deploy(&module).await.unwrap();
    fake.set_mode(Mode::Hang);
    let first = {
        let provider = provider.clone();
        let module = module.clone();
        tokio::spawn(async move {
            provider
                .invoke(
                    &module,
                    Invocation {
                        timeout: Some(Duration::from_secs(5)),
                        ..Invocation::default()
                    },
                    None,
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    let error = provider
        .invoke(&module, Invocation::default(), None)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::QuotaExhausted);
    assert!(error.message.contains("max_concurrent_invocations"));
    assert_eq!(fake.invocations(), 1, "the refused invocation was not sent");
    first.abort();
}

// ── unsupported capabilities ───────────────────────────────────────────

#[tokio::test]
async fn workloads_a_worker_cannot_host_are_refused_explicitly() {
    let fake = FakeCloudflare::start().await;
    let provider = fake.provider();
    let hello = modules::hello();
    let cases: Vec<(Vec<u8>, ProviderErrorKind, &str)> = vec![
        (
            bundle_as(RuntimeKind::Node, "app.js", b"console.log(1)", |_| {}),
            ProviderErrorKind::OperationUnsupported,
            "process_runtime",
        ),
        (
            bundle_as(RuntimeKind::Python, "app.py", b"print(1)", |_| {}),
            ProviderErrorKind::OperationUnsupported,
            "process_runtime",
        ),
        (
            bundle_as(RuntimeKind::Native, "app", b"\x7fELF", |_| {}),
            ProviderErrorKind::OperationUnsupported,
            "native_executable",
        ),
        (
            bundle_as(RuntimeKind::Shell, "app.sh", b"sleep infinity", |_| {}),
            ProviderErrorKind::OperationUnsupported,
            "persistent process",
        ),
        (
            bundle(&hello, |spec| {
                spec.inputs = vec![WorkloadInput {
                    path: "data.txt".into(),
                    source: InputSource::Inline {
                        data: b"x".to_vec(),
                    },
                }]
            }),
            ProviderErrorKind::OperationUnsupported,
            "filesystem",
        ),
        (
            bundle(&hello, |spec| spec.network = NetworkPolicy::Network),
            ProviderErrorKind::OperationUnsupported,
            "outbound_network",
        ),
        (
            bundle(&hello, |spec| spec.resources.memory_bytes = Some(1 << 20)),
            ProviderErrorKind::CapabilityMismatch,
            "memory limit",
        ),
        (
            bundle(&hello, |spec| {
                spec.resources.cpu_time = Some(Duration::from_millis(5))
            }),
            ProviderErrorKind::CapabilityMismatch,
            "CPU-time limit",
        ),
        (
            bundle(&hello, |spec| {
                spec.resources.wall_time = Some(Duration::from_secs(3600))
            }),
            ProviderErrorKind::CapabilityMismatch,
            "limits.timeout_ms",
        ),
        (
            bundle(&hello, |spec| {
                spec.isolation.profile = IsolationProfile::Strict
            }),
            ProviderErrorKind::CapabilityMismatch,
            "strict isolation",
        ),
    ];
    for (bytes, kind, mention) in cases {
        let error = provider
            .execute(ProviderRequest::bundle(bytes))
            .await
            .unwrap_err();
        assert_eq!(error.kind, kind, "{}", error.message);
        assert!(
            error
                .message
                .to_lowercase()
                .contains(&mention.to_lowercase()),
            "{} lacks {mention}",
            error.message
        );
    }
    assert!(
        fake.requests().is_empty(),
        "refusals send nothing to Cloudflare"
    );
}

#[tokio::test]
async fn admission_is_computes_and_a_denial_never_reaches_cloudflare() {
    let fake = FakeCloudflare::start().await;
    let module = modules::hello();
    let provider = deployed(&fake, &module).await;
    let before = fake.requests().len();
    let mut request = ProviderRequest::bundle(bundle(&module, |_| {}));
    request.execution.policy =
        Some(Policy::from_json(br#"{"version":1,"allowed_os":["linux"]}"#).unwrap());
    let error = provider.execute(request).await.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::AdmissionDenied);
    assert!(error.admission.is_some());
    assert_eq!(fake.requests().len(), before);
    assert_eq!(fake.invocations(), 0);
}

// ── evidence ───────────────────────────────────────────────────────────

#[tokio::test]
async fn an_answer_for_another_workload_is_not_reported_as_this_one() {
    let fake = FakeCloudflare::start().await;
    let module = modules::hello();
    let provider = deployed(&fake, &module).await;
    fake.set_mode(Mode::WrongWorkload);
    let error = provider
        .execute(ProviderRequest::bundle(bundle(&module, |_| {})))
        .await
        .unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::EvidenceInvalid);
}

#[tokio::test]
async fn invocation_records_point_at_their_deployment() {
    let fake = FakeCloudflare::start().await;
    let module = modules::exit3();
    let provider = fake.provider();
    let deployment = provider.deploy(&module).await.unwrap();
    let (invoked, record) = provider
        .invoke(&module, Invocation::default(), None)
        .await
        .unwrap();
    assert_eq!(invoked.exit_code, Some(3));
    assert_eq!(record.deployment_id, deployment.deployment_id);
    assert_eq!(record.worker, deployment.worker);
    assert_eq!(record.expected_workload, deployment.workload_digest);
    assert_eq!(
        record.reported_workload.as_deref(),
        Some(deployment.workload_digest.as_str())
    );
    assert_eq!(record.http_status, Some(200));
    assert_eq!(record.cf_ray.as_deref(), Some("8f00test-LAX"));
    assert_eq!(record.exit_code, Some(3));
    assert!(record.verify());
    let mut tampered = record.clone();
    tampered.exit_code = Some(0);
    assert!(!tampered.verify());
}

#[tokio::test]
async fn a_rotated_token_stops_existing_workers_until_redeployed() {
    let fake = FakeCloudflare::start().await;
    let module = modules::hello();
    deployed(&fake, &module).await;
    // The deployment's invocation secret was derived from TOKEN; this
    // provider's credential differs (and the fake API would refuse it, so
    // the account calls are only reached through invocation).
    let rotated = fake.provider_with(fake.settings(), Some("cf-rotated-token"));
    let error = rotated
        .invoke(&module, Invocation::default(), None)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Unauthorized);
    assert!(
        error.message.contains("deploy it again"),
        "{}",
        error.message
    );
    assert!(error.message.contains("nothing ran"));
}

// ── lifecycle and cleanup ──────────────────────────────────────────────

#[tokio::test]
async fn deletion_is_explicit_idempotent_and_limited_to_compute_workers() {
    let fake = FakeCloudflare::start().await;
    let module = modules::hello();
    let provider = fake.provider();
    let deployment = provider.deploy(&module).await.unwrap();
    assert_eq!(
        provider
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|worker| worker.worker)
            .collect::<Vec<_>>(),
        [deployment.worker.clone()],
        "only Workers with this provider's prefix are listed"
    );

    let deleted = provider.delete(&deployment.worker).await.unwrap();
    assert_eq!(deleted.result, DeletionResult::Deleted);
    let again = provider.delete(&deployment.worker).await.unwrap();
    assert_eq!(again.result, DeletionResult::Absent);
    assert!(fake.scripts().is_empty());
    assert_eq!(
        provider.status(&module).await.unwrap().state,
        "not_deployed"
    );

    let before = fake.requests().len();
    let error = provider.delete("someone-elses-worker").await.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::OperationUnsupported);
    assert_eq!(
        fake.requests().len(),
        before,
        "a foreign Worker is never touched"
    );

    // A deleted workload is not deployed any more: execution refuses it.
    let error = provider
        .execute(ProviderRequest::bundle(bundle(&module, |_| {})))
        .await
        .unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::RuntimeUnavailable);
}

// ── secrets ────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_api_token_never_appears_in_evidence_results_or_invocations() {
    let fake = FakeCloudflare::start().await;
    let module = modules::show();
    let provider = fake.provider();
    let deployment = provider.deploy(&module).await.unwrap();
    let response = provider
        .execute(ProviderRequest::bundle(bundle(&module, |_| {})))
        .await
        .unwrap();
    let (_, record) = provider
        .invoke(&module, Invocation::default(), None)
        .await
        .unwrap();
    let deletion = provider.delete(&deployment.worker).await.unwrap();
    let capabilities = provider.capabilities().await.unwrap();

    for text in [
        serde_json::to_string(&deployment).unwrap(),
        serde_json::to_string(&response).unwrap(),
        serde_json::to_string(&record).unwrap(),
        serde_json::to_string(&deletion).unwrap(),
        serde_json::to_string(&capabilities).unwrap(),
        format!("{provider:?}"),
    ] {
        assert!(!text.contains(TOKEN), "token leaked into {text}");
    }
    // Invocations authenticate with the derived per-Worker secret, never
    // with the API token; uploads carry that secret, not the API token.
    for request in fake.requests() {
        if request.path.starts_with("/w/") {
            assert_ne!(
                request.authorization.as_deref(),
                Some(&*format!("Bearer {TOKEN}"))
            );
        }
        assert!(
            !String::from_utf8_lossy(&request.body).contains(TOKEN),
            "token sent in a body to {}",
            request.path
        );
    }
}
