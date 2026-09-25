//! The Compute control-state boundary.
//!
//! The daemon remains the execution authority; this crate defines what it
//! keeps durably — desired state and evidence — and the interface through
//! which it keeps it. Backends are separate crates:
//!
//! - `compute-state-memory`: in-process, for tests and ephemeral daemons
//! - `compute-state-file`: one JSON document on local disk
//! - `compute-state-feltdb`: Managed FeltDB, the durable control-plane
//!   authority
//!
//! Nothing here depends on a particular backend, and no backend is a
//! dependency of the execution engine.

pub mod artifacts;
pub mod conformance;
pub mod control;
pub mod model;
pub mod snapshot;
pub mod store;
pub mod time;

pub use artifacts::{ArtifactStore, StateArtifacts};
pub use control::{Batch, ControlState, Stored};
pub use model::*;
pub use snapshot::{
    Coherence, Incoherent, Refreshed, Snapshot, SnapshotDefinition, SnapshotHandle, SnapshotReport,
    SnapshotSource, Validation,
};
pub use store::{
    AccessReport, BackendInfo, Collection, Comparison, Filter, ID_FIELD, Query, Record, Revision,
    StateError, StateStore, Tables, Write, apply_in_memory,
};
