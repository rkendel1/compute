//! A host-backed fixture runtime catalog for every `compute` process a test
//! spawns, so no test downloads a runtime (see
//! `compute_provider::testing`).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The catalog file, written once per test binary.
pub fn catalog() -> &'static Path {
    static CATALOG: OnceLock<PathBuf> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let directory = tempfile::tempdir().unwrap().keep();
        compute_provider::testing::host_fixture_catalog(&directory)
            .unwrap()
            .path
    })
}

/// Point a `compute` process at the fixture catalog, with a runtime store
/// of its own: a daemon (and the supervisor that inherits its environment)
/// prepares into it without racing other tests' daemons.
pub fn with_fixture_runtimes(command: &mut std::process::Command) -> &mut std::process::Command {
    let store = tempfile::tempdir().unwrap().keep();
    command
        .env("COMPUTE_RUNTIME_CATALOG", catalog())
        .env("COMPUTE_RUNTIME_STORE", store)
}
