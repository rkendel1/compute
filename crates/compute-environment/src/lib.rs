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
pub mod auth;
pub mod client;
pub mod daemon;
pub mod identity;
pub mod manifest;
pub mod model;
pub mod status;
pub mod tls;

pub use compute_network::acme::AcmeConfig;
pub use compute_network::dns::DnsProviderConfig;
pub use daemon::{Daemon, DaemonConfig, EventFilter, NetworkConfig};
pub use model::*;
pub use status::*;

#[derive(Debug, thiserror::Error)]
pub enum EnvironmentError {
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("not found: {0}")]
    NotFound(String),
    /// The Compute API has no such operation.
    #[error("no such operation: {0}")]
    NoRoute(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("admission denied: {0}")]
    Denied(String),
    /// The request carried no valid credential.
    #[error("authentication failed: {0}")]
    Unauthorized(String),
    /// The credential is valid but lacks the scope the operation needs.
    #[error("authorization denied: {0}")]
    Forbidden(String),
    /// The runtime or provider could not run the workload. This is an
    /// infrastructure failure, not the workload's.
    #[error("runtime unavailable: {0}")]
    RuntimeUnavailable(String),
    /// The invocation was stopped before it ran.
    #[error("cancelled: {0}")]
    Cancelled(String),
    /// The Compute controller cannot be reached.
    #[error("controller unavailable: {0}")]
    ControllerUnavailable(String),
    /// A binary upgrade did not complete; the previous controller serves.
    #[error("upgrade failed: {0}")]
    UpgradeFailed(String),
    /// The durable control state cannot be reached. Compute fails closed:
    /// nothing is changed until it can.
    #[error("control state unavailable: {0}")]
    Unavailable(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Compute(#[from] compute_core::ComputeError),
}

impl From<compute_state::StateError> for EnvironmentError {
    fn from(error: compute_state::StateError) -> Self {
        use compute_state::StateError;
        match error {
            StateError::Conflict { .. } | StateError::Precondition { .. } => Self::Conflict(
                format!("{error}; another change landed first, retry against the current state"),
            ),
            StateError::NotFound { .. } => Self::NotFound(error.to_string()),
            StateError::Invalid(message) => Self::Invalid(message),
            StateError::Unavailable(message) => Self::Unavailable(message),
        }
    }
}

impl EnvironmentError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Invalid(_) | Self::Json(_) | Self::Compute(_) => "invalid",
            Self::NotFound(_) => "not_found",
            Self::NoRoute(_) => "no_route",
            Self::Conflict(_) => "conflict",
            Self::Denied(_) => "admission_denied",
            Self::Unauthorized(_) => "authentication_failed",
            Self::Forbidden(_) => "authorization_denied",
            Self::RuntimeUnavailable(_) => "runtime_unavailable",
            Self::Cancelled(_) => "cancelled",
            Self::ControllerUnavailable(_) => "controller_unavailable",
            Self::UpgradeFailed(_) => "upgrade_failed",
            Self::Unavailable(_) => "state_unavailable",
            Self::Io(_) => "io",
        }
    }

    /// The detail without the kind prefix; the API carries the kind
    /// separately.
    pub fn message(&self) -> String {
        match self {
            Self::Invalid(message)
            | Self::NotFound(message)
            | Self::NoRoute(message)
            | Self::Conflict(message)
            | Self::Denied(message)
            | Self::Unauthorized(message)
            | Self::Forbidden(message)
            | Self::RuntimeUnavailable(message)
            | Self::Cancelled(message)
            | Self::ControllerUnavailable(message)
            | Self::UpgradeFailed(message)
            | Self::Unavailable(message) => message.clone(),
            other => other.to_string(),
        }
    }

    pub fn status(&self) -> u16 {
        match self {
            Self::Invalid(_) | Self::Json(_) | Self::Compute(_) => 400,
            Self::Unauthorized(_) => 401,
            Self::Denied(_) | Self::Forbidden(_) => 403,
            Self::Cancelled(_) => 409,
            Self::UpgradeFailed(_) => 500,
            Self::RuntimeUnavailable(_) | Self::ControllerUnavailable(_) => 503,
            Self::NotFound(_) | Self::NoRoute(_) => 404,
            Self::Conflict(_) => 409,
            Self::Io(_) => 500,
            Self::Unavailable(_) => 503,
        }
    }
}
