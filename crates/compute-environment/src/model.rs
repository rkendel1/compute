//! The `compute.environment@1` model.
//!
//! Project = software. Environment = a deployed instance of software.
//! Workload = a service or task within a project in an environment.
//! Execution = one invocation of a workload.
//!
//! Durable records live in `compute-state`; this module defines what
//! clients submit and the live states the daemon observes.

use std::collections::{BTreeMap, BTreeSet};

use compute_policy::Policy;
use serde::{Deserialize, Serialize};

pub use compute_state::{
    DeploymentStatus, DesiredState, InstanceState, PortBinding, PortSpec, Readiness,
    ReadinessCheck, RestartPolicy, WorkloadKind,
};

use crate::EnvironmentError;
use crate::status::ComputerView;

pub const ENVIRONMENT_VERSION: &str = "compute.environment@1";

/// What Compute observes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActualState {
    /// Not started yet.
    Pending,
    Starting,
    Running,
    Stopping,
    Stopped,
    /// A task finished successfully.
    Completed,
    /// The workload exited unsuccessfully; siblings are unaffected.
    Failed,
    /// Admission denied execution; nothing ran.
    Denied,
    /// Some children run and some do not.
    Degraded,
}

impl ActualState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Denied => "denied",
            Self::Degraded => "degraded",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadDefinition {
    pub name: String,
    pub kind: WorkloadKind,
    /// The canonical `.compute` bundle.
    #[serde(with = "compute_core::bytes_json")]
    pub bundle: Vec<u8>,
    #[serde(default)]
    pub ports: Vec<PortSpec>,
    #[serde(default)]
    pub restart: RestartPolicy,
    /// For services: whether this service should run while its project
    /// runs. Tasks run only when invoked.
    #[serde(default)]
    pub desired_state: DesiredState,
    /// For services: what proves a new instance is ready for traffic.
    /// Defaults to its first port accepting connections, or, without
    /// ports, to its process staying up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness: Option<Readiness>,
}

/// Immutable project content: a revision label and its workloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevisionDefinition {
    /// Operator-supplied label, such as a commit. A label always names the
    /// same content.
    pub revision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub workloads: Vec<WorkloadDefinition>,
}

/// A revision plus how to run it in one environment: the shape
/// `compute project add` and manifests submit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectDefinition {
    pub name: String,
    /// Operator-supplied revision label, such as a commit.
    pub revision: String,
    /// Where the project came from; informational.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default)]
    pub desired_state: DesiredState,
    /// Project configuration for this environment.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub workloads: Vec<WorkloadDefinition>,
}

impl ProjectDefinition {
    pub fn revision_definition(&self) -> RevisionDefinition {
        RevisionDefinition {
            revision: self.revision.clone(),
            source: self.source.clone(),
            workloads: self.workloads.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentDefinition {
    pub name: String,
    #[serde(default)]
    pub desired_state: DesiredState,
    /// Environment configuration, visible to every project workload.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Execution policy for every workload in this environment. It is
    /// intersected with the daemon's policy by the existing admission
    /// boundary; it can only restrict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<Policy>,
    /// Pin workloads to one provider of the daemon's pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// The computer an environment asks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputerRequest {
    #[serde(default)]
    pub lifecycle: compute_core::ComputerLifecycle,
    #[serde(default)]
    pub requirements: compute_core::ComputerRequirements,
    /// Constrain placement to one target. Placement chooses when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// An ephemeral computer's lifetime. Defaults to an hour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
}

/// Create an environment on a computer: an environment, the computer it
/// asks for, and what belongs in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputerEnvironmentDefinition {
    pub name: String,
    #[serde(default)]
    pub desired_state: DesiredState,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<Policy>,
    pub computer: ComputerRequest,
    #[serde(default)]
    pub contents: compute_core::EnvironmentContents,
}

/// Fork an environment: a new environment, on a new computer, from the
/// source's portable state: its workspace files, declared contents, and
/// policy. Never its configuration values, sessions, or machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkRequest {
    /// The new environment's name.
    pub name: String,
    /// Constrain placement of the new computer; placement chooses when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Copy the source's configuration *values*. Off by default, because
    /// configuration is where credentials live; the names left behind are
    /// reported.
    #[serde(default)]
    pub copy_config: bool,
}

/// What a fork did and what it verified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkReport {
    pub source: String,
    pub environment: String,
    /// The workspace digest (`compute.workspace@1`) carried and verified.
    pub workspace: String,
    pub archive: String,
    pub files: usize,
    pub directories: usize,
    pub bytes: u64,
    pub workspace_verified: bool,
    /// The declared repositories: `(source commit, fork commit)`.
    pub repositories: BTreeMap<String, (Option<String>, Option<String>)>,
    /// Configuration names not copied.
    pub omitted_config: Vec<String>,
    /// The durable jobs that did the work, in order.
    pub jobs: Vec<String>,
    /// The fork's computer once its contents converged.
    pub computer: ComputerView,
}

