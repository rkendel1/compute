//! The boundary between project tools and Compute.
//!
//! A project tool (PAX) describes what software requires. This crate turns
//! that description into Compute's own [`compute_core::ProjectRequirements`]
//! and checks that an environment can honor them; Compute's planner,
//! placement, materialization, and receipts consume only Compute's types.
//!
//! Nothing here is authoritative state: every result is a pure function of
//! the project directory and the tool's observation of it.

mod app_bundle;
mod error;
mod pax;
mod plan;
mod stack;

pub use app_bundle::{
    APP_BUNDLE_PROTOCOL, SuppliedFile, bind_bundle, declared_bundle, parse_inspection,
    resolve_bundle,
};
pub use error::{FailureKind, ProjectError};
pub use pax::{PAX_ENV, PAX_SCHEMA_VERSION, PAX_SOURCE, PaxExecutable, PaxObservation, PaxSource};
pub use plan::{
    CommandSelection, DiscoveredProject, EnvironmentInputs, SelectedCommand, discover, materialize,
    select_command,
};
pub use stack::{
    Component, PROBE, PROBE_ENTRYPOINT, STACK_FILE, STACKS_ENV, Stack, WASM_PROBE,
    WASM_PROBE_ENTRYPOINT, find_stack, parse_probe_report, probe_identity, wasm_probe_identity,
};
