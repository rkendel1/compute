use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use compute_core::{
    ComputeError, DependencyCapsule, EnvironmentVariable, InputSource, IsolationProfile,
    IsolationRequirement, NetworkPolicy, ResourceLimits, RuntimeKind, WORKLOAD_SPEC_VERSION,
    WorkloadDependencies, WorkloadInput, WorkloadOutput, WorkloadSpec,
};
use serde::Deserialize;

#[derive(Debug)]
pub struct DirectOptions {
    pub path: PathBuf,
    pub runtime: Option<String>,
    pub args: Vec<String>,
    pub env: Vec<EnvironmentVariable>,
    pub env_file: Option<PathBuf>,
    pub inputs: Vec<PathBuf>,
    pub outputs: Vec<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub entrypoint: Option<PathBuf>,
    pub deps: Option<PathBuf>,
    pub network: Option<NetworkPolicy>,
    pub isolation: Option<IsolationProfile>,
    pub memory: Option<u64>,
    pub timeout: Option<Duration>,
    /// Policy defaults for values neither the caller nor project
    /// configuration chose. They never override a stated value.
    pub defaults: compute_policy::PolicyDefaults,
}

#[derive(Debug)]
pub struct ResolvedDirect {
    pub root: PathBuf,
    pub config: Option<PathBuf>,
    pub workload: WorkloadSpec,
    pub dependency_capsule: Option<DependencyCapsule>,
    pub placement: DirectPlacement,
}

