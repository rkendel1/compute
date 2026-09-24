use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;

use async_trait::async_trait;
use compute_core::{
    ComputeError, ExecutionError, ExecutionErrorKind, ExecutionPhase, ExecutionResult,
    ExecutionStatus, Output, ResolvedRuntime, ResourceUsage, Result, RuntimeAdapter,
    RuntimeAvailability, RuntimeCapabilities, RuntimeDescriptor, RuntimeKind, RuntimeSource,
    Workload, apply_output_contract, collect_artifacts, new_execution_id, stage_workload,
};
use wasmtime::{Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};
use wasmtime_wasi::preview1::{self, WasiP1Ctx};
use wasmtime_wasi::{DirPerms, FilePerms, I32Exit, WasiCtxBuilder};

#[derive(Debug, Default, Clone)]
pub struct WasmRuntime;

struct WasmState {
    wasi: WasiP1Ctx,
    limits: StoreLimits,
}

#[async_trait]
impl RuntimeAdapter for WasmRuntime {
    fn kind(&self) -> RuntimeKind {
        RuntimeKind::Wasm
    }

    fn descriptor(&self) -> RuntimeDescriptor {
        let version = locked_wasm_version();
        RuntimeDescriptor {
            id: RuntimeKind::Wasm,
            version,
            executable: "<embedded>".into(),
            capabilities: self.capabilities(),
        }
    }

    async fn availability(&self, requested: Option<&str>) -> RuntimeAvailability {
        RuntimeAvailability {
            kind: RuntimeKind::Wasm,
            version: Some("wasi".to_string()),
            known: true,
            installed: true,
            available: requested
                .map(|value| value.eq_ignore_ascii_case("wasi"))
                .unwrap_or(true),
            compatible: requested
                .map(|value| value.eq_ignore_ascii_case("wasi"))
                .unwrap_or(true),
            selected: false,
            executable: None,
            source: RuntimeSource::Embedded,
            expected_version: Some(locked_wasm_version()),
            remediation: None,
        }
    }

    async fn resolve(&self, workload: &Workload) -> Result<ResolvedRuntime> {
        if let Some(version) = &workload.runtime.version
            && !version.eq_ignore_ascii_case("wasi")
        {
            return Err(ComputeError::RuntimeVersionMismatch {
                kind: RuntimeKind::Wasm,
                requested: version.clone(),
                found: "wasi".to_string(),
            });
        }

        Ok(ResolvedRuntime {
            kind: RuntimeKind::Wasm,
            requested_version: workload.runtime.version.clone(),
            resolved_version: Some("wasi".to_string()),
            executable: None,
        })
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities::wasm()
    }

    async fn execute(
        &self,
        workload: &Workload,
        _runtime: &ResolvedRuntime,
    ) -> Result<ExecutionResult> {
        self.capabilities().validate(RuntimeKind::Wasm, workload)?;

        let workload = workload.clone();
        tokio::task::spawn_blocking(move || execute_blocking(&workload))
            .await
            .map_err(|error| ComputeError::Runtime(error.to_string()))?
    }
}

fn locked_wasm_version() -> String {
    let lock: serde_json::Value =
        serde_json::from_str(include_str!("../../../distribution/runtime-lock.json"))
            .expect("valid embedded runtime lock");
    lock["runtimes"]["wasm"]["version"]
        .as_str()
        .expect("WASM version in runtime lock")
        .to_owned()
}

