use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

use async_trait::async_trait;
use compute_core::{
    ComputeError, ExecutionControl, ExecutionError, ExecutionErrorKind, ExecutionPhase,
    ExecutionResult, ExecutionStatus, Output, ResolvedRuntime, ResourceUsage, Result,
    RuntimeAdapter, RuntimeAvailability, RuntimeCapabilities, RuntimeDescriptor, RuntimeKind,
    RuntimeSource, Workload, apply_output_contract, collect_artifacts, new_execution_id,
    stage_workload,
};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

#[cfg(target_os = "linux")]
mod sandbox;

#[derive(Debug, Clone)]
pub struct ProcessRuntime {
    kind: RuntimeKind,
    distribution_root: Option<PathBuf>,
}

impl ProcessRuntime {
    pub const fn new(kind: RuntimeKind) -> Self {
        Self {
            kind,
            distribution_root: None,
        }
    }

    pub fn with_distribution_root(kind: RuntimeKind, root: PathBuf) -> Self {
        Self {
            kind,
            distribution_root: Some(root),
        }
    }

    fn definition(&self) -> RuntimeDefinition {
        runtime_definition(self.kind)
    }

    fn discover(&self) -> DiscoveredRuntime {
        let definition = self.definition();
        if self.kind == RuntimeKind::Native {
            return if cfg!(target_os = "linux") {
                DiscoveredRuntime::available(
                    PathBuf::from("<workload-entrypoint>"),
                    definition.version.clone(),
                    RuntimeSource::Embedded,
                )
            } else {
                DiscoveredRuntime::unavailable(
                    RuntimeSource::Unavailable,
                    "native workloads require a Linux Compute distribution".into(),
                )
            };
        }
        match self.distribution_root() {
            Some(Ok(root)) => {
                let discovered = discover_distribution_runtime(&root, &definition);
                if !discovered.installed
                    && discovered
                        .remediation
                        .as_deref()
                        .is_some_and(|message| message.starts_with("runtime is not prepared:"))
                {
                    discover_host_runtime(&definition)
                } else {
                    discovered
                }
            }
            Some(Err(message)) => {
                DiscoveredRuntime::unavailable(RuntimeSource::Distribution, message)
            }
            None => discover_host_runtime(&definition),
        }
    }

    fn distribution_root(&self) -> Option<std::result::Result<PathBuf, String>> {
        if let Some(root) = &self.distribution_root {
            return root
                .join("runtime-manifest.json")
                .is_file()
                .then(|| Ok(root.clone()));
        }
        distribution_root()
    }
}

#[derive(Debug, Clone)]
struct RuntimeDefinition {
    kind: RuntimeKind,
    version: String,
    executable: String,
    host_names: &'static [&'static str],
    invocation: Invocation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Invocation {
    Direct,
    Deno,
    Jvm,
    Dotnet,
    Native,
    Shell,
}

fn runtime_definition(kind: RuntimeKind) -> RuntimeDefinition {
    let lock: RuntimeLock =
        serde_json::from_str(include_str!("../../../distribution/runtime-lock.json"))
            .expect("valid embedded runtime lock");
    assert_eq!(lock.schema_version, 2, "supported runtime lock version");
    let locked = lock
        .runtimes
        .get(kind.as_str())
        .unwrap_or_else(|| panic!("runtime lock is missing {kind}"));
    let (host_names, invocation) = match kind {
        RuntimeKind::Python => (&["python3", "python"][..], Invocation::Direct),
        RuntimeKind::Node => (&["node"][..], Invocation::Direct),
        RuntimeKind::Bun => (&["bun"][..], Invocation::Direct),
        RuntimeKind::Deno => (&["deno"][..], Invocation::Deno),
        RuntimeKind::Ruby => (&["ruby"][..], Invocation::Direct),
        RuntimeKind::Php => (&["php"][..], Invocation::Direct),
        RuntimeKind::Jvm => (&["java"][..], Invocation::Jvm),
        RuntimeKind::Dotnet => (&["dotnet"][..], Invocation::Dotnet),
        RuntimeKind::Native => (&[][..], Invocation::Native),
        RuntimeKind::Shell => (&["sh"][..], Invocation::Shell),
        RuntimeKind::Wasm => unreachable!("WASM is embedded"),
    };
    RuntimeDefinition {
        kind,
        version: locked.version.clone(),
        executable: locked.executable.clone(),
        host_names,
        invocation,
    }
}

