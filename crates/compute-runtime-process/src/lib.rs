use std::path::PathBuf;
use std::process::Stdio;
use std::time::Instant;

use async_trait::async_trait;
use compute_core::{
    ComputeError, ExecutionError, ExecutionErrorKind, ExecutionPhase, ExecutionResult,
    ExecutionStatus, NetworkPolicy, Output, ResolvedRuntime, ResourceUsage, Result, RuntimeAdapter,
    RuntimeAvailability, RuntimeCapabilities, RuntimeKind, Workload, collect_artifacts,
    new_execution_id, stage_workload,
};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct ProcessRuntime {
    kind: RuntimeKind,
    executable_names: &'static [&'static str],
}

impl ProcessRuntime {
    pub const fn new(kind: RuntimeKind, executable_names: &'static [&'static str]) -> Self {
        Self {
            kind,
            executable_names,
        }
    }

    fn discover(&self) -> Option<(PathBuf, String)> {
        for executable in self.executable_names {
            let Ok(path) = which::which(executable) else {
                continue;
            };
            let output = std::process::Command::new(&path)
                .arg("--version")
                .output()
                .ok()?;
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let version = if stdout.is_empty() { stderr } else { stdout };
            return Some((path, version));
        }
        None
    }
}

#[async_trait]
impl RuntimeAdapter for ProcessRuntime {
    fn kind(&self) -> RuntimeKind {
        self.kind
    }

    async fn availability(&self, requested: Option<&str>) -> RuntimeAvailability {
        match self.discover() {
            Some((path, version)) => RuntimeAvailability {
                kind: self.kind,
                version: Some(version.clone()),
                known: true,
                installed: true,
                available: true,
                compatible: requested
                    .map(|requested| version.contains(requested))
                    .unwrap_or(true),
                selected: false,
                executable: Some(path),
            },
            None => RuntimeAvailability {
                kind: self.kind,
                version: None,
                known: true,
                installed: false,
                available: false,
                compatible: false,
                selected: false,
                executable: None,
            },
        }
    }

    async fn resolve(&self, workload: &Workload) -> Result<ResolvedRuntime> {
        let runtime = self.availability(workload.runtime.version.as_deref()).await;
        if !runtime.available {
            return Err(ComputeError::RuntimeUnavailable(self.kind));
        }

        if let (Some(requested), Some(found)) = (&workload.runtime.version, &runtime.version)
            && !found.contains(requested)
        {
            return Err(ComputeError::RuntimeVersionMismatch {
                kind: self.kind,
                requested: requested.clone(),
                found: found.clone(),
            });
        }
        Ok(ResolvedRuntime {
            kind: self.kind,
            requested_version: workload.runtime.version.clone(),
            resolved_version: runtime.version,
            executable: runtime.executable,
        })
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities::process()
    }

    async fn execute(
        &self,
        workload: &Workload,
        runtime: &ResolvedRuntime,
    ) -> Result<ExecutionResult> {
        if workload.network != NetworkPolicy::Network {
            return Err(ComputeError::UnsupportedCapability {
                runtime: self.kind,
                capability: "network isolation is not yet supported for process-backed runtimes"
                    .to_string(),
            });
        }
        if workload.resources.memory_bytes.is_some()
            || workload.resources.cpu_time.is_some()
            || workload.resources.process_count.is_some()
        {
            return Err(ComputeError::UnsupportedCapability {
                runtime: self.kind,
                capability: "requested resource limit is not supported for process-backed runtimes"
                    .to_string(),
            });
        }

        let staged = stage_workload(workload)?;
        let executable = runtime
            .executable
            .as_ref()
            .ok_or(ComputeError::RuntimeUnavailable(self.kind))?;
        let mut command = Command::new(executable);
        command
            .arg(&staged.entrypoint)
            .args(&workload.args)
            .current_dir(&staged.work_dir)
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear()
            .env("COMPUTE_WORK_DIR", &staged.work_dir)
            .env("COMPUTE_TMP_DIR", &staged.tmp_dir)
            .env("COMPUTE_OUTPUT_DIR", &staged.output_dir);

        for pair in &workload.env {
            command.env(&pair.key, &pair.value);
        }

        let started = Instant::now();
        let mut child = command.spawn()?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| ComputeError::Runtime("stdout pipe missing".into()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| ComputeError::Runtime("stderr pipe missing".into()))?;
        let stdout_limit = workload.resources.stdout_bytes;
        let stderr_limit = workload.resources.stderr_bytes;
        let read_output = async {
            let stdout_read = read_limited(&mut stdout, stdout_limit);
            let stderr_read = read_limited(&mut stderr, stderr_limit);
            let (stdout, stderr, status) = tokio::join!(stdout_read, stderr_read, child.wait());
            Ok::<_, std::io::Error>((stdout?, stderr?, status?))
        };
        let (stdout, stderr, status, timed_out) =
            if let Some(timeout) = workload.resources.wall_time {
                match tokio::time::timeout(timeout, read_output).await {
                    Ok(result) => {
                        let (stdout, stderr, status) = result?;
                        (stdout, stderr, Some(status), false)
                    }
                    Err(_) => {
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        let (stdout, stderr) = tokio::join!(
                            read_limited(&mut stdout, stdout_limit),
                            read_limited(&mut stderr, stderr_limit)
                        );
                        (stdout?, stderr?, None, true)
                    }
                }
            } else {
                let (stdout, stderr, status) = read_output.await?;
                (stdout, stderr, Some(status), false)
            };

        let process_status = status;
        let execution_status = if timed_out {
            ExecutionStatus::TimedOut
        } else {
            // A workload's exit status is data, not a failure of Compute itself.
            ExecutionStatus::Completed
        };

        let execution_id = new_execution_id();
        Ok(ExecutionResult {
            execution_id: execution_id.clone(),
            runtime: self.kind,
            network: workload.network.clone(),
            lifecycle: vec![
                ExecutionStatus::Created,
                ExecutionStatus::Resolved,
                ExecutionStatus::Prepared,
                ExecutionStatus::Started,
                ExecutionStatus::Running,
                execution_status.clone(),
            ],
            status: execution_status,
            exit_code: process_status
                .as_ref()
                .and_then(std::process::ExitStatus::code),
            stdout: Output::from_bytes(stdout, workload.resources.stdout_bytes),
            stderr: Output::from_bytes(stderr, workload.resources.stderr_bytes),
            duration: started.elapsed(),
            resource_usage: ResourceUsage::default(),
            artifacts: collect_artifacts(&staged.output_dir)?,
            error: timed_out.then(|| ExecutionError {
                execution_id,
                phase: ExecutionPhase::Running,
                kind: ExecutionErrorKind::Timeout,
                message: "wall time limit exceeded".to_string(),
                runtime: Some(self.kind),
                exit_code: None,
                started: true,
            }),
        })
    }
}