fn execute_blocking(workload: &Workload) -> Result<ExecutionResult> {
    let execution_id = new_execution_id();
    let staged = match stage_workload(workload) {
        Ok(staged) => staged,
        Err(error) => {
            return Ok(failure_result(
                workload,
                execution_id,
                ExecutionPhase::Resolved,
                ExecutionErrorKind::Preparation,
                error.to_string(),
                vec![ExecutionStatus::Created, ExecutionStatus::Resolved],
            ));
        }
    };
    let capture_limit = match (
        workload.resources.stdout_bytes,
        workload.resources.stderr_bytes,
    ) {
        (Some(stdout), Some(stderr)) => stdout.max(stderr).saturating_add(1),
        (Some(limit), None) | (None, Some(limit)) => limit.saturating_add(1),
        (None, None) => 8 * 1024 * 1024,
    };
    // Capture beyond the declared result limit so exceeding a limit truncates
    // output instead of becoming a WASI pipe failure.
    let capture_limit =
        usize::try_from(capture_limit.max(8 * 1024 * 1024)).unwrap_or(8 * 1024 * 1024);
    let stdout = MemoryOutputPipe::new(capture_limit);
    let stderr = MemoryOutputPipe::new(capture_limit);

    let mut config = Config::new();
    if workload.resources.wall_time.is_some() {
        config.epoch_interruption(true);
    }
    let engine = Engine::new(&config).map_err(|error| ComputeError::Runtime(error.to_string()))?;
    let module = match Module::from_file(&engine, &staged.entrypoint) {
        Ok(module) => module,
        Err(error) => {
            return Ok(failure_result(
                workload,
                execution_id,
                ExecutionPhase::Prepared,
                ExecutionErrorKind::Start,
                error.to_string(),
                vec![
                    ExecutionStatus::Created,
                    ExecutionStatus::Resolved,
                    ExecutionStatus::Prepared,
                ],
            ));
        }
    };
    let mut linker = Linker::new(&engine);
    preview1::add_to_linker_sync(&mut linker, |state: &mut WasmState| &mut state.wasi)
        .map_err(|error| ComputeError::Runtime(error.to_string()))?;

    let mut limits = StoreLimitsBuilder::new();
    if let Some(memory) = workload.resources.memory_bytes {
        limits = limits.memory_size(memory as usize);
    }

    let mut wasi = WasiCtxBuilder::new();
    wasi.allow_blocking_current_thread(true);
    wasi.stdin(MemoryInputPipe::new(workload.stdin.clone()));
    wasi.preopened_dir(&staged.work_dir, "/work", DirPerms::all(), FilePerms::all())
        .map_err(|error| ComputeError::Runtime(error.to_string()))?;
    wasi.preopened_dir(&staged.tmp_dir, "/tmp", DirPerms::all(), FilePerms::all())
        .map_err(|error| ComputeError::Runtime(error.to_string()))?;
    wasi.preopened_dir(
        &staged.output_dir,
        "/output",
        DirPerms::all(),
        FilePerms::all(),
    )
    .map_err(|error| ComputeError::Runtime(error.to_string()))?;
    let argv = std::iter::once(staged.entrypoint.to_string_lossy().to_string())
        .chain(workload.args.iter().cloned())
        .collect::<Vec<_>>();
    wasi.args(&argv);
    for env in &workload.env {
        wasi.env(&env.key, &env.value);
    }
    wasi.env("COMPUTE_WORK_DIR", "/work");
    wasi.env("COMPUTE_TMP_DIR", "/tmp");
    wasi.env("COMPUTE_OUTPUT_DIR", "/output");
    wasi.stdout(stdout.clone());
    wasi.stderr(stderr.clone());

    let state = WasmState {
        wasi: wasi.build_p1(),
        limits: limits.build(),
    };
    let mut store = Store::new(&engine, state);
    store.limiter(|state| &mut state.limits);

    let instance = linker
        .instantiate(&mut store, &module)
        .map_err(|error| ComputeError::Runtime(error.to_string()))?;
    let command = instance
        .get_typed_func::<(), ()>(&mut store, "_start")
        .map_err(|error| ComputeError::Runtime(error.to_string()))?;

    let start = Instant::now();
    let timeout_fired = Arc::new(AtomicBool::new(false));
    let deadline_cancel = if let Some(timeout) = workload.resources.wall_time {
        store.set_epoch_deadline(1);
        let engine = engine.clone();
        let timeout_fired = Arc::clone(&timeout_fired);
        let (cancel, wait) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if wait.recv_timeout(timeout).is_err() {
                timeout_fired.store(true, Ordering::Release);
                engine.increment_epoch();
            }
        });
        Some(cancel)
    } else {
        None
    };
    let call_result = command.call(&mut store, ());
    if let Some(cancel) = deadline_cancel {
        let _ = cancel.send(());
    }
    let duration = start.elapsed();

    let stdout = stdout.contents().to_vec();
    let stderr = stderr.contents().to_vec();

    match call_result {
        Ok(()) => finalize_result(
            ExecutionResult {
                execution_id: execution_id.clone(),
                runtime: RuntimeKind::Wasm,
                network: workload.network.clone(),
                lifecycle: vec![
                    ExecutionStatus::Created,
                    ExecutionStatus::Resolved,
                    ExecutionStatus::Prepared,
                    ExecutionStatus::Started,
                    ExecutionStatus::Running,
                    ExecutionStatus::Completed,
                ],
                status: ExecutionStatus::Completed,
                exit_code: Some(0),
                stdout: Output::from_bytes(stdout, workload.resources.stdout_bytes),
                stderr: Output::from_bytes(stderr, workload.resources.stderr_bytes),
                duration,
                resource_usage: ResourceUsage {
                    max_memory_bytes: workload.resources.memory_bytes,
                },
                artifacts: collect_artifacts(&staged.output_dir)?,
                outputs: vec![],
                missing_outputs: vec![],
                error: None,
                isolation: None,
                receipt: None,
            },
            workload,
            &staged.output_dir,
        ),
        Err(error) => {
            if let Some(exit) = error.downcast_ref::<I32Exit>() {
                let code = exit.0;
                return finalize_result(
                    ExecutionResult {
                        execution_id: execution_id.clone(),
                        runtime: RuntimeKind::Wasm,
                        network: workload.network.clone(),
                        lifecycle: vec![
                            ExecutionStatus::Created,
                            ExecutionStatus::Resolved,
                            ExecutionStatus::Prepared,
                            ExecutionStatus::Started,
                            ExecutionStatus::Running,
                            ExecutionStatus::Completed,
                        ],
                        status: ExecutionStatus::Completed,
                        exit_code: Some(code),
                        stdout: Output::from_bytes(stdout, workload.resources.stdout_bytes),
                        stderr: Output::from_bytes(stderr, workload.resources.stderr_bytes),
                        duration,
                        resource_usage: ResourceUsage {
                            max_memory_bytes: workload.resources.memory_bytes,
                        },
                        artifacts: collect_artifacts(&staged.output_dir)?,
                        outputs: vec![],
                        missing_outputs: vec![],
                        error: None,
                        isolation: None,
                        receipt: None,
                    },
                    workload,
                    &staged.output_dir,
                );
            }

            let status = if timeout_fired.load(Ordering::Acquire) {
                ExecutionStatus::TimedOut
            } else {
                ExecutionStatus::Failed
            };
            let timed_out = status == ExecutionStatus::TimedOut;
            finalize_result(
                ExecutionResult {
                    execution_id: execution_id.clone(),
                    runtime: RuntimeKind::Wasm,
                    network: workload.network.clone(),
                    lifecycle: vec![
                        ExecutionStatus::Created,
                        ExecutionStatus::Resolved,
                        ExecutionStatus::Prepared,
                        ExecutionStatus::Started,
                        ExecutionStatus::Running,
                        status.clone(),
                    ],
                    status,
                    exit_code: None,
                    stdout: Output::from_bytes(stdout, workload.resources.stdout_bytes),
                    stderr: Output::from_bytes(stderr, workload.resources.stderr_bytes),
                    duration,
                    resource_usage: ResourceUsage {
                        max_memory_bytes: workload.resources.memory_bytes,
                    },
                    artifacts: collect_artifacts(&staged.output_dir)?,
                    outputs: vec![],
                    missing_outputs: vec![],
                    error: Some(compute_core::ExecutionError {
                        execution_id,
                        phase: compute_core::ExecutionPhase::Running,
                        kind: if timed_out {
                            compute_core::ExecutionErrorKind::Timeout
                        } else {
                            compute_core::ExecutionErrorKind::Runtime
                        },
                        message: error.to_string(),
                        runtime: Some(RuntimeKind::Wasm),
                        exit_code: None,
                        started: true,
                    }),
                    isolation: None,
                    receipt: None,
                },
                workload,
                &staged.output_dir,
            )
        }
    }
}

