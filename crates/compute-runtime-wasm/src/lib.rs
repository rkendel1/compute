use std::time::Instant;

use async_trait::async_trait;
use compute_core::{
    ComputeError, ExecutionResult, ExecutionStatus, NetworkPolicy, Output, ResolvedRuntime,
    ResourceUsage, Result, RuntimeAdapter, RuntimeAvailability, RuntimeKind, Workload,
    stage_workload,
};
use wasmtime::{Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::preview1::{self, WasiP1Ctx};
use wasmtime_wasi::{I32Exit, WasiCtxBuilder};

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
        }
    }

    async fn resolve(&self, workload: &Workload) -> Result<ResolvedRuntime> {
        if let Some(version) = &workload.runtime.version {
            if !version.eq_ignore_ascii_case("wasi") {
                return Err(ComputeError::RuntimeVersionMismatch {
                    kind: RuntimeKind::Wasm,
                    requested: version.clone(),
                    found: "wasi".to_string(),
                });
            }
        }

        Ok(ResolvedRuntime {
            kind: RuntimeKind::Wasm,
            requested_version: workload.runtime.version.clone(),
            resolved_version: Some("wasi".to_string()),
            executable: None,
        })
    }

    async fn execute(
        &self,
        workload: &Workload,
        _runtime: &ResolvedRuntime,
    ) -> Result<ExecutionResult> {
        if workload.network != NetworkPolicy::None {
            return Err(ComputeError::UnsupportedCapability {
                runtime: RuntimeKind::Wasm,
                capability: "only --network none is currently supported for wasm workloads"
                    .to_string(),
            });
        }

        let workload = workload.clone();
        tokio::task::spawn_blocking(move || execute_blocking(&workload))
            .await
            .map_err(|error| ComputeError::Runtime(error.to_string()))?
    }
}

fn execute_blocking(workload: &Workload) -> Result<ExecutionResult> {
    let staged = stage_workload(workload)?;
    let capture_limit = workload
        .resources
        .stdout_bytes
        .or(workload.resources.stderr_bytes)
        .unwrap_or(8 * 1024 * 1024);
    let capture_limit = usize::try_from(capture_limit).unwrap_or(8 * 1024 * 1024);
    let stdout = MemoryOutputPipe::new(capture_limit);
    let stderr = MemoryOutputPipe::new(capture_limit);

    let mut config = Config::new();
    if workload.resources.wall_time.is_some() {
        config.epoch_interruption(true);
    }
    let engine = Engine::new(&config).map_err(|error| ComputeError::Runtime(error.to_string()))?;
    let module = Module::from_file(&engine, &staged.entrypoint)
        .map_err(|error| ComputeError::Runtime(error.to_string()))?;
    let mut linker = Linker::new(&engine);
    preview1::add_to_linker_sync(&mut linker, |state: &mut WasmState| &mut state.wasi)
        .map_err(|error| ComputeError::Runtime(error.to_string()))?;

    let mut limits = StoreLimitsBuilder::new();
    if let Some(memory) = workload.resources.memory_bytes {
        limits = limits.memory_size(memory as usize);
    }

    let mut wasi = WasiCtxBuilder::new();
    wasi.allow_blocking_current_thread(true);
    let argv = std::iter::once(staged.entrypoint.to_string_lossy().to_string())
        .chain(workload.args.iter().cloned())
        .collect::<Vec<_>>();
    wasi.args(&argv);
    for env in &workload.env {
        wasi.env(&env.key, &env.value);
    }
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
    if let Some(timeout) = workload.resources.wall_time {
        store.set_epoch_deadline(1);
        let engine = engine.clone();
        std::thread::spawn(move || {
            std::thread::sleep(timeout);
            engine.increment_epoch();
        });
    }
    let call_result = command.call(&mut store, ());
    let duration = start.elapsed();

    let stdout = stdout.contents().to_vec();
    let stderr = stderr.contents().to_vec();

    match call_result {
        Ok(()) => Ok(ExecutionResult {
            status: ExecutionStatus::Completed,
            exit_code: Some(0),
            stdout: Output::from_bytes(stdout, workload.resources.stdout_bytes),
            stderr: Output::from_bytes(stderr, workload.resources.stderr_bytes),
            duration,
            resource_usage: ResourceUsage {
                max_memory_bytes: workload.resources.memory_bytes,
            },
            artifacts: vec![],
        }),
        Err(error) => {
            if let Some(exit) = error.downcast_ref::<I32Exit>() {
                let code = exit.0;
                return Ok(ExecutionResult {
                    status: if code == 0 {
                        ExecutionStatus::Completed
                    } else {
                        ExecutionStatus::Failed
                    },
                    exit_code: Some(code),
                    stdout: Output::from_bytes(stdout, workload.resources.stdout_bytes),
                    stderr: Output::from_bytes(stderr, workload.resources.stderr_bytes),
                    duration,
                    resource_usage: ResourceUsage {
                        max_memory_bytes: workload.resources.memory_bytes,
                    },
                    artifacts: vec![],
                });
            }

            let status = if workload.resources.wall_time.is_some()
                && error.to_string().to_ascii_lowercase().contains("interrupt")
            {
                ExecutionStatus::TimedOut
            } else {
                ExecutionStatus::Failed
            };
            Ok(ExecutionResult {
                status,
                exit_code: Some(1),
                stdout: Output::from_bytes(stdout, workload.resources.stdout_bytes),
                stderr: Output::from_bytes(stderr, workload.resources.stderr_bytes),
                duration,
                resource_usage: ResourceUsage {
                    max_memory_bytes: workload.resources.memory_bytes,
                },
                artifacts: vec![],
            })
        }
    }
}
