//! An external worker adapter: run one ephemeral GitHub Actions runner job
//! under Compute's execution lifecycle.
//!
//! This crate is the only place GitHub Actions appears. Compute's core,
//! runtime, provider, placement and environment crates do not know about it
//! (`crates/compute-cli/tests/architecture.rs` enforces that). It composes
//! what exists: the `shell` runtime, the execution receipt, and the runtime's
//! timeout, cancellation and cleanup.

mod error;
mod github;
mod runner;
mod secret;

pub use error::WorkerError;
pub use github::{GitHubApi, Repository, RestGitHubApi};
pub use runner::{
    CleanupEvidence, DEFAULT_CREDENTIAL_ENV, DEFAULT_SERVER_URL, DEFAULT_TIMEOUT, JobEvidence,
    METADATA_OUTPUT, RECIPE_NAME, REGISTRATION_TOKEN_ENV, RecipeEvidence, RunnerArch,
    RunnerIdentity, RunnerOs, RunnerReport, RunnerRun, RunnerSpec, RunnerWorker,
};
pub use secret::{REDACTED, Secret, scrub};
