//! The documented example (examples/cloudflare-workers): its module is its
//! WAT source compiled, and it deploys, runs, and is deleted as the guide
//! says. Regenerate the module with COMPUTE_UPDATE_EXAMPLES=1.

mod support;

use std::path::Path;

use compute_provider::{ComputeProvider, ProviderRequest};
use compute_provider_cloudflare::DeletionResult;
use support::FakeCloudflare;

fn example() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/cloudflare-workers")
}

#[test]
fn the_example_module_is_its_source_compiled() {
    let compiled = wat::parse_file(example().join("hello.wat")).unwrap();
    let path = example().join("hello.wasm");
    if std::env::var_os("COMPUTE_UPDATE_EXAMPLES").is_some() {
        std::fs::write(&path, &compiled).unwrap();
    }
    assert_eq!(
        std::fs::read(&path).unwrap(),
        compiled,
        "hello.wasm is stale; rerun with COMPUTE_UPDATE_EXAMPLES=1"
    );
}

#[test]
fn the_example_pool_file_is_valid() {
    let text = std::fs::read_to_string(example().join("compute-pool.toml")).unwrap();
    let config = compute_placement::PoolConfig::parse(&text).unwrap();
    let edge = &config.providers["cloudflare"];
    assert_eq!(edge.token_env.as_deref(), Some("CLOUDFLARE_API_TOKEN"));
    assert!(
        !text.contains("api_token ="),
        "no credential in configuration"
    );
}

#[tokio::test]
async fn the_example_deploys_runs_and_cleans_up() {
    let fake = FakeCloudflare::start().await;
    let provider = fake.provider();
    let module = std::fs::read(example().join("hello.wasm")).unwrap();
    let deployment = provider.deploy(&module).await.unwrap();

    let spec = std::fs::read_to_string(example().join("workload.json")).unwrap();
    let spec = serde_json::from_str(&spec).unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("hello.wasm"), &module).unwrap();
    let bundle = compute_core::WorkloadBundle::create_from(spec, root.path()).unwrap();
    let response = provider
        .execute(ProviderRequest::bundle(bundle.to_bytes().unwrap()))
        .await
        .unwrap();
    assert_eq!(
        response.result.stdout.text,
        "hello from Compute on Cloudflare Workers\n"
    );
    response.result.receipt.unwrap().verify().unwrap();
    let deleted = provider.delete(&deployment.worker).await.unwrap();
    assert_eq!(deleted.result, DeletionResult::Deleted);
    assert!(fake.scripts().is_empty());
}
