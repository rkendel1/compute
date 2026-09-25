//! Dependency-capsule placement: identity is authoritative, capsules are
//! either transferred and verified or already resident, and there is no host
//! dependency fallback. This binary owns `COMPUTE_DEPENDENCY_CACHE`.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::Utc;
use compute_core::{
    DependencyCapsule, IsolationRequirement, NetworkPolicy, PlatformIdentity, ResourceLimits,
    RuntimeKind, WORKLOAD_SPEC_VERSION, WorkloadBundle, WorkloadDependencies, WorkloadSpec,
};
use compute_placement::{
    CapabilityCache, DiscoveryMode, EvaluationStatus, PlacementRequirements, PoolPolicy,
    ProviderConfig, ProviderKind, ProviderPool, ReasonCode, RequirementOptions, SubmissionMode,
    dispatch, place,
};
use compute_provider::{LocalProvider, ProviderRequest, RemoteProvider, ServerConfig};

struct Fixture {
    kind: RuntimeKind,
    entrypoint: &'static str,
    source: &'static str,
}

const PYTHON: Fixture = Fixture {
    kind: RuntimeKind::Python,
    entrypoint: "main.py",
    source: "import placed_dependency\nprint(placed_dependency.VALUE, end='')\n",
};

const NODE: Fixture = Fixture {
    kind: RuntimeKind::Node,
    entrypoint: "main.js",
    source: "process.stdout.write(require('placed_dependency').value);\n",
};

fn capsule(fixture: &Fixture, version: &str, value: &str) -> DependencyCapsule {
    let payload = tempfile::tempdir().unwrap();
    match fixture.kind {
        RuntimeKind::Python => std::fs::write(
            payload.path().join("placed_dependency.py"),
            format!("VALUE = '{value}'\n"),
        )
        .unwrap(),
        _ => {
            let module = payload.path().join("placed_dependency");
            std::fs::create_dir(&module).unwrap();
            std::fs::write(
                module.join("index.js"),
                format!("exports.value = '{value}';\n"),
            )
            .unwrap();
        }
    }
    DependencyCapsule::create(
        payload.path(),
        fixture.kind,
        Some(version.into()),
        PlatformIdentity::current(),
        vec![],
        None,
    )
    .unwrap()
}

fn bundle(fixture: &Fixture, capsule: &DependencyCapsule, embed: bool) -> WorkloadBundle {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(fixture.entrypoint), fixture.source).unwrap();
    let spec = WorkloadSpec {
        version: WORKLOAD_SPEC_VERSION.into(),
        runtime: fixture.kind,
        runtime_version: None,
        architecture: None,
        entrypoint: fixture.entrypoint.into(),
        args: vec![],
        env: BTreeMap::new(),
        inputs: vec![],
        outputs: vec![],
        resources: ResourceLimits::default(),
        network: NetworkPolicy::Network,
        isolation: IsolationRequirement::default(),
        dependencies: Some(WorkloadDependencies {
            capsule: capsule.capsule_id().unwrap(),
        }),
    };
    WorkloadBundle::create_from_with_capsule(spec, root.path(), embed.then(|| capsule.clone()))
        .unwrap()
}

fn prepare(
    bundle: &WorkloadBundle,
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
        SubmissionMode::Synchronous,
        &RequirementOptions::default(),
    )
    .unwrap();
    let admission = compute_placement::AdmissionContext::new(
        &[],
        compute_policy::ExecutionContract::from_bundle(bundle, None).unwrap(),
    );
    (request, requirements, admission)
}

async fn discover(pool: &ProviderPool) -> Vec<compute_placement::DiscoveryRecord> {
    let mut cache = CapabilityCache::default();
    pool.capabilities(&mut cache, DiscoveryMode::Refresh, None, Utc::now())
        .await
}