/// A computer's workspace, exported: the archive and its identity
/// (`compute.workspace@1`, see `docs/workspace.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceExport {
    pub identity: String,
    pub digest: String,
    pub archive_digest: String,
    pub files: usize,
    /// Empty directories, the only ones the identity records.
    pub directories: usize,
    pub bytes: u64,
    /// Where it was read: `Linux-x86_64`. Provenance for a reader of the
    /// archive, never part of its identity.
    #[serde(default)]
    pub platform: String,
    pub job_id: String,
    #[serde(with = "compute_core::bytes_json")]
    pub archive: Vec<u8>,
}

/// Seed an empty workspace from an archive. If `digest` is given the archive
/// must hold exactly that workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSeedRequest {
    #[serde(with = "compute_core::bytes_json")]
    pub archive: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSeed {
    pub digest: String,
    pub files: usize,
    pub directories: usize,
    /// The digest recomputed inside the computer matched.
    pub verified: bool,
    pub jobs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceVerifyRequest {
    /// The digest the workspace should have. Absent: only measure it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

/// A workspace measured, and compared when a digest was expected. A mismatch
/// is a result (`verified: false`), not an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceVerification {
    /// What the computer's workspace is.
    pub digest: String,
    pub expected: Option<String>,
    /// An expected digest was given and equals `digest`.
    pub verified: bool,
    pub job_id: String,
}

/// Replace an environment's desired contents, optionally only if they are
/// still at the generation the caller last saw (what the UI's GO sends).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentsUpdate {
    pub contents: compute_core::EnvironmentContents,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_generation: Option<u64>,
    /// Replace the environment's configuration (what every process, build,
    /// and command sees) in the same change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<BTreeMap<String, String>>,
    /// Change how long the environment lives, in the same change. In place:
    /// never a replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<LifecycleChange>,
}

/// How long an environment lives: kept until destroyed, or temporary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleChange {
    pub lifecycle: compute_core::ComputerLifecycle,
    /// A temporary environment's lifetime from now. Defaults to an hour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
}

/// Run one of a project's commands inside the environment's computer:
/// `build`, `test`, or a named command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectCommandRequest {
    pub project: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

/// Release a revision of a project: a change to desired state, reconciled
/// in place by the environment's computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseRequest {
    pub project: String,
    pub revision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_generation: Option<u64>,
}

/// Open a work session: enter an existing environment, or have a temporary
/// one made for the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct OpenSessionRequest {
    /// Enter this environment. Its lifecycle is untouched by the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// Without `environment`: the temporary computer to make. Always
    /// ephemeral; the session owns it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computer: Option<ComputerRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contents: Option<compute_core::EnvironmentContents>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

/// Deploy a registered revision of a project to an environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DeployRequest {
    pub project: String,
    pub environment: String,
    /// A revision label or `rev_` ID. Defaults to the latest revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// Replace the project's configuration in this environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<BTreeMap<String, String>>,
    /// The project's desired state after deploying. Defaults to its
    /// current desired state, or running for a new membership.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_state: Option<DesiredState>,
    /// The caller's pool placement that chose this node, recorded as
    /// evidence with the release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<compute_state::PoolPlacement>,
    /// The application artifact the release comes from, recorded as
    /// evidence with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<compute_state::ApplicationArtifactEvidence>,
    /// Configuration the release needs. It is refused, before anything is
    /// recorded, when the resolved configuration lacks one of these names.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub required_config: BTreeSet<String>,
}

/// Deploy the exact revision current in one environment to another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromoteRequest {
    pub project: String,
    pub from: String,
    pub to: String,
    /// Promote even when the source deployment is not healthy.
    #[serde(default)]
    pub allow_unhealthy: bool,
    /// The project's configuration in the target environment. Promotion
    /// copies the revision, never the source environment's configuration.
    /// Defaults to the target's existing configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<BTreeMap<String, String>>,
}

