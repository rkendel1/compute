//! Process ownership on a node, and terminating what an environment owns.
//!
//! Every command Compute runs in a session carries `COMPUTE_SESSION_ID` and
//! `COMPUTE_SESSION_WORKSPACE` in its environment, and a caller cannot set
//! either (`session_request`). Children inherit them. That is the ownership
//! record: a process belongs to the workspace it names, however deep in the
//! tree it is and whether or not it left its parent's process group. On
//! Linux the kernel keeps that record for us (`/proc/<pid>/environ`), so
//! there is no second table of processes to lose or to drift from what is
//! running.
//!
//! [`terminate_owned`] is the one primitive a provider needs to keep the
//! contract `stop` and `destroy` make: it returns only when no process owned
//! by the workspace is alive, and fails with `TerminationFailed`, naming
//! what survived, when it cannot make that true. It never reports success
//! for a request it merely issued.
//!
//! What it cannot do: a process that scrubbed its own environment (`env -i`)
//! and left its process group is invisible to a host that has no cgroup or
//! container to enclose it. That limit is a property of the provider, and is
//! declared as such: `SessionCapabilities::process_tree_termination` is true
//! only where this scan is available.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::{ProviderError, ProviderErrorKind};

/// The variable whose value names the workspace that owns a process.
pub const OWNER_MARKER: &str = "COMPUTE_SESSION_WORKSPACE";

/// How long processes are asked to exit before they are killed.
pub const TERMINATION_GRACE: Duration = Duration::from_millis(1500);
/// How long a workspace's processes may take to be gone, in all.
pub const TERMINATION_DEADLINE: Duration = Duration::from_secs(15);

/// Whether this host can enumerate the processes a workspace owns, and so
/// confirm that none remain.
pub const fn ownership_scan_available() -> bool {
    cfg!(target_os = "linux")
}

/// A process the workspace owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnedProcess {
    pub pid: u32,
    pub pgid: u32,
}

/// Every live (not zombie) process that carries `workspace`'s marker,
/// excluding this one.
#[cfg(target_os = "linux")]
pub fn owned_processes(workspace: &Path) -> Vec<OwnedProcess> {
    let wanted = format!("{OWNER_MARKER}={}", workspace.display());
    let own = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return vec![];
    };
    let mut owned = vec![];
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == own {
            continue;
        }
        let Ok(environment) = std::fs::read(entry.path().join("environ")) else {
            continue;
        };
        if !environment
            .split(|byte| *byte == 0)
            .any(|variable| variable == wanted.as_bytes())
        {
            continue;
        }
        if let Some((state, pgid)) = stat(pid)
            && state != 'Z'
            && state != 'X'
        {
            owned.push(OwnedProcess { pid, pgid });
        }
    }
    owned.sort_by_key(|process| process.pid);
    owned
}

#[cfg(not(target_os = "linux"))]
pub fn owned_processes(_workspace: &Path) -> Vec<OwnedProcess> {
    vec![]
}

/// State and process group from `/proc/<pid>/stat`. The command name is in
/// parentheses and may contain anything, so fields are counted from the
/// last `)`.
#[cfg(target_os = "linux")]
fn stat(pid: u32) -> Option<(char, u32)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &text[text.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let _parent = fields.next()?;
    let group = fields.next()?.parse().ok()?;
    Some((state, group))
}

/// Whether the process is alive (not a zombie).
#[cfg(target_os = "linux")]
pub fn alive(pid: u32) -> bool {
    stat(pid).is_some_and(|(state, _)| state != 'Z' && state != 'X')
}

#[cfg(not(target_os = "linux"))]
pub fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only probes for existence.
    i32::try_from(pid).is_ok_and(|pid| unsafe { libc::kill(pid, 0) } == 0)
}

#[cfg(unix)]
fn signal(process: OwnedProcess, signal: i32) {
    let (Ok(pid), Ok(group)) = (i32::try_from(process.pid), i32::try_from(process.pgid)) else {
        return;
    };
    // SAFETY: signalling a process this workspace owns, identified by its
    // environment marker moments ago. A group is signalled only when the
    // owned process leads it, so an unrelated group is never touched.
    unsafe {
        libc::kill(pid, signal);
        if group == pid {
            libc::kill(-group, signal);
        }
    }
}

#[cfg(not(unix))]
fn signal(_process: OwnedProcess, _signal: i32) {}

/// Terminate every process `workspace` owns, and return the number that
/// were running. Success means none is alive: it is confirmed by scanning
/// again, not assumed from having signalled.
pub async fn terminate_owned(workspace: &Path) -> Result<usize, ProviderError> {
    terminate_owned_within(workspace, TERMINATION_GRACE, TERMINATION_DEADLINE).await
}

pub async fn terminate_owned_within(
    workspace: &Path,
    grace: Duration,
    deadline: Duration,
) -> Result<usize, ProviderError> {
    let started = Instant::now();
    let mut found = owned_processes(workspace);
    let count = found.len();
    if found.is_empty() {
        return Ok(0);
    }
    for process in &found {
        signal(*process, libc_sigterm());
    }
    let mut killed = false;
    loop {
        tokio::time::sleep(Duration::from_millis(25)).await;
        // A process can start another while the tree comes down: scan each
        // time rather than trusting the first list.
        found = owned_processes(workspace);
        if found.is_empty() {
            return Ok(count);
        }
        let elapsed = started.elapsed();
        if elapsed >= deadline {
            return Err(ProviderError::new(
                ProviderErrorKind::TerminationFailed,
                format!(
                    "{} process(es) owned by the workspace are still alive after SIGKILL: {}",
                    found.len(),
                    found
                        .iter()
                        .map(|process| process.pid.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        }
        if elapsed >= grace || killed {
            killed = true;
            for process in &found {
                signal(*process, libc_sigkill());
            }
        }
    }
}

#[cfg(unix)]
const fn libc_sigterm() -> i32 {
    libc::SIGTERM
}
#[cfg(unix)]
const fn libc_sigkill() -> i32 {
    libc::SIGKILL
}
#[cfg(not(unix))]
const fn libc_sigterm() -> i32 {
    15
}
#[cfg(not(unix))]
const fn libc_sigkill() -> i32 {
    9
}
