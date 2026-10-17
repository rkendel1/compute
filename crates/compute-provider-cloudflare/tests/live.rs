//! Live smoke test against a real Cloudflare account. Ignored by default;
//! it never runs in normal CI and never needs paid resources.
//!
//! ```sh
//! export CLOUDFLARE_API_TOKEN=…                 # Workers Scripts Edit on the account
//! export COMPUTE_CLOUDFLARE_LIVE_ACCOUNT_ID=…   # 32 hex characters
//! cargo test -p compute-provider-cloudflare --test live -- --ignored --nocapture
//! ```
//!
//! Asked to run without those variables, it fails rather than passing: a
//! skipped live test is never reported as a passed one.

use std::time::{Duration, Instant};

use compute_core::{ExecutionStatus, ProviderIdentity, WorkloadBundle};
use compute_provider::{ComputeProvider, ProviderErrorKind, ProviderRequest};
use compute_provider_cloudflare::{
    CloudflareProvider, CloudflareSettings, Credential, CredentialSource, DeletionResult,
    Invocation, InvocationOutcome,
};

fn module(nonce: &str) -> Vec<u8> {
    // The nonce makes this run's Worker its own: a fresh name, nothing
    // shared with earlier runs.
    let line = format!("compute live smoke {nonce}\n");
    wat::parse_str(format!(
        r#"(module
          (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
          (memory (export "memory") 1)
          (data (i32.const 64) "{line}")
          (func (export "_start")
            (i32.store (i32.const 0) (i32.const 64))
            (i32.store (i32.const 4) (i32.const {len}))
            (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8)))))"#,
        line = line.escape_default(),
        len = line.len()
    ))
    .unwrap()
}

#[tokio::test]
#[ignore = "live: needs CLOUDFLARE_API_TOKEN and COMPUTE_CLOUDFLARE_LIVE_ACCOUNT_ID"]
async fn live_deploy_invoke_verify_and_clean_up() {
    let (Ok(token), Ok(account)) = (
        std::env::var("CLOUDFLARE_API_TOKEN"),
        std::env::var("COMPUTE_CLOUDFLARE_LIVE_ACCOUNT_ID"),
    ) else {
        panic!(
            "NOT RUN: the live smoke test was requested but CLOUDFLARE_API_TOKEN and COMPUTE_CLOUDFLARE_LIVE_ACCOUNT_ID are not both set"
        );
    };
    let mut settings = CloudflareSettings::new(account);
    settings.worker_prefix = "compute-smoke-".into();
    let provider = std::sync::Arc::new(
        CloudflareProvider::new(
            "live",
            settings,
            Credential::new(token),
            CredentialSource::Environment("CLOUDFLARE_API_TOKEN".into()),
        )
        .unwrap(),
    );
    let account = provider.validate_account().await.unwrap();
    eprintln!(
        "account ok: workers.dev subdomain {}",
        account.workers_dev_subdomain
    );

    let nonce = format!("{}", chrono::Utc::now().timestamp_nanos_opt().unwrap());
    let module = module(&nonce);
    let deployment = provider.deploy(&module).await.unwrap();
    eprintln!(
        "deployed {} ({})",
        deployment.worker, deployment.deployment_id
    );
    let worker = deployment.worker.clone();

    // Everything after the deployment runs before cleanup is checked, so a
    // failed assertion still leaves nothing behind.
    let outcome = {
        let (provider, module, deployment) = (provider.clone(), module.clone(), deployment.clone());
        async move {
            // A new workers.dev route can take a few seconds to answer.
            let deadline = Instant::now() + Duration::from_secs(90);
            let (invoked, record) = loop {
                match provider.invoke(&module, Invocation::default(), None).await {
                    Ok(done) => break done,
                    Err(error)
                        if Instant::now() < deadline
                            && matches!(
                                error.kind,
                                ProviderErrorKind::ProviderUnavailable
                                    | ProviderErrorKind::RuntimeUnavailable
                                    | ProviderErrorKind::RemoteExecutionFailure
                            ) =>
                    {
                        eprintln!("waiting for the route: {}", error.message);
                        tokio::time::sleep(Duration::from_secs(3)).await;
                    }
                    Err(error) => panic!("invocation failed: {error}"),
                }
            };
            assert_eq!(record.outcome, InvocationOutcome::Exited);
            assert_eq!(invoked.exit_code, Some(0));
            assert_eq!(
                String::from_utf8_lossy(&invoked.stdout),
                format!("compute live smoke {nonce}\n")
            );
            // The evidence corresponds to this invocation of this deployment.
            assert!(record.verify());
            assert!(record.workload_identity_matched);
            assert_eq!(record.deployment_id, deployment.deployment_id);
            assert_eq!(
                record.reported_workload.as_deref(),
                Some(deployment.workload_digest.as_str())
            );
            assert!(
                record.cf_ray.is_some(),
                "a real Cloudflare answer carries cf-ray"
            );
            eprintln!(
                "invoked: {} cf-ray {:?}",
                record.execution_id, record.cf_ray
            );

            // The same workload through the provider boundary, with a receipt.
            let root = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join("main.wasm"), &module).unwrap();
            let spec = serde_json::from_value(serde_json::json!({
                "version": "1", "runtime": "wasm", "entrypoint": "main.wasm"
            }))
            .unwrap();
            let bundle = WorkloadBundle::create_from(spec, root.path()).unwrap();
            let response = provider
                .execute(ProviderRequest::bundle(bundle.to_bytes().unwrap()))
                .await
                .unwrap();
            assert_eq!(response.result.status, ExecutionStatus::Completed);
            let receipt = response.result.receipt.unwrap();
            receipt.verify().unwrap();
            assert_eq!(receipt.distribution.id, deployment.deployment_id);
            assert!(matches!(
                receipt.provider,
                Some(ProviderIdentity::Cloudflare { .. })
            ));
        }
    };
    let result = tokio::spawn(outcome).await;

    let deleted = provider.delete(&worker).await.unwrap();
    assert_eq!(deleted.result, DeletionResult::Deleted);
    assert_eq!(
        provider.status(&module).await.unwrap().state,
        "not_deployed"
    );
    assert!(
        provider
            .list()
            .await
            .unwrap()
            .iter()
            .all(|listed| listed.worker != worker),
        "the smoke test's Worker is gone"
    );
    eprintln!("cleaned up {worker}");
    if let Err(panic) = result {
        std::panic::resume_unwind(panic.into_panic());
    }
}
