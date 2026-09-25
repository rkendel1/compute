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
