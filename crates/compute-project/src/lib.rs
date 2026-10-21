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
mod stack_inventory;
mod stack_lock;

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
    Acceptance, Component, CopyOrigin, Divergence, FINDING_KINDS, PROBE, PROBE_ENTRYPOINT,
    Relationship, STACK_FILE, STACKS_ENV, SourceCopy, Stack, StackSource, WASM_PROBE,
    WASM_PROBE_ENTRYPOINT, find_stack, find_stack_file, parse_probe_report, pin_pattern,
    probe_identity, wasm_probe_identity,
};
pub use stack_inventory::{
    Canonical, ConsumerPlan, Finding, Inventory, InventoryPackage, PackageUse, TreeEntry,
    UpgradePlan, range_satisfies, upgrade_plan,
};
pub use stack_lock::{
    FindingChange, LOCK_FILE, LOCK_FORMAT, LockDiff, LockedPackage, LockedSource, LockedStack,
    Overrides, PackageChange, Provenance, PublishedPackage, RECORDED_FILES, RelationshipChange,
    RelationshipResult, SourceChange, StackLock, StackResolver, diff as diff_locks, extract_pin,
    highest_satisfying, resolve as resolve_stack, sha256_hex, short as short_commit,
};
