//! A cloudflare member in a caller-owned pool: configuration, discovery,
//! placement, dispatch, and receipt verification through the same path as
//! every other provider. Uses the local fake; no account.

mod support;

use std::collections::BTreeMap;

use chrono::Utc;
use compute_core::{
    IsolationRequirement, NetworkPolicy, ResourceLimits, RuntimeKind, WORKLOAD_SPEC_VERSION,
    WorkloadBundle, WorkloadSpec,
};
use compute_placement::{
    AdmissionContext, CapabilityCache, DiscoveryMode, DiscoveryStatus, PlacementOutcome,
    PlacementRequirements, PoolConfig, ProviderKind, ProviderPool, ReasonCode, RequirementOptions,
    SubmissionMode, dispatch, place,
};
use compute_provider::ProviderRequest;
use support::{ACCOUNT, FakeCloudflare, modules};

fn pool_toml(fake: Option<&FakeCloudflare>, token_env: &str) -> String {
    let overrides = fake
        .map(|fake| {
            format!(
                "api_base = \"http://{0}/client/v4\"\nworkers_base = \"http://{0}/w\"\n",
                fake.addr
            )
        })
        .unwrap_or_default();
    format!(
        r#"
[providers.local]
kind = "local"
priority = 100

[providers.edge]
kind = "cloudflare"
token_env = "{token_env}"
priority = -100

[providers.edge.cloudflare]
account_id = "{ACCOUNT}"
{overrides}"#
    )
}

fn bundle(runtime: RuntimeKind, entrypoint: &str, content: &[u8]) -> WorkloadBundle {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(entrypoint), content).unwrap();
    WorkloadBundle::create_from(
        WorkloadSpec {
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
        },
        root.path(),
    )
    .unwrap()
}

fn prepare(
    bundle: &WorkloadBundle,
    submission: SubmissionMode,
) -> (ProviderRequest, PlacementRequirements, AdmissionContext) {
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
    let admission = AdmissionContext::new(
        &[],
        compute_policy::ExecutionContract::from_bundle(bundle, None).unwrap(),
    );
    (request, requirements, admission)
}

#[test]
fn pool_configuration_requires_an_account_and_a_named_credential() {
    let config = PoolConfig::parse(&pool_toml(None, "CF_TOKEN_UNSET_FOR_TEST")).unwrap();
    let edge = &config.providers["edge"];
    assert_eq!(edge.kind, ProviderKind::Cloudflare);
    assert_eq!(edge.cloudflare.as_ref().unwrap().account_id, ACCOUNT);

    for (toml, mention) in [
        (
            "[providers.edge]\nkind = \"cloudflare\"\ntoken_env = \"T\"\n",
            "account_id",
        ),
        (
            &format!(
                "[providers.edge]\nkind = \"cloudflare\"\n[providers.edge.cloudflare]\naccount_id = \"{ACCOUNT}\"\n"
            ),
            "token_env",
        ),
        (
            &format!(
                "[providers.edge]\nkind = \"cloudflare\"\ntoken_env = \"T\"\n[providers.edge.cloudflare]\naccount_id = \"{ACCOUNT}\"\napi_token = \"secret\"\n"
            ),
            "api_token",
        ),
        (
            &format!(
                "[providers.edge]\nkind = \"cloudflare\"\ntoken_env = \"T\"\nendpoint = \"https://x.example\"\n[providers.edge.cloudflare]\naccount_id = \"{ACCOUNT}\"\n"
            ),
            "no endpoint",
        ),
        (
            &format!(
                "[providers.r]\nkind = \"remote\"\nendpoint = \"https://x.example\"\n[providers.r.cloudflare]\naccount_id = \"{ACCOUNT}\"\n"
            ),
            "only to a cloudflare provider",
        ),
        (
            "[providers.edge]\nkind = \"cloudflare\"\ntoken_env = \"T\"\n[providers.edge.cloudflare]\naccount_id = \"short\"\n",
            "account_id",
        ),
        (
            &format!(
                "[providers.edge]\nkind = \"cloudflare\"\ntoken_env = \"T\"\n[providers.edge.cloudflare]\naccount_id = \"{ACCOUNT}\"\n[providers.edge.cloudflare.limits]\ntimeout_ms = 99999999\n"
            ),
            "timeout_ms",
        ),
    ] {
        let error = PoolConfig::parse(toml).unwrap_err().to_string();
        assert!(error.contains(mention), "{error} lacks {mention}");
    }
}

#[tokio::test]
async fn existing_pools_are_unchanged_by_the_new_kind() {
    // A pool without a cloudflare member parses and builds exactly as before.
    let config = PoolConfig::parse(
        "[providers.local]\nkind = \"local\"\n\n[providers.dev]\nkind = \"remote\"\nendpoint = \"http://127.0.0.1:1\"\n",
    )
    .unwrap();
    assert!(config.providers.values().all(|p| p.cloudflare.is_none()));
    let pool = ProviderPool::from_config(&config).unwrap();
    assert_eq!(pool.members().count(), 2);
    let local_only = PoolConfig::local_only();
    assert_eq!(
        serde_json::to_value(&local_only.providers["local"]).unwrap(),
        serde_json::json!({"kind": "local", "priority": 0})
    );
}