#[derive(Debug, Deserialize)]
struct RuntimeLock {
    schema_version: u32,
    runtimes: BTreeMap<String, DistributionRuntime>,
}

#[derive(Debug)]
struct DiscoveredRuntime {
    path: Option<PathBuf>,
    version: Option<String>,
    installed: bool,
    available: bool,
    source: RuntimeSource,
    remediation: Option<String>,
}

impl DiscoveredRuntime {
    fn available(path: PathBuf, version: String, source: RuntimeSource) -> Self {
        Self {
            path: Some(path),
            version: Some(version),
            installed: true,
            available: true,
            source,
            remediation: None,
        }
    }

    fn unavailable(source: RuntimeSource, remediation: String) -> Self {
        Self {
            path: None,
            version: None,
            installed: false,
            available: false,
            source,
            remediation: Some(remediation),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DistributionManifest {
    #[serde(rename = "schema_version")]
    _schema_version: u32,
    compute_version: String,
    #[serde(rename = "distribution_id")]
    _distribution_id: String,
    distribution_version: String,
    platform: String,
    #[serde(rename = "os")]
    _os: String,
    #[serde(rename = "architecture")]
    _architecture: String,
    #[serde(rename = "runtime_lock_sha256")]
    _runtime_lock_sha256: String,
    #[serde(rename = "certification_status")]
    _certification_status: String,
    #[serde(rename = "build")]
    _build: serde_json::Value,
    runtimes: BTreeMap<String, DistributionRuntime>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DistributionRuntime {
    version: String,
    executable: String,
    #[serde(default)]
    #[serde(rename = "artifacts")]
    _artifacts: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    #[serde(rename = "artifact_sha256")]
    _artifact_sha256: String,
    #[serde(default)]
    #[serde(rename = "payload_sha256")]
    _payload_sha256: String,
    #[serde(default)]
    #[serde(rename = "reported_version")]
    _reported_version: String,
    #[serde(default)]
    #[serde(rename = "distribution_id")]
    _distribution_id: Option<String>,
    #[serde(default)]
    #[serde(rename = "distribution_digest")]
    _distribution_digest: Option<String>,
    #[serde(default)]
    #[serde(rename = "capabilities")]
    _capabilities: Option<RuntimeCapabilities>,
}

fn distribution_root() -> Option<std::result::Result<PathBuf, String>> {
    if let Some(root) = std::env::var_os("COMPUTE_HOME") {
        let root = PathBuf::from(root);
        return Some(if root.join("runtime-manifest.json").is_file() {
            Ok(root)
        } else {
            Err(format!(
                "COMPUTE_HOME does not contain runtime-manifest.json: {}",
                root.display()
            ))
        });
    }
    let executable = std::env::current_exe().ok()?;
    let root = executable.parent()?.parent()?.to_path_buf();
    root.join("runtime-manifest.json")
        .is_file()
        .then_some(Ok(root))
}

fn discover_distribution_runtime(root: &Path, definition: &RuntimeDefinition) -> DiscoveredRuntime {
    let manifest_path = root.join("runtime-manifest.json");
    let manifest = match std::fs::read(&manifest_path)
        .map_err(|error| error.to_string())
        .and_then(|bytes| {
            serde_json::from_slice::<DistributionManifest>(&bytes)
                .map_err(|error| error.to_string())
        }) {
        Ok(manifest) => manifest,
        Err(error) => {
            return DiscoveredRuntime::unavailable(
                RuntimeSource::Distribution,
                format!("invalid runtime manifest: {error}"),
            );
        }
    };
    let expected_compute = env!("CARGO_PKG_VERSION");
    let expected_platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let expected_distribution = format!("compute-{expected_compute}-{expected_platform}");
    if manifest.compute_version != expected_compute
        || manifest.platform != expected_platform
        || manifest.distribution_version != expected_distribution
    {
        return DiscoveredRuntime::unavailable(
            RuntimeSource::Distribution,
            format!(
                "incompatible Compute distribution: expected {expected_distribution} for Compute {expected_compute}, found {} for Compute {}",
                manifest.distribution_version, manifest.compute_version
            ),
        );
    }
    let Some(runtime) = manifest.runtimes.get(definition.kind.as_str()) else {
        return DiscoveredRuntime::unavailable(
            RuntimeSource::Distribution,
            format!("runtime is not prepared: {}", definition.kind),
        );
    };
    if runtime.version != definition.version || runtime.executable != definition.executable {
        return DiscoveredRuntime::unavailable(
            RuntimeSource::Distribution,
            format!(
                "runtime manifest mismatch for {}: expected {} at {}, declared {} at {}",
                definition.kind,
                definition.version,
                definition.executable,
                runtime.version,
                runtime.executable
            ),
        );
    }
    let relative = Path::new(&runtime.executable);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return DiscoveredRuntime::unavailable(
            RuntimeSource::Distribution,
            format!(
                "runtime executable must be a portable relative path: {}",
                runtime.executable
            ),
        );
    }
    let path = root.join(relative);
    discover_executable(&path, definition, RuntimeSource::Distribution, true)
}

fn discover_host_runtime(definition: &RuntimeDefinition) -> DiscoveredRuntime {
    for name in definition.host_names {
        if let Ok(path) = which::which(name) {
            let discovered =
                discover_executable(&path, definition, RuntimeSource::HostDevelopment, false);
            if discovered.available {
                return discovered;
            }
        }
    }
    DiscoveredRuntime::unavailable(
        RuntimeSource::Unavailable,
        format!(
            "install the Compute runtime distribution containing {} {}",
            definition.kind, definition.version
        ),
    )
}

fn discover_executable(
    path: &Path,
    definition: &RuntimeDefinition,
    source: RuntimeSource,
    require_pinned_version: bool,
) -> DiscoveredRuntime {
    if !path.is_file() {
        return DiscoveredRuntime::unavailable(
            source,
            format!("runtime executable is missing: {}", path.display()),
        );
    }
    let mut probe = std::process::Command::new(path);
    match definition.invocation {
        Invocation::Dotnet => {
            probe.arg("--list-runtimes");
        }
        Invocation::Shell => {
            probe.arg("--help");
        }
        _ => {
            probe.arg("--version");
        }
    }
    let output = match probe.output() {
        Ok(output) if output.status.success() => output,
        // A POSIX shell without `--help` (dash is /bin/sh on Debian and
        // Ubuntu) is identified by the shell it resolves to.
        Ok(_) if definition.invocation == Invocation::Shell => {
            match std::process::Command::new(path)
                .args(["-c", "echo posix-sh"])
                .output()
            {
                Ok(output) if output.status.success() => {
                    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.into());
                    let name = resolved
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "sh".into());
                    std::process::Output {
                        status: output.status,
                        stdout: format!("POSIX shell ({name})").into_bytes(),
                        stderr: vec![],
                    }
                }
                _ => {
                    return DiscoveredRuntime::unavailable(
                        source,
                        format!("{} is not a working POSIX shell", path.display()),
                    );
                }
            }
        }
        Ok(output) => {
            return DiscoveredRuntime::unavailable(
                source,
                format!(
                    "runtime version probe failed for {} with status {}",
                    path.display(),
                    output.status
                ),
            );
        }
        Err(error) => {
            return DiscoveredRuntime::unavailable(
                source,
                format!(
                    "runtime version probe failed for {}: {error}",
                    path.display()
                ),
            );
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let version = if stdout.is_empty() { stderr } else { stdout };
    if require_pinned_version
        && !compute_core::runtime_version_matches(definition.kind, &definition.version, &version)
    {
        return DiscoveredRuntime {
            path: Some(path.to_path_buf()),
            version: Some(version.clone()),
            installed: true,
            available: false,
            source,
            remediation: Some(format!(
                "replace {} with Compute-pinned {} {} (detected {version})",
                path.display(),
                definition.kind,
                definition.version
            )),
        };
    }
    DiscoveredRuntime::available(path.to_path_buf(), version, source)
}

#[async_trait]
impl RuntimeAdapter for ProcessRuntime {
    fn kind(&self) -> RuntimeKind {
        self.kind
    }

    fn descriptor(&self) -> RuntimeDescriptor {
        let definition = self.definition();
        RuntimeDescriptor {
            id: self.kind,
            version: definition.version,
            executable: definition.executable,
            capabilities: self.capabilities(),
        }
    }

    async fn availability(&self, requested: Option<&str>) -> RuntimeAvailability {
        let discovered = self.discover();
        let compatible = discovered.available
            && requested
                .map(|requested| {
                    discovered.version.as_deref().is_some_and(|version| {
                        compute_core::runtime_version_matches(self.kind, requested, version)
                    })
                })
                .unwrap_or(true);
        RuntimeAvailability {
            kind: self.kind,
            version: discovered.version,
            known: true,
            installed: discovered.installed,
            available: discovered.available,
            compatible,
            selected: false,
            executable: discovered.path,
            source: discovered.source,
            expected_version: Some(self.definition().version),
            remediation: discovered.remediation,
        }
    }

    async fn resolve(&self, workload: &Workload) -> Result<ResolvedRuntime> {
        let runtime = self.availability(workload.runtime.version.as_deref()).await;
        if !runtime.available {
            return Err(ComputeError::RuntimeUnavailable(self.kind));
        }

        if let (Some(requested), Some(found)) = (&workload.runtime.version, &runtime.version)
            && !compute_core::runtime_version_matches(self.kind, requested, found)
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
        if self.kind == RuntimeKind::Deno {
            RuntimeCapabilities::deno()
        } else {
            RuntimeCapabilities::process()
        }
    }

    async fn execute(
        &self,
        workload: &Workload,
        runtime: &ResolvedRuntime,
    ) -> Result<ExecutionResult> {
        self.execute_with(workload, runtime, None).await
    }

    async fn execute_controlled(
        &self,
        workload: &Workload,
        runtime: &ResolvedRuntime,
        control: &ExecutionControl,
    ) -> Result<ExecutionResult> {
        self.execute_with(workload, runtime, Some(control)).await
    }
}

impl ProcessRuntime {
    async fn execute_with(
        &self,
        workload: &Workload,
        runtime: &ResolvedRuntime,
        control: Option<&ExecutionControl>,
    ) -> Result<ExecutionResult> {
        self.capabilities().validate(self.kind, workload)?;

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
        if self.definition().invocation == Invocation::Dotnet {
            stage_dotnet_companions(&workload.entrypoint, &staged.entrypoint)?;
        }
        let definition = self.definition();
        let mut command = match definition.invocation {
            Invocation::Native => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mut permissions = std::fs::metadata(&staged.entrypoint)?.permissions();
                    permissions.set_mode(0o700);
                    std::fs::set_permissions(&staged.entrypoint, permissions)?;
                }
                Command::new(&staged.entrypoint)
            }
            _ => Command::new(
                runtime
                    .executable
                    .as_ref()
                    .ok_or(ComputeError::RuntimeUnavailable(self.kind))?,
            ),
        };
        match definition.invocation {
            Invocation::Deno => {
                command
                    .arg("run")
                    .arg(format!(
                        "--allow-env={}",
                        std::iter::once("COMPUTE_WORK_DIR")
                            .chain(std::iter::once("COMPUTE_TMP_DIR"))
                            .chain(std::iter::once("COMPUTE_OUTPUT_DIR"))
                            .chain(workload.env.iter().map(|pair| pair.key.as_str()))
                            .collect::<Vec<_>>()
                            .join(",")
                    ))
                    .arg(format!("--allow-read={}", staged.work_dir.display()))
                    .arg(format!(
                        "--allow-write={},{}",
                        staged.tmp_dir.display(),
                        staged.output_dir.display()
                    ));
                match workload.network {
                    compute_core::NetworkPolicy::None => {}
                    compute_core::NetworkPolicy::Localhost => {
                        command.arg("--allow-net=localhost,127.0.0.1,[::1]");
                    }
                    compute_core::NetworkPolicy::Network => {
                        command.arg("--allow-net");
                    }
                }
                command.arg(&staged.entrypoint);
            }
            Invocation::Jvm => {
                command.args(["-jar"]).arg(&staged.entrypoint);
            }
            Invocation::Direct
                if self.kind == RuntimeKind::Python && workload.dependencies.is_some() =>
            {
                command.arg("-S").arg(&staged.entrypoint);
            }
            Invocation::Dotnet | Invocation::Shell | Invocation::Direct => {
                command.arg(&staged.entrypoint);
            }
            Invocation::Native => {}
        }
        command
            .args(&workload.args)
            .current_dir(&staged.work_dir)
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear();
        #[cfg(unix)]
        command.process_group(0);

        for pair in &workload.env {
            command.env(&pair.key, &pair.value);
        }
        // With a cleared environment the JVM has no locale and decodes
        // arguments and file names as ASCII, replacing non-ASCII text with
        // '?'. Supply a UTF-8 character type unless the workload chose one.
        if self.kind == RuntimeKind::Jvm
            && !workload
                .env
                .iter()
                .any(|pair| pair.key == "LC_ALL" || pair.key == "LC_CTYPE")
        {
            command.env("LC_CTYPE", "C.UTF-8");
        }
        if self.kind == RuntimeKind::Python {
            command.env("PYTHONDONTWRITEBYTECODE", "1");
            command.env("PYTHONNOUSERSITE", "1");
        }
        if let Some(dependencies) = &staged.dependencies_dir {
            match self.kind {
                RuntimeKind::Python => {
                    command.env("PYTHONPATH", dependencies);
                }
                RuntimeKind::Node | RuntimeKind::Bun => {
                    command.env("NODE_PATH", dependencies);
                }
                RuntimeKind::Ruby => {
                    command
                        .env("GEM_HOME", dependencies)
                        .env("GEM_PATH", dependencies);
                }
                RuntimeKind::Jvm => {
                    let classpath = std::fs::read_dir(dependencies)?
                        .filter_map(std::result::Result::ok)
                        .map(|entry| entry.path())
                        .filter(|path| path.extension().is_some_and(|extension| extension == "jar"))
                        .collect::<Vec<_>>();
                    let classpath = std::env::join_paths(classpath).map_err(|error| {
                        ComputeError::Runtime(format!("invalid dependency classpath: {error}"))
                    })?;
                    command.env("CLASSPATH", classpath);
                }
                RuntimeKind::Dotnet => {
                    command.env("NUGET_PACKAGES", dependencies);
                }
                _ => {}
            }
        }
        command
            .env("COMPUTE_WORK_DIR", &staged.work_dir)
            .env("COMPUTE_TMP_DIR", &staged.tmp_dir)
            .env("COMPUTE_OUTPUT_DIR", &staged.output_dir);

        // A restricted or isolated host profile is enforced by the kernel,
        // or the execution does not start: never a silent downgrade.
        #[cfg(target_os = "linux")]
        let _sandbox = match self.capabilities().host_plan(self.kind, workload)? {
            Some(plan) => {
                let sandbox = sandbox::Sandbox::prepare(
                    &plan,
                    compute_core::host::host_capabilities(),
                    workload,
                    staged.root.path(),
                    runtime.executable.as_deref(),
                    staged.dependencies_dir.as_deref(),
                )?;
                sandbox.install(&mut command);
                command.env("TMPDIR", &staged.tmp_dir);
                Some(sandbox)
            }
            None => None,
        };

        let started = Instant::now();
        let mut child = match command.spawn() {
            Ok(child) => {
                if let (Some(control), Some(pid)) = (control, child.id()) {
                    control.record_process(pid);
                }
                child
            }
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
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| ComputeError::Runtime("stdin pipe missing".into()))?;
        let input = workload.stdin.clone();
        let stdin_task = tokio::spawn(async move {
            stdin.write_all(&input).await?;
            stdin.shutdown().await
        });
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
        let (mut stdout_log, mut stderr_log) = open_logs(control).await?;
        let mut stdout_buffer = Vec::new();
        let mut stderr_buffer = Vec::new();
        let read_output = async {
            let stdout_read = read_teed(
                &mut stdout,
                stdout_limit,
                &mut stdout_buffer,
                stdout_log.as_mut(),
            );
            let stderr_read = read_teed(
                &mut stderr,
                stderr_limit,
                &mut stderr_buffer,
                stderr_log.as_mut(),
            );
            let (stdout, stderr, status) = tokio::join!(stdout_read, stderr_read, child.wait());
            stdout?;
            stderr?;
            status
        };
        let deadline = async {
            match workload.resources.wall_time {
                Some(timeout) => tokio::time::sleep(timeout).await,
                None => std::future::pending::<()>().await,
            }
        };
        let cancelled = async {
            match control {
                Some(control) => loop {
                    if control.is_cancelled() {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                },
                None => std::future::pending::<()>().await,
            }
        };
        let outcome = tokio::select! {
            result = read_output => Interruption::None(result?),
            () = deadline => Interruption::TimedOut,
            () = cancelled => Interruption::Cancelled,
        };
        let (status, timed_out, was_cancelled) = match outcome {
            Interruption::None(status) => (Some(status), false, false),
            interrupted => {
                terminate(&mut child).await;
                let _ = child.wait().await;
                let (stdout_rest, stderr_rest) = tokio::join!(
                    read_teed(
                        &mut stdout,
                        stdout_limit,
                        &mut stdout_buffer,
                        stdout_log.as_mut()
                    ),
                    read_teed(
                        &mut stderr,
                        stderr_limit,
                        &mut stderr_buffer,
                        stderr_log.as_mut()
                    )
                );
                stdout_rest?;
                stderr_rest?;
                (
                    None,
                    matches!(interrupted, Interruption::TimedOut),
                    matches!(interrupted, Interruption::Cancelled),
                )
            }
        };
        let (stdout, stderr) = (stdout_buffer, stderr_buffer);

        let process_status = status;
        stdin_task
            .await
            .map_err(|error| ComputeError::Runtime(error.to_string()))??;
        let execution_status = if was_cancelled {
            ExecutionStatus::Cancelled
        } else if timed_out {
            ExecutionStatus::TimedOut
        } else if process_status
            .as_ref()
            .is_some_and(|status| status.code().is_none())
        {
            // Terminated by a signal from outside Compute: not a clean exit.
            ExecutionStatus::Killed
        } else {
            // A workload's exit status is data, not a failure of Compute itself.
            ExecutionStatus::Completed
        };

        let mut result = ExecutionResult {
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
            outputs: vec![],
            missing_outputs: vec![],
            error: (timed_out || was_cancelled).then(|| ExecutionError {
                execution_id,
                phase: ExecutionPhase::Running,
                kind: if was_cancelled {
                    ExecutionErrorKind::Cancelled
                } else {
                    ExecutionErrorKind::Timeout
                },
                message: if was_cancelled {
                    "execution was cancelled".to_string()
                } else {
                    "wall time limit exceeded".to_string()
                },
                runtime: Some(self.kind),
                exit_code: None,
                started: true,
            }),
            isolation: None,
            dependencies: None,
            provider: None,
            admission: None,
            receipt: None,
        };
        apply_output_contract(&mut result, &staged.output_dir, &workload.outputs)?;
        Ok(result)
    }
}

fn stage_dotnet_companions(source: &Path, staged: &Path) -> Result<()> {
    let Some(stem) = source.file_stem().and_then(|value| value.to_str()) else {
        return Ok(());
    };
    for suffix in ["runtimeconfig.json", "deps.json"] {
        let companion = source.with_file_name(format!("{stem}.{suffix}"));
        if companion.is_file() {
            let destination = staged.with_file_name(format!("{stem}.{suffix}"));
            std::fs::copy(companion, destination)?;
        }
    }
    Ok(())
}

async fn terminate(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // The child is placed in a fresh process group before spawn. Killing
        // that group reaps workload descendants as part of timeout cleanup.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
        return;
    }
    let _ = child.kill().await;
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
        runtime: workload.runtime.kind,
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
            runtime: Some(workload.runtime.kind),
            exit_code: None,
            started: false,
        }),
        isolation: None,
        dependencies: None,
        provider: None,
        admission: None,
        receipt: None,
    }
}

