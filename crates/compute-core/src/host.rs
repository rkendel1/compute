//! Host isolation for process runtimes.
//!
//! WASM is sandboxed by its runtime. Process runtimes (Python, Node, Bun,
//! Ruby, PHP, JVM, .NET, native, shell) are ordinary host processes; the
//! only boundary they can have is one the operating system enforces. A
//! host profile says how much of that boundary a workload gets:
//!
//! | Profile      | Filesystem                         | Network (declared `none`) | Memory, CPU, processes          |
//! |--------------|------------------------------------|---------------------------|---------------------------------|
//! | `trusted`    | unrestricted                       | not enforceable: refused  | declared limits only, if any    |
//! | `restricted` | writes only to the workspace       | no network (namespace)    | declared limits, enforced       |
//! | `isolated`   | reads the runtime and workspace, writes the workspace | no network (namespace) | limits always enforced (defaults when undeclared) |
//!
//! Every dimension reports what the host actually enforces:
//! `enforced`, `restricted`, `unrestricted`, `not_requested`, or
//! `unsupported`. A profile that needs something this host cannot
//! enforce is refused, never silently downgraded. Enforcement uses
//! Landlock (filesystem, TCP), a network namespace, cgroups (memory, CPU,
//! process count), resource limits, and `no_new_privs`.

use serde::{Deserialize, Serialize};

use crate::{NetworkPolicy, ResourceLimits};

/// How much of an operating-system boundary a process workload gets.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum HostProfile {
    /// Operator-controlled code: no boundary beyond the workspace.
    #[default]
    Trusted,
    /// Normal application workloads.
    Restricted,
    /// Untrusted workloads.
    Isolated,
}

impl HostProfile {
    pub const ALL: [Self; 3] = [Self::Trusted, Self::Restricted, Self::Isolated];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::Restricted => "restricted",
            Self::Isolated => "isolated",
        }
    }

    pub fn is_trusted(&self) -> bool {
        *self == Self::Trusted
    }
}

impl std::fmt::Display for HostProfile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::str::FromStr for HostProfile {
    type Err = crate::ComputeError;

    fn from_str(value: &str) -> crate::Result<Self> {
        Self::ALL
            .into_iter()
            .find(|profile| profile.as_str() == value)
            .ok_or_else(|| {
                crate::ComputeError::InvalidWorkload(format!(
                    "unknown host isolation profile {value}; profiles are trusted, restricted, isolated"
                ))
            })
    }
}

/// What the host enforces on one dimension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Enforcement {
    /// A hard limit the kernel enforces.
    Enforced,
    /// Confined to what the workload needs (for example, writes only to
    /// its workspace).
    Restricted,
    /// No boundary: the workload can do what its user can.
    Unrestricted,
    /// Nothing asked for.
    NotRequested,
    /// Asked for, and this host cannot enforce it.
    Unsupported,
}

/// What a profile enforces on this host, per dimension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostEnforcement {
    pub profile: HostProfile,
    pub filesystem: Enforcement,
    pub network: Enforcement,
    pub memory: Enforcement,
    pub cpu: Enforcement,
    pub process: Enforcement,
    /// The mechanisms used, for operators and auditors.
    pub mechanisms: Vec<String>,
    /// Limits applied, with defaults the profile supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_quota_percent: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_count: Option<u32>,
}

/// Defaults `isolated` applies when a workload declares no limit.
pub const ISOLATED_MEMORY_BYTES: u64 = 1024 * 1024 * 1024;
pub const ISOLATED_CPU_PERCENT: u32 = 100;
pub const ISOLATED_PROCESS_COUNT: u32 = 256;

/// Which cgroup hierarchy the host offers, and its controllers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CgroupSupport {
    /// `v1` or `v2`.
    pub version: String,
    pub memory: bool,
    pub cpu: bool,
    pub pids: bool,
    /// Where Compute creates its groups, per controller for v1.
    pub root: String,
}

/// What this host can enforce, detected once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostCapabilities {
    pub os: String,
    /// 0 when Landlock is unavailable.
    pub landlock_abi: u32,
    pub network_namespaces: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroups: Option<CgroupSupport>,
    pub resource_limits: bool,
    pub no_new_privs: bool,
    /// Whether this process runs as root: then `RLIMIT_NPROC` does not
    /// apply, so process counts need a cgroup.
    pub root: bool,
}

impl HostCapabilities {
    pub fn landlock_filesystem(&self) -> bool {
        self.landlock_abi >= 1
    }

    pub fn landlock_network(&self) -> bool {
        self.landlock_abi >= 4
    }

    fn cgroup(&self, controller: fn(&CgroupSupport) -> bool) -> bool {
        self.cgroups.as_ref().is_some_and(controller)
    }
}