#[tokio::test]
async fn a_cloudflare_member_is_discovered_without_its_credential() {
    let config = PoolConfig::parse(&pool_toml(None, "CF_TOKEN_UNSET_FOR_TEST")).unwrap();
    let pool = ProviderPool::from_config(&config).unwrap();
    let mut cache = CapabilityCache::default();
    let records = pool
        .capabilities(&mut cache, DiscoveryMode::Refresh, Some("edge"), Utc::now())
        .await;
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].status,
        DiscoveryStatus::Discovered,
        "{:?}",
        records[0].error
    );
    let descriptor = records[0].descriptor.as_ref().unwrap();
    assert_eq!(descriptor.provider_kind, ProviderKind::Cloudflare);
    assert_eq!(descriptor.protocol_version, "compute.cloudflare@1");
    assert_eq!(
        descriptor.distribution.platform.label(),
        "cloudflare-workers"
    );
    assert_eq!(
        descriptor
            .runtimes
            .iter()
            .map(|offer| offer.kind)
            .collect::<Vec<_>>(),
        [RuntimeKind::Wasm]
    );
    assert!(descriptor.artifact_limits.run);
    assert!(!descriptor.artifact_limits.jobs);
    assert!(!descriptor.artifact_limits.deployments);
    assert!(!descriptor.artifact_limits.sessions);
    // The cached capabilities re-validate into the same descriptor.
    let cached = pool
        .capabilities(
            &mut cache,
            DiscoveryMode::PreferCache,
            Some("edge"),
            Utc::now(),
        )
        .await;
    assert_eq!(cached[0].status, DiscoveryStatus::Cached);
    assert_eq!(
        cached[0].descriptor.as_ref().unwrap().capability_version,
        descriptor.capability_version
    );
}

#[tokio::test]
async fn placement_explains_what_a_worker_cannot_host() {
    let config = PoolConfig::parse(&pool_toml(None, "CF_TOKEN_UNSET_FOR_TEST")).unwrap();
    let pool = ProviderPool::from_config(&config).unwrap();
    let mut cache = CapabilityCache::default();
    let records = pool
        .capabilities(&mut cache, DiscoveryMode::Refresh, None, Utc::now())
        .await;
    let reasons = |bundle: &WorkloadBundle, submission| {
        let (_, requirements, admission) = prepare(bundle, submission);
        let report = place(
            &pool.configs(),
            pool.policy(),
            &records,
            &requirements,
            &admission,
            Some("edge"),
        );
        assert_eq!(
            report.outcome,
            PlacementOutcome::PlacementFailed,
            "{report:#?}"
        );
        report
            .providers
            .iter()
            .find(|provider| provider.provider_id == "edge")
            .unwrap()
            .reasons
            .iter()
            .map(|reason| reason.code)
            .collect::<Vec<_>>()
    };
    let node = bundle(RuntimeKind::Node, "app.js", b"console.log(1)");
    assert!(reasons(&node, SubmissionMode::Synchronous).contains(&ReasonCode::RuntimeUnsupported));
    let wasm = bundle(RuntimeKind::Wasm, "main.wasm", &modules::hello());
    assert!(reasons(&wasm, SubmissionMode::Session).contains(&ReasonCode::SessionsUnsupported));
    assert!(
        reasons(&wasm, SubmissionMode::Deployment).contains(&ReasonCode::DeploymentUnsupported)
    );
    assert!(reasons(&wasm, SubmissionMode::Job).contains(&ReasonCode::JobsUnsupported));
}

#[tokio::test]
async fn a_placed_wasm_workload_runs_on_the_worker_and_its_receipt_verifies() {
    let fake = FakeCloudflare::start().await;
    let module = modules::hello();
    fake.provider().deploy(&module).await.unwrap();
    // SAFETY: tests in this binary that read this variable are this one.
    unsafe { std::env::set_var("CF_TOKEN_FOR_PLACEMENT_TEST", support::TOKEN) };
    let config = PoolConfig::parse(&pool_toml(Some(&fake), "CF_TOKEN_FOR_PLACEMENT_TEST")).unwrap();
    let pool = ProviderPool::from_config(&config).unwrap();
    let mut cache = CapabilityCache::default();
    let records = pool
        .capabilities(&mut cache, DiscoveryMode::Refresh, None, Utc::now())
        .await;

    let wasm = bundle(RuntimeKind::Wasm, "main.wasm", &module);
    let (request, requirements, admission) = prepare(&wasm, SubmissionMode::Synchronous);
    let report = place(
        &pool.configs(),
        pool.policy(),
        &records,
        &requirements,
        &admission,
        Some("edge"),
    );
    assert_eq!(report.outcome, PlacementOutcome::Placed, "{report:#?}");
    let response = dispatch::execute(&pool, &report, request).await.unwrap();
    assert_eq!(response.result.stdout.text, "hello from cloudflare\n");
    let receipt = response.result.receipt.unwrap();
    report.verify_receipt(&receipt).unwrap();
    assert_eq!(receipt.placement.as_ref().unwrap().provider_id, "edge");
    assert_eq!(
        receipt.provider_protocol.as_deref(),
        Some("compute.cloudflare@1")
    );

    // Without an explicit provider, the higher-priority local member is
    // chosen: adding a cloudflare member does not move existing work.
    let (_, requirements, admission) = prepare(&wasm, SubmissionMode::Synchronous);
    let report = place(
        &pool.configs(),
        pool.policy(),
        &records,
        &requirements,
        &admission,
        None,
    );
    assert_eq!(report.selected.as_ref().unwrap().provider_id, "local");
    assert_eq!(fake.invocations(), 1);
}
