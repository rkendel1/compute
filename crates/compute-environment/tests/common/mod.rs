//! Test support shared by the daemon suites.

use compute_provider::LocalProvider;

/// A local provider whose managed runtimes come from a host-backed fixture
/// catalog, so no test downloads a runtime. Each call gets its own catalog,
/// and therefore its own runtime store.
pub fn provider() -> LocalProvider {
    LocalProvider::new().with_runtime_catalog(catalog())
}

/// A fresh host-backed fixture runtime catalog.
pub fn catalog() -> compute_provider::RuntimeCatalog {
    let directory = tempfile::tempdir().expect("temporary directory").keep();
    compute_provider::testing::host_fixture_catalog(&directory)
        .expect("fixture runtime catalog")
        .catalog
}

/// A consumer's wait for readiness: returns once Compute admits workloads
/// to the environment (`ready` or `degraded`), and panics with Compute's
/// own explanation if it does not within the deadline.
#[allow(dead_code)]
pub async fn wait_admitting(daemon: &compute_environment::Daemon, name: &str) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        let readiness = daemon.computer(name).await.map(|view| view.readiness);
        if let Ok(readiness) = &readiness
            && readiness.state.admits_workloads()
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{name} never admitted workloads: {readiness:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