enum Interruption {
    None(std::process::ExitStatus),
    TimedOut,
    Cancelled,
}

async fn open_logs(
    control: Option<&ExecutionControl>,
) -> std::io::Result<(Option<tokio::fs::File>, Option<tokio::fs::File>)> {
    let Some(directory) = control.and_then(ExecutionControl::log_directory) else {
        return Ok((None, None));
    };
    tokio::fs::create_dir_all(directory).await?;
    let mut options = tokio::fs::OpenOptions::new();
    options.create(true).append(true);
    Ok((
        Some(options.open(directory.join("stdout.log")).await?),
        Some(options.open(directory.join("stderr.log")).await?),
    ))
}

/// Read to end, keeping at most `limit + 1` bytes in memory and appending
/// every byte to `log` as it arrives.
async fn read_teed<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    limit: Option<u64>,
    output: &mut Vec<u8>,
    mut log: Option<&mut tokio::fs::File>,
) -> std::io::Result<()> {
    let max = limit.and_then(|value| usize::try_from(value).ok());
    let mut buffer = [0_u8; 8192];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        if let Some(log) = log.as_deref_mut() {
            log.write_all(&buffer[..count]).await?;
            log.flush().await?;
        }
        match max {
            Some(max) => {
                let remaining = max.saturating_add(1).saturating_sub(output.len());
                output.extend_from_slice(&buffer[..count.min(remaining)]);
            }
            None => output.extend_from_slice(&buffer[..count]),
        }
    }
}

