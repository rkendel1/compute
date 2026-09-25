//! The operating-system boundary of a process workload under a
//! `restricted` or `isolated` host profile.
//!
//! Everything that allocates or can fail in interesting ways happens in
//! the parent: the Landlock ruleset, the cgroups and their limits, and the
//! open `cgroup.procs` files. Between fork and exec the child only makes
//! plain syscalls: join its cgroups, leave the host's network, lower its
//! resource limits, forbid gaining privileges, and restrict itself.

use std::path::{Path, PathBuf};

use compute_core::host::{Enforcement, HostCapabilities, HostEnforcement};
use compute_core::{ComputeError, NetworkPolicy, Result, Workload};

/// Access rights Landlock handles, by ABI.
const LANDLOCK_ACCESS_FS_EXECUTE: u64 = 1 << 0;
const LANDLOCK_ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
const LANDLOCK_ACCESS_FS_READ_FILE: u64 = 1 << 2;
const LANDLOCK_ACCESS_FS_READ_DIR: u64 = 1 << 3;
const LANDLOCK_ACCESS_FS_TRUNCATE: u64 = 1 << 14;
const LANDLOCK_ACCESS_FS_IOCTL_DEV: u64 = 1 << 15;
const LANDLOCK_ACCESS_NET_BIND_TCP: u64 = 1 << 0;
const LANDLOCK_ACCESS_NET_CONNECT_TCP: u64 = 1 << 1;
const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;

fn handled_fs(abi: u32) -> u64 {
    let mut mask = (1 << 13) - 1;
    if abi >= 2 {
        mask |= 1 << 13;
    }
    if abi >= 3 {
        mask |= LANDLOCK_ACCESS_FS_TRUNCATE;
    }
    if abi >= 5 {
        mask |= LANDLOCK_ACCESS_FS_IOCTL_DEV;
    }
    mask
}

/// Rights that apply to a file (rather than a directory).
fn file_rights(abi: u32) -> u64 {
    (LANDLOCK_ACCESS_FS_EXECUTE
        | LANDLOCK_ACCESS_FS_WRITE_FILE
        | LANDLOCK_ACCESS_FS_READ_FILE
        | LANDLOCK_ACCESS_FS_TRUNCATE
        | LANDLOCK_ACCESS_FS_IOCTL_DEV)
        & handled_fs(abi)
}

const READ: u64 =
    LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_READ_DIR | LANDLOCK_ACCESS_FS_EXECUTE;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// The boundary prepared for one execution. Its cgroups are removed when
/// it is dropped, after the workload has ended.
pub(crate) struct Sandbox {
    ruleset: Option<std::os::fd::OwnedFd>,
    procs: Vec<std::os::fd::OwnedFd>,
    groups: Vec<PathBuf>,
    new_network: bool,
    loopback: bool,
    address_space: Option<u64>,
    cpu_seconds: Option<u64>,
    processes: Option<u64>,
}

