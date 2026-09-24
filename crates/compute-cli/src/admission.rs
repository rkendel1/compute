//! Execution policy sources and local admission for the CLI.
//!
//! Sources: the documented baseline, local configuration (`compute.toml`
//! `[policy] path`), and an explicit `--policy` file. They are intersected;
//! none can widen another. Servers add `[server.policy] path` (or
//! `compute serve --policy`).

use std::path::{Path, PathBuf};

use clap::Args;
use compute_core::{ComputeError, DependencyCapsule, IsolationProfile, WorkloadBundle};
use compute_policy::{Policy, PolicyDefaults, PolicySourceKind};
use compute_provider::{Admission, ComputeProvider, LocalProvider, ProviderRequest};
use serde::Deserialize;

pub const LOCAL_CONFIG: &str = "compute.toml";

#[derive(Args, Debug, Clone, Default)]
pub struct PolicyLocation {
    /// Execution policy (compute.policy@1 JSON) that also applies to this
    /// invocation. It can only restrict.
    #[arg(long = "policy", global = true)]
    pub policy: Option<PathBuf>,
    /// Local configuration file. Defaults to $COMPUTE_CONFIG, then
    /// ./compute.toml when present.
    #[arg(long = "config", global = true)]
    pub config: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
struct LocalConfig {
    #[serde(default)]
    policy: Option<PolicyReference>,
    #[serde(default)]
    server: Option<ServerSection>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyReference {
    path: PathBuf,
}

#[derive(Debug, Default, Deserialize)]
struct ServerSection {
    #[serde(default)]
    policy: Option<PolicyReference>,
}

pub fn load_policy(path: &Path) -> compute_core::Result<Policy> {
    let bytes = std::fs::read(path).map_err(|error| {
        ComputeError::InvalidWorkload(format!("cannot read policy {}: {error}", path.display()))
    })?;
    Policy::from_json(&bytes)
        .map_err(|error| ComputeError::InvalidWorkload(format!("{}: {error}", path.display())))
}

impl PolicyLocation {
    fn config_path(&self) -> Option<PathBuf> {
        self.config
            .clone()
            .or_else(|| std::env::var_os("COMPUTE_CONFIG").map(PathBuf::from))
            .or_else(|| {
                Path::new(LOCAL_CONFIG)
                    .is_file()
                    .then(|| LOCAL_CONFIG.into())
            })
    }

    fn local_config(&self) -> compute_core::Result<Option<(PathBuf, LocalConfig)>> {
        let Some(path) = self.config_path() else {
            return Ok(None);
        };
        let text = std::fs::read_to_string(&path).map_err(|error| {
            ComputeError::InvalidWorkload(format!("cannot read {}: {error}", path.display()))
        })?;
        let config: LocalConfig = toml::from_str(&text).map_err(|error| {
            ComputeError::InvalidWorkload(format!("invalid {}: {error}", path.display()))
        })?;
        Ok(Some((path, config)))
    }

    fn resolve(config: &Path, reference: &PolicyReference) -> PathBuf {
        if reference.path.is_absolute() {
            reference.path.clone()
        } else {
            config
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."))
                .join(&reference.path)
        }
    }

    /// Caller-side policy sources, excluding the baseline.
    pub fn sources(&self) -> compute_core::Result<Vec<(PolicySourceKind, Policy)>> {
        let mut sources = vec![];
        if let Some((path, config)) = self.local_config()?
            && let Some(reference) = &config.policy
        {
            sources.push((
                PolicySourceKind::Local,
                load_policy(&Self::resolve(&path, reference))?,
            ));
        }
        if let Some(path) = &self.policy {
            sources.push((PolicySourceKind::Explicit, load_policy(path)?));
        }
        Ok(sources)
    }

    /// The server's own policy: `--policy`, else `[server.policy] path`.
    pub fn server_policy(&self) -> compute_core::Result<Option<Policy>> {
        if let Some(path) = &self.policy {
            return load_policy(path).map(Some);
        }
        match self.local_config()? {
            Some((path, config)) => config
                .server
                .and_then(|server| server.policy)
                .map(|reference| load_policy(&Self::resolve(&path, &reference)))
                .transpose(),
            None => Ok(None),
        }
    }

    /// Defaults for generated workloads, from the effective caller policy.
    pub fn defaults(&self) -> compute_core::Result<PolicyDefaults> {
        Ok(compute_policy::EffectivePolicy::compose(&self.sources()?)
            .policy
            .defaults)
    }
}

/// The caller's restrictions as one policy to send with a request.
pub fn request_policy(sources: &[(PolicySourceKind, Policy)]) -> Option<Policy> {
    sources
        .iter()
        .map(|(_, policy)| policy.clone())
        .reduce(|left, right| left.intersect(&right))
        .map(|mut policy| {
            policy.name = None;
            policy
        })
}

/// Admission on the local provider for a bundle about to run locally.
pub async fn admit_locally(
    bundle: &WorkloadBundle,
    supplied_capsule: Option<&DependencyCapsule>,
    isolation: Option<IsolationProfile>,
    sources: &[(PolicySourceKind, Policy)],
) -> compute_core::Result<Admission> {
    let mut bundle = bundle.clone();
    if bundle.dependency_capsule.is_none() && bundle.workload.dependencies.is_some() {
        bundle.dependency_capsule = supplied_capsule.cloned();
    }
    let mut request = ProviderRequest::bundle(bundle.to_bytes()?);
    request.execution.isolation = isolation;
    request.execution.policy = request_policy(sources);
    LocalProvider::new()
        .admit(request)
        .await
        .map_err(crate::provider_error)
}

/// Print a denial's evidence and exit with the placement/admission status.
pub fn deny(admission: &Admission, json: bool) -> ! {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "admission": admission.decision }))
                .expect("admission is serializable")
        );
    }
    eprintln!(
        "admission denied ({}); nothing was executed",
        admission.decision.admission_id
    );
    for reason in &admission.decision.reasons {
        eprintln!("  {}: {}", reason.code, reason.message);
    }
    std::process::exit(crate::pool::PLACEMENT_FAILED_EXIT);
}

/// Bind an admission into a local execution result and its receipt.
pub fn bind(
    result: &mut compute_core::ExecutionResult,
    admission: &Admission,
) -> compute_core::Result<()> {
    let summary = admission.summary();
    result.admission = Some(summary.clone());
    if let Some(receipt) = &mut result.receipt {
        receipt.bind_admission(&summary);
        receipt.seal()?;
    }
    Ok(())
}