fn write_to_cache(root: &std::path::Path, capsule: &DependencyCapsule) {
    let id = capsule.capsule_id().unwrap();
    capsule
        .write(&root.join(format!("{}.deps", id.trim_start_matches("sha256:"))))
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dependency_capsule_matrix() {
    let cache_root = tempfile::tempdir().unwrap();
    // SAFETY: this test binary contains a single test; nothing else reads
    // the environment concurrently.
    unsafe { std::env::set_var("COMPUTE_DEPENDENCY_CACHE", cache_root.path()) };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let jobs = tempfile::tempdir().unwrap();
    // Managed runtimes come from a host-backed fixture catalog: both
    // providers offer the same pinned versions without a download.
    let catalog_directory = tempfile::tempdir().unwrap();
    let catalog = compute_provider::testing::host_fixture_catalog(catalog_directory.path())
        .unwrap()
        .catalog;
    let mut config = ServerConfig::local(endpoint.clone());
    config.provider = Arc::new(
        LocalProvider::with_identity(compute_core::ProviderIdentity::Remote {
            id: endpoint.clone(),
            endpoint: endpoint.clone(),
        })
        .with_runtime_catalog(catalog.clone()),
    );
    config.job_store = jobs.path().to_path_buf();
    let server = tokio::spawn(async move {
        let _ = compute_provider::serve_listener(listener, config).await;
    });
    let mut pool = ProviderPool::new(PoolPolicy::default());
    pool.add(
        "local",
        ProviderConfig {
            kind: ProviderKind::Local,
            endpoint: None,
            application_endpoint: None,
            priority: 0,
            token_env: None,
        },
        Arc::new(LocalProvider::new().with_runtime_catalog(catalog)),
    )
    .unwrap();
    pool.add_remote(
        "remote",
        ProviderConfig {
            kind: ProviderKind::Remote,
            endpoint: Some(endpoint.clone()),
            application_endpoint: None,
            priority: 10,
            token_env: None,
        },
        Arc::new(RemoteProvider::new(endpoint.clone())),
    )
    .unwrap();

    // Prepare each offered runtime first, so the version a provider offers
    // is the version it observes, which is what a capsule is checked
    // against at execution.
    for member in pool.members() {
        for kind in [PYTHON.kind, NODE.kind] {
            let resolution = member
                .provider
                .resolve_runtime(compute_core::ProviderRuntimeRequirement {
                    runtime: kind,
                    version: None,
                    platform: None,
                })
                .await
                .unwrap();
            if let Some(distribution) = resolution.distribution
                && resolution.status.can_satisfy()
            {
                member.provider.prepare_runtime(distribution).await.unwrap();
            }
        }
    }
    let offers = discover(&pool).await;
    let local = offers
        .iter()
        .find(|record| record.provider_id == "local")
        .and_then(|record| record.descriptor.clone())
        .unwrap();
    let mut exercised = 0;
    for fixture in [PYTHON, NODE] {
        // A capsule is built for the runtime version the providers offer.
        let Some(offer) = local.runtime(fixture.kind) else {
            eprintln!(
                "skipping {}: runtime unavailable on this host",
                fixture.kind
            );
            continue;
        };
        exercised += 1;
        let version = offer.effective_version().to_owned();
        let wanted = capsule(&fixture, &version, "packaged");
        let wrong = capsule(&fixture, &version, "impostor");
        // Present in transfer: embedded capsules are sent and verified.
        let embedded = bundle(&fixture, &wanted, true);
        let (request, requirements, admission) = prepare(&embedded);
        assert!(requirements.dependencies.as_ref().unwrap().embedded);
        let records = discover(&pool).await;
        let report = place(
            &pool.configs(),
            pool.policy(),
            &records,
            &requirements,
            &admission,
            None,
        );
        assert_eq!(
            report.compatible_providers,
            ["remote", "local"],
            "{:#?}",
            report.providers
        );
        let response = dispatch::execute(&pool, &report, request).await.unwrap();
        assert_eq!(response.result.stdout.text, "packaged", "{}", fixture.kind);
        let receipt = response.result.receipt.unwrap();
        report.verify_receipt(&receipt).unwrap();
        assert_eq!(
            receipt.dependencies.as_ref().unwrap().capsule_id,
            wanted.capsule_id().unwrap()
        );

        // Absent: referenced but neither embedded nor resident.
        let referenced = bundle(&fixture, &wanted, false);
        let (request, requirements, admission) = prepare(&referenced);
        let records = discover(&pool).await;
        let report = place(
            &pool.configs(),
            pool.policy(),
            &records,
            &requirements,
            &admission,
            None,
        );
        assert_eq!(
            report.failure.as_ref().unwrap().code,
            "no_compatible_provider"
        );
        for provider in &report.providers {
            assert_eq!(provider.status, EvaluationStatus::Incompatible);
            assert_eq!(
                provider.reasons[0].code,
                ReasonCode::DependencyCapsuleMissing
            );
        }
        assert!(
            dispatch::execute(&pool, &report, request.clone())
                .await
                .is_err()
        );

        // Wrong capsule resident: the same runtime version is not the same
        // dependency environment.
        write_to_cache(cache_root.path(), &wrong);
        let records = discover(&pool).await;
        let report = place(
            &pool.configs(),
            pool.policy(),
            &records,
            &requirements,
            &admission,
            None,
        );
        assert!(
            report
                .providers
                .iter()
                .all(|provider| provider.reasons[0].code == ReasonCode::DependencyCapsuleMismatch)
        );

        // Resident: the exact capsule is present and verified at execution.
        write_to_cache(cache_root.path(), &wanted);
        let records = discover(&pool).await;
        let report = place(
            &pool.configs(),
            pool.policy(),
            &records,
            &requirements,
            &admission,
            None,
        );
        assert_eq!(report.selected.as_ref().unwrap().provider_id, "local");
        let response = dispatch::execute(&pool, &report, request).await.unwrap();
        assert_eq!(response.result.stdout.text, "packaged");
        report
            .verify_receipt(response.result.receipt.as_ref().unwrap())
            .unwrap();

        for entry in std::fs::read_dir(cache_root.path()).unwrap() {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
        }
    }
    if std::env::var("COMPUTE_REQUIRE_ALL_RUNTIMES").as_deref() == Ok("1") {
        assert_eq!(exercised, 2, "Python and Node are required");
    }
    server.abort();
}