#[derive(Debug, Clone, Default)]
pub struct DirectPlacement {
    pub policy: Option<String>,
    pub prefer_provider: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ProjectConfig {
    /// Product-level metadata consumed by application lifecycle commands.
    application: Option<toml::Value>,
    runtime: RuntimeConfig,
    run: RunConfig,
    resources: ResourceConfig,
    network: NetworkConfig,
    dependencies: DependenciesConfig,
    placement: PlacementConfig,
    /// Local execution policy reference; read by the policy loader.
    policy: Option<toml::Value>,
    /// Server configuration; read by `compute serve`.
    server: Option<toml::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RuntimeConfig {
    name: Option<String>,
    version: Option<String>,
    architecture: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RunConfig {
    runtime: Option<String>,
    /// A workload-layer alias or constraint. Distribution identity remains
    /// exact and is chosen by placement.
    version: Option<String>,
    entrypoint: Option<PathBuf>,
    isolation: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ResourceConfig {
    cpu: Option<u32>,
    timeout: Option<String>,
    memory: Option<String>,
    disk: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct NetworkConfig {
    mode: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct DependenciesConfig {
    capsule: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PlacementConfig {
    policy: Option<String>,
    prefer_provider: Option<String>,
}

pub fn resolve(options: DirectOptions) -> compute_core::Result<ResolvedDirect> {
    let explicit_root = options
        .cwd
        .as_deref()
        .map(|path| canonical_directory(path, "working directory"))
        .transpose()?;
    let requested_path = match (&explicit_root, options.path.is_relative()) {
        (Some(root), true) => root.join(&options.path),
        _ => options.path.clone(),
    };
    if fs::symlink_metadata(&requested_path).is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(ComputeError::InvalidWorkload(format!(
            "workload path may not be a symbolic link: {}",
            options.path.display()
        )));
    }
    let requested = canonical_existing(&requested_path, "workload path")?;
    let root = match (&explicit_root, requested.is_dir()) {
        (Some(root), _) => root.clone(),
        (None, true) => requested.clone(),
        (None, false) => requested
            .parent()
            .expect("canonical file has a parent")
            .to_path_buf(),
    };

    if !requested.starts_with(&root) {
        return Err(ComputeError::InvalidWorkload(format!(
            "entrypoint escapes the working directory: {}",
            options.path.display()
        )));
    }

    let config_path = root.join("compute.toml");
    let (config, config_source) = if config_path.exists() {
        let text = fs::read_to_string(&config_path)?;
        let config: ProjectConfig = toml::from_str(&text).map_err(|error| {
            ComputeError::InvalidWorkload(format!("invalid compute.toml: {error}"))
        })?;
        (config, Some(config_path))
    } else {
        (ProjectConfig::default(), None)
    };

    let entrypoint = if requested.is_dir() {
        resolve_directory_entrypoint(
            &root,
            options
                .entrypoint
                .as_deref()
                .or(config.run.entrypoint.as_deref()),
        )?
    } else {
        if options.entrypoint.is_some() {
            return Err(ComputeError::InvalidWorkload(
                "--entrypoint can only be used with a directory workload".into(),
            ));
        }
        relative_portable(&root, &requested, "entrypoint")?
    };
    let entrypoint_host = root.join(&entrypoint);

    let runtime_name = options
        .runtime
        .as_deref()
        .or(config.runtime.name.as_deref())
        .or(config.run.runtime.as_deref());
    let runtime = match runtime_name {
        Some(value) => value.parse::<RuntimeKind>()?,
        None => detect_runtime(&root, &entrypoint_host)?
            .ok_or_else(|| runtime_resolution_error(&entrypoint))?,
    };

    let configured_isolation = config
        .run
        .isolation
        .as_deref()
        .map(str::parse::<IsolationProfile>)
        .transpose()?;
    let isolation = options
        .isolation
        .or(configured_isolation)
        .or(options.defaults.isolation)
        .unwrap_or_default();

    let network = match options.network {
        Some(value) => value,
        None => config
            .network
            .mode
            .as_deref()
            .map(super::parse_network)
            .transpose()
            .map_err(ComputeError::InvalidWorkload)?
            .or_else(|| options.defaults.network.clone())
            .unwrap_or_default(),
    };
    let timeout = match options.timeout {
        Some(value) => Some(value),
        None => config
            .resources
            .timeout
            .as_deref()
            .map(super::parse_duration)
            .transpose()
            .map_err(ComputeError::InvalidWorkload)?,
    };
    let memory = match options.memory {
        Some(value) => Some(value),
        None => config
            .resources
            .memory
            .as_deref()
            .map(super::parse_memory)
            .transpose()
            .map_err(ComputeError::InvalidWorkload)?,
    };
    let disk = config
        .resources
        .disk
        .as_deref()
        .map(super::parse_memory)
        .transpose()
        .map_err(ComputeError::InvalidWorkload)?;

    let mut environment = BTreeMap::new();
    if let Some(path) = options.env_file.as_deref() {
        load_env_file(&root, path, &mut environment)?;
    }
    // Explicit CLI values take precedence over the env file. The host
    // environment is intentionally never consulted.
    for pair in options.env {
        environment.insert(pair.key, pair.value);
    }

    let mut inputs = Vec::new();
    for path in options.inputs {
        let host = resolve_within(&root, &path, "input")?;
        if !host.is_file() {
            return Err(ComputeError::InvalidWorkload(format!(
                "input is not a regular file: {}",
                path.display()
            )));
        }
        let logical = relative_portable(&root, &host, "input")?;
        inputs.push(WorkloadInput {
            path: logical.clone(),
            source: InputSource::File { path: logical },
        });
    }

    let outputs = options
        .outputs
        .into_iter()
        .map(|path| WorkloadOutput {
            path,
            required: true,
        })
        .collect();

    let dependency_path = match options.deps {
        Some(path) => Some(canonical_existing(&path, "dependency capsule")?),
        None => config
            .dependencies
            .capsule
            .as_deref()
            .map(|path| resolve_within(&root, path, "dependency capsule"))
            .transpose()?,
    };
    let dependency_capsule = dependency_path
        .as_deref()
        .map(DependencyCapsule::read)
        .transpose()?;
    if let Some(capsule) = &dependency_capsule
        && capsule.runtime != runtime
    {
        return Err(ComputeError::InvalidDependencyCapsule(format!(
            "dependency capsule runtime {} does not match workload runtime {runtime}",
            capsule.runtime
        )));
    }
    let dependencies = match &dependency_capsule {
        Some(capsule) => Some(WorkloadDependencies {
            capsule: capsule.capsule_id()?,
        }),
        None => None,
    };

    let workload = WorkloadSpec {
        version: WORKLOAD_SPEC_VERSION.into(),
        runtime,
        runtime_version: config.runtime.version.or(config.run.version),
        architecture: config.runtime.architecture,
        entrypoint,
        args: options.args,
        env: environment,
        inputs,
        outputs,
        resources: ResourceLimits {
            cpu_count: config.resources.cpu,
            memory_required_bytes: config
                .resources
                .memory
                .is_some()
                .then_some(memory)
                .flatten(),
            memory_bytes: options.memory,
            disk_bytes: disk,
            wall_time: timeout,
            ..ResourceLimits::default()
        },
        network,
        isolation: IsolationRequirement {
            profile: isolation,
            host: Default::default(),
        },
        dependencies,
    };
    workload.validate()?;

    Ok(ResolvedDirect {
        root,
        config: config_source,
        workload,
        dependency_capsule,
        placement: DirectPlacement {
            policy: config.placement.policy,
            prefer_provider: config.placement.prefer_provider,
        },
    })
}

fn resolve_directory_entrypoint(
    root: &Path,
    configured: Option<&Path>,
) -> compute_core::Result<PathBuf> {
    if let Some(path) = configured {
        let host = resolve_within(root, path, "configured entrypoint")?;
        if !host.is_file() {
            return Err(ComputeError::InvalidWorkload(format!(
                "configured entrypoint is not a regular file: {}",
                path.display()
            )));
        }
        return relative_portable(root, &host, "entrypoint");
    }

    if let Some(main) = package_json_entrypoint(root)? {
        return Ok(main);
    }

    let names = [
        "main.py",
        "app.py",
        "main.wasm",
        "main.js",
        "app.js",
        "index.js",
        "main.ts",
        "index.ts",
        "main.rb",
        "main.php",
        "main.jar",
        "main.dll",
        "main.sh",
    ];
    let candidates = names
        .iter()
        .map(PathBuf::from)
        .filter(|name| root.join(name).is_file())
        .collect::<Vec<_>>();
    match candidates.as_slice() {
        [only] => Ok(only.clone()),
        [] => Err(ComputeError::InvalidWorkload(
            "no unambiguous entrypoint found; add compute.toml or use --entrypoint".into(),
        )),
        many => Err(ComputeError::InvalidWorkload(format!(
            "multiple entrypoints detected:\n{}\nSpecify --entrypoint or compute.toml [run].entrypoint",
            many.iter()
                .map(|path| format!("  {}", path.display()))
                .collect::<Vec<_>>()
                .join("\n")
        ))),
    }
}

fn package_json_entrypoint(root: &Path) -> compute_core::Result<Option<PathBuf>> {
    let path = root.join("package.json");
    if !path.is_file() {
        return Ok(None);
    }
    let document: serde_json::Value = serde_json::from_slice(&fs::read(path)?)?;
    let Some(main) = document.get("main").and_then(serde_json::Value::as_str) else {
        return Ok(None);
    };
    let host = resolve_within(root, Path::new(main), "package.json main")?;
    if !host.is_file() {
        return Err(ComputeError::InvalidWorkload(format!(
            "package.json main is not a regular file: {main}"
        )));
    }
    relative_portable(root, &host, "entrypoint").map(Some)
}

fn detect_runtime(root: &Path, path: &Path) -> compute_core::Result<Option<RuntimeKind>> {
    let extension = path.extension().and_then(|value| value.to_str());
    let runtime = match extension {
        Some("wasm") => Some(RuntimeKind::Wasm),
        Some("py") => Some(RuntimeKind::Python),
        Some("rb") => Some(RuntimeKind::Ruby),
        Some("php") => Some(RuntimeKind::Php),
        Some("jar") => Some(RuntimeKind::Jvm),
        Some("dll") => Some(RuntimeKind::Dotnet),
        Some("sh") => Some(RuntimeKind::Shell),
        Some("js" | "mjs" | "cjs" | "ts" | "mts" | "cts") => {
            if root.join("package.json").is_file() {
                Some(RuntimeKind::Node)
            } else {
                return Err(runtime_resolution_error(
                    path.file_name().map(Path::new).unwrap_or(path),
                ));
            }
        }
        _ if is_executable(path) => Some(RuntimeKind::Native),
        _ => None,
    };
    Ok(runtime)
}

fn runtime_resolution_error(path: &Path) -> ComputeError {
    let extension = path.extension().and_then(|value| value.to_str());
    if matches!(extension, Some("js" | "mjs" | "cjs" | "ts" | "mts" | "cts")) {
        return ComputeError::InvalidWorkload(format!(
            "Cannot resolve runtime for {}.\nThe file extension is compatible with:\n  node\n  bun\n  deno\nChoose one with --runtime node, --runtime bun, or --runtime deno.",
            path.display()
        ));
    }
    ComputeError::InvalidWorkload(format!(
        "cannot determine runtime for {}; use --runtime",
        path.display()
    ))
}

fn load_env_file(
    root: &Path,
    path: &Path,
    environment: &mut BTreeMap<String, String>,
) -> compute_core::Result<()> {
    let resolved = resolve_within(root, path, "environment file")?;
    let text = fs::read_to_string(resolved)?;
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or_else(|| {
            ComputeError::InvalidWorkload(format!(
                "invalid environment file line {}: expected NAME=value",
                index + 1
            ))
        })?;
        environment.insert(key.trim().to_string(), value.to_string());
    }
    Ok(())
}

fn canonical_existing(path: &Path, label: &str) -> compute_core::Result<PathBuf> {
    path.canonicalize().map_err(|error| {
        ComputeError::InvalidWorkload(format!(
            "{label} does not exist ({}): {error}",
            path.display()
        ))
    })
}

fn canonical_directory(path: &Path, label: &str) -> compute_core::Result<PathBuf> {
    let path = canonical_existing(path, label)?;
    if !path.is_dir() {
        return Err(ComputeError::InvalidWorkload(format!(
            "{label} is not a directory: {}",
            path.display()
        )));
    }
    Ok(path)
}

fn resolve_within(root: &Path, path: &Path, label: &str) -> compute_core::Result<PathBuf> {
    if path.is_absolute() {
        return Err(ComputeError::InvalidWorkload(format!(
            "{label} must be relative to the workload root: {}",
            path.display()
        )));
    }
    let resolved = root.join(path).canonicalize().map_err(|error| {
        ComputeError::InvalidWorkload(format!("invalid {label} {}: {error}", path.display()))
    })?;
    if !resolved.starts_with(root) {
        return Err(ComputeError::InvalidWorkload(format!(
            "{label} escapes the workload root: {}",
            path.display()
        )));
    }
    Ok(resolved)
}

fn relative_portable(root: &Path, path: &Path, label: &str) -> compute_core::Result<PathBuf> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| ComputeError::InvalidWorkload(format!("{label} escapes the workload root")))?;
    if relative.as_os_str().is_empty() {
        return Err(ComputeError::InvalidWorkload(format!(
            "{label} cannot be the workload root"
        )));
    }
    Ok(relative.to_path_buf())
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file() && path.extension().is_none()
}