/// Register a shared service other projects can consume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceDefinition {
    pub name: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default = "local_provider")]
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

fn local_provider() -> String {
    "local".into()
}

/// Names are ordinary identifiers: `preprod`, `prod`, `review-123`,
/// `customer-acme`. Nothing is reserved.
/// A domain routed to one workload port of one project in one
/// environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomainDefinition {
    pub name: String,
    pub environment: String,
    pub project: String,
    /// The service; optional when the project has exactly one with ports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload: Option<String>,
    /// The port name; defaults to the service's first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
    /// The configured DNS provider; defaults to the one whose zone holds
    /// the domain. `none` leaves DNS to you.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns_provider: Option<String>,
    /// Whether to issue a certificate; defaults to whether ACME is
    /// configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<bool>,
}

pub fn validate_name(kind: &str, name: &str) -> Result<(), EnvironmentError> {
    let valid = !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(EnvironmentError::Invalid(format!(
            "invalid {kind} name {name:?}: use 1-63 lowercase letters, digits, or '-', starting with a letter or digit"
        )))
    }
}

/// Configuration keys: environment variable names that Compute does not
/// own.
pub fn validate_env(scope: &str, env: &BTreeMap<String, String>) -> Result<(), EnvironmentError> {
    for (key, value) in env {
        if key.is_empty()
            || key.contains('=')
            || key.contains('\0')
            || value.contains('\0')
            || key.starts_with("COMPUTE_")
            || key == "PORT"
        {
            return Err(EnvironmentError::Invalid(format!(
                "{scope} configuration key {key:?} is invalid or reserved by Compute"
            )));
        }
    }
    Ok(())
}

pub fn validate_revision_label(revision: &str) -> Result<(), EnvironmentError> {
    if revision.is_empty()
        || revision.len() > 128
        || !revision
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:@".contains(&byte))
    {
        return Err(EnvironmentError::Invalid(
            "revision must be 1-128 letters, digits, '-', '_', '.', ':', or '@'".into(),
        ));
    }
    Ok(())
}

/// Deploy a new version of an application on this node: `POST
/// /applications/{name}/deployments`. The daemon owns everything after
/// this: the revision, the release, the stable endpoint, and the evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationDeployRequest {
    /// The application as a portable artifact
    /// (`compute.application-artifact@1`), inline or by reference. Its
    /// manifest supplies the port and the environment contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<ApplicationArtifactSource>,
    /// Or: the canonical `.compute` bundle of the application, with `port`.
    #[serde(
        default,
        with = "compute_core::bytes_json",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub bundle: Vec<u8>,
    /// The port the application listens on (it is also given `PORT`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// The application's configuration (its environment), replacing the
    /// current one. Omitted, the current configuration is kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    /// Where the source came from, for people reading history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The caller's pool placement that chose this node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<compute_state::PoolPlacement>,
}

/// Where a provider gets an application artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ApplicationArtifactSource {
    /// The artifact's bytes, sent with the request.
    Inline {
        #[serde(with = "compute_core::bytes_json")]
        data: Vec<u8>,
    },
    /// A `file://` path on the provider or an `http(s)://` URL, and the
    /// digest the fetched bytes must have.
    Reference(compute_core::ArtifactReference),
}

/// Deploy an earlier version again, as the next version: `POST
/// /applications/{name}/rollback`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationRollbackRequest {
    /// A version (`3`, `v3`) or a deployment ID (`dep_…`).
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<compute_state::PoolPlacement>,
}

/// Inspect a project's source in an environment's computer and propose
/// how to run it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposeRequest {
    /// A Git URL, or a folder the computer can read that is a Git
    /// repository.
    pub url: String,
    /// A branch, tag, or commit. Defaults to the repository's default
    /// branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// The project's name. Defaults to the repository's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Provider-neutral, immutable Git source produced by a capability provider.
/// Compute deliberately owns this shape rather than importing a GitHub,
/// GitLab, or other provider package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitRepositorySource {
    pub source: GitSourceProtocol,
    pub url: String,
    pub owner: String,
    pub repository: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub commit: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitSourceProtocol {
    Git,
}

