//! Shared by the ephemeral and mount tests: a shell workload built the way
//! the CLI builds one, run through the real runtime.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use compute_core::{
    ExecutionRequest, IsolationProfile, Mount, NetworkPolicy, ResourceLimits, RuntimeKind,
    RuntimeSpec, WorkloadOutput,
};

/// A `sh` workload running `script`.
pub fn shell(directory: &Path, script: &str) -> ExecutionRequest {
    let entrypoint = directory.join("entry.sh");
    std::fs::write(&entrypoint, script).unwrap();
    ExecutionRequest {
        runtime: RuntimeSpec {
            kind: RuntimeKind::Shell,
            version: None,
        },
        entrypoint,
        args: vec![],
        stdin: vec![],
        env: vec![],
        inputs: vec![],
        outputs: vec![],
        mounts: vec![],
        network: NetworkPolicy::Network,
        resources: ResourceLimits::default(),
        isolation: IsolationProfile::Process,
        host_isolation: Default::default(),
        dependencies: None,
    }
}

pub fn with_timeout(mut request: ExecutionRequest, timeout: Duration) -> ExecutionRequest {
    request.resources.wall_time = Some(timeout);
    request
}

pub fn with_output(mut request: ExecutionRequest, path: &str) -> ExecutionRequest {
    request.outputs.push(WorkloadOutput {
        path: path.into(),
        required: false,
    });
    request
}

pub fn with_mount(mut request: ExecutionRequest, host: &Path, guest: &str) -> ExecutionRequest {
    request.mounts.push(Mount {
        host_path: host.to_path_buf(),
        execution_path: PathBuf::from(guest),
    });
    request
}

/// Whether a process is running (a zombie is not).
pub fn alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .and_then(|rest| rest.split_whitespace().next())
            .is_some_and(|state| state != "Z"),
        Err(_) => false,
    }
}

pub fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}
