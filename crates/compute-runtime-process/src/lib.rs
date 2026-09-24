use std::path::PathBuf;
use std::process::Stdio;
use std::time::Instant;

use async_trait::async_trait;
use compute_core::{
    ComputeError, ExecutionResult, ExecutionStatus, NetworkPolicy, Output, ResolvedRuntime,
    ResourceUsage, Result, RuntimeAdapter, RuntimeAvailability, RuntimeKind, Workload,
    stage_workload,
};
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
        if let (Some(requested), Some(found)) = (&workload.runtime.version, &runtime.version) {
            if !found.contains(requested) {
                return Err(ComputeError::RuntimeVersionMismatch {
                    kind: self.kind,
                    requested: requested.clone(),
                    found: found.clone(),
                });
            }
        }
        Ok(ResolvedRuntime {
            kind: self.kind,
            requested_version: workload.runtime.version.clone(),
            resolved_version: runtime.version,
            executable: runtime.executable,
        })
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
        let child = command.spawn()?;
        let output = if let Some(timeout) = workload.resources.wall_time {
            match tokio::time::timeout(timeout, child.wait_with_output()).await {
                Ok(result) => result?,
                Err(_) => {
                    return Ok(ExecutionResult {
                        status: ExecutionStatus::TimedOut,
                        exit_code: None,
                        stdout: Output::from_bytes(Vec::new(), workload.resources.stdout_bytes),
                        stderr: Output::from_bytes(Vec::new(), workload.resources.stderr_bytes),
                        duration: started.elapsed(),
                        resource_usage: ResourceUsage::default(),
                        artifacts: vec![],
                    });
                }
            }
        } else {
            child.wait_with_output().await?
        };

        let status = if output.status.success() {
            ExecutionStatus::Completed
        } else {
            ExecutionStatus::Failed
        };

        Ok(ExecutionResult {
            status,
            exit_code: output.status.code(),
            stdout: Output::from_bytes(output.stdout, workload.resources.stdout_bytes),
            stderr: Output::from_bytes(output.stderr, workload.resources.stderr_bytes),
            duration: started.elapsed(),
            resource_usage: ResourceUsage::default(),
            artifacts: vec![],
        })
    }
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