impl Sandbox {
    pub(crate) fn prepare(
        plan: &HostEnforcement,
        host: &HostCapabilities,
        workload: &Workload,
        workspace: &Path,
        executable: Option<&Path>,
        dependencies: Option<&Path>,
    ) -> Result<Self> {
        let unavailable = |what: String| {
            ComputeError::Runtime(format!(
                "{} host isolation could not be applied: {what}",
                plan.profile
            ))
        };
        let mut sandbox = Self {
            ruleset: None,
            procs: vec![],
            groups: vec![],
            new_network: plan.network == Enforcement::Enforced
                && workload.network != NetworkPolicy::Network,
            loopback: workload.network == NetworkPolicy::Localhost,
            address_space: None,
            cpu_seconds: workload.resources.cpu_time.map(|cpu| cpu.as_secs().max(1)),
            processes: None,
        };
        let cgroup_memory = plan.mechanisms.iter().any(|m| m == "cgroup_memory");
        if plan.memory == Enforcement::Enforced && !cgroup_memory {
            sandbox.address_space = plan.memory_bytes;
        }
        if plan.mechanisms.iter().any(|m| m == "rlimit_nproc") {
            sandbox.processes = plan.process_count.map(u64::from);
        }
        // Cgroups for memory, CPU share, and process count.
        if let Some(cgroups) = &host.cgroups {
            let id = format!(
                "exec-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            );
            let mut limits: Vec<(&str, &str, String)> = vec![];
            if cgroup_memory && let Some(bytes) = plan.memory_bytes {
                limits.push((
                    "memory",
                    if cgroups.version == "v1" {
                        "memory.limit_in_bytes"
                    } else {
                        "memory.max"
                    },
                    bytes.to_string(),
                ));
            }
            if plan.mechanisms.iter().any(|m| m == "cgroup_cpu_quota")
                && let Some(percent) = plan.cpu_quota_percent
            {
                let quota = u64::from(percent) * 1000;
                if cgroups.version == "v1" {
                    limits.push(("cpu", "cpu.cfs_period_us", "100000".into()));
                    limits.push(("cpu", "cpu.cfs_quota_us", quota.to_string()));
                } else {
                    limits.push(("cpu", "cpu.max", format!("{quota} 100000")));
                }
            }
            if plan.mechanisms.iter().any(|m| m == "cgroup_pids")
                && let Some(count) = plan.process_count
            {
                limits.push(("pids", "pids.max", count.to_string()));
            }
            let mut directories = std::collections::BTreeMap::new();
            for (controller, file, value) in limits {
                let directory = if cgroups.version == "v1" {
                    PathBuf::from("/sys/fs/cgroup")
                        .join(controller)
                        .join("compute")
                        .join(&id)
                } else {
                    PathBuf::from(&cgroups.root).join(&id)
                };
                if !directories.contains_key(&directory) {
                    if cgroups.version == "v2" {
                        let parent = directory.parent().expect("under root");
                        std::fs::create_dir_all(parent).map_err(|error| {
                            unavailable(format!("{}: {error}", parent.display()))
                        })?;
                        // Delegate the controllers to Compute's groups.
                        let _ = std::fs::write(
                            parent.join("cgroup.subtree_control"),
                            "+memory +cpu +pids",
                        );
                    }
                    std::fs::create_dir_all(&directory).map_err(|error| {
                        unavailable(format!("{}: {error}", directory.display()))
                    })?;
                    sandbox.groups.push(directory.clone());
                    directories.insert(directory.clone(), ());
                }
                std::fs::write(directory.join(file), &value).map_err(|error| {
                    unavailable(format!("{}: {error}", directory.join(file).display()))
                })?;
            }
            for directory in directories.keys() {
                let procs = std::fs::OpenOptions::new()
                    .write(true)
                    .open(directory.join("cgroup.procs"))
                    .map_err(|error| unavailable(format!("{}: {error}", directory.display())))?;
                sandbox.procs.push(procs.into());
            }
        }
        // Landlock.
        if matches!(
            plan.filesystem,
            Enforcement::Restricted | Enforcement::Enforced
        ) {
            sandbox.ruleset = Some(
                ruleset(
                    plan,
                    host.landlock_abi,
                    workload,
                    workspace,
                    executable,
                    dependencies,
                )
                .map_err(|error| unavailable(format!("landlock: {error}")))?,
            );
        }
        Ok(sandbox)
    }

    /// Apply the boundary to the command's child between fork and exec.
    pub(crate) fn install(&self, command: &mut tokio::process::Command) {
        use std::os::fd::AsRawFd;
        let procs = self
            .procs
            .iter()
            .map(AsRawFd::as_raw_fd)
            .collect::<Vec<_>>();
        let ruleset = self.ruleset.as_ref().map(AsRawFd::as_raw_fd);
        let new_network = self.new_network;
        let loopback = self.loopback;
        let address_space = self.address_space;
        let cpu_seconds = self.cpu_seconds;
        let processes = self.processes;
        // SAFETY: the closure makes only async-signal-safe syscalls on
        // values prepared before fork; it allocates nothing.
        unsafe {
            command.pre_exec(move || {
                for fd in &procs {
                    if libc::write(*fd, b"0".as_ptr().cast(), 1) != 1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                if new_network {
                    if libc::unshare(libc::CLONE_NEWNET) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if loopback {
                        loopback_up()?;
                    }
                }
                let limit = |resource, value: u64| {
                    let limit = libc::rlimit {
                        rlim_cur: value,
                        rlim_max: value,
                    };
                    if libc::setrlimit(resource, &limit) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                };
                limit(libc::RLIMIT_CORE, 0)?;
                if let Some(bytes) = address_space {
                    limit(libc::RLIMIT_AS, bytes)?;
                }
                if let Some(seconds) = cpu_seconds {
                    limit(libc::RLIMIT_CPU, seconds)?;
                }
                if let Some(count) = processes {
                    limit(libc::RLIMIT_NPROC, count)?;
                }
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if let Some(ruleset) = ruleset
                    && libc::syscall(libc::SYS_landlock_restrict_self, ruleset, 0u32) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}

static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl Drop for Sandbox {
    fn drop(&mut self) {
        // The workload has ended; anything it left in its groups goes too.
        for group in &self.groups {
            for _ in 0..50 {
                if std::fs::remove_dir(group).is_ok() {
                    break;
                }
                if let Ok(procs) = std::fs::read_to_string(group.join("cgroup.procs")) {
                    for pid in procs
                        .lines()
                        .filter_map(|line| line.trim().parse::<i32>().ok())
                    {
                        // SAFETY: signalling a process confined to this
                        // execution's own cgroup.
                        unsafe { libc::kill(pid, libc::SIGKILL) };
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
}

/// Bring up the loopback interface of a fresh network namespace, so
/// `localhost` works and nothing else does.
unsafe fn loopback_up() -> std::io::Result<()> {
    #[repr(C)]
    struct InterfaceRequest {
        name: [libc::c_char; libc::IFNAMSIZ],
        flags: libc::c_short,
        padding: [u8; 22],
    }
    // SAFETY: a datagram socket and two ioctls on a stack buffer.
    unsafe {
        let socket = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if socket < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut request = InterfaceRequest {
            name: [0; libc::IFNAMSIZ],
            flags: 0,
            padding: [0; 22],
        };
        request.name[0] = b'l' as libc::c_char;
        request.name[1] = b'o' as libc::c_char;
        if libc::ioctl(socket, libc::SIOCGIFFLAGS, &mut request) != 0 {
            libc::close(socket);
            return Err(std::io::Error::last_os_error());
        }
        request.flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
        let result = libc::ioctl(socket, libc::SIOCSIFFLAGS, &mut request);
        libc::close(socket);
        if result != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The Landlock ruleset for a profile: `restricted` reads everything and
/// writes only its workspace; `isolated` also reads only its runtime, the
/// system's shared libraries and certificates, and its workspace. With a
/// declared `network: none` and a kernel that handles TCP, TCP is denied
/// too (on top of the network namespace).
fn ruleset(
    plan: &HostEnforcement,
    abi: u32,
    workload: &Workload,
    workspace: &Path,
    executable: Option<&Path>,
    dependencies: Option<&Path>,
) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::OpenOptionsExt;
    let handled = handled_fs(abi);
    let deny_tcp = abi >= 4 && workload.network == NetworkPolicy::None;
    let attributes = RulesetAttr {
        handled_access_fs: handled,
        handled_access_net: if deny_tcp {
            LANDLOCK_ACCESS_NET_BIND_TCP | LANDLOCK_ACCESS_NET_CONNECT_TCP
        } else {
            0
        },
    };
    let size = if abi >= 4 {
        std::mem::size_of::<RulesetAttr>()
    } else {
        std::mem::size_of::<u64>()
    };
    // SAFETY: a valid attribute struct of the stated size.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attributes as *const RulesetAttr,
            size,
            0u32,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the kernel returned a new file descriptor we now own.
    let ruleset = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) };
    let add = |path: &Path, rights: u64| -> std::io::Result<()> {
        // `O_PATH` names a file or directory without needing to read it.
        let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
            .open(path)
        else {
            // Absent on this host: nothing to allow.
            return Ok(());
        };
        let is_directory = file
            .metadata()
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false);
        let rights = if is_directory {
            rights & handled
        } else {
            rights & file_rights(abi)
        };
        let rule = PathBeneathAttr {
            allowed_access: rights,
            parent_fd: file.as_raw_fd(),
        };
        // SAFETY: a valid rule for an open file descriptor.
        let result = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                ruleset.as_raw_fd(),
                LANDLOCK_RULE_PATH_BENEATH,
                &rule as *const PathBeneathAttr,
                0u32,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    };
    let devices = LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_WRITE_FILE;
    match plan.filesystem {
        Enforcement::Restricted => {
            add(Path::new("/"), READ)?;
        }
        _ => {
            // Only what running the runtime needs.
            for path in [
                "/usr",
                "/lib",
                "/lib64",
                "/lib32",
                "/bin",
                "/sbin",
                "/opt",
                "/etc/ld.so.cache",
                "/etc/ld.so.conf",
                "/etc/ld.so.conf.d",
                "/etc/ssl",
                "/etc/ca-certificates",
                "/etc/pki",
                "/etc/alternatives",
                "/etc/localtime",
                "/etc/resolv.conf",
                "/etc/hosts",
                "/etc/nsswitch.conf",
                "/etc/passwd",
                "/etc/group",
                "/etc/java-21-openjdk",
                "/etc/java-17-openjdk",
                "/etc/php",
                "/proc",
                "/sys/devices/system/cpu",
                "/sys/fs/cgroup",
            ] {
                add(Path::new(path), READ)?;
            }
            if let Some(executable) = executable {
                let resolved =
                    std::fs::canonicalize(executable).unwrap_or_else(|_| executable.to_path_buf());
                // The runtime's installation: the parent of its bin/.
                let root = resolved
                    .parent()
                    .and_then(|bin| {
                        if bin.file_name().is_some_and(|name| name == "bin") {
                            bin.parent()
                        } else {
                            Some(bin)
                        }
                    })
                    .map(Path::to_path_buf);
                if let Some(root) = root
                    && root != Path::new("/")
                {
                    add(&root, READ)?;
                }
            }
            if let Some(dependencies) = dependencies {
                add(dependencies, READ)?;
            }
        }
    }
    for device in [
        "/dev/null",
        "/dev/zero",
        "/dev/urandom",
        "/dev/random",
        "/dev/full",
    ] {
        add(Path::new(device), devices)?;
    }
    // The workspace: its own, fully.
    add(workspace, handled)?;
    Ok(ruleset)
}