impl GitRepositorySource {
    /// Convert a provider result into Compute's existing generic repository
    /// proposal request. The immutable commit wins over a movable branch.
    pub fn propose(self) -> Result<ProposeRequest, String> {
        if self.url.trim().is_empty() || self.repository.trim().is_empty() {
            return Err("Git source URL and repository must be non-empty".into());
        }
        if !matches!(self.commit.len(), 40 | 64)
            || !self.commit.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("Git source commit must be a full hexadecimal object ID".into());
        }
        Ok(ProposeRequest {
            url: self.url,
            revision: Some(self.commit),
            name: Some(self.repository),
        })
    }
}

#[cfg(test)]
mod git_source_tests {
    use super::*;

    #[test]
    fn a_provider_neutral_git_source_becomes_an_immutable_compute_source() {
        let source: GitRepositorySource = serde_json::from_value(serde_json::json!({
            "source": "git",
            "url": "https://example.test/owner/project.git",
            "owner": "owner",
            "repository": "project",
            "ref": "main",
            "commit": "0123456789abcdef0123456789abcdef01234567"
        }))
        .unwrap();
        let request = source.propose().unwrap();
        assert_eq!(request.url, "https://example.test/owner/project.git");
        assert_eq!(request.name.as_deref(), Some("project"));
        assert_eq!(
            request.revision.as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
    }
}

/// Publish a version of a project from the environment it is developed in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishRequest {
    pub environment: String,
    /// The label. Defaults to the next patch version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Deploy a published version to an environment: a change to its desired
/// state, reconciled in place by its computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployVersionRequest {
    pub environment: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_generation: Option<u64>,
}

/// Promote the version running in one environment to another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromoteVersionRequest {
    pub from: String,
    pub to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_generation: Option<u64>,
}

/// Roll an environment back to an earlier version (the one before the
/// current, unless named).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollbackRequest {
    pub environment: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Capture a checkpoint of an environment's workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointRequest {
    /// The checkpoint this one is derived from, when it is: recorded as
    /// lineage. It must be a checkpoint of the same environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

/// What a capture did and what it verified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointReport {
    pub environment: String,
    pub checkpoint_id: String,
    /// The format of the artifact: `compute.checkpoint@1`.
    pub format: String,
    /// The workspace digest (`compute.workspace@1`) the artifact reproduces.
    pub workspace: String,
    /// The artifact's digest in the artifact store.
    pub artifact: String,
    pub size: u64,
    pub files: usize,
    pub directories: usize,
    pub computer_generation: u64,
    pub contents_generation: u64,
    pub platform: String,
    pub parent: Option<String>,
    /// The state was already captured: this is the existing checkpoint,
    /// unchanged. A checkpoint is named by its content.
    pub existing: bool,
    /// The artifact was read back from the store and validated.
    pub verified: bool,
    /// The durable job that read the workspace; its receipt is the evidence.
    pub job_id: String,
}

/// A checkpoint record and, when asked for, whether its artifact still
/// validates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointView {
    #[serde(flatten)]
    pub checkpoint: compute_state::CheckpointRecord,
    /// `Some` only where the artifact was read and validated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid_reason: Option<String>,
}

/// Restore a checkpoint into a new environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreRequest {
    /// The new environment's name.
    pub name: String,
    /// Constrain placement of the new computer; placement chooses when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// What a restore consumed, created, and verified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreReport {
    /// The checkpoint consumed: input state, never an authority.
    pub checkpoint_id: String,
    pub artifact: String,
    /// The environment the checkpoint was captured from, and whose declared
    /// state the new environment inherited.
    pub source: String,
    pub environment: String,
    pub environment_id: String,
    pub computer_id: String,
    /// The workspace digest (`compute.workspace@1`) restored and verified.
    pub workspace: String,
    pub files: usize,
    pub directories: usize,
    pub bytes: u64,
    /// The seeded workspace was measured inside the new computer and matches.
    pub workspace_verified: bool,
    /// The declared contents generation at capture, and the one applied.
    pub captured_contents_generation: u64,
    pub applied_contents_generation: u64,
    /// `matches the state at capture` or `changed since capture`.
    pub declared_state: String,
    /// Configuration names not restored: values are never restored.
    pub omitted_config: Vec<String>,
    /// The variables the checkpointed environment had configured: names and
    /// treatment from the checkpoint, never values. They are for the caller to
    /// supply (`compute environment config`); restore does not.
    #[serde(default)]
    pub configuration_required: Vec<crate::configuration::ConfigurationRequirement>,
    /// The durable jobs that did the work, in order.
    pub jobs: Vec<String>,
    /// The new computer once its declared contents converged.
    pub computer: ComputerView,
}