macro_rules! process_adapter {
    ($name:ident, $kind:ident) => {
        #[derive(Debug, Default, Clone)]
        pub struct $name;

        #[async_trait]
        impl RuntimeAdapter for $name {
            fn kind(&self) -> RuntimeKind {
                RuntimeKind::$kind
            }

            fn descriptor(&self) -> RuntimeDescriptor {
                ProcessRuntime::new(self.kind()).descriptor()
            }

            fn capabilities(&self) -> RuntimeCapabilities {
                ProcessRuntime::new(self.kind()).capabilities()
            }

            async fn availability(&self, requested: Option<&str>) -> RuntimeAvailability {
                ProcessRuntime::new(self.kind())
                    .availability(requested)
                    .await
            }

            async fn resolve(&self, workload: &Workload) -> Result<ResolvedRuntime> {
                ProcessRuntime::new(self.kind()).resolve(workload).await
            }

            async fn execute(
                &self,
                workload: &Workload,
                runtime: &ResolvedRuntime,
            ) -> Result<ExecutionResult> {
                ProcessRuntime::new(self.kind())
                    .execute(workload, runtime)
                    .await
            }

            async fn execute_controlled(
                &self,
                workload: &Workload,
                runtime: &ResolvedRuntime,
                control: &ExecutionControl,
            ) -> Result<ExecutionResult> {
                ProcessRuntime::new(self.kind())
                    .execute_controlled(workload, runtime, control)
                    .await
            }
        }
    };
}

