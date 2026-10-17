//! Named Cloudflare execution profiles.
//!
//! A profile is what a Cloudflare plan actually provides to Compute
//! workloads, as Cloudflare documents it. It is provider data, kept apart
//! from Compute's portable contract: the adapter derives its capability
//! report from the profile, and refuses anything the profile does not
//! offer. Cloudflare can change its plans; the profile records the date its
//! limits were checked and where they come from.

use serde::{Deserialize, Serialize};

pub const PROFILE_VERSION: &str = "compute.cloudflare.profile@1";
pub const WORKERS_FREE: &str = "workers-free";

const MIB: u64 = 1024 * 1024;

/// Limits Cloudflare enforces on the plan. Compute cannot raise any of
/// them; a configured limit above one is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanLimits {
    /// CPU time per HTTP invocation. Cloudflare stops the invocation with
    /// error 1102 when it is exceeded.
    pub cpu_ms_per_invocation: u64,
    /// Requests per day across the account, reset at 00:00 UTC. Beyond it,
    /// Cloudflare answers with error 1027.
    pub requests_per_day: u64,
    /// Memory per isolate, JavaScript heap and WebAssembly together.
    pub memory_bytes: u64,
    /// Uncompressed size of one Worker (the harness and the module).
    pub worker_size_bytes: u64,
    pub workers_per_account: u64,
    pub subrequests_per_invocation: u64,
    pub request_body_bytes: u64,
    pub environment_variables_per_worker: u64,
    pub startup_ms: u64,
}

/// What a workload can and cannot use on this profile. Every `false` here
/// is reported as an explicit unsupported capability, never approximated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileCapabilities {
    /// Request-driven execution: one invocation, one result.
    pub request_driven: bool,
    /// WASI (preview 1) modules run by the Compute harness Worker.
    pub wasm_modules: bool,
    /// A process that outlives the request (a server, a daemon).
    pub persistent_processes: bool,
    /// An interactive shell or terminal.
    pub interactive_shell: bool,
    /// Arbitrary native executables (ELF, Mach-O).
    pub native_executables: bool,
    /// Linux containers. Cloudflare Containers is a separate, paid product
    /// with its own provider contract; Workers does not run containers.
    pub containers: bool,
    /// Language runtimes Compute runs as processes (node, python, ...).
    pub process_runtimes: bool,
    /// A writable filesystem: inputs, outputs, artifacts.
    pub filesystem: bool,
    /// Outbound network from the workload. The harness exposes no sockets.
    pub outbound_network: bool,
    /// Local model inference.
    pub local_inference: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudflareProfile {
    pub profile_version: String,
    pub name: String,
    pub plan: String,
    /// The Cloudflare product this profile uses. Not a VM, not a container.
    pub product: String,
    /// When the limits below were last checked against Cloudflare's
    /// documentation.
    pub limits_checked: String,
    pub source: String,
    pub limits: PlanLimits,
    pub capabilities: ProfileCapabilities,
    /// Said wherever the profile is shown.
    pub notes: Vec<String>,
}

impl CloudflareProfile {
    /// The named profile, or `None` when no such profile exists.
    pub fn named(name: &str) -> Option<Self> {
        (name == WORKERS_FREE).then(Self::workers_free)
    }

    pub fn names() -> &'static [&'static str] {
        &[WORKERS_FREE]
    }

    /// Cloudflare Workers on the Free plan, as documented at
    /// <https://developers.cloudflare.com/workers/platform/limits/>.
    pub fn workers_free() -> Self {
        Self {
            profile_version: PROFILE_VERSION.into(),
            name: WORKERS_FREE.into(),
            plan: "Workers Free".into(),
            product: "Cloudflare Workers".into(),
            limits_checked: "2026-10-17".into(),
            source: "https://developers.cloudflare.com/workers/platform/limits/".into(),
            limits: PlanLimits {
                cpu_ms_per_invocation: 10,
                requests_per_day: 100_000,
                memory_bytes: 128 * MIB,
                worker_size_bytes: 64 * MIB,
                workers_per_account: 100,
                subrequests_per_invocation: 50,
                request_body_bytes: 100 * 1_000_000,
                environment_variables_per_worker: 64,
                startup_ms: 1_000,
            },
            capabilities: ProfileCapabilities {
                request_driven: true,
                wasm_modules: true,
                persistent_processes: false,
                interactive_shell: false,
                native_executables: false,
                containers: false,
                process_runtimes: false,
                filesystem: false,
                outbound_network: false,
                local_inference: false,
            },
            notes: vec![
                "Short-lived, request-driven WASI modules only. A Worker is a V8 isolate, not a Linux VM or a container.".into(),
                "Cloudflare enforces 10 ms of CPU time per invocation and 100,000 requests per day on this plan; Compute reports exhaustion as quota_exhausted and never moves the workload elsewhere.".into(),
                "Cloudflare may change plan limits. You remain responsible for your Cloudflare account and its usage; this profile does not make execution free or unlimited.".into(),
            ],
        }
    }

    /// The capabilities this profile refuses, by their Compute names.
    pub fn unsupported(&self) -> Vec<&'static str> {
        let capabilities = &self.capabilities;
        [
            (capabilities.persistent_processes, "persistent_process"),
            (capabilities.interactive_shell, "interactive_shell"),
            (capabilities.native_executables, "native_executable"),
            (capabilities.containers, "container"),
            (capabilities.process_runtimes, "process_runtime"),
            (capabilities.filesystem, "filesystem"),
            (capabilities.outbound_network, "outbound_network"),
            (capabilities.local_inference, "local_inference"),
        ]
        .into_iter()
        .filter(|(supported, _)| !supported)
        .map(|(_, name)| name)
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_free_profile_offers_only_request_driven_wasm() {
        let profile = CloudflareProfile::named(WORKERS_FREE).unwrap();
        assert!(profile.capabilities.request_driven && profile.capabilities.wasm_modules);
        assert_eq!(
            profile.unsupported(),
            [
                "persistent_process",
                "interactive_shell",
                "native_executable",
                "container",
                "process_runtime",
                "filesystem",
                "outbound_network",
                "local_inference"
            ]
        );
        assert_eq!(profile.limits.cpu_ms_per_invocation, 10);
        assert_eq!(profile.limits.requests_per_day, 100_000);
        assert!(CloudflareProfile::named("workers-paid").is_none());
    }
}
