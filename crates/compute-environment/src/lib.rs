//! Persistent environments and the Compute daemon.
//!
//! ```text
//! Compute daemon
//! ├── Environment: preprod
//! │   ├── Project: authboundry ── workloads (services, tasks)
//! │   └── Project: factory
//! └── Environment: prod
//!     ├── Project: authboundry
//!     └── Project: factory
//! ```
//!
//! Environments, projects, and workloads have desired state; the daemon
//! reconciles actual state toward it. Every execution goes through the
//! existing admission and placement boundary. Stopping a child never stops
//! its parent or siblings, and the daemon outlives every lifecycle
//! operation.

pub mod api;
pub mod client;
pub mod daemon;
pub mod manifest;
pub mod model;
pub mod status;

pub use daemon::{Daemon, DaemonConfig};
pub use model::*;
pub use status::*;

#[derive(Debug, thiserror::Error)]
pub enum EnvironmentError {
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("admission denied: {0}")]
    Denied(String),
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Compute(#[from] compute_core::ComputeError),
}

impl EnvironmentError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Invalid(_) | Self::Json(_) | Self::Compute(_) => "invalid",
            Self::NotFound(_) => "not_found",
            Self::Conflict(_) => "conflict",
            Self::Denied(_) => "admission_denied",
            Self::Unauthorized(_) => "unauthorized",
            Self::Io(_) => "io",
        }
    }

    pub fn status(&self) -> u16 {
        match self {
            Self::Invalid(_) | Self::Json(_) | Self::Compute(_) => 400,
            Self::Unauthorized(_) => 401,
            Self::Denied(_) => 403,
            Self::NotFound(_) => 404,
            Self::Conflict(_) => 409,
            Self::Io(_) => 500,
        }
    }
}