fn finalize_result(
    mut result: ExecutionResult,
    workload: &Workload,
    output_dir: &std::path::Path,
) -> Result<ExecutionResult> {
    apply_output_contract(&mut result, output_dir, &workload.outputs)?;
    Ok(result)
}

fn failure_result(
    workload: &Workload,
    execution_id: String,
    phase: ExecutionPhase,
    kind: ExecutionErrorKind,
    message: String,
    mut lifecycle: Vec<ExecutionStatus>,
) -> ExecutionResult {
    lifecycle.push(ExecutionStatus::Failed);
    ExecutionResult {
        execution_id: execution_id.clone(),
        runtime: RuntimeKind::Wasm,
        network: workload.network.clone(),
        lifecycle,
        status: ExecutionStatus::Failed,
        exit_code: None,
        stdout: Output::from_bytes(vec![], None),
        stderr: Output::from_bytes(vec![], None),
        duration: std::time::Duration::ZERO,
        resource_usage: ResourceUsage::default(),
        artifacts: vec![],
        outputs: vec![],
        missing_outputs: vec![],
        error: Some(ExecutionError {
            execution_id,
            phase,
            kind,
            message,
            runtime: Some(RuntimeKind::Wasm),
            exit_code: None,
            started: false,
        }),
        isolation: None,
        receipt: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use compute_core::{NetworkPolicy, ResourceLimits, RuntimeSpec};

    #[tokio::test]
    async fn invalid_module_is_a_structured_start_failure() {
        let temp = tempfile::tempdir().unwrap();
        let entrypoint = temp.path().join("invalid.wasm");
        std::fs::write(&entrypoint, b"not wasm").unwrap();
        let workload = Workload {
            runtime: RuntimeSpec {
                kind: RuntimeKind::Wasm,
                version: Some("wasi".into()),
            },
            entrypoint,
            args: vec![],
            stdin: vec![],
            env: vec![],
            inputs: vec![],
            outputs: vec![],
            mounts: vec![],
            network: NetworkPolicy::None,
            resources: ResourceLimits::default(),
            isolation: compute_core::IsolationProfile::Process,
        };
        let adapter = WasmRuntime;
        let resolved = adapter.resolve(&workload).await.unwrap();
        let result = adapter.execute(&workload, &resolved).await.unwrap();

        assert_eq!(result.status, ExecutionStatus::Failed);
        assert!(!result.execution_id.is_empty());
        assert_eq!(result.lifecycle.last(), Some(&ExecutionStatus::Failed));
        let error = result.error.unwrap();
        assert_eq!(error.kind, ExecutionErrorKind::Start);
        assert_eq!(error.phase, ExecutionPhase::Prepared);
        assert!(!error.started);
        assert_eq!(error.execution_id, result.execution_id);
    }
}
