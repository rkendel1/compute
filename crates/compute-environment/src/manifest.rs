//! Declarative manifests. They describe what Compute should operate, not
//! how a project builds.
//!
//! An environment manifest (`compute.environment.toml`):
//!
//! ```toml
//! [environment]
//! name = "prod"
//! desired_state = "running"
//! env = { LOG_LEVEL = "info" }
//!
//! [environment.policy]          # compute.policy@1, inline
//! version = 1
//! minimum_isolation = "process"
//!
//! [[project]]
//! name = "authboundry"
//! source = "../authboundry"     # a directory containing compute.project.toml
//! revision = "abc123"
//! desired_state = "running"
//! ```
//!
//! A project manifest (`compute.project.toml` in the project source):
//!
//! ```toml
//! [project]
//! name = "authboundry"
//!
//! [[workload]]
//! name = "api"
//! kind = "service"
//! bundle = "dist/api.compute"   # or: workload = "api/workload.json"
//! ports = [{ name = "http", port = 8000 }]
//! restart = "on_failure"
//! readiness = { check = "http", path = "/health" }   # or "port", "process", "task"
//!
//! [[workload]]
//! name = "migrate"
//! kind = "task"
//! workload = "migrate/workload.json"
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use compute_core::WorkloadBundle;
use compute_policy::Policy;
use serde::Deserialize;

use crate::EnvironmentError;
use crate::model::*;

pub const PROJECT_MANIFEST: &str = "compute.project.toml";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentManifest {
    environment: EnvironmentSection,
    #[serde(default)]
    project: Vec<ProjectEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentSection {
    name: String,
    #[serde(default)]
    desired_state: DesiredState,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    policy: Option<toml::Value>,
    #[serde(default)]
    policy_file: Option<PathBuf>,
    #[serde(default)]
    provider: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectEntry {
    name: String,
    source: PathBuf,
    #[serde(default)]
    revision: Option<String>,
    #[serde(default)]
    desired_state: Option<DesiredState>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectManifest {
    #[serde(default)]
    project: ProjectSection,
    #[serde(default)]
    workload: Vec<WorkloadEntry>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectSection {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    revision: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkloadEntry {
    name: String,
    kind: WorkloadKind,
    #[serde(default)]
    bundle: Option<PathBuf>,
    #[serde(default)]
    workload: Option<PathBuf>,
    #[serde(default)]
    ports: Vec<PortSpec>,
    #[serde(default)]
    restart: RestartPolicy,
    #[serde(default)]
    desired_state: DesiredState,
    #[serde(default)]
    readiness: Option<Readiness>,
}

fn relative(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn parent(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Parse an inline or file policy into a validated `compute.policy@1`.
fn policy_from(
    inline: Option<toml::Value>,
    file: Option<PathBuf>,
    base: &Path,
) -> Result<Option<Policy>, EnvironmentError> {
    let invalid = |error: compute_policy::PolicyError| EnvironmentError::Invalid(error.to_string());
    match (inline, file) {
        (Some(_), Some(_)) => Err(EnvironmentError::Invalid(
            "use either [environment.policy] or policy_file, not both".into(),
        )),
        (Some(value), None) => {
            let json = serde_json::to_vec(&value)?;
            Policy::from_json(&json).map(Some).map_err(invalid)
        }
        (None, Some(path)) => Policy::from_json(&std::fs::read(relative(base, &path))?)
            .map(Some)
            .map_err(invalid),
        (None, None) => Ok(None),
    }
}

/// Load an environment manifest and every project it lists.
pub fn load_environment(
    path: &Path,
) -> Result<(EnvironmentDefinition, Vec<ProjectDefinition>), EnvironmentError> {
    let manifest: EnvironmentManifest = toml::from_str(&std::fs::read_to_string(path)?)
        .map_err(|error| EnvironmentError::Invalid(format!("{}: {error}", path.display())))?;
    let base = parent(path);
    let section = manifest.environment;
    let definition = EnvironmentDefinition {
        name: section.name,
        desired_state: section.desired_state,
        env: section.env,
        policy: policy_from(section.policy, section.policy_file, &base)?,
        provider: section.provider,
    };
    let mut projects = vec![];
    for entry in manifest.project {
        let mut project = load_project(&relative(&base, &entry.source))?;
        if project.name != entry.name {
            return Err(EnvironmentError::Invalid(format!(
                "project {} in the manifest names a source whose project is {}",
                entry.name, project.name
            )));
        }
        if let Some(revision) = entry.revision {
            project.revision = revision;
        }
        if let Some(desired) = entry.desired_state {
            project.desired_state = desired;
        }
        project.env.extend(entry.env);
        projects.push(project);
    }
    Ok((definition, projects))
}

/// Load a project from a source directory containing
/// `compute.project.toml`, or from that file directly.
pub fn load_project(source: &Path) -> Result<ProjectDefinition, EnvironmentError> {
    let manifest_path = if source.is_dir() {
        source.join(PROJECT_MANIFEST)
    } else {
        source.to_path_buf()
    };
    let manifest: ProjectManifest = toml::from_str(&std::fs::read_to_string(&manifest_path)?)
        .map_err(|error| {
            EnvironmentError::Invalid(format!("{}: {error}", manifest_path.display()))
        })?;
    let base = parent(&manifest_path);
    let mut workloads = vec![];
    let mut identities = vec![];
    for entry in manifest.workload {
        let bundle = match (&entry.bundle, &entry.workload) {
            (Some(bundle), None) => WorkloadBundle::read(&relative(&base, bundle))?,
            (None, Some(workload)) => WorkloadBundle::create(&relative(&base, workload))?,
            _ => {
                return Err(EnvironmentError::Invalid(format!(
                    "workload {} needs exactly one of bundle or workload",
                    entry.name
                )));
            }
        };
        identities.push(bundle.bundle_id()?);
        workloads.push(WorkloadDefinition {
            name: entry.name,
            kind: entry.kind,
            bundle: bundle.to_bytes()?,
            ports: entry.ports,
            restart: entry.restart,
            desired_state: entry.desired_state,
            readiness: entry.readiness,
        });
    }
    identities.sort();
    let digest = compute_core::sha256_identity(identities.join("\n").as_bytes());
    let name = manifest
        .project
        .name
        .or_else(|| {
            base.canonicalize().ok().and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
        })
        .ok_or_else(|| EnvironmentError::Invalid("the project needs a name".into()))?;
    Ok(ProjectDefinition {
        name,
        revision: manifest
            .project
            .revision
            .unwrap_or_else(|| format!("content-{}", &digest["sha256:".len()..][..12])),
        source: Some(base.display().to_string()),
        desired_state: DesiredState::Running,
        env: manifest.project.env,
        workloads,
    })
}
