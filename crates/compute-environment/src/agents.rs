//! Agent runtimes a *configured* distribution supplies, read from its own
//! profile.
//!
//! Compute itself is agent-runtime agnostic: it executes workloads and knows
//! nothing about agents. A configured distribution (`compute-configured`)
//! ships one — Chip — and declares it in its distribution profile
//! (`compute.distribution-profile@1`, the `agent` section), the same profile
//! that already declares managed services.
//!
//! This module therefore reads a declaration it never writes, never installs,
//! and never requires. Base Compute has no profile, so [`configured`] returns
//! an empty view and the controller advertises no agent runtime at all.
//!
//! ```text
//! OpenDots ──discovers──▶ GET /info ──▶ agents[]
//!                                      └─ chip @appport/chip 0.54.3 (default)
//! ```
//!
//! The distinction is deliberate and load-bearing: an *agent runtime* is the
//! capability that runs an agent (here, Chip). The *agent* — a Dot, its
//! context, its approvals — belongs to the control plane above and is never
//! stored here.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Where a configured distribution declares that it is installed. The Homebrew
/// formula sets this to its `libexec`, which is where `stack.json`, the pinned
/// `node_modules` and the bundled Node runtime live.
pub const PROFILE_HOME_ENV: &str = "COMPUTE_CONFIGURED_HOME";

/// The distribution profile's agent section, exactly as a configured
/// distribution declares it. Absent on base Compute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProfile {
    /// Name of the runtime an execution uses when nothing else is asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default)]
    pub runtimes: Vec<AgentRuntime>,
}

/// One agent runtime the configured distribution ships.
///
/// This is a *capability contract*, not a handle: it names what is available
/// and how to prove it, and it is read from an immutable artifact rather than
/// probed or resolved at run time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRuntime {
    /// Short, stable identifier an execution refers to (`chip`).
    pub name: String,
    /// The public npm identity. This is what must be pinned: the package's
    /// historical internal entry file name is an implementation detail and is
    /// deliberately not part of this contract.
    pub package: String,
    pub version: String,
    /// Executable path relative to the installed distribution root.
    pub executable: String,
    /// The runtime that executes it, relative to the distribution root. Chip
    /// requires Node >= 24, so this is the bundled Node and never the host's:
    /// a configured distribution ships the runtime its pinned packages declare.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// Whether this is the distribution's default agent runtime.
    #[serde(default)]
    pub default: bool,
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// `execution`: the runtime is invoked inside a Compute execution and does
    /// not outlive it. Compute owns the execution lifecycle; the runtime owns
    /// only the agent loop within it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<AgentHealth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
}

/// How to prove a declared runtime is really present and working.
///
/// `invocation` runs the executable and compares its output. This is the right
/// check for a runtime with no listening socket: readiness is "it answers",
/// not "something accepts a connection".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentHealth {
    pub kind: String,
    pub command: Vec<String>,
    pub expect: String,
}

/// What a controller advertises about agent runtimes.
///
/// `default` is `None` when the installation declares none, which is the base
/// Compute case and is reported as an absent capability rather than an error.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default)]
    pub runtimes: Vec<AgentRuntime>,
}

impl AgentCapabilities {
    /// The runtime an execution uses by default, if this installation has one.
    pub fn default_runtime(&self) -> Option<&AgentRuntime> {
        let name = self.default.as_deref()?;
        self.runtimes.iter().find(|runtime| runtime.name == name)
    }

    /// Whether any agent runtime is available at all. Base Compute: `false`.
    pub fn is_empty(&self) -> bool {
        self.runtimes.is_empty()
    }
}

/// The installed distribution's root, if this is a configured installation.
///
/// A missing `stack.json` means "not configured", so base Compute never
/// pretends to ship an agent runtime.
pub fn distribution_home() -> Option<PathBuf> {
    std::env::var_os(PROFILE_HOME_ENV)
        .map(PathBuf::from)
        .filter(|path| path.join("stack.json").is_file())
}

