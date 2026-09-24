use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use thiserror::Error;
use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeKind {
    Wasm,
    Node,
    Bun,
    Deno,
    Python,
}

impl RuntimeKind {
    pub const ALL: [RuntimeKind; 5] = [
        RuntimeKind::Wasm,
        RuntimeKind::Node,
        RuntimeKind::Bun,
        RuntimeKind::Deno,
        RuntimeKind::Python,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            RuntimeKind::Wasm => "wasm",
            RuntimeKind::Node => "node",
            RuntimeKind::Bun => "bun",
            RuntimeKind::Deno => "deno",
            RuntimeKind::Python => "python",
        }
    }
}

impl std::fmt::Display for RuntimeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for RuntimeKind {
    type Err = ComputeError;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "wasm" => Ok(Self::Wasm),
            "node" => Ok(Self::Node),
            "bun" => Ok(Self::Bun),
            "deno" => Ok(Self::Deno),
            "python" => Ok(Self::Python),
            other => Err(ComputeError::UnknownRuntime(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeSpec {
    pub kind: RuntimeKind,
    pub version: Option<String>,
}

impl RuntimeSpec {
    pub fn new(kind: impl AsRef<str>, version: impl Into<Option<String>>) -> Result<Self> {
        Ok(Self {
            kind: kind.as_ref().parse()?,
            version: version.into(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentVariable {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Input {
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    pub host_path: PathBuf,
    pub execution_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkPolicy {
    None,
    Localhost,
    Network,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ResourceLimits {
    pub memory_bytes: Option<u64>,
    #[serde(with = "duration_option_millis")]
    pub cpu_time: Option<Duration>,
    #[serde(with = "duration_option_millis")]
    pub wall_time: Option<Duration>,
    pub process_count: Option<u32>,
    pub stdout_bytes: Option<u64>,
    pub stderr_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workload {
    pub runtime: RuntimeSpec,
    pub entrypoint: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<EnvironmentVariable>,
    pub inputs: Vec<Input>,
    pub mounts: Vec<Mount>,
    pub network: NetworkPolicy,
    pub resources: ResourceLimits,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedRuntime {
    pub kind: RuntimeKind,
    pub requested_version: Option<String>,
    pub resolved_version: Option<String>,
    pub executable: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Execution {
    pub id: String,
    pub workload: Workload,
    pub runtime: ResolvedRuntime,
    pub resources: ResourceLimits,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    Created,
    Resolved,
    Prepared,
    Started,
    Running,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
    Killed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPhase {
    Created,
    Resolved,
    Prepared,
    Started,
    Running,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Output {
    pub text: String,
    pub truncated: bool,
    pub bytes: u64,
}

impl Output {
    pub fn from_bytes(bytes: Vec<u8>, limit: Option<u64>) -> Self {
        let original_len = bytes.len() as u64;
        let (slice, truncated) = match limit {
            Some(limit) if original_len > limit => (bytes[..limit as usize].to_vec(), true),
            _ => (bytes, false),
        };

        Self {
            text: String::from_utf8_lossy(&slice).into_owned(),
            truncated,
            bytes: original_len,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ResourceUsage {
    pub max_memory_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionResult {
    pub execution_id: String,
    pub runtime: RuntimeKind,
    pub network: NetworkPolicy,
    pub lifecycle: Vec<ExecutionStatus>,
    pub status: ExecutionStatus,
    pub exit_code: Option<i32>,
    pub stdout: Output,
    pub stderr: Output,
    #[serde(with = "duration_millis")]
    pub duration: Duration,
    pub resource_usage: ResourceUsage,
    pub artifacts: Vec<Artifact>,
    pub error: Option<ExecutionError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionErrorKind {
    Resolution,
    Preparation,
    Start,
    Runtime,
    ResourceLimit,
    Timeout,
    Cancelled,
    Killed,
    UnsupportedCapability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionError {
    pub execution_id: String,
    pub phase: ExecutionPhase,
    pub kind: ExecutionErrorKind,
    pub message: String,
    pub runtime: Option<RuntimeKind>,
    pub exit_code: Option<i32>,
    pub started: bool,
}

static NEXT_EXECUTION_ID: AtomicU64 = AtomicU64::new(1);

pub fn new_execution_id() -> String {
    format!(
        "exec_{}_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
        NEXT_EXECUTION_ID.fetch_add(1, Ordering::Relaxed)
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capability {
    pub supported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeCapabilities {
    pub memory: Capability,
    pub cpu_time: Capability,
    pub process_count: Capability,
    pub timeout: Capability,
    pub stdout_limit: Capability,
    pub stderr_limit: Capability,
    pub network: std::collections::BTreeMap<NetworkPolicy, Capability>,
}

impl RuntimeCapabilities {
    pub fn process() -> Self {
        Self {
            memory: Capability { supported: false },
            cpu_time: Capability { supported: false },
            process_count: Capability { supported: false },
            timeout: Capability { supported: true },
            stdout_limit: Capability { supported: true },
            stderr_limit: Capability { supported: true },
            network: [
                (NetworkPolicy::None, false),
                (NetworkPolicy::Localhost, false),
                (NetworkPolicy::Network, true),
            ]
            .into_iter()
            .map(|(policy, supported)| (policy, Capability { supported }))
            .collect(),
        }
    }

    pub fn wasm() -> Self {
        Self {
            memory: Capability { supported: true },
            cpu_time: Capability { supported: false },
            process_count: Capability { supported: false },
            timeout: Capability { supported: true },
            stdout_limit: Capability { supported: true },
            stderr_limit: Capability { supported: true },
            network: [
                (NetworkPolicy::None, true),
                (NetworkPolicy::Localhost, false),
                (NetworkPolicy::Network, false),
            ]
            .into_iter()
            .map(|(policy, supported)| (policy, Capability { supported }))
            .collect(),
        }
    }
}

impl std::fmt::Display for NetworkPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::None => "none",
            Self::Localhost => "localhost",
            Self::Network => "network",
        };
        f.write_str(value)
    }
}

impl Ord for NetworkPolicy {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.to_string().cmp(&other.to_string())
    }
}

impl PartialOrd for NetworkPolicy {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeAvailability {
    pub kind: RuntimeKind,
    pub version: Option<String>,
    pub known: bool,
    pub installed: bool,
    pub available: bool,
    pub compatible: bool,
    pub selected: bool,
    pub executable: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inspection {
    pub path: PathBuf,
    pub entrypoint: PathBuf,
    pub runtime: Option<RuntimeSpec>,
    pub candidates: Vec<RuntimeKind>,
    pub ambiguous: bool,
    pub manifest: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputeManifest {
    pub name: Option<String>,
    pub runtime: RuntimeSpec,
    pub entrypoint: PathBuf,
}

impl ComputeManifest {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)?;
        let manifest: Self = serde_json::from_str(&contents)?;
        Ok(manifest)
    }
}

#[derive(Debug)]
pub struct StagedWorkload {
    pub root: TempDir,
    pub work_dir: PathBuf,
    pub tmp_dir: PathBuf,
    pub output_dir: PathBuf,
    pub entrypoint: PathBuf,
}

pub fn stage_workload(workload: &Workload) -> Result<StagedWorkload> {
    let root = tempfile::tempdir()?;
    let work_dir = root.path().join("work");
    let tmp_dir = root.path().join("tmp");
    let output_dir = root.path().join("output");
    fs::create_dir_all(&work_dir)?;
    fs::create_dir_all(&tmp_dir)?;
    fs::create_dir_all(&output_dir)?;

    let entry_name = workload
        .entrypoint
        .file_name()
        .unwrap_or_else(|| OsStr::new("entrypoint"));
    let staged_entrypoint = work_dir.join(entry_name);
    copy_path(&workload.entrypoint, &staged_entrypoint)?;

    for mount in &workload.mounts {
        let execution_path = sanitize_execution_path(root.path(), &mount.execution_path)?;
        copy_path(&mount.host_path, &execution_path)?;
    }

    for input in &workload.inputs {
        let name = input.path.file_name().ok_or_else(|| {
            ComputeError::InvalidWorkload(format!(
                "input has no file name: {}",
                input.path.display()
            ))
        })?;
        copy_path(&input.path, &work_dir.join(name))?;
    }

    Ok(StagedWorkload {
        root,
        work_dir,
        tmp_dir,
        output_dir,
        entrypoint: staged_entrypoint,
    })
}

pub fn collect_artifacts(output_dir: &Path) -> Result<Vec<Artifact>> {
    let mut artifacts = Vec::new();
    for entry in WalkDir::new(output_dir).min_depth(1) {
        let entry = entry.map_err(|error| ComputeError::InvalidWorkload(error.to_string()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path().to_path_buf();
        let relative = path.strip_prefix(output_dir).map_err(|error| {
            ComputeError::InvalidWorkload(format!("failed to collect artifact: {error}"))
        })?;
        artifacts.push(Artifact {
            name: relative.to_string_lossy().into_owned(),
            path,
            size: entry
                .metadata()
                .map_err(|error| ComputeError::InvalidWorkload(error.to_string()))?
                .len(),
        });
    }
    artifacts.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(artifacts)
}

fn sanitize_execution_path(root: &Path, execution_path: &Path) -> Result<PathBuf> {
    let mut sanitized = root.to_path_buf();
    for component in execution_path.components() {
        match component {
            std::path::Component::Normal(part) => sanitized.push(part),
            std::path::Component::RootDir => {}
            _ => {
                return Err(ComputeError::InvalidMountPath(
                    execution_path.display().to_string(),
                ));
            }
        }
    }
    Ok(sanitized)
}

fn copy_path(from: &Path, to: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(from)?;
    if metadata.file_type().is_symlink() {
        return Err(ComputeError::InvalidWorkload(format!(
            "symlinks are not allowed in staged paths: {}",
            from.display()
        )));
    }
    if metadata.is_dir() {
        fs::create_dir_all(to)?;
        for entry in WalkDir::new(from) {
            let entry = entry.map_err(|error| ComputeError::InvalidWorkload(error.to_string()))?;
            let relative = entry.path().strip_prefix(from).map_err(|error| {
                ComputeError::InvalidWorkload(format!(
                    "failed to stage {}: {error}",
                    from.display()
                ))
            })?;
            let target = to.join(relative);
            if entry.file_type().is_dir() {
                fs::create_dir_all(&target)?;
            } else {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(entry.path(), &target)?;
            }
        }
        return Ok(());
    }

    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(from, to)?;
    Ok(())
}

#[async_trait]
pub trait RuntimeAdapter: Send + Sync {
    fn kind(&self) -> RuntimeKind;

    async fn availability(&self, requested: Option<&str>) -> RuntimeAvailability;

    async fn resolve(&self, workload: &Workload) -> Result<ResolvedRuntime>;

    fn capabilities(&self) -> RuntimeCapabilities;

    async fn execute(
        &self,
        workload: &Workload,
        runtime: &ResolvedRuntime,
    ) -> Result<ExecutionResult>;
}

#[derive(Debug, Error)]
pub enum ComputeError {
    #[error("runtime selection is ambiguous. candidates: {0}")]
    AmbiguousRuntime(String),
    #[error("unsupported capability for runtime {runtime}: {capability}")]
    UnsupportedCapability {
        runtime: RuntimeKind,
        capability: String,
    },
    #[error("unknown runtime: {0}")]
    UnknownRuntime(String),
    #[error("runtime {0} is unavailable")]
    RuntimeUnavailable(RuntimeKind),
    #[error("runtime {kind} version mismatch: requested {requested}, found {found}")]
    RuntimeVersionMismatch {
        kind: RuntimeKind,
        requested: String,
        found: String,
    },
    #[error("invalid workload: {0}")]
    InvalidWorkload(String),
    #[error("invalid mount path: {0}")]
    InvalidMountPath(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("runtime error: {0}")]
    Runtime(String),
}

pub type Result<T> = std::result::Result<T, ComputeError>;

pub mod duration_millis {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(value.as_millis() as u64)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u64::deserialize(deserializer)?;
        Ok(Duration::from_millis(value))
    }
}

pub mod duration_option_millis {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(value: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        value
            .map(|duration| duration.as_millis() as u64)
            .serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Option::<u64>::deserialize(deserializer)?;
        Ok(value.map(Duration::from_millis))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_workload_copies_entrypoint() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("hello.py");
        fs::write(&source, "print('hello')").unwrap();

        let staged = stage_workload(&Workload {
            runtime: RuntimeSpec {
                kind: RuntimeKind::Python,
                version: None,
            },
            entrypoint: source.clone(),
            args: vec![],
            env: vec![],
            inputs: vec![],
            mounts: vec![],
            network: NetworkPolicy::Network,
            resources: ResourceLimits::default(),
        })
        .unwrap();

        assert_eq!(
            fs::read_to_string(staged.entrypoint).unwrap(),
            "print('hello')"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stage_workload_rejects_symlinked_entrypoint() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.py");
        let link = temp.path().join("link.py");
        fs::write(&source, "print('hello')").unwrap();
        std::os::unix::fs::symlink(&source, &link).unwrap();

        let workload = Workload {
            runtime: RuntimeSpec {
                kind: RuntimeKind::Python,
                version: None,
            },
            entrypoint: link,
            args: vec![],
            env: vec![],
            inputs: vec![],
            mounts: vec![],
            network: NetworkPolicy::Network,
            resources: ResourceLimits::default(),
        };

        assert!(matches!(
            stage_workload(&workload),
            Err(ComputeError::InvalidWorkload(_))
        ));
    }

    #[test]
    fn execution_ids_are_unique_and_machine_readable() {
        let first = new_execution_id();
        let second = new_execution_id();

        assert!(first.starts_with("exec_"));
        assert!(second.starts_with("exec_"));
        assert_ne!(first, second);
    }
}