/// This host's capabilities, detected on first use.
pub fn host_capabilities() -> &'static HostCapabilities {
    static CAPABILITIES: std::sync::OnceLock<HostCapabilities> = std::sync::OnceLock::new();
    CAPABILITIES.get_or_init(detect)
}

#[cfg(target_os = "linux")]
fn detect() -> HostCapabilities {
    // SAFETY: landlock_create_ruleset(NULL, 0, VERSION) only reports the ABI.
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<u8>(),
            0usize,
            1u32,
        )
    };
    HostCapabilities {
        os: "linux".into(),
        landlock_abi: u32::try_from(abi).unwrap_or(0),
        network_namespaces: probe_network_namespace(),
        cgroups: probe_cgroups(),
        resource_limits: true,
        no_new_privs: true,
        // SAFETY: geteuid has no preconditions.
        root: unsafe { libc::geteuid() } == 0,
    }
}

#[cfg(not(target_os = "linux"))]
fn detect() -> HostCapabilities {
    HostCapabilities {
        os: std::env::consts::OS.into(),
        landlock_abi: 0,
        network_namespaces: false,
        cgroups: None,
        resource_limits: cfg!(unix),
        no_new_privs: false,
        root: false,
    }
}

/// Whether a child can enter a new network namespace: tried, not assumed.
#[cfg(target_os = "linux")]
fn probe_network_namespace() -> bool {
    use std::os::unix::process::CommandExt;
    let Ok(true_path) = which_true() else {
        return false;
    };
    let mut command = std::process::Command::new(true_path);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: unshare is a single syscall, safe between fork and exec.
    unsafe {
        command.pre_exec(|| {
            if libc::unshare(libc::CLONE_NEWNET) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.status().is_ok_and(|status| status.success())
}

#[cfg(target_os = "linux")]
fn which_true() -> std::io::Result<std::path::PathBuf> {
    ["/bin/true", "/usr/bin/true"]
        .into_iter()
        .map(std::path::PathBuf::from)
        .find(|path| path.is_file())
        .ok_or_else(|| std::io::Error::other("no true"))
}

/// Cgroups Compute can create groups in: tried by creating and removing
/// one, not assumed from what is mounted.
#[cfg(target_os = "linux")]
fn probe_cgroups() -> Option<CgroupSupport> {
    // A controller is usable when Compute can create a group under it and
    // the group has the limit file (the root group never does).
    let probe = |directory: &std::path::Path, limit: &str| {
        let group = directory.join(format!("compute-probe-{}", std::process::id()));
        if std::fs::create_dir(&group).is_err() {
            return false;
        }
        let usable = group.join(limit).is_file();
        let _ = std::fs::remove_dir(&group);
        usable
    };
    let v1 = std::path::Path::new("/sys/fs/cgroup");
    let memory = v1.join("memory").is_dir() && probe(&v1.join("memory"), "memory.limit_in_bytes");
    let cpu = v1.join("cpu").is_dir() && probe(&v1.join("cpu"), "cpu.cfs_quota_us");
    let pids = v1.join("pids").is_dir() && probe(&v1.join("pids"), "pids.max");
    if memory || cpu || pids {
        return Some(CgroupSupport {
            version: "v1".into(),
            memory,
            cpu,
            pids,
            root: "/sys/fs/cgroup/{controller}/compute".into(),
        });
    }
    // cgroup v2: controllers must be available and delegatable here.
    let v2 = if v1.join("cgroup.controllers").is_file() {
        v1.to_path_buf()
    } else {
        v1.join("unified")
    };
    let controllers = std::fs::read_to_string(v2.join("cgroup.controllers")).unwrap_or_default();
    let has = |name: &str| {
        controllers
            .split_whitespace()
            .any(|controller| controller == name)
    };
    if (has("memory") || has("cpu") || has("pids")) && probe(&v2, "cgroup.procs") {
        return Some(CgroupSupport {
            version: "v2".into(),
            memory: has("memory"),
            cpu: has("cpu"),
            pids: has("pids"),
            root: v2.join("compute").display().to_string(),
        });
    }
    None
}

/// Why a profile cannot be satisfied here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostRefusal {
    pub code: String,
    pub message: String,
}

/// What `profile` enforces for a workload with this network policy and
/// these limits on a host with `host`'s capabilities — or why it cannot.
pub fn plan(
    profile: HostProfile,
    network: &NetworkPolicy,
    resources: &ResourceLimits,
    host: &HostCapabilities,
) -> Result<HostEnforcement, HostRefusal> {
    let refuse = |code: &str, message: String| HostRefusal {
        code: code.into(),
        message: format!("{profile} host isolation: {message}"),
    };
    let mut mechanisms = vec![];
    let confined = profile != HostProfile::Trusted;
    if confined && host.no_new_privs {
        mechanisms.push("no_new_privs".to_string());
    }
    // Filesystem.
    let filesystem = match profile {
        HostProfile::Trusted => Enforcement::Unrestricted,
        _ if !host.landlock_filesystem() => {
            return Err(refuse(
                "filesystem_isolation_unavailable",
                "Landlock is unavailable on this host".into(),
            ));
        }
        HostProfile::Restricted => {
            mechanisms.push(format!("landlock_fs_abi_{}", host.landlock_abi));
            Enforcement::Restricted
        }
        HostProfile::Isolated => {
            mechanisms.push(format!("landlock_fs_abi_{}", host.landlock_abi));
            Enforcement::Enforced
        }
    };
    // Network: a declared `network` stays reachable; `none` and
    // `localhost` need a namespace.
    let network = match (profile, network) {
        (_, NetworkPolicy::Network) => Enforcement::Unrestricted,
        (HostProfile::Trusted, _) => {
            return Err(refuse(
                "network_isolation_unavailable",
                format!("network {network} needs a restricted or isolated profile"),
            ));
        }
        (_, NetworkPolicy::None | NetworkPolicy::Localhost) if host.network_namespaces => {
            mechanisms.push(if *network == NetworkPolicy::Localhost {
                "network_namespace_loopback".to_string()
            } else {
                "network_namespace".to_string()
            });
            Enforcement::Enforced
        }
        (_, _) => {
            return Err(refuse(
                "network_isolation_unavailable",
                format!(
                    "network {network} needs network namespaces, which this host does not allow"
                ),
            ));
        }
    };
    // Memory.
    let memory_bytes = resources
        .memory_bytes
        .or((profile == HostProfile::Isolated).then_some(ISOLATED_MEMORY_BYTES));
    let memory = match memory_bytes {
        None => Enforcement::NotRequested,
        Some(_) if host.cgroup(|cgroup| cgroup.memory) => {
            mechanisms.push("cgroup_memory".into());
            Enforcement::Enforced
        }
        Some(_) if host.resource_limits && profile != HostProfile::Isolated => {
            mechanisms.push("rlimit_address_space".into());
            Enforcement::Enforced
        }
        Some(_) => {
            return Err(refuse(
                "memory_enforcement_unavailable",
                "a memory cgroup is unavailable".into(),
            ));
        }
    };
    // CPU: a CPU-time limit is an rlimit; `isolated` also caps its share.
    let cpu_quota_percent = (profile == HostProfile::Isolated).then_some(ISOLATED_CPU_PERCENT);
    let cpu = match (cpu_quota_percent, resources.cpu_time) {
        (Some(_), _) if host.cgroup(|cgroup| cgroup.cpu) => {
            mechanisms.push("cgroup_cpu_quota".into());
            if resources.cpu_time.is_some() {
                mechanisms.push("rlimit_cpu_time".into());
            }
            Enforcement::Enforced
        }
        (Some(_), _) => {
            return Err(refuse(
                "cpu_enforcement_unavailable",
                "a CPU cgroup is unavailable".into(),
            ));
        }
        (None, Some(_)) if host.resource_limits => {
            mechanisms.push("rlimit_cpu_time".into());
            Enforcement::Enforced
        }
        (None, Some(_)) => {
            return Err(refuse(
                "cpu_enforcement_unavailable",
                "CPU-time limits are unavailable".into(),
            ));
        }
        (None, None) => Enforcement::NotRequested,
    };
    // Processes: a count needs a pids cgroup (RLIMIT_NPROC does not bind
    // root); confined profiles also forbid gaining privileges.
    let process_count = resources
        .process_count
        .or((profile == HostProfile::Isolated).then_some(ISOLATED_PROCESS_COUNT));
    let process = match (process_count, profile) {
        (Some(_), _) if host.cgroup(|cgroup| cgroup.pids) => {
            mechanisms.push("cgroup_pids".into());
            Enforcement::Enforced
        }
        (Some(_), _) if host.resource_limits && !host.root => {
            mechanisms.push("rlimit_nproc".into());
            Enforcement::Enforced
        }
        (Some(_), _) => {
            return Err(refuse(
                "process_enforcement_unavailable",
                "a pids cgroup is unavailable".into(),
            ));
        }
        (None, HostProfile::Trusted) => Enforcement::Unrestricted,
        (None, _) => Enforcement::Restricted,
    };
    Ok(HostEnforcement {
        profile,
        filesystem,
        network,
        memory,
        cpu,
        process,
        mechanisms,
        memory_bytes: if memory == Enforcement::Enforced {
            memory_bytes
        } else {
            None
        },
        cpu_quota_percent: if cpu == Enforcement::Enforced {
            cpu_quota_percent
        } else {
            None
        },
        process_count: if process == Enforcement::Enforced {
            process_count
        } else {
            None
        },
    })
}

/// Every profile on this host, for a workload that declares no network
/// and no limits: what `compute isolation` reports.
pub fn profile_matrix(
    host: &HostCapabilities,
) -> Vec<(HostProfile, Result<HostEnforcement, HostRefusal>)> {
    HostProfile::ALL
        .into_iter()
        .map(|profile| {
            (
                profile,
                plan(
                    profile,
                    &if profile == HostProfile::Trusted {
                        NetworkPolicy::Network
                    } else {
                        NetworkPolicy::None
                    },
                    &ResourceLimits::default(),
                    host,
                ),
            )
        })
        .collect()
}

/// One host profile as this host can (or cannot) enforce it, for
/// `compute isolation`, `/info` and `compute doctor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostProfileSupport {
    pub profile: HostProfile,
    pub supported: bool,
    /// Per dimension, for a workload declaring `network: none` (or
    /// `network` under `trusted`) and no limits of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enforcement: Option<HostEnforcement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<HostRefusal>,
}

/// This host's capabilities and what each profile enforces on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostIsolationReport {
    pub capabilities: HostCapabilities,
    pub profiles: Vec<HostProfileSupport>,
}