/// Read the agent runtimes the installed distribution declares.
///
/// An absent, unreadable, or `agent`-less profile yields no runtimes rather
/// than an error: the controller must start and answer on base Compute, where
/// there is nothing to declare.
pub fn declared(home: &Path) -> AgentCapabilities {
    let profile = home.join("stack.json");
    let Ok(text) = std::fs::read_to_string(&profile) else {
        return AgentCapabilities::default();
    };
    #[derive(Deserialize)]
    struct Profile {
        #[serde(default)]
        agent: Option<AgentProfile>,
    }
    let Ok(profile) = serde_json::from_str::<Profile>(&text) else {
        return AgentCapabilities::default();
    };
    let Some(agent) = profile.agent else {
        return AgentCapabilities::default();
    };
    // A profile that names a default must actually declare it; otherwise the
    // default is dropped rather than advertised as something that cannot run.
    let default = agent
        .default
        .filter(|name| agent.runtimes.iter().any(|runtime| &runtime.name == name));
    AgentCapabilities {
        default,
        runtimes: agent.runtimes,
    }
}

/// The agent runtimes this controller advertises, from the installed
/// distribution profile if there is one.
pub fn configured() -> AgentCapabilities {
    distribution_home()
        .map(|home| declared(&home))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHIP: &str = r#"{"default":"chip","runtimes":[
        {"name":"chip","package":"@appport/chip","version":"0.54.3",
         "executable":"node_modules/.bin/chip","default":true}]}"#;

    /// A profile directory that lives as long as the test.
    fn profile(agent: &str) -> PathBuf {
        let home = tempfile::tempdir().expect("tempdir");
        let root = home.path().to_path_buf();
        std::fs::write(
            root.join("stack.json"),
            format!(r#"{{"format":"compute.distribution-profile@1","agent":{agent}}}"#),
        )
        .expect("write profile");
        std::mem::forget(home);
        root
    }

    #[test]
    fn a_profile_without_an_agent_section_declares_nothing() {
        let capabilities = declared(&profile(""));
        assert!(capabilities.is_empty());
        assert_eq!(capabilities.default_runtime(), None);
    }

    #[test]
    fn a_missing_profile_declares_nothing_rather_than_failing() {
        let home = tempfile::tempdir().expect("tempdir");
        assert!(declared(home.path()).is_empty());
    }

    #[test]
    fn an_unreadable_profile_declares_nothing_rather_than_failing() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::write(home.path().join("stack.json"), "{ not json").unwrap();
        assert!(declared(home.path()).is_empty());
    }

    #[test]
    fn a_declared_default_runtime_is_resolvable() {
        let capabilities = declared(&profile(CHIP));
        assert_eq!(capabilities.default.as_deref(), Some("chip"));
        let runtime = capabilities.default_runtime().expect("chip is default");
        assert_eq!(runtime.package, "@appport/chip");
        assert_eq!(runtime.version, "0.54.3");
        assert!(runtime.default);
    }

    #[test]
    fn a_default_naming_an_undeclared_runtime_is_not_advertised() {
        let capabilities = declared(&profile(
            r#"{"default":"absent","runtimes":[
                {"name":"chip","package":"@appport/chip","version":"0.54.3",
                 "executable":"node_modules/.bin/chip"}]}"#,
        ));
        assert_eq!(capabilities.default, None);
        assert_eq!(capabilities.default_runtime(), None);
        // The runtime that does exist is still discoverable.
        assert_eq!(capabilities.runtimes.len(), 1);
    }

    #[test]
    fn the_package_identity_is_the_contract_not_the_entry_file_name() {
        // Chip's published bin target is bin/eve.js. That filename is Chip's
        // internal history; what callers depend on is the public package
        // identity, so that is what this view carries.
        let serialized = serde_json::to_string(&declared(&profile(CHIP))).expect("serialize");
        assert!(serialized.contains("@appport/chip"));
        assert!(!serialized.contains("chip-framework"));
        assert!(!serialized.contains("node_modules/eve"));
    }

    #[test]
    fn base_compute_declares_no_agent_runtime() {
        // Without COMPUTE_CONFIGURED_HOME there is no profile to read, which is
        // exactly how a base Compute installation presents itself.
        if distribution_home().is_none() {
            assert!(configured().is_empty());
        }
    }
}