process_adapter!(NodeRuntime, Node);
process_adapter!(BunRuntime, Bun);
process_adapter!(DenoRuntime, Deno);
process_adapter!(PythonRuntime, Python);
process_adapter!(RubyRuntime, Ruby);
process_adapter!(PhpRuntime, Php);
process_adapter!(JvmRuntime, Jvm);
process_adapter!(DotnetRuntime, Dotnet);
process_adapter!(NativeRuntime, Native);
process_adapter!(ShellRuntime, Shell);

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn availability_reports_known_runtime() {
        let runtime = ProcessRuntime::new(RuntimeKind::Node);
        let availability = runtime.availability(None).await;
        assert!(availability.known);
    }

    #[test]
    fn process_runtime_can_be_constructed() {
        let runtime = ProcessRuntime::new(RuntimeKind::Python);
        assert_eq!(runtime.kind(), RuntimeKind::Python);
    }

    #[cfg(unix)]
    #[test]
    fn distribution_resolution_is_pinned_and_never_falls_back_to_path() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let definition = runtime_definition(RuntimeKind::Ruby);
        let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
        let manifest = serde_json::json!({
            "schema_version": 2,
            "compute_version": "0.1.0",
            "distribution_id": "sha256:test",
            "distribution_version": format!("compute-0.1.0-{platform}"),
            "platform": platform,
            "os": std::env::consts::OS,
            "architecture": std::env::consts::ARCH,
            "runtime_lock_sha256": "test",
            "certification_status": "not_run",
            "build": {},
            "runtimes": {
                "ruby": {
                    "version": definition.version,
                    "executable": definition.executable,
                    "artifact_sha256": "test",
                    "payload_sha256": "test",
                    "reported_version": definition.version,
                }
            }
        });
        std::fs::write(
            root.path().join("runtime-manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();

        let missing = discover_distribution_runtime(root.path(), &definition);
        assert!(!missing.available);
        assert_eq!(missing.source, RuntimeSource::Distribution);
        assert!(missing.remediation.unwrap().contains("missing"));

        let executable = root.path().join(&definition.executable);
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(
            &executable,
            format!("#!/bin/sh\nprintf 'ruby {}\\n'\n", definition.version),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();
        let available = discover_distribution_runtime(root.path(), &definition);
        assert!(available.available);
        assert_eq!(available.path, Some(executable.clone()));

        std::fs::write(&executable, "#!/bin/sh\necho wrong-version\n").unwrap();
        let wrong = discover_distribution_runtime(root.path(), &definition);
        assert!(wrong.installed);
        assert!(!wrong.available);
        assert!(wrong.remediation.unwrap().contains(&definition.version));
    }
}
