use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use thiserror::Error;
use walkdir::WalkDir;

mod receipt;
pub use receipt::*;
mod dependencies;
pub use dependencies::*;
mod jobs;
pub use jobs::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeKind {
    Wasm,
    Node,
    Bun,
    Deno,
    Python,
    Ruby,
    Php,
    Jvm,
    Dotnet,
    Native,
    Shell,
}

impl RuntimeKind {
    pub const ALL: [RuntimeKind; 11] = [
        RuntimeKind::Wasm,
        RuntimeKind::Node,
        RuntimeKind::Bun,
        RuntimeKind::Deno,
        RuntimeKind::Python,
        RuntimeKind::Ruby,
        RuntimeKind::Php,
        RuntimeKind::Jvm,
        RuntimeKind::Dotnet,
        RuntimeKind::Native,
        RuntimeKind::Shell,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            RuntimeKind::Wasm => "wasm",
            RuntimeKind::Node => "node",
            RuntimeKind::Bun => "bun",
            RuntimeKind::Deno => "deno",
            RuntimeKind::Python => "python",
            RuntimeKind::Ruby => "ruby",
            RuntimeKind::Php => "php",
            RuntimeKind::Jvm => "jvm",
            RuntimeKind::Dotnet => "dotnet",
            RuntimeKind::Native => "native",
            RuntimeKind::Shell => "shell",
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
            "ruby" => Ok(Self::Ruby),
            "php" => Ok(Self::Php),
            "jvm" | "java" => Ok(Self::Jvm),
            "dotnet" | ".net" => Ok(Self::Dotnet),
            "native" | "linux" => Ok(Self::Native),
            "shell" | "sh" => Ok(Self::Shell),
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
    /// Destination relative to `/work`.
    pub path: PathBuf,
    pub source: ExecutionInputSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ExecutionInputSource {
    Inline {
        #[serde(with = "bytes_json")]
        data: Vec<u8>,
    },
    File {
        path: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    pub host_path: PathBuf,
    pub execution_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NetworkPolicy {
    #[default]
    None,
    Localhost,
    Network,
}

pub const ISOLATION_MODEL_VERSION: &str = "1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IsolationProfile {
    #[default]
    Process,
    Sandboxed,
    Strict,
}

impl IsolationProfile {
    pub const ALL: [Self; 3] = [Self::Process, Self::Sandboxed, Self::Strict];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Process => "process",
            Self::Sandboxed => "sandboxed",
            Self::Strict => "strict",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Process => {
                "Host process execution with a Compute-managed workspace; not a security sandbox."
            }
            Self::Sandboxed => "Runtime-enforced filesystem, network, and environment boundaries.",
            Self::Strict => {
                "The strongest boundaries the selected runtime can enforce; not a VM or container boundary."
            }
        }
    }
}

impl std::fmt::Display for IsolationProfile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::str::FromStr for IsolationProfile {
    type Err = ComputeError;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "process" => Ok(Self::Process),
            "sandboxed" => Ok(Self::Sandboxed),
            "strict" => Ok(Self::Strict),
            other => Err(ComputeError::InvalidIsolationProfile(other.into())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct IsolationRequirement {
    #[serde(default)]
    pub profile: IsolationProfile,
}

impl IsolationRequirement {
    fn is_process(&self) -> bool {
        self.profile == IsolationProfile::Process
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundaryStatus {
    Enforced,
    Disabled,
    Unavailable,
    NotRequested,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IsolationCapabilities {
    pub process_boundary: bool,
    pub filesystem_boundary: bool,
    pub network_boundary: bool,
    pub environment_boundary: bool,
    pub timeout_enforcement: bool,
    pub memory_enforcement: bool,
    pub cpu_enforcement: bool,
    pub process_enforcement: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IsolationEvidence {
    pub profile: IsolationProfile,
    pub requested: IsolationProfile,
    pub effective: IsolationProfile,
    pub filesystem: BoundaryStatus,
    pub network: BoundaryStatus,
    pub environment: BoundaryStatus,
    pub resources: BoundaryStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IsolationRejection {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IsolationPlan {
    pub requested: IsolationProfile,
    pub effective: Option<IsolationProfile>,
    pub compatible: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<IsolationEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<IsolationRejection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ResourceLimits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    #[serde(
        rename = "cpu_time_ms",
        alias = "cpu_time",
        skip_serializing_if = "Option::is_none",
        with = "duration_option_millis"
    )]
    pub cpu_time: Option<Duration>,
    #[serde(
        rename = "timeout_ms",
        alias = "wall_time",
        skip_serializing_if = "Option::is_none",
        with = "duration_option_millis"
    )]
    pub wall_time: Option<Duration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionRequest {
    pub runtime: RuntimeSpec,
    pub entrypoint: PathBuf,
    pub args: Vec<String>,
    /// Bytes supplied to the workload's standard input. An empty vector means
    /// an immediate EOF; adapters must never inherit the host terminal.
    #[serde(default)]
    pub stdin: Vec<u8>,
    pub env: Vec<EnvironmentVariable>,
    pub inputs: Vec<Input>,
    #[serde(default)]
    pub outputs: Vec<WorkloadOutput>,
    pub mounts: Vec<Mount>,
    pub network: NetworkPolicy,
    pub resources: ResourceLimits,
    #[serde(default)]
    pub isolation: IsolationProfile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependencies: Option<DependencyCapsule>,
}

/// Backwards-compatible name for the adapter-facing, materialized request.
pub type Workload = ExecutionRequest;

pub const WORKLOAD_SPEC_VERSION: &str = "1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadInput {
    pub path: PathBuf,
    pub source: InputSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum InputSource {
    Inline {
        #[serde(with = "bytes_json")]
        data: Vec<u8>,
    },
    File {
        path: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadOutput {
    pub path: PathBuf,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadDependencies {
    pub capsule: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadSpec {
    #[serde(with = "schema_version")]
    pub version: String,
    pub runtime: RuntimeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_version: Option<String>,
    pub entrypoint: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub inputs: Vec<WorkloadInput>,
    #[serde(default)]
    pub outputs: Vec<WorkloadOutput>,
    #[serde(default)]
    pub resources: ResourceLimits,
    #[serde(default)]
    pub network: NetworkPolicy,
    #[serde(default, skip_serializing_if = "IsolationRequirement::is_process")]
    pub isolation: IsolationRequirement,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependencies: Option<WorkloadDependencies>,
}

impl WorkloadSpec {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)?;
        let mut specification: Self = serde_json::from_str(&contents)?;
        specification.validate()?;
        specification.normalize();
        Ok(specification)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != WORKLOAD_SPEC_VERSION {
            return Err(ComputeError::UnsupportedWorkloadVersion {
                version: self.version.clone(),
            });
        }
        validate_portable_path(&self.entrypoint, "entrypoint")?;

        for pair in &self.env {
            if pair.0.is_empty() || pair.0.contains('=') || pair.0.contains('\0') {
                return Err(ComputeError::InvalidWorkload(format!(
                    "invalid environment variable name: {:?}",
                    pair.0
                )));
            }
            if pair.1.contains('\0') {
                return Err(ComputeError::InvalidWorkload(format!(
                    "environment variable {} contains NUL",
                    pair.0
                )));
            }
        }
        if let Some(dependencies) = &self.dependencies {
            validate_sha256_identity(&dependencies.capsule)?;
        }

        let mut input_paths = BTreeSet::new();
        let staged_entrypoint = self.entrypoint.file_name().map(PathBuf::from);
        for input in &self.inputs {
            validate_portable_path(&input.path, "input path")?;
            if let InputSource::File { path } = &input.source {
                validate_portable_path(path, "file input source")?;
            }
            if input_paths.iter().any(|existing: &PathBuf| {
                input.path.starts_with(existing) || existing.starts_with(&input.path)
            }) {
                return Err(ComputeError::InvalidWorkload(format!(
                    "conflicting input destination: {}",
                    input.path.display()
                )));
            }
            input_paths.insert(input.path.clone());
            if staged_entrypoint.as_ref().is_some_and(|entrypoint| {
                input.path.starts_with(entrypoint) || entrypoint.starts_with(&input.path)
            }) {
                return Err(ComputeError::InvalidWorkload(format!(
                    "input path conflicts with entrypoint: {}",
                    input.path.display()
                )));
            }
        }

        let mut outputs = BTreeSet::new();
        for output in &self.outputs {
            validate_portable_path(&output.path, "output")?;
            if outputs.iter().any(|existing: &PathBuf| {
                output.path.starts_with(existing) || existing.starts_with(&output.path)
            }) {
                return Err(ComputeError::InvalidWorkload(format!(
                    "conflicting output: {}",
                    output.path.display()
                )));
            }
            outputs.insert(output.path.clone());
        }

        validate_resource_value(self.resources.memory_bytes, "memory_bytes")?;
        validate_resource_value(
            self.resources
                .cpu_time
                .map(|value| value.as_millis() as u64),
            "cpu_time_ms",
        )?;
        validate_resource_value(
            self.resources
                .wall_time
                .map(|value| value.as_millis() as u64),
            "timeout_ms",
        )?;
        validate_resource_value(self.resources.process_count.map(u64::from), "process_count")?;
        validate_resource_value(self.resources.stdout_bytes, "stdout_bytes")?;
        validate_resource_value(self.resources.stderr_bytes, "stderr_bytes")?;
        Ok(())
    }

    /// Resolve declared files relative to the workload file, producing the
    /// existing adapter-facing execution request.
    pub fn materialize(&self, workload_file: &Path) -> Result<ExecutionRequest> {
        let base = workload_file.parent().unwrap_or_else(|| Path::new("."));
        self.materialize_from(base)
    }

    /// Materialize a generated portable specification relative to its logical
    /// workload root. This is the same operation used for file-backed specs;
    /// the separate entry point keeps host paths out of the portable identity.
    pub fn materialize_from(&self, base: &Path) -> Result<ExecutionRequest> {
        self.validate()?;
        let entrypoint = resolve_declared_path(base, &self.entrypoint, "entrypoint")?;
        let mut declared_inputs = self.inputs.clone();
        declared_inputs.sort_by(|left, right| left.path.cmp(&right.path));
        let inputs = declared_inputs
            .iter()
            .map(|input| {
                Ok(Input {
                    path: input.path.clone(),
                    source: match &input.source {
                        InputSource::Inline { data } => {
                            ExecutionInputSource::Inline { data: data.clone() }
                        }
                        InputSource::File { path } => {
                            let resolved = resolve_declared_path(base, path, "file input source")?;
                            if !resolved.is_file() {
                                return Err(ComputeError::InvalidWorkload(format!(
                                    "file input source is not a regular file: {}",
                                    path.display()
                                )));
                            }
                            ExecutionInputSource::File { path: resolved }
                        }
                    },
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut outputs = self.outputs.clone();
        outputs.sort_by(|left, right| left.path.cmp(&right.path));

        Ok(ExecutionRequest {
            runtime: RuntimeSpec {
                kind: self.runtime,
                version: self.runtime_version.clone(),
            },
            entrypoint,
            args: self.args.clone(),
            stdin: vec![],
            env: self
                .env
                .iter()
                .map(|(key, value)| EnvironmentVariable {
                    key: key.clone(),
                    value: value.clone(),
                })
                .collect(),
            inputs,
            outputs,
            mounts: vec![],
            network: self.network.clone(),
            resources: self.resources.clone(),
            isolation: self.isolation.profile,
            dependencies: None,
        })
    }

    pub fn to_pretty_json(&self) -> Result<String> {
        self.validate()?;
        let mut normalized = self.clone();
        normalized.normalize();
        Ok(serde_json::to_string_pretty(&normalized)?)
    }

    pub fn to_canonical_json(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut normalized = self.clone();
        normalized.normalize();
        Ok(serde_json::to_vec(&normalized)?)
    }

    /// Stable identity of the portable contract. Execution and provider
    /// metadata are deliberately excluded.
    pub fn workload_id(&self) -> Result<String> {
        self.validate()?;
        let digest = Sha256::digest(self.to_canonical_json()?);
        Ok(format!("sha256:{digest:x}"))
    }

    pub fn require_id(&self, expected: &str) -> Result<()> {
        let actual = self.workload_id()?;
        if actual == expected {
            Ok(())
        } else {
            Err(ComputeError::InvalidWorkload(format!(
                "workload identity mismatch: expected {expected}, found {actual}"
            )))
        }
    }

    fn normalize(&mut self) {
        self.inputs
            .sort_by(|left, right| left.path.cmp(&right.path));
        self.outputs
            .sort_by(|left, right| left.path.cmp(&right.path));
    }
}

pub const WORKLOAD_BUNDLE_FORMAT: &str = "compute.bundle";
pub const WORKLOAD_BUNDLE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleInput {
    pub path: PathBuf,
    #[serde(with = "bytes_json")]
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkloadBundle {
    pub version: u32,
    pub workload: WorkloadSpec,
    pub entrypoint: BundleInput,
    pub inputs: Vec<BundleInput>,
    pub dependency_capsule: Option<DependencyCapsule>,
}

#[derive(Debug)]
pub struct MaterializedBundle {
    pub request: ExecutionRequest,
    _source: TempDir,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleEntryManifest {
    pub path: PathBuf,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadBundleManifest {
    pub format: String,
    pub version: u32,
    pub workload_id: String,
    pub bundle_id: String,
    pub entrypoint: BundleEntryManifest,
    pub inputs: Vec<BundleEntryManifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependencies: Option<BundleDependencyManifest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleDependencyManifest {
    pub capsule_id: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleVerification {
    pub valid: bool,
    pub format: String,
    pub version: u32,
    pub workload_id: String,
    pub bundle_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleInspection {
    pub format: String,
    pub version: u32,
    pub workload_id: String,
    pub bundle_id: String,
    pub runtime: RuntimeKind,
    pub entrypoint: PathBuf,
    pub args: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub resources: ResourceLimits,
    pub network: NetworkPolicy,
    pub inputs: Vec<BundleEntryManifest>,
    pub outputs: Vec<WorkloadOutput>,
    pub dependencies: Option<BundleDependencyManifest>,
}

impl WorkloadBundle {
    pub fn create(workload_file: &Path) -> Result<Self> {
        let workload = WorkloadSpec::load(workload_file)?;
        let base = workload_file.parent().unwrap_or_else(|| Path::new("."));
        Self::create_from(workload, base)
    }

    /// Create the canonical bundle from an already-resolved WorkloadSpec.
    pub fn create_from(workload: WorkloadSpec, base: &Path) -> Result<Self> {
        Self::create_from_with_capsule(workload, base, None)
    }

    pub fn create_from_with_capsule(
        workload: WorkloadSpec,
        base: &Path,
        dependency_capsule: Option<DependencyCapsule>,
    ) -> Result<Self> {
        workload.validate()?;
        let entrypoint_path = resolve_declared_path(base, &workload.entrypoint, "entrypoint")?;
        if !entrypoint_path.is_file() {
            return Err(ComputeError::InvalidBundle(
                "bundle entrypoint must be a regular file".into(),
            ));
        }
        let entrypoint = BundleInput {
            path: workload.entrypoint.clone(),
            data: fs::read(entrypoint_path)?,
        };
        let mut inputs = Vec::new();
        for input in &workload.inputs {
            if let InputSource::File { path } = &input.source {
                let source = resolve_declared_path(base, path, "file input source")?;
                if !source.is_file() {
                    return Err(ComputeError::InvalidBundle(format!(
                        "file input source is not a regular file: {}",
                        path.display()
                    )));
                }
                inputs.push(BundleInput {
                    path: input.path.clone(),
                    data: fs::read(source)?,
                });
            }
        }
        inputs.sort_by(|left, right| left.path.cmp(&right.path));
        let bundle = Self {
            version: WORKLOAD_BUNDLE_VERSION,
            workload,
            entrypoint,
            inputs,
            dependency_capsule,
        };
        bundle.validate()?;
        Ok(bundle)
    }

    pub fn read(path: &Path) -> Result<Self> {
        Self::from_bytes(&fs::read(path)?)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let mut archive = tar::Archive::new(Cursor::new(bytes));
        let mut files = BTreeMap::<PathBuf, Vec<u8>>::new();
        for entry in archive.entries().map_err(bundle_io)? {
            let mut entry = entry.map_err(bundle_io)?;
            if !entry.header().entry_type().is_file() {
                return Err(ComputeError::InvalidBundle(
                    "bundle may contain only regular files".into(),
                ));
            }
            let path = entry.path().map_err(bundle_io)?.into_owned();
            validate_archive_path(&path)?;
            let mut data = Vec::new();
            entry.read_to_end(&mut data).map_err(bundle_io)?;
            if files.insert(path.clone(), data).is_some() {
                return Err(ComputeError::InvalidBundle(format!(
                    "duplicate archive entry: {}",
                    path.display()
                )));
            }
        }

        let manifest_bytes = take_bundle_file(&mut files, Path::new("manifest.json"))?;
        let workload_bytes = take_bundle_file(&mut files, Path::new("workload.json"))?;
        let manifest: WorkloadBundleManifest = serde_json::from_slice(&manifest_bytes)?;
        if manifest.format != WORKLOAD_BUNDLE_FORMAT || manifest.version != WORKLOAD_BUNDLE_VERSION
        {
            return Err(ComputeError::InvalidBundle(format!(
                "unsupported bundle format/version: {}@{}",
                manifest.format, manifest.version
            )));
        }
        let mut workload: WorkloadSpec = serde_json::from_slice(&workload_bytes)?;
        workload.validate()?;
        workload.normalize();
        let entrypoint_archive_path = Path::new("entrypoint").join(&workload.entrypoint);
        let entrypoint = BundleInput {
            path: workload.entrypoint.clone(),
            data: take_bundle_file(&mut files, &entrypoint_archive_path)?,
        };
        let mut inputs = Vec::new();
        for input in &workload.inputs {
            if matches!(input.source, InputSource::File { .. }) {
                inputs.push(BundleInput {
                    path: input.path.clone(),
                    data: take_bundle_file(&mut files, &Path::new("inputs").join(&input.path))?,
                });
            }
        }
        let dependency_capsule = if let Some(dependency) = &manifest.dependencies {
            let data =
                take_bundle_file(&mut files, Path::new("dependencies/capsule.compute.deps"))?;
            if data.len() as u64 != dependency.size || sha256_identity(&data) != dependency.sha256 {
                return Err(ComputeError::InvalidBundle(
                    "embedded dependency capsule metadata mismatch".into(),
                ));
            }
            Some(DependencyCapsule::from_bytes(&data)?)
        } else {
            None
        };
        if let Some(unexpected) = files.keys().next() {
            return Err(ComputeError::InvalidBundle(format!(
                "unexpected bundle input: {}",
                unexpected.display()
            )));
        }
        inputs.sort_by(|left, right| left.path.cmp(&right.path));
        let bundle = Self {
            version: manifest.version,
            workload,
            entrypoint,
            inputs,
            dependency_capsule,
        };
        bundle.validate()?;
        if bundle.manifest()? != manifest {
            return Err(ComputeError::InvalidBundle(
                "bundle manifest identity or entry metadata mismatch".into(),
            ));
        }
        if bundle.to_bytes()? != bytes {
            return Err(ComputeError::InvalidBundle(
                "bundle archive is not in canonical deterministic form".into(),
            ));
        }
        Ok(bundle)
    }

    pub fn write(&self, path: &Path) -> Result<u64> {
        let bytes = self.to_bytes()?;
        fs::write(path, &bytes)?;
        Ok(bytes.len() as u64)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != WORKLOAD_BUNDLE_VERSION {
            return Err(ComputeError::InvalidBundle(format!(
                "unsupported bundle version: {}",
                self.version
            )));
        }
        self.workload.validate()?;
        validate_portable_path(&self.entrypoint.path, "bundle entrypoint")?;
        if self.entrypoint.path != self.workload.entrypoint {
            return Err(ComputeError::InvalidBundle(
                "bundle entrypoint does not match WorkloadSpec".into(),
            ));
        }
        let expected = self
            .workload
            .inputs
            .iter()
            .filter(|input| matches!(input.source, InputSource::File { .. }))
            .map(|input| input.path.clone())
            .collect::<BTreeSet<_>>();
        let mut actual = BTreeSet::new();
        for input in &self.inputs {
            validate_portable_path(&input.path, "bundle input")?;
            if !actual.insert(input.path.clone()) {
                return Err(ComputeError::InvalidBundle(format!(
                    "duplicate bundle input: {}",
                    input.path.display()
                )));
            }
        }
        if expected != actual {
            let missing = expected.difference(&actual).next();
            let unexpected = actual.difference(&expected).next();
            let detail = match (missing, unexpected) {
                (Some(path), _) => format!("missing declared bundle input: {}", path.display()),
                (_, Some(path)) => format!("unexpected bundle input: {}", path.display()),
                _ => "bundle inputs do not match WorkloadSpec".into(),
            };
            return Err(ComputeError::InvalidBundle(detail));
        }
        match (&self.workload.dependencies, &self.dependency_capsule) {
            (None, None) | (Some(_), None) => {}
            (None, Some(_)) => {
                return Err(ComputeError::InvalidBundle(
                    "bundle embeds an undeclared dependency capsule".into(),
                ));
            }
            (Some(reference), Some(capsule)) => {
                let capsule_id = capsule.capsule_id()?;
                if reference.capsule != capsule_id {
                    return Err(ComputeError::InvalidBundle(format!(
                        "dependency capsule identity mismatch: declared {}, embedded {capsule_id}",
                        reference.capsule
                    )));
                }
                if capsule.runtime != self.workload.runtime {
                    return Err(ComputeError::InvalidBundle(
                        "dependency capsule runtime does not match workload".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn workload_id(&self) -> Result<String> {
        self.workload.workload_id()
    }

    pub fn bundle_id(&self) -> Result<String> {
        self.validate()?;
        let mut hasher = Sha256::new();
        hash_bundle_field(&mut hasher, b"compute.bundle\0");
        hash_bundle_field(&mut hasher, &self.version.to_be_bytes());
        hash_bundle_field(&mut hasher, &self.workload.to_canonical_json()?);
        hash_bundle_input(&mut hasher, &self.entrypoint);
        let mut inputs = self.inputs.clone();
        inputs.sort_by(|left, right| left.path.cmp(&right.path));
        for input in &inputs {
            hash_bundle_input(&mut hasher, input);
        }
        if let Some(capsule) = &self.dependency_capsule {
            hash_bundle_field(&mut hasher, &capsule.to_bytes()?);
        }
        Ok(format!("sha256:{:x}", hasher.finalize()))
    }

    pub fn require_ids(
        &self,
        expected_workload_id: Option<&str>,
        expected_bundle_id: Option<&str>,
    ) -> Result<()> {
        if let Some(expected) = expected_workload_id {
            self.workload.require_id(expected)?;
        }
        if let Some(expected) = expected_bundle_id {
            let actual = self.bundle_id()?;
            if actual != expected {
                return Err(ComputeError::InvalidBundle(format!(
                    "bundle identity mismatch: expected {expected}, found {actual}"
                )));
            }
        }
        Ok(())
    }

    pub fn manifest(&self) -> Result<WorkloadBundleManifest> {
        self.validate()?;
        let mut inputs = self
            .inputs
            .iter()
            .map(bundle_entry_manifest)
            .collect::<Vec<_>>();
        inputs.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(WorkloadBundleManifest {
            format: WORKLOAD_BUNDLE_FORMAT.into(),
            version: self.version,
            workload_id: self.workload_id()?,
            bundle_id: self.bundle_id()?,
            entrypoint: bundle_entry_manifest(&self.entrypoint),
            inputs,
            dependencies: match &self.dependency_capsule {
                Some(capsule) => {
                    let data = capsule.to_bytes()?;
                    Some(BundleDependencyManifest {
                        capsule_id: capsule.capsule_id()?,
                        size: data.len() as u64,
                        sha256: sha256_identity(&data),
                    })
                }
                None => None,
            },
        })
    }

    pub fn verification(&self) -> Result<BundleVerification> {
        let manifest = self.manifest()?;
        Ok(BundleVerification {
            valid: true,
            format: manifest.format,
            version: manifest.version,
            workload_id: manifest.workload_id,
            bundle_id: manifest.bundle_id,
        })
    }

    pub fn inspection(&self) -> Result<BundleInspection> {
        let manifest = self.manifest()?;
        let bundled = manifest
            .inputs
            .iter()
            .cloned()
            .map(|input| (input.path.clone(), input))
            .collect::<BTreeMap<_, _>>();
        let mut inputs = self
            .workload
            .inputs
            .iter()
            .map(|input| match &input.source {
                InputSource::Inline { data } => BundleEntryManifest {
                    path: input.path.clone(),
                    size: data.len() as u64,
                    sha256: format!("sha256:{:x}", Sha256::digest(data)),
                },
                InputSource::File { .. } => bundled
                    .get(&input.path)
                    .cloned()
                    .expect("validated bundle input"),
            })
            .collect::<Vec<_>>();
        inputs.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(BundleInspection {
            format: manifest.format,
            version: manifest.version,
            workload_id: manifest.workload_id,
            bundle_id: manifest.bundle_id,
            runtime: self.workload.runtime,
            entrypoint: self.workload.entrypoint.clone(),
            args: self.workload.args.clone(),
            environment: self.workload.env.clone(),
            resources: self.workload.resources.clone(),
            network: self.workload.network.clone(),
            inputs,
            outputs: self.workload.outputs.clone(),
            dependencies: manifest.dependencies,
        })
    }

    pub fn materialize(&self) -> Result<MaterializedBundle> {
        self.validate()?;
        let source = tempfile::tempdir()?;
        let entrypoint = source.path().join(&self.entrypoint.path);
        if let Some(parent) = entrypoint.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&entrypoint, &self.entrypoint.data)?;
        let bundled = self
            .inputs
            .iter()
            .map(|input| (input.path.clone(), input.data.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut inputs = self
            .workload
            .inputs
            .iter()
            .map(|input| {
                let data = match &input.source {
                    InputSource::Inline { data } => data.clone(),
                    InputSource::File { .. } => {
                        bundled.get(&input.path).cloned().ok_or_else(|| {
                            ComputeError::InvalidBundle(format!(
                                "missing declared bundle input: {}",
                                input.path.display()
                            ))
                        })?
                    }
                };
                Ok(Input {
                    path: input.path.clone(),
                    source: ExecutionInputSource::Inline { data },
                })
            })
            .collect::<Result<Vec<_>>>()?;
        inputs.sort_by(|left, right| left.path.cmp(&right.path));
        let mut outputs = self.workload.outputs.clone();
        outputs.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(MaterializedBundle {
            request: ExecutionRequest {
                runtime: RuntimeSpec {
                    kind: self.workload.runtime,
                    version: self.workload.runtime_version.clone(),
                },
                entrypoint,
                args: self.workload.args.clone(),
                stdin: vec![],
                env: self
                    .workload
                    .env
                    .iter()
                    .map(|(key, value)| EnvironmentVariable {
                        key: key.clone(),
                        value: value.clone(),
                    })
                    .collect(),
                inputs,
                outputs,
                mounts: vec![],
                network: self.workload.network.clone(),
                resources: self.workload.resources.clone(),
                isolation: self.workload.isolation.profile,
                dependencies: self.dependency_capsule.clone(),
            },
            _source: source,
        })
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let manifest = serde_json::to_vec(&self.manifest()?)?;
        let workload = self.workload.to_canonical_json()?;
        let mut entries = vec![
            (Path::new("manifest.json").to_path_buf(), manifest),
            (Path::new("workload.json").to_path_buf(), workload),
            (
                Path::new("entrypoint").join(&self.entrypoint.path),
                self.entrypoint.data.clone(),
            ),
        ];
        entries.extend(
            self.inputs
                .iter()
                .map(|input| (Path::new("inputs").join(&input.path), input.data.clone())),
        );
        if let Some(capsule) = &self.dependency_capsule {
            entries.push((
                PathBuf::from("dependencies/capsule.compute.deps"),
                capsule.to_bytes()?,
            ));
        }
        entries.sort_by(|left, right| left.0.cmp(&right.0));

        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            builder.mode(tar::HeaderMode::Deterministic);
            for (path, data) in entries {
                validate_archive_path(&path)?;
                let mut header = tar::Header::new_ustar();
                header.set_path(&path).map_err(bundle_io)?;
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_uid(0);
                header.set_gid(0);
                header.set_mtime(0);
                header.set_cksum();
                builder
                    .append(&header, Cursor::new(data))
                    .map_err(bundle_io)?;
            }
            builder.finish().map_err(bundle_io)?;
        }
        Ok(bytes)
    }
}

fn bundle_entry_manifest(input: &BundleInput) -> BundleEntryManifest {
    BundleEntryManifest {
        path: input.path.clone(),
        size: input.data.len() as u64,
        sha256: format!("sha256:{:x}", Sha256::digest(&input.data)),
    }
}

fn hash_bundle_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn hash_bundle_input(hasher: &mut Sha256, input: &BundleInput) {
    hash_bundle_field(hasher, input.path.to_string_lossy().as_bytes());
    hash_bundle_field(hasher, &input.data);
}

fn validate_archive_path(path: &Path) -> Result<()> {
    validate_portable_path(path, "bundle archive entry")
        .map_err(|error| ComputeError::InvalidBundle(error.to_string()))
}

fn take_bundle_file(files: &mut BTreeMap<PathBuf, Vec<u8>>, path: &Path) -> Result<Vec<u8>> {
    files.remove(path).ok_or_else(|| {
        ComputeError::InvalidBundle(format!("missing bundle entry: {}", path.display()))
    })
}

fn bundle_io(error: std::io::Error) -> ComputeError {
    ComputeError::InvalidBundle(format!("invalid bundle archive: {error}"))
}

fn validate_resource_value(value: Option<u64>, name: &str) -> Result<()> {
    if value == Some(0) {
        return Err(ComputeError::InvalidWorkload(format!(
            "resource {name} must be greater than zero"
        )));
    }
    Ok(())
}

fn validate_portable_path(path: &Path, label: &str) -> Result<()> {
    let text = path.to_string_lossy();
    let windows_absolute = text.as_bytes().get(1) == Some(&b':');
    if path.as_os_str().is_empty() || path.is_absolute() || text.contains('\\') || windows_absolute
    {
        return Err(ComputeError::InvalidWorkload(format!(
            "{label} must be a non-empty relative path: {}",
            path.display()
        )));
    }
    for component in path.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err(ComputeError::InvalidWorkload(format!(
                "{label} contains invalid traversal: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn resolve_declared_path(base: &Path, path: &Path, label: &str) -> Result<PathBuf> {
    validate_portable_path(path, label)?;
    let base = base.canonicalize()?;
    let resolved = base.join(path).canonicalize().map_err(|error| {
        ComputeError::InvalidWorkload(format!(
            "{label} does not resolve inside the workload context ({}): {error}",
            path.display()
        ))
    })?;
    if !resolved.starts_with(&base) {
        return Err(ComputeError::InvalidWorkload(format!(
            "{label} escapes the workload context: {}",
            path.display()
        )));
    }
    Ok(resolved)
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
pub struct OutputArtifact {
    pub path: PathBuf,
    #[serde(with = "bytes_json")]
    pub data: Vec<u8>,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingOutput {
    pub path: PathBuf,
    pub required: bool,
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
    pub outputs: Vec<OutputArtifact>,
    pub missing_outputs: Vec<MissingOutput>,
    pub error: Option<ExecutionError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<IsolationEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependencies: Option<ExecutionDependencyEvidence>,
    /// The execution provider that produced this result. Runtime adapters do
    /// not set this; provider implementations bind it before returning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderIdentity>,
    /// Verifiable evidence attached by the orchestration layer. Runtime
    /// adapters leave this empty because they do not own distribution or
    /// portable workload identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<ExecutionReceipt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum ProviderIdentity {
    Local { id: String },
    Remote { id: String, endpoint: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionDependencyEvidence {
    pub capsule_id: String,
    pub file_count: u64,
    pub verified: bool,
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
    OutputContract,
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

impl Capability {
    pub const fn supported() -> Self {
        Self { supported: true }
    }

    pub const fn unsupported() -> Self {
        Self { supported: false }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeCapabilities {
    pub stdin: Capability,
    pub stdout: Capability,
    pub stderr: Capability,
    pub environment: Capability,
    pub filesystem_isolation: Capability,
    pub artifacts: Capability,
    pub timeout: Capability,
    pub cancellation: Capability,
    pub stdout_limit: Capability,
    pub stderr_limit: Capability,
    pub memory_limit: Capability,
    pub cpu_limit: Capability,
    pub process_limit: Capability,
    pub network: std::collections::BTreeMap<NetworkPolicy, Capability>,
    pub isolation: IsolationCapabilities,
}

impl RuntimeCapabilities {
    pub fn process() -> Self {
        Self {
            stdin: Capability::supported(),
            stdout: Capability::supported(),
            stderr: Capability::supported(),
            environment: Capability::supported(),
            // Staging narrows the working directory but does not sandbox an
            // arbitrary process from other host paths.
            filesystem_isolation: Capability::unsupported(),
            artifacts: Capability::supported(),
            timeout: Capability::supported(),
            cancellation: Capability::unsupported(),
            stdout_limit: Capability::supported(),
            stderr_limit: Capability::supported(),
            memory_limit: Capability::unsupported(),
            cpu_limit: Capability::unsupported(),
            process_limit: Capability::unsupported(),
            network: [
                (NetworkPolicy::None, false),
                (NetworkPolicy::Localhost, false),
                (NetworkPolicy::Network, true),
            ]
            .into_iter()
            .map(|(policy, supported)| (policy, Capability { supported }))
            .collect(),
            isolation: IsolationCapabilities {
                process_boundary: true,
                filesystem_boundary: false,
                network_boundary: false,
                environment_boundary: true,
                timeout_enforcement: true,
                memory_enforcement: false,
                cpu_enforcement: false,
                process_enforcement: false,
            },
        }
    }

    pub fn wasm() -> Self {
        Self {
            stdin: Capability::supported(),
            stdout: Capability::supported(),
            stderr: Capability::supported(),
            environment: Capability::supported(),
            filesystem_isolation: Capability::supported(),
            artifacts: Capability::supported(),
            timeout: Capability::supported(),
            cancellation: Capability::unsupported(),
            stdout_limit: Capability::supported(),
            stderr_limit: Capability::supported(),
            memory_limit: Capability::supported(),
            cpu_limit: Capability::unsupported(),
            process_limit: Capability::unsupported(),
            network: [
                (NetworkPolicy::None, true),
                (NetworkPolicy::Localhost, false),
                (NetworkPolicy::Network, false),
            ]
            .into_iter()
            .map(|(policy, supported)| (policy, Capability { supported }))
            .collect(),
            isolation: IsolationCapabilities {
                process_boundary: true,
                filesystem_boundary: true,
                network_boundary: true,
                environment_boundary: true,
                timeout_enforcement: true,
                memory_enforcement: true,
                cpu_enforcement: false,
                process_enforcement: false,
            },
        }
    }

    pub fn deno() -> Self {
        let mut capabilities = Self::process();
        capabilities.filesystem_isolation = Capability::supported();
        capabilities.network = [
            (NetworkPolicy::None, Capability::supported()),
            (NetworkPolicy::Localhost, Capability::supported()),
            (NetworkPolicy::Network, Capability::supported()),
        ]
        .into_iter()
        .collect();
        capabilities.isolation.filesystem_boundary = true;
        capabilities.isolation.network_boundary = true;
        capabilities
    }

    pub fn isolation_plan(&self, runtime: RuntimeKind, workload: &Workload) -> IsolationPlan {
        match self.resolve_isolation(runtime, workload) {
            Ok(evidence) => IsolationPlan {
                requested: workload.isolation,
                effective: Some(evidence.effective),
                compatible: true,
                evidence: Some(evidence),
                reason: None,
            },
            Err(reason) => IsolationPlan {
                requested: workload.isolation,
                effective: None,
                compatible: false,
                evidence: None,
                reason: Some(reason),
            },
        }
    }

    pub fn resolve_isolation(
        &self,
        runtime: RuntimeKind,
        workload: &Workload,
    ) -> std::result::Result<IsolationEvidence, IsolationRejection> {
        let reject = |code: &str, boundary: &str| IsolationRejection {
            code: code.into(),
            message: format!(
                "{runtime} runtime cannot satisfy {} isolation: {boundary}",
                workload.isolation
            ),
        };
        if let Err(error) = self.validate(runtime, workload) {
            let message = error.to_string();
            let code = if message.contains("network policy") {
                "network_policy_unavailable"
            } else if message.contains("memory") {
                "memory_enforcement_unavailable"
            } else if message.contains("CPU") {
                "cpu_enforcement_unavailable"
            } else if message.contains("process-count") {
                "process_enforcement_unavailable"
            } else if message.contains("wall-time") {
                "timeout_enforcement_unavailable"
            } else {
                "runtime_capability_unavailable"
            };
            return Err(IsolationRejection {
                code: code.into(),
                message,
            });
        }
        let stronger = workload.isolation >= IsolationProfile::Sandboxed;
        if stronger && !self.isolation.filesystem_boundary {
            return Err(reject(
                "filesystem_isolation_unavailable",
                "filesystem boundary is unavailable",
            ));
        }
        if stronger && !self.isolation.network_boundary {
            return Err(reject(
                "network_isolation_unavailable",
                "network boundary is unavailable",
            ));
        }
        if stronger && !self.isolation.environment_boundary {
            return Err(reject(
                "environment_isolation_unavailable",
                "environment boundary is unavailable",
            ));
        }
        if stronger && !self.isolation.timeout_enforcement {
            return Err(reject(
                "timeout_enforcement_unavailable",
                "timeout enforcement is unavailable",
            ));
        }
        let resource_requested = workload.resources.wall_time.is_some()
            || workload.resources.memory_bytes.is_some()
            || workload.resources.cpu_time.is_some()
            || workload.resources.process_count.is_some();
        let resources_enforced = (workload.resources.wall_time.is_none()
            || self.isolation.timeout_enforcement)
            && (workload.resources.memory_bytes.is_none() || self.isolation.memory_enforcement);
        let resources_enforced = resources_enforced
            && (workload.resources.cpu_time.is_none() || self.isolation.cpu_enforcement)
            && (workload.resources.process_count.is_none() || self.isolation.process_enforcement);
        if workload.isolation == IsolationProfile::Strict && !resources_enforced {
            return Err(reject(
                "resource_isolation_unavailable",
                "a requested resource boundary is unavailable",
            ));
        }
        Ok(IsolationEvidence {
            profile: workload.isolation,
            requested: workload.isolation,
            effective: workload.isolation,
            filesystem: if self.isolation.filesystem_boundary {
                BoundaryStatus::Enforced
            } else {
                BoundaryStatus::Unavailable
            },
            network: if self.isolation.network_boundary {
                if workload.network == NetworkPolicy::None {
                    BoundaryStatus::Disabled
                } else {
                    BoundaryStatus::Enforced
                }
            } else if workload.network == NetworkPolicy::Network {
                BoundaryStatus::NotRequested
            } else {
                BoundaryStatus::Unavailable
            },
            environment: if self.isolation.environment_boundary {
                BoundaryStatus::Enforced
            } else {
                BoundaryStatus::Unavailable
            },
            resources: if resource_requested {
                if resources_enforced {
                    BoundaryStatus::Enforced
                } else {
                    BoundaryStatus::Unavailable
                }
            } else {
                BoundaryStatus::NotRequested
            },
        })
    }

    /// Reject a request whenever satisfying it would require silently
    /// weakening the workload contract.
    pub fn validate(&self, runtime: RuntimeKind, workload: &Workload) -> Result<()> {
        let require = |supported: bool, capability: &str| {
            if supported {
                Ok(())
            } else {
                Err(ComputeError::UnsupportedCapability {
                    runtime,
                    capability: capability.to_string(),
                })
            }
        };

        if !workload.stdin.is_empty() {
            require(self.stdin.supported, "stdin")?;
        }
        let network = self
            .network
            .get(&workload.network)
            .is_some_and(|c| c.supported);
        require(network, &format!("network policy {}", workload.network))?;
        if workload.resources.wall_time.is_some() {
            require(self.timeout.supported, "wall-time limit")?;
        }
        if workload.resources.stdout_bytes.is_some() {
            require(self.stdout_limit.supported, "stdout limit")?;
        }
        if workload.resources.stderr_bytes.is_some() {
            require(self.stderr_limit.supported, "stderr limit")?;
        }
        if workload.resources.memory_bytes.is_some() {
            require(self.memory_limit.supported, "memory limit")?;
        }
        if workload.resources.cpu_time.is_some() {
            require(self.cpu_limit.supported, "CPU-time limit")?;
        }
        if workload.resources.process_count.is_some() {
            require(self.process_limit.supported, "process-count limit")?;
        }
        Ok(())
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
    pub source: RuntimeSource,
    pub expected_version: Option<String>,
    pub remediation: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeSource {
    Embedded,
    #[serde(rename = "compute-distribution")]
    Distribution,
    HostDevelopment,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeDescriptor {
    pub id: RuntimeKind,
    pub version: String,
    pub executable: String,
    pub capabilities: RuntimeCapabilities,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeReport {
    pub runtime: RuntimeKind,
    pub descriptor: RuntimeDescriptor,
    pub availability: RuntimeAvailability,
    pub capabilities: RuntimeCapabilities,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeInventory {
    pub compute_version: String,
    pub platform: String,
    pub runtimes: Vec<RuntimeInventoryEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeInventoryEntry {
    pub id: RuntimeKind,
    pub version: String,
    pub executable: String,
    pub available: bool,
    pub compatible: bool,
    pub detected_version: Option<String>,
    pub detected_executable: Option<PathBuf>,
    pub source: RuntimeSource,
    pub capabilities: RuntimeCapabilities,
    pub remediation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadValidationStatus {
    Valid,
}

pub const COMPUTE_CAPABILITY_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputeCapability {
    pub name: String,
    pub version: u32,
    pub operations: Vec<String>,
}

impl Default for ComputeCapability {
    fn default() -> Self {
        Self {
            name: "compute".into(),
            version: COMPUTE_CAPABILITY_VERSION,
            operations: vec!["compute.inspect".into(), "compute.run".into()],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableDataFlow {
    pub steps: Vec<String>,
    pub input_root: PathBuf,
    pub entrypoint: PathBuf,
    pub runtime: RuntimeKind,
    pub declared_outputs: Vec<WorkloadOutput>,
    pub output_root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadPlan {
    pub valid: bool,
    pub workload_id: String,
    pub capability: ComputeCapability,
    pub workload: WorkloadSpec,
    pub validation: WorkloadValidationStatus,
    pub resolved_runtime: RuntimeSpec,
    pub backend_capabilities: RuntimeCapabilities,
    pub capability_compatible: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capability_error: Option<String>,
    pub isolation: IsolationPlan,
    pub input_preparation: Vec<WorkloadInput>,
    pub output_root: PathBuf,
    pub data_flow: PortableDataFlow,
    pub dependencies: DependencyRequirementPlan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyRequirementPlan {
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capsule_id: Option<String>,
    pub available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleWorkloadPlan {
    pub bundle_verification: BundleVerification,
    pub plan: WorkloadPlan,
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
    pub dependencies_dir: Option<PathBuf>,
}

pub fn stage_workload(workload: &Workload) -> Result<StagedWorkload> {
    let root = tempfile::tempdir()?;
    let work_dir = root.path().join("work");
    let tmp_dir = root.path().join("tmp");
    let output_dir = root.path().join("output");
    fs::create_dir_all(&work_dir)?;
    fs::create_dir_all(&tmp_dir)?;
    fs::create_dir_all(&output_dir)?;
    let dependencies_dir = if let Some(capsule) = &workload.dependencies {
        capsule.validate()?;
        capsule.require_compatible(workload.runtime.kind)?;
        let directory = root.path().join("dependencies");
        fs::create_dir(&directory)?;
        for file in &capsule.files {
            validate_portable_path(&file.path, "dependency path")?;
            let destination = directory.join(&file.path);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&destination, &file.data)?;
            #[cfg(unix)]
            if file.executable {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&destination, fs::Permissions::from_mode(0o755))?;
            }
        }
        Some(directory)
    } else {
        None
    };

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
        validate_portable_path(&input.path, "input path")?;
        let destination = work_dir.join(&input.path);
        if destination.exists() {
            return Err(ComputeError::InvalidWorkload(format!(
                "input destination conflicts with an existing workspace path: {}",
                input.path.display()
            )));
        }
        match &input.source {
            ExecutionInputSource::Inline { data } => {
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(destination, data)?;
            }
            ExecutionInputSource::File { path } => copy_path(path, &destination)?,
        }
    }

    for output in &workload.outputs {
        validate_portable_path(&output.path, "output")?;
        if let Some(parent) = output_dir.join(&output.path).parent() {
            fs::create_dir_all(parent)?;
        }
    }

    Ok(StagedWorkload {
        root,
        work_dir,
        tmp_dir,
        output_dir,
        entrypoint: staged_entrypoint,
        dependencies_dir,
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
            // Results expose the stable execution namespace, never the
            // temporary host path that is removed during cleanup.
            path: Path::new("/output").join(relative),
            size: entry
                .metadata()
                .map_err(|error| ComputeError::InvalidWorkload(error.to_string()))?
                .len(),
        });
    }
    artifacts.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(artifacts)
}

pub fn apply_output_contract(
    result: &mut ExecutionResult,
    output_dir: &Path,
    declarations: &[WorkloadOutput],
) -> Result<()> {
    let root = output_dir.canonicalize()?;
    let mut declarations = declarations.to_vec();
    declarations.sort_by(|left, right| left.path.cmp(&right.path));
    let mut violations = Vec::new();

    for declaration in declarations {
        validate_portable_path(&declaration.path, "output")?;
        let candidate = output_dir.join(&declaration.path);
        let mut cursor = output_dir.to_path_buf();
        let mut symlink_component = false;
        for component in declaration.path.components() {
            let std::path::Component::Normal(component) = component else {
                unreachable!("portable paths contain only normal components")
            };
            cursor.push(component);
            match fs::symlink_metadata(&cursor) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    symlink_component = true;
                    break;
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => {
                    violations.push(format!("{}: {error}", declaration.path.display()));
                    symlink_component = true;
                    break;
                }
            }
        }
        if symlink_component {
            violations.push(format!(
                "declared output contains a symlink: {}",
                declaration.path.display()
            ));
            continue;
        }
        let metadata = match fs::symlink_metadata(&candidate) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                result.missing_outputs.push(MissingOutput {
                    path: declaration.path,
                    required: declaration.required,
                });
                continue;
            }
            Err(error) => {
                violations.push(format!("{}: {error}", declaration.path.display()));
                continue;
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            violations.push(format!(
                "declared output is not a regular file: {}",
                declaration.path.display()
            ));
            continue;
        }
        let resolved = candidate.canonicalize()?;
        if !resolved.starts_with(&root) {
            violations.push(format!(
                "declared output escapes the execution workspace: {}",
                declaration.path.display()
            ));
            continue;
        }
        let data = fs::read(&resolved)?;
        result.outputs.push(OutputArtifact {
            path: declaration.path,
            size: data.len() as u64,
            data,
        });
    }

    result
        .outputs
        .sort_by(|left, right| left.path.cmp(&right.path));
    result
        .missing_outputs
        .sort_by(|left, right| left.path.cmp(&right.path));
    let missing_required = result
        .missing_outputs
        .iter()
        .filter(|output| output.required)
        .map(|output| output.path.display().to_string())
        .collect::<Vec<_>>();
    let successful_workload = result.status == ExecutionStatus::Completed
        && result.exit_code.is_none_or(|code| code == 0);
    if result.status == ExecutionStatus::Completed
        && (!violations.is_empty() || (successful_workload && !missing_required.is_empty()))
    {
        let mut failures = violations;
        failures.extend(
            missing_required
                .iter()
                .map(|path| format!("required output is missing: {path}")),
        );
        result.status = ExecutionStatus::Failed;
        if let Some(last) = result.lifecycle.last_mut() {
            *last = ExecutionStatus::Failed;
        }
        result.error = Some(ExecutionError {
            execution_id: result.execution_id.clone(),
            phase: ExecutionPhase::Completed,
            kind: ExecutionErrorKind::OutputContract,
            message: failures.join("; "),
            runtime: Some(result.runtime),
            exit_code: result.exit_code,
            started: true,
        });
    }
    Ok(())
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
            if entry.file_type().is_symlink() {
                return Err(ComputeError::InvalidWorkload(format!(
                    "symlinks are not allowed in staged paths: {}",
                    entry.path().display()
                )));
            }
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
    fn descriptor(&self) -> RuntimeDescriptor;

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
    #[error("unsupported workload specification version: {version}")]
    UnsupportedWorkloadVersion { version: String },
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
    #[error("invalid isolation profile: {0}")]
    InvalidIsolationProfile(String),
    #[error(
        "unsupported capability: isolation profile {profile} is unavailable for runtime {runtime}: {code}"
    )]
    IsolationUnavailable {
        runtime: RuntimeKind,
        profile: IsolationProfile,
        code: String,
    },
    #[error("invalid workload bundle: {0}")]
    InvalidBundle(String),
    #[error("invalid dependency capsule: {0}")]
    InvalidDependencyCapsule(String),
    #[error("invalid execution receipt: {0}")]
    InvalidReceipt(String),
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

pub mod bytes_json {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Bytes {
        Text(String),
        Array(Vec<u8>),
    }

    pub fn serialize<S>(value: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match std::str::from_utf8(value) {
            Ok(text) => serializer.serialize_str(text),
            Err(_) => value.serialize(serializer),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match Bytes::deserialize(deserializer)? {
            Bytes::Text(text) => text.into_bytes(),
            Bytes::Array(bytes) => bytes,
        })
    }
}

pub mod schema_version {
    use serde::{Deserialize, Deserializer, Serializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Version {
        Text(String),
        Number(u64),
    }

    pub fn serialize<S>(value: &str, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(value)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<String, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match Version::deserialize(deserializer)? {
            Version::Text(value) => value,
            Version::Number(value) => value.to_string(),
        })
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
            stdin: vec![],
            env: vec![],
            inputs: vec![],
            outputs: vec![],
            mounts: vec![],
            network: NetworkPolicy::Network,
            resources: ResourceLimits::default(),
            isolation: IsolationProfile::Process,
            dependencies: None,
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
            stdin: vec![],
            env: vec![],
            inputs: vec![],
            outputs: vec![],
            mounts: vec![],
            network: NetworkPolicy::Network,
            resources: ResourceLimits::default(),
            isolation: IsolationProfile::Process,
            dependencies: None,
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

    #[test]
    fn capabilities_reject_unsupported_boundaries() {
        let temp = tempfile::tempdir().unwrap();
        let entrypoint = temp.path().join("main.py");
        fs::write(&entrypoint, "").unwrap();
        let workload = Workload {
            runtime: RuntimeSpec {
                kind: RuntimeKind::Python,
                version: None,
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
            isolation: IsolationProfile::Process,
            dependencies: None,
        };

        assert!(matches!(
            RuntimeCapabilities::process().validate(RuntimeKind::Python, &workload),
            Err(ComputeError::UnsupportedCapability { .. })
        ));
    }

    #[test]
    fn isolation_profiles_resolve_without_downgrades() {
        assert_eq!(
            "process".parse::<IsolationProfile>().unwrap(),
            IsolationProfile::Process
        );
        assert_eq!(
            "sandboxed".parse::<IsolationProfile>().unwrap(),
            IsolationProfile::Sandboxed
        );
        assert_eq!(
            "strict".parse::<IsolationProfile>().unwrap(),
            IsolationProfile::Strict
        );
        assert!("secure".parse::<IsolationProfile>().is_err());

        let temp = tempfile::tempdir().unwrap();
        let entrypoint = temp.path().join("module.wasm");
        fs::write(&entrypoint, []).unwrap();
        let mut request = Workload {
            runtime: RuntimeSpec {
                kind: RuntimeKind::Wasm,
                version: None,
            },
            entrypoint,
            args: vec![],
            stdin: vec![],
            env: vec![],
            inputs: vec![],
            outputs: vec![],
            mounts: vec![],
            network: NetworkPolicy::None,
            resources: ResourceLimits {
                memory_bytes: Some(64 * 1024),
                ..ResourceLimits::default()
            },
            isolation: IsolationProfile::Strict,
            dependencies: None,
        };
        let strict = RuntimeCapabilities::wasm()
            .resolve_isolation(RuntimeKind::Wasm, &request)
            .unwrap();
        assert_eq!(strict.effective, IsolationProfile::Strict);
        assert_eq!(strict.filesystem, BoundaryStatus::Enforced);
        assert_eq!(strict.network, BoundaryStatus::Disabled);
        assert_eq!(strict.resources, BoundaryStatus::Enforced);

        let mut deno = request.clone();
        deno.runtime.kind = RuntimeKind::Deno;
        deno.resources.memory_bytes = None;
        assert!(
            RuntimeCapabilities::deno()
                .resolve_isolation(RuntimeKind::Deno, &deno)
                .is_ok()
        );
        deno.resources.memory_bytes = Some(64 * 1024);
        assert_eq!(
            RuntimeCapabilities::deno()
                .resolve_isolation(RuntimeKind::Deno, &deno)
                .unwrap_err()
                .code,
            "memory_enforcement_unavailable"
        );

        request.runtime.kind = RuntimeKind::Python;
        request.network = NetworkPolicy::Network;
        request.resources.memory_bytes = None;
        let rejected = RuntimeCapabilities::process()
            .resolve_isolation(RuntimeKind::Python, &request)
            .unwrap_err();
        assert_eq!(rejected.code, "filesystem_isolation_unavailable");
    }

    fn valid_spec() -> WorkloadSpec {
        WorkloadSpec {
            version: WORKLOAD_SPEC_VERSION.into(),
            runtime: RuntimeKind::Python,
            runtime_version: None,
            entrypoint: "main.py".into(),
            args: vec!["hello world".into()],
            env: [("MODE".into(), "test".into())].into_iter().collect(),
            inputs: vec![WorkloadInput {
                path: "data/input.json".into(),
                source: InputSource::File {
                    path: "fixtures/input.json".into(),
                },
            }],
            outputs: vec![WorkloadOutput {
                path: "result.json".into(),
                required: false,
            }],
            resources: ResourceLimits {
                wall_time: Some(Duration::from_millis(500)),
                ..ResourceLimits::default()
            },
            network: NetworkPolicy::Network,
            isolation: IsolationRequirement::default(),
            dependencies: None,
        }
    }

    #[test]
    fn workload_spec_round_trips_deterministically() {
        let spec = valid_spec();
        let first = spec.to_pretty_json().unwrap();
        let parsed: WorkloadSpec = serde_json::from_str(&first).unwrap();
        let second = parsed.to_pretty_json().unwrap();
        assert_eq!(first, second);
        assert!(first.contains("\"timeout_ms\": 500"));

        let mut reordered = spec.clone();
        reordered.outputs = vec![
            WorkloadOutput {
                path: "z.txt".into(),
                required: false,
            },
            WorkloadOutput {
                path: "a.txt".into(),
                required: true,
            },
        ];
        let mut equivalent = reordered.clone();
        equivalent.outputs.reverse();
        assert_eq!(
            reordered.to_pretty_json().unwrap(),
            equivalent.to_pretty_json().unwrap()
        );
    }

    #[test]
    fn workload_identity_is_canonical_and_changes_with_the_contract() {
        let spec = valid_spec();
        let id = spec.workload_id().unwrap();
        assert!(id.starts_with("sha256:"));
        assert_eq!(id.len(), "sha256:".len() + 64);

        let mut reordered = spec.clone();
        reordered.inputs.reverse();
        reordered.outputs.reverse();
        assert_eq!(reordered.workload_id().unwrap(), id);

        let mut changed = spec;
        changed.args.push("contract-change".into());
        assert_ne!(changed.workload_id().unwrap(), id);
        assert!(changed.require_id(&id).is_err());

        let mut strict = changed;
        strict.isolation.profile = IsolationProfile::Strict;
        assert_ne!(strict.workload_id().unwrap(), id);
    }

    #[test]
    fn dependency_capsule_reference_is_part_of_workload_identity() {
        let mut first = valid_spec();
        first.dependencies = Some(WorkloadDependencies {
            capsule: sha256_identity(b"capsule-a"),
        });
        let mut second = first.clone();
        second.dependencies = Some(WorkloadDependencies {
            capsule: sha256_identity(b"capsule-b"),
        });
        assert_ne!(first.workload_id().unwrap(), second.workload_id().unwrap());
    }

    fn bundle_fixture() -> (TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("main.py"), "print('bundle')").unwrap();
        fs::create_dir(root.path().join("fixtures")).unwrap();
        fs::write(root.path().join("fixtures/input.json"), b"first input").unwrap();
        let workload = root.path().join("workload.json");
        fs::write(&workload, valid_spec().to_pretty_json().unwrap()).unwrap();
        (root, workload)
    }

    #[test]
    fn bundle_bytes_and_identity_are_deterministic() {
        let (_root, workload) = bundle_fixture();
        let first = WorkloadBundle::create(&workload).unwrap();
        let second = WorkloadBundle::create(&workload).unwrap();
        assert_eq!(first.to_bytes().unwrap(), second.to_bytes().unwrap());
        assert_eq!(first.bundle_id().unwrap(), second.bundle_id().unwrap());
        assert_eq!(first.workload_id().unwrap(), second.workload_id().unwrap());

        let decoded = WorkloadBundle::from_bytes(&first.to_bytes().unwrap()).unwrap();
        assert_eq!(decoded, first);
        assert_eq!(
            decoded.manifest().unwrap().inputs[0].path,
            Path::new("data/input.json")
        );

        let bytes = first.to_bytes().unwrap();
        let mut archive = tar::Archive::new(Cursor::new(bytes));
        let mut paths = Vec::new();
        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            paths.push(entry.path().unwrap().into_owned());
            assert_eq!(entry.header().mtime().unwrap(), 0);
            assert_eq!(entry.header().uid().unwrap(), 0);
            assert_eq!(entry.header().gid().unwrap(), 0);
            assert_eq!(entry.header().mode().unwrap(), 0o644);
        }
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted);
    }

    #[test]
    fn bundle_identity_includes_input_data_but_workload_identity_does_not() {
        let (root, workload) = bundle_fixture();
        let first = WorkloadBundle::create(&workload).unwrap();
        fs::write(root.path().join("fixtures/input.json"), b"second input").unwrap();
        let second = WorkloadBundle::create(&workload).unwrap();
        assert_eq!(first.workload_id().unwrap(), second.workload_id().unwrap());
        assert_ne!(first.bundle_id().unwrap(), second.bundle_id().unwrap());

        let mut changed = valid_spec();
        changed.args.push("different".into());
        fs::write(&workload, changed.to_pretty_json().unwrap()).unwrap();
        let third = WorkloadBundle::create(&workload).unwrap();
        assert_ne!(second.workload_id().unwrap(), third.workload_id().unwrap());
        assert_ne!(second.bundle_id().unwrap(), third.bundle_id().unwrap());
    }

    #[test]
    fn bundle_rejects_tampering_duplicates_and_unsafe_paths() {
        let (_root, workload) = bundle_fixture();
        let bundle = WorkloadBundle::create(&workload).unwrap();
        let mut tampered = bundle.to_bytes().unwrap();
        let offset = tampered
            .windows(b"first input".len())
            .position(|window| window == b"first input")
            .unwrap();
        tampered[offset] = b'X';
        assert!(matches!(
            WorkloadBundle::from_bytes(&tampered),
            Err(ComputeError::InvalidBundle(_))
        ));

        let mut workload_tampered = bundle.to_bytes().unwrap();
        let offset = workload_tampered
            .windows(b"hello world".len())
            .position(|window| window == b"hello world")
            .unwrap();
        workload_tampered[offset] = b'j';
        assert!(matches!(
            WorkloadBundle::from_bytes(&workload_tampered),
            Err(ComputeError::InvalidBundle(_))
        ));

        let mut duplicate = bundle.clone();
        duplicate.inputs.push(duplicate.inputs[0].clone());
        assert!(matches!(
            duplicate.validate(),
            Err(ComputeError::InvalidBundle(message)) if message.contains("duplicate")
        ));

        let mut traversal = bundle;
        traversal.inputs[0].path = "../secret".into();
        assert!(traversal.validate().is_err());
    }

    fn rebuild_test_archive(
        original: &[u8],
        mutate: impl FnOnce(&mut Vec<(PathBuf, Vec<u8>)>),
    ) -> Vec<u8> {
        let mut archive = tar::Archive::new(Cursor::new(original));
        let mut entries = archive
            .entries()
            .unwrap()
            .map(|entry| {
                let mut entry = entry.unwrap();
                let path = entry.path().unwrap().into_owned();
                let mut data = Vec::new();
                entry.read_to_end(&mut data).unwrap();
                (path, data)
            })
            .collect::<Vec<_>>();
        mutate(&mut entries);
        let mut result = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut result);
            for (path, data) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_uid(0);
                header.set_gid(0);
                header.set_mtime(0);
                header.set_cksum();
                builder
                    .append_data(&mut header, path, Cursor::new(data))
                    .unwrap();
            }
            builder.finish().unwrap();
        }
        result
    }

    #[test]
    fn bundle_archive_rejects_duplicate_missing_unexpected_and_traversal_entries() {
        let (_root, workload) = bundle_fixture();
        let canonical = WorkloadBundle::create(&workload)
            .unwrap()
            .to_bytes()
            .unwrap();

        let duplicate = rebuild_test_archive(&canonical, |entries| {
            entries.push(entries.last().unwrap().clone());
        });
        assert!(matches!(
            WorkloadBundle::from_bytes(&duplicate),
            Err(ComputeError::InvalidBundle(message)) if message.contains("duplicate")
        ));

        let missing = rebuild_test_archive(&canonical, |entries| {
            entries.retain(|(path, _)| path != Path::new("inputs/data/input.json"));
        });
        assert!(matches!(
            WorkloadBundle::from_bytes(&missing),
            Err(ComputeError::InvalidBundle(message)) if message.contains("missing")
        ));

        let unexpected = rebuild_test_archive(&canonical, |entries| {
            entries.push((PathBuf::from("inputs/unexpected"), b"bad".to_vec()));
        });
        assert!(matches!(
            WorkloadBundle::from_bytes(&unexpected),
            Err(ComputeError::InvalidBundle(message)) if message.contains("unexpected")
        ));

        let mut traversal = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut traversal);
            let data = b"escape";
            let mut header = tar::Header::new_gnu();
            header.set_path("safe").unwrap();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.as_mut_bytes()[..100].fill(0);
            header.as_mut_bytes()[..9].copy_from_slice(b"../secret");
            header.set_cksum();
            builder.append(&header, &data[..]).unwrap();
            builder.finish().unwrap();
        }
        assert!(WorkloadBundle::from_bytes(&traversal).is_err());
    }

    #[test]
    fn materialized_bundle_no_longer_depends_on_original_input_file() {
        let (root, workload) = bundle_fixture();
        let bundle = WorkloadBundle::create(&workload).unwrap();
        fs::remove_file(root.path().join("fixtures/input.json")).unwrap();
        fs::remove_file(root.path().join("main.py")).unwrap();
        let materialized = bundle.materialize().unwrap();
        assert_eq!(
            fs::read(&materialized.request.entrypoint).unwrap(),
            b"print('bundle')"
        );
        assert!(matches!(
            &materialized.request.inputs[0].source,
            ExecutionInputSource::Inline { data } if data == b"first input"
        ));
    }

    #[test]
    fn inline_bundle_inputs_remain_in_workload_without_archive_duplication() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("main.py"), "print('inline')").unwrap();
        let mut spec = valid_spec();
        spec.inputs = vec![WorkloadInput {
            path: "data/inline.txt".into(),
            source: InputSource::Inline {
                data: b"inline bytes".to_vec(),
            },
        }];
        let workload = root.path().join("workload.json");
        fs::write(&workload, spec.to_pretty_json().unwrap()).unwrap();
        let bundle = WorkloadBundle::create(&workload).unwrap();
        assert!(bundle.inputs.is_empty());
        assert!(bundle.manifest().unwrap().inputs.is_empty());
        assert_eq!(bundle.inspection().unwrap().inputs[0].size, 12);
        let bytes = bundle.to_bytes().unwrap();
        let mut archive = tar::Archive::new(Cursor::new(bytes));
        assert!(archive.entries().unwrap().all(|entry| {
            !entry
                .unwrap()
                .path()
                .unwrap()
                .starts_with(Path::new("inputs"))
        }));
    }

    #[test]
    fn workload_spec_rejects_missing_and_unsupported_versions() {
        let missing = r#"{"runtime":"python","entrypoint":"main.py"}"#;
        assert!(serde_json::from_str::<WorkloadSpec>(missing).is_err());

        let mut spec = valid_spec();
        spec.version = "999".into();
        assert!(matches!(
            spec.validate(),
            Err(ComputeError::UnsupportedWorkloadVersion { .. })
        ));
    }

    #[test]
    fn workload_spec_rejects_malformed_required_fields() {
        for json in [
            "{",
            r#"{"version":"1","entrypoint":"main.py"}"#,
            r#"{"version":"1","runtime":"unknown","entrypoint":"main.py"}"#,
            r#"{"version":"1","runtime":"python"}"#,
            r#"{"version":"1","runtime":"python","entrypoint":"main.py","network":"invalid"}"#,
            r#"{"version":"1","runtime":"python","entrypoint":"main.py","inputs":[{"path":"input","source":{"type":"file"}}]}"#,
            r#"{"version":"1","runtime":"python","entrypoint":"main.py","outputs":[{"required":true}]}"#,
        ] {
            assert!(
                serde_json::from_str::<WorkloadSpec>(json).is_err(),
                "{json}"
            );
        }
    }

    #[test]
    fn workload_spec_rejects_unsafe_paths_and_invalid_values() {
        let mut spec = valid_spec();
        spec.entrypoint = "../../outside".into();
        assert!(matches!(
            spec.validate(),
            Err(ComputeError::InvalidWorkload(_))
        ));

        for path in ["../foo", "../../foo", "a/../../foo", "/tmp/foo", "C:\\foo"] {
            let mut spec = valid_spec();
            spec.inputs[0].path = path.into();
            assert!(matches!(
                spec.validate(),
                Err(ComputeError::InvalidWorkload(_))
            ));
        }

        for path in ["../result", "../../result", "/tmp/result", "C:\\result"] {
            let mut spec = valid_spec();
            spec.outputs[0].path = path.into();
            assert!(matches!(
                spec.validate(),
                Err(ComputeError::InvalidWorkload(_))
            ));
        }

        let mut spec = valid_spec();
        spec.inputs.push(WorkloadInput {
            path: "data/input.json/nested".into(),
            source: InputSource::Inline { data: vec![] },
        });
        assert!(matches!(
            spec.validate(),
            Err(ComputeError::InvalidWorkload(_))
        ));

        let mut spec = valid_spec();
        spec.outputs = vec![WorkloadOutput {
            path: "/host/output".into(),
            required: true,
        }];
        assert!(matches!(
            spec.validate(),
            Err(ComputeError::InvalidWorkload(_))
        ));

        let mut spec = valid_spec();
        spec.inputs[0].path = "../secret".into();
        assert!(matches!(
            spec.validate(),
            Err(ComputeError::InvalidWorkload(_))
        ));

        let mut spec = valid_spec();
        spec.inputs[0].source = InputSource::File {
            path: "C:\\secret".into(),
        };
        assert!(matches!(
            spec.validate(),
            Err(ComputeError::InvalidWorkload(_))
        ));

        let mut spec = valid_spec();
        spec.env = [("BAD=NAME".into(), "value".into())].into_iter().collect();
        assert!(matches!(
            spec.validate(),
            Err(ComputeError::InvalidWorkload(_))
        ));

        let mut spec = valid_spec();
        spec.resources.wall_time = Some(Duration::ZERO);
        assert!(matches!(
            spec.validate(),
            Err(ComputeError::InvalidWorkload(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn workload_materialization_rejects_symlink_escape() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("main.py"), "").unwrap();
        std::os::unix::fs::symlink(outside.path().join("main.py"), root.path().join("main.py"))
            .unwrap();
        fs::create_dir(root.path().join("data")).unwrap();
        fs::write(root.path().join("data/input.json"), "{}").unwrap();
        let spec_file = root.path().join("compute.json");
        fs::write(&spec_file, "{}").unwrap();

        assert!(matches!(
            valid_spec().materialize(&spec_file),
            Err(ComputeError::InvalidWorkload(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn file_input_symlink_cannot_escape_specification_context() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(root.path().join("main.py"), "").unwrap();
        fs::create_dir(root.path().join("fixtures")).unwrap();
        fs::write(outside.path().join("input.json"), "secret").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("input.json"),
            root.path().join("fixtures/input.json"),
        )
        .unwrap();
        let spec_file = root.path().join("workload.json");
        fs::write(&spec_file, "{}").unwrap();

        assert!(matches!(
            valid_spec().materialize(&spec_file),
            Err(ComputeError::InvalidWorkload(_))
        ));
    }

    #[test]
    fn portable_inputs_serialize_and_materialize_deterministically() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("main.py"), "").unwrap();
        fs::create_dir(root.path().join("fixtures")).unwrap();
        fs::write(root.path().join("fixtures/file.txt"), b"file bytes").unwrap();
        let spec_file = root.path().join("workload.json");
        fs::write(&spec_file, "{}").unwrap();
        let mut spec = valid_spec();
        spec.inputs = vec![
            WorkloadInput {
                path: "z/file.txt".into(),
                source: InputSource::File {
                    path: "fixtures/file.txt".into(),
                },
            },
            WorkloadInput {
                path: "a/inline.txt".into(),
                source: InputSource::Inline {
                    data: b"inline bytes".to_vec(),
                },
            },
        ];
        let json = spec.to_pretty_json().unwrap();
        assert!(json.find("a/inline.txt").unwrap() < json.find("z/file.txt").unwrap());
        assert!(json.contains("\"data\": \"inline bytes\""));

        let request = spec.materialize(&spec_file).unwrap();
        let staged = stage_workload(&request).unwrap();
        assert_eq!(
            fs::read(staged.work_dir.join("a/inline.txt")).unwrap(),
            b"inline bytes"
        );
        assert_eq!(
            fs::read(staged.work_dir.join("z/file.txt")).unwrap(),
            b"file bytes"
        );
    }

    #[test]
    fn missing_file_input_fails_before_execution() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("main.py"), "").unwrap();
        let spec_file = root.path().join("workload.json");
        fs::write(&spec_file, "{}").unwrap();
        let mut spec = valid_spec();
        spec.inputs = vec![WorkloadInput {
            path: "input.txt".into(),
            source: InputSource::File {
                path: "missing.txt".into(),
            },
        }];
        assert!(matches!(
            spec.materialize(&spec_file),
            Err(ComputeError::InvalidWorkload(_))
        ));
    }

    fn output_test_result() -> ExecutionResult {
        ExecutionResult {
            execution_id: "exec_test".into(),
            runtime: RuntimeKind::Python,
            network: NetworkPolicy::Network,
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
            stdout: Output::from_bytes(vec![], None),
            stderr: Output::from_bytes(vec![], None),
            duration: Duration::ZERO,
            resource_usage: ResourceUsage::default(),
            artifacts: vec![],
            outputs: vec![],
            missing_outputs: vec![],
            error: None,
            isolation: None,
            dependencies: None,
            provider: None,
            receipt: None,
        }
    }

    #[test]
    fn declared_outputs_are_collected_and_missing_optional_is_recorded() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("nested")).unwrap();
        fs::write(root.path().join("nested/result.txt"), b"result bytes").unwrap();
        fs::write(root.path().join("a.txt"), b"a").unwrap();
        let declarations = vec![
            WorkloadOutput {
                path: "nested/result.txt".into(),
                required: true,
            },
            WorkloadOutput {
                path: "optional.txt".into(),
                required: false,
            },
            WorkloadOutput {
                path: "a.txt".into(),
                required: true,
            },
        ];
        let mut result = output_test_result();
        apply_output_contract(&mut result, root.path(), &declarations).unwrap();

        assert_eq!(result.status, ExecutionStatus::Completed);
        assert_eq!(result.outputs[0].path, Path::new("a.txt"));
        assert_eq!(result.outputs[1].path, Path::new("nested/result.txt"));
        assert_eq!(result.outputs[1].data, b"result bytes");
        assert_eq!(result.outputs[1].size, 12);
        assert_eq!(
            result.missing_outputs,
            vec![MissingOutput {
                path: "optional.txt".into(),
                required: false,
            }]
        );
    }

    #[test]
    fn missing_required_output_is_a_distinct_execution_failure() {
        let root = tempfile::tempdir().unwrap();
        let mut result = output_test_result();
        apply_output_contract(
            &mut result,
            root.path(),
            &[WorkloadOutput {
                path: "required.txt".into(),
                required: true,
            }],
        )
        .unwrap();

        assert_eq!(result.status, ExecutionStatus::Failed);
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.lifecycle.last(), Some(&ExecutionStatus::Failed));
        assert_eq!(
            result.error.unwrap().kind,
            ExecutionErrorKind::OutputContract
        );
    }

    #[test]
    fn repeated_staging_uses_independent_workspaces() {
        let root = tempfile::tempdir().unwrap();
        let entrypoint = root.path().join("main.py");
        fs::write(&entrypoint, "").unwrap();
        let request = Workload {
            runtime: RuntimeSpec {
                kind: RuntimeKind::Python,
                version: None,
            },
            entrypoint,
            args: vec![],
            stdin: vec![],
            env: vec![],
            inputs: vec![Input {
                path: "input.txt".into(),
                source: ExecutionInputSource::Inline {
                    data: b"same input".to_vec(),
                },
            }],
            outputs: vec![WorkloadOutput {
                path: "result.txt".into(),
                required: false,
            }],
            mounts: vec![],
            network: NetworkPolicy::Network,
            resources: ResourceLimits::default(),
            isolation: IsolationProfile::Process,
            dependencies: None,
        };
        let first = stage_workload(&request).unwrap();
        let second = stage_workload(&request).unwrap();
        fs::write(first.output_dir.join("result.txt"), "first").unwrap();

        assert_ne!(first.root.path(), second.root.path());
        assert!(!second.output_dir.join("result.txt").exists());
        assert_eq!(
            fs::read(second.work_dir.join("input.txt")).unwrap(),
            b"same input"
        );
    }

    #[cfg(unix)]
    #[test]
    fn output_symlink_escape_is_an_output_contract_failure() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "secret").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret"),
            root.path().join("result.txt"),
        )
        .unwrap();
        let mut result = output_test_result();
        apply_output_contract(
            &mut result,
            root.path(),
            &[WorkloadOutput {
                path: "result.txt".into(),
                required: false,
            }],
        )
        .unwrap();
        assert_eq!(result.status, ExecutionStatus::Failed);
        assert_eq!(
            result.error.unwrap().kind,
            ExecutionErrorKind::OutputContract
        );
    }
}
