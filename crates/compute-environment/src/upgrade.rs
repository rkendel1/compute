//! Upgrading the Compute controller without touching its workloads.
//!
//! ```text
//! controller vN ── validate artifact ── stop supervising (workloads keep
//!    │               (digest, version,     running on the supervisor) ──
//!    │                protocols)           release port and node lock
//!    ▼
//! controller vN+1 (same arguments and environment)
//!    ├── reach the supervisor (same protocol, or refuse)
//!    ├── reattach every unit that was running (or refuse)
//!    └── ready ──▶ upgrade.completed; vN exits
//!
//! vN+1 refuses, dies, or is not ready in time
//!    └── vN stops it and starts itself again ──▶ upgrade.rolled_back
//! ```
//!
//! The record of an upgrade lives on the node, in `<state_dir>/upgrade.json`,
//! because it describes this node's controller process, not desired state.
//! Every binary taken over is kept in `<state_dir>/binaries/`, so a later
//! rollback runs exactly the build that ran before.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::EnvironmentError;

/// A Compute build, as it describes itself (`compute version --json`) and
/// as its bytes hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildIdentity {
    pub path: String,
    pub version: String,
    pub git_commit: String,
    /// SHA-256 of the executable.
    pub build_id: String,
    pub platform: String,
    pub api: String,
    pub supervisor_protocol: u32,
}

/// An operator's request to replace the controller.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpgradeRequest {
    /// The new Compute executable, on this node.
    pub artifact: String,
    /// Refuse unless the artifact has exactly this SHA-256.
    #[serde(default)]
    pub expect_sha256: Option<String>,
    /// How long the new controller has to become ready, in seconds.
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

/// Where an upgrade stands. `started` → `completed`, or `started` →
/// `rolling_back` → `rolled_back`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeRecord {
    pub upgrade_id: String,
    /// `upgrade` or `rollback`.
    pub kind: String,
    pub status: String,
    pub from: BuildIdentity,
    pub to: BuildIdentity,
    /// Units running when it began; the new controller must reattach
    /// every one still running, or refuse.
    pub units: Vec<String>,
    pub timeout_seconds: u64,
    pub requested_by: String,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The controller instance serving after it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller: Option<String>,
}

impl UpgradeRecord {
    pub fn is_terminal(&self) -> bool {
        matches!(self.status.as_str(), "completed" | "rolled_back" | "failed")
    }
}

pub fn record_path(state_dir: &Path) -> PathBuf {
    state_dir.join("upgrade.json")
}

pub fn read_record(state_dir: &Path) -> Option<UpgradeRecord> {
    std::fs::read(record_path(state_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

pub fn write_record(state_dir: &Path, record: &UpgradeRecord) -> Result<(), EnvironmentError> {
    let path = record_path(state_dir);
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(record)?)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

/// This process's build.
pub fn current_build() -> BuildIdentity {
    let identity = crate::identity::ControllerIdentity::current();
    BuildIdentity {
        path: identity.executable,
        version: identity.version,
        git_commit: identity.git_commit,
        build_id: identity.build_id,
        platform: identity.platform,
        api: crate::api::API_VERSION.into(),
        supervisor_protocol: crate::dataplane::SUPERVISOR_PROTOCOL,
    }
}

/// Establish what an artifact is: its digest, and what it says about
/// itself when run. Fails closed on anything that does not look like a
/// compatible Compute: another platform, another API, another supervisor
/// protocol, or a digest other than the one expected.
pub fn inspect_artifact(
    artifact: &Path,
    expect_sha256: Option<&str>,
) -> Result<BuildIdentity, EnvironmentError> {
    let refused = |message: String| EnvironmentError::UpgradeFailed(message);
    let metadata = std::fs::metadata(artifact)
        .map_err(|error| refused(format!("{}: {error}", artifact.display())))?;
    if !metadata.is_file() {
        return Err(refused(format!("{} is not a file", artifact.display())));
    }
    let digest = compute_core::sha256_file_identity(artifact)
        .map_err(|error| refused(format!("{}: {error}", artifact.display())))?;
    if let Some(expected) = expect_sha256 {
        let expected = if expected.starts_with("sha256:") {
            expected.to_string()
        } else {
            format!("sha256:{expected}")
        };
        if expected != digest {
            return Err(refused(format!(
                "{} is {digest}, not the expected {expected}",
                artifact.display()
            )));
        }
    }
    let output = run_with_timeout(artifact, &["version", "--json"], Duration::from_secs(20))
        .map_err(|error| refused(format!("{} does not run: {error}", artifact.display())))?;
    let reported: serde_json::Value = serde_json::from_slice(&output).map_err(|error| {
        refused(format!(
            "{} is not a Compute build (version --json: {error})",
            artifact.display()
        ))
    })?;
    let text = |name: &str| reported[name].as_str().unwrap_or_default().to_string();
    let identity = BuildIdentity {
        path: artifact.display().to_string(),
        version: text("version"),
        git_commit: text("git_commit"),
        build_id: digest,
        platform: text("platform"),
        api: text("api"),
        supervisor_protocol: reported["supervisor_protocol"].as_u64().unwrap_or(0) as u32,
    };
    let current = current_build();
    if reported["name"] != "compute" {
        return Err(refused(format!(
            "{} does not identify as Compute",
            artifact.display()
        )));
    }
    if identity.platform != current.platform {
        return Err(refused(format!(
            "{} is built for {}, this node is {}",
            artifact.display(),
            identity.platform,
            current.platform
        )));
    }
    if identity.api != current.api {
        return Err(refused(format!(
            "{} serves {}, not {}; clients would break",
            artifact.display(),
            identity.api,
            current.api
        )));
    }
    if identity.supervisor_protocol != current.supervisor_protocol {
        return Err(refused(format!(
            "{} speaks supervisor protocol {}, this node's supervisor speaks {}; it could not reattach",
            artifact.display(),
            identity.supervisor_protocol,
            current.supervisor_protocol
        )));
    }
    Ok(identity)
}

fn run_with_timeout(program: &Path, args: &[&str], timeout: Duration) -> std::io::Result<Vec<u8>> {
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            let mut output = vec![];
            if let Some(mut stdout) = child.stdout.take() {
                std::io::Read::read_to_end(&mut stdout, &mut output)?;
            }
            return if status.success() {
                Ok(output)
            } else {
                Err(std::io::Error::other(format!("exited with {status}")))
            };
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "did not answer",
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Keep a build under the node's binaries, by build ID, and return where.
/// Rollbacks run from here, so they run exactly the build that ran.
pub fn keep_binary(
    state_dir: &Path,
    identity: &BuildIdentity,
) -> Result<PathBuf, EnvironmentError> {
    let digest = identity.build_id.trim_start_matches("sha256:");
    let directory = state_dir
        .join("binaries")
        .join(&digest[..digest.len().min(16)]);
    let path = directory.join("compute");
    if path.is_file()
        && compute_core::sha256_file_identity(&path).ok().as_deref()
            == Some(identity.build_id.as_str())
    {
        return Ok(path);
    }
    std::fs::create_dir_all(&directory)?;
    let temporary = directory.join("compute.tmp");
    std::fs::copy(&identity.path, &temporary)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&temporary, &path)?;
    if compute_core::sha256_file_identity(&path)? != identity.build_id {
        return Err(EnvironmentError::UpgradeFailed(format!(
            "the copy of {} kept at {} does not match its build ID",
            identity.path,
            path.display()
        )));
    }
    Ok(path)
}