async fn read_limited<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    limit: Option<u64>,
) -> std::io::Result<Vec<u8>> {
    let max = limit.and_then(|value| usize::try_from(value).ok());
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        if let Some(max) = max {
            let remaining = max.saturating_add(1).saturating_sub(output.len());
            output.extend_from_slice(&buffer[..count.min(remaining)]);
        } else {
            output.extend_from_slice(&buffer[..count]);
        }
    }
    Ok(output)
}

#[derive(Debug, Default, Clone)]
pub struct NodeRuntime;
#[derive(Debug, Default, Clone)]
pub struct BunRuntime;
#[derive(Debug, Default, Clone)]
pub struct DenoRuntime;
#[derive(Debug, Default, Clone)]
pub struct PythonRuntime;

#[async_trait]
impl RuntimeAdapter for NodeRuntime {
    fn kind(&self) -> RuntimeKind {
        RuntimeKind::Node
    }
    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities::process()
    }
    async fn availability(&self, requested: Option<&str>) -> RuntimeAvailability {
        ProcessRuntime::new(RuntimeKind::Node, &["node"])
            .availability(requested)
            .await
    }
    async fn resolve(&self, workload: &Workload) -> Result<ResolvedRuntime> {
        ProcessRuntime::new(RuntimeKind::Node, &["node"])
            .resolve(workload)
            .await
    }
    async fn execute(
        &self,
        workload: &Workload,
        runtime: &ResolvedRuntime,
    ) -> Result<ExecutionResult> {
        ProcessRuntime::new(RuntimeKind::Node, &["node"])
            .execute(workload, runtime)
            .await
    }
}

#[async_trait]
impl RuntimeAdapter for BunRuntime {
    fn kind(&self) -> RuntimeKind {
        RuntimeKind::Bun
    }
    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities::process()
    }
    async fn availability(&self, requested: Option<&str>) -> RuntimeAvailability {
        ProcessRuntime::new(RuntimeKind::Bun, &["bun"])
            .availability(requested)
            .await
    }
    async fn resolve(&self, workload: &Workload) -> Result<ResolvedRuntime> {
        ProcessRuntime::new(RuntimeKind::Bun, &["bun"])
            .resolve(workload)
            .await
    }
    async fn execute(
        &self,
        workload: &Workload,
        runtime: &ResolvedRuntime,
    ) -> Result<ExecutionResult> {
        ProcessRuntime::new(RuntimeKind::Bun, &["bun"])
            .execute(workload, runtime)
            .await
    }
}

#[async_trait]
impl RuntimeAdapter for DenoRuntime {
    fn kind(&self) -> RuntimeKind {
        RuntimeKind::Deno
    }
    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities::process()
    }
    async fn availability(&self, requested: Option<&str>) -> RuntimeAvailability {
        ProcessRuntime::new(RuntimeKind::Deno, &["deno"])
            .availability(requested)
            .await
    }
    async fn resolve(&self, workload: &Workload) -> Result<ResolvedRuntime> {
        ProcessRuntime::new(RuntimeKind::Deno, &["deno"])
            .resolve(workload)
            .await
    }
    async fn execute(
        &self,
        workload: &Workload,
        runtime: &ResolvedRuntime,
    ) -> Result<ExecutionResult> {
        ProcessRuntime::new(RuntimeKind::Deno, &["deno"])
            .execute(workload, runtime)
            .await
    }
}

#[async_trait]
impl RuntimeAdapter for PythonRuntime {
    fn kind(&self) -> RuntimeKind {
        RuntimeKind::Python
    }
    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities::process()
    }
    async fn availability(&self, requested: Option<&str>) -> RuntimeAvailability {
        ProcessRuntime::new(RuntimeKind::Python, &["python3", "python"])
            .availability(requested)
            .await
    }
    async fn resolve(&self, workload: &Workload) -> Result<ResolvedRuntime> {
        ProcessRuntime::new(RuntimeKind::Python, &["python3", "python"])
            .resolve(workload)
            .await
    }
    async fn execute(
        &self,
        workload: &Workload,
        runtime: &ResolvedRuntime,
    ) -> Result<ExecutionResult> {
        ProcessRuntime::new(RuntimeKind::Python, &["python3", "python"])
            .execute(workload, runtime)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn availability_reports_known_runtime() {
        let runtime = ProcessRuntime::new(RuntimeKind::Node, &["node"]);
        let availability = runtime.availability(None).await;
        assert!(availability.known);
    }

    #[test]
    fn process_runtime_can_be_constructed() {
        let runtime = ProcessRuntime::new(RuntimeKind::Python, &["python3", "python"]);
        assert_eq!(runtime.kind(), RuntimeKind::Python);
    }
}