pub fn host_isolation_report() -> HostIsolationReport {
    let capabilities = host_capabilities().clone();
    let profiles = profile_matrix(&capabilities)
        .into_iter()
        .map(|(profile, plan)| match plan {
            Ok(enforcement) => HostProfileSupport {
                profile,
                supported: true,
                enforcement: Some(enforcement),
                refusal: None,
            },
            Err(refusal) => HostProfileSupport {
                profile,
                supported: false,
                enforcement: None,
                refusal: Some(refusal),
            },
        })
        .collect();
    HostIsolationReport {
        capabilities,
        profiles,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> HostCapabilities {
        HostCapabilities {
            os: "linux".into(),
            landlock_abi: 7,
            network_namespaces: true,
            cgroups: Some(CgroupSupport {
                version: "v2".into(),
                memory: true,
                cpu: true,
                pids: true,
                root: "/sys/fs/cgroup/compute".into(),
            }),
            resource_limits: true,
            no_new_privs: true,
            root: true,
        }
    }

    #[test]
    fn profiles_never_downgrade() {
        let bare = HostCapabilities {
            landlock_abi: 0,
            network_namespaces: false,
            cgroups: None,
            ..full()
        };
        let none = ResourceLimits::default();
        // Trusted cannot confine the network.
        assert!(plan(HostProfile::Trusted, &NetworkPolicy::None, &none, &full()).is_err());
        let trusted = plan(HostProfile::Trusted, &NetworkPolicy::Network, &none, &bare).unwrap();
        assert_eq!(trusted.filesystem, Enforcement::Unrestricted);
        // Without Landlock, nothing above trusted is offered.
        let refused = plan(
            HostProfile::Restricted,
            &NetworkPolicy::Network,
            &none,
            &bare,
        )
        .unwrap_err();
        assert_eq!(refused.code, "filesystem_isolation_unavailable");
        // Without namespaces, `none` is refused, not weakened.
        let partial = HostCapabilities {
            network_namespaces: false,
            ..full()
        };
        assert_eq!(
            plan(
                HostProfile::Restricted,
                &NetworkPolicy::None,
                &none,
                &partial
            )
            .unwrap_err()
            .code,
            "network_isolation_unavailable"
        );
        // Isolated needs cgroups for its limits.
        let no_cgroups = HostCapabilities {
            cgroups: None,
            ..full()
        };
        assert!(
            plan(
                HostProfile::Isolated,
                &NetworkPolicy::None,
                &none,
                &no_cgroups
            )
            .is_err()
        );
    }

    #[test]
    fn isolated_enforces_every_dimension_with_defaults() {
        let isolated = plan(
            HostProfile::Isolated,
            &NetworkPolicy::None,
            &ResourceLimits::default(),
            &full(),
        )
        .unwrap();
        assert_eq!(isolated.filesystem, Enforcement::Enforced);
        assert_eq!(isolated.network, Enforcement::Enforced);
        assert_eq!(isolated.memory, Enforcement::Enforced);
        assert_eq!(isolated.cpu, Enforcement::Enforced);
        assert_eq!(isolated.process, Enforcement::Enforced);
        assert_eq!(isolated.memory_bytes, Some(ISOLATED_MEMORY_BYTES));
        let restricted = plan(
            HostProfile::Restricted,
            &NetworkPolicy::Network,
            &ResourceLimits::default(),
            &full(),
        )
        .unwrap();
        assert_eq!(restricted.filesystem, Enforcement::Restricted);
        assert_eq!(restricted.network, Enforcement::Unrestricted);
        assert_eq!(restricted.memory, Enforcement::NotRequested);
        assert_eq!(restricted.process, Enforcement::Restricted);
    }
}
