//! Node-local records of running service process groups.
//!
//! A daemon that is killed leaves its services running. The daemon that
//! replaces it on the same node must not start second copies next to them,
//! so it reaps what a previous instance recorded before reconciling. A
//! record is only acted on when its process group leader is provably the
//! same process: same boot, same start time. A reused PID is never
//! signalled.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::Key;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ProcessRecord {
    environment: String,
    project: String,
    workload: String,
    pid: u32,
    boot_id: String,
    start_time: u64,
}

fn directory(state_dir: &Path) -> PathBuf {
    state_dir.join("processes")
}

fn path(state_dir: &Path, key: &Key) -> PathBuf {
    directory(state_dir).join(format!(
        "{}.json",
        compute_state::short_digest(&[&key.0, &key.1, &key.2])
    ))
}

#[cfg(target_os = "linux")]
fn boot_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|id| id.trim().to_string())
}

#[cfg(not(target_os = "linux"))]
fn boot_id() -> Option<String> {
    None
}

/// Field 22 of `/proc/<pid>/stat`: the start time in clock ticks.
#[cfg(target_os = "linux")]
fn start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 2..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(not(target_os = "linux"))]
fn start_time(_pid: u32) -> Option<u64> {
    None
}

/// Record a running service's process group.
pub(crate) fn record(state_dir: &Path, key: &Key, pid: u32) {
    let (Some(boot_id), Some(start_time)) = (boot_id(), start_time(pid)) else {
        return;
    };
    let record = ProcessRecord {
        environment: key.0.clone(),
        project: key.1.clone(),
        workload: key.2.clone(),
        pid,
        boot_id,
        start_time,
    };
    let path = path(state_dir, key);
    if std::fs::create_dir_all(directory(state_dir)).is_ok()
        && let Ok(bytes) = serde_json::to_vec(&record)
    {
        let temporary = path.with_extension("tmp");
        if std::fs::write(&temporary, bytes).is_ok() {
            let _ = std::fs::rename(temporary, path);
        }
    }
}

pub(crate) fn forget(state_dir: &Path, key: &Key) {
    let _ = std::fs::remove_file(path(state_dir, key));
}

/// Terminate every recorded process group still led by the recorded
/// process, and forget every record. Returns the reaped workloads.
pub(crate) async fn reap(state_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(directory(state_dir)) else {
        return vec![];
    };
    let current_boot = boot_id();
    let mut reaped = vec![];
    for entry in entries.flatten() {
        let path = entry.path();
        let record = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<ProcessRecord>(&bytes).ok());
        let _ = std::fs::remove_file(&path);
        let Some(record) = record else {
            continue;
        };
        let same_process = current_boot.as_deref() == Some(record.boot_id.as_str())
            && start_time(record.pid) == Some(record.start_time);
        if same_process && terminate_group(record.pid).await {
            reaped.push(format!(
                "{}/{}/{}",
                record.environment, record.project, record.workload
            ));
        }
    }
    reaped
}

#[cfg(unix)]
async fn terminate_group(pid: u32) -> bool {
    let Ok(group) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signalling a process group this node recorded, verified by
    // boot ID and start time to be the same process.
    if unsafe { libc::kill(-group, libc::SIGTERM) } != 0 {
        return false;
    }
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        // SAFETY: probing for existence with signal 0.
        if unsafe { libc::kill(-group, 0) } != 0 {
            return true;
        }
    }
    // SAFETY: as above.
    unsafe { libc::kill(-group, libc::SIGKILL) };
    true
}

#[cfg(not(unix))]
async fn terminate_group(_pid: u32) -> bool {
    false
}
