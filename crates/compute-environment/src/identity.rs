//! The running controller's identity: which Compute this is, established
//! once when the process starts.

use serde::{Deserialize, Serialize};

/// Which Compute binary is running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControllerIdentity {
    pub version: String,
    /// The source revision it was built from; `-dirty` when the tree had
    /// uncommitted changes.
    pub git_commit: String,
    /// SHA-256 of the executable itself: the build's identity.
    pub build_id: String,
    pub build_profile: String,
    pub platform: String,
    pub executable: String,
}

impl ControllerIdentity {
    /// This process's identity. The executable is hashed once per process.
    pub fn current() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").into(),
            git_commit: env!("COMPUTE_GIT_COMMIT").into(),
            build_id: compute_core::compute_executable_identity()
                .unwrap_or("unknown")
                .into(),
            build_profile: env!("COMPUTE_BUILD_PROFILE").into(),
            platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
            executable: std::env::current_exe()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
        }
    }
}
