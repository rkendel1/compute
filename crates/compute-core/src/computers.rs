//! Environments on a computer: the portable types of a durable computer that
//! Compute provisions on a target and keeps changing in place.
//!
//! An environment has always been a place projects run. When it asks for a
//! computer, Compute also provisions one on a target that satisfies its
//! requirements, keeps it (persistent) or lets it expire (ephemeral), and
//! reconciles what the environment says belongs in it — repositories,
//! packages, processes — by running ordinary durable jobs inside it. Nothing
//! is redeployed to change what runs there.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{IsolationProfile, NetworkPolicy, SessionCapabilities};

/// Whether a computer outlives the work that created it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputerLifecycle {
    /// Kept until it is destroyed.
    #[default]
    Persistent,
    /// Destroyed when its TTL passes.
    Ephemeral,
}

impl ComputerLifecycle {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Persistent => "persistent",
            Self::Ephemeral => "ephemeral",
        }
    }
}

/// The durable lifecycle of an environment's computer, as the controller
/// last observed it. `destroyed`, `expired`, and `failed` are terminal: a
/// terminal computer never changes status again, and its record stays as
/// evidence.
///
/// `unreachable` and `lost` are observed reality, not decisions: the
/// environment still wants its computer. An unreachable computer's target
/// did not answer (or refused this control plane); it returns to `running`
/// when the target answers with the same machine. A lost computer's target
/// answered and no longer has the machine; it stays lost until it is
/// replaced or destroyed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputerStatus {
    Pending,
    Provisioning,
    Running,
    Stopping,
    Stopped,
    Resuming,
    Failed,
    Destroying,
    Destroyed,
    Expired,
    Unreachable,
    Lost,
}

impl ComputerStatus {
    pub const ALL: [Self; 12] = [
        Self::Pending,
        Self::Provisioning,
        Self::Running,
        Self::Stopping,
        Self::Stopped,
        Self::Resuming,
        Self::Failed,
        Self::Destroying,
        Self::Destroyed,
        Self::Expired,
        Self::Unreachable,
        Self::Lost,
    ];

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Destroyed | Self::Expired | Self::Failed)
    }

    /// What an operator sees: the one vocabulary every surface (API, CLI,
    /// UI) uses for observed reality. `starting` covers placement,
    /// provisioning, and resuming; `stopping` covers destroying.
    pub const fn observed(self) -> &'static str {
        match self {
            Self::Pending | Self::Provisioning | Self::Resuming => "starting",
            Self::Running => "running",
            Self::Unreachable => "unreachable",
            Self::Lost => "lost",
            Self::Stopping | Self::Destroying => "stopping",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
            Self::Destroyed => "destroyed",
            Self::Expired => "expired",
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Provisioning => "provisioning",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Resuming => "resuming",
            Self::Failed => "failed",
            Self::Destroying => "destroying",
            Self::Destroyed => "destroyed",
            Self::Expired => "expired",
            Self::Unreachable => "unreachable",
            Self::Lost => "lost",
        }
    }
}

impl std::fmt::Display for ComputerStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Features of a target's machine that are not resources or session
/// capabilities: what the hardware and host software offer.
pub const TARGET_FEATURES: [&str; 6] = [
    "containers",
    "kvm",
    "firecracker",
    "gpu",
    "virtualization",
    "runtime_store_visible",
];

/// Validate target feature names, so a typo is never read as "not required".
pub fn validate_target_features(features: &[String]) -> crate::Result<()> {
    for feature in features {
        if !TARGET_FEATURES.contains(&feature.as_str()) {
            return Err(crate::ComputeError::InvalidWorkload(format!(
                "unknown target feature `{feature}`: expected one of {}",
                TARGET_FEATURES.join(", ")
            )));
        }
    }
    Ok(())
}

/// What an environment's computer must be. Provider identity is not part
/// of it: placement finds a target that satisfies it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputerRequirements {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_bytes: Option<u64>,
    /// `x86_64`, `arm64`, ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub architecture: Option<String>,
    /// A computer is a machine to work on: it reaches the network unless
    /// asked not to.
    #[serde(default = "network_by_default")]
    pub network: NetworkPolicy,
    #[serde(default)]
    pub isolation: IsolationProfile,
    /// Session capabilities the computer must offer (`persistent_storage`,
    /// `public_endpoint`, `terminal`, ...).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    /// Target features the machine must have (`kvm`, `firecracker`, ...).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    /// Runtimes processes in this computer require its target to be able to
    /// resolve and execute. These supplement the shell runtime used to
    /// provision the session itself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runtimes: Vec<crate::ProviderRuntimeRequirement>,
}

impl Default for ComputerRequirements {
    fn default() -> Self {
        Self {
            cpu_count: None,
            memory_bytes: None,
            disk_bytes: None,
            architecture: None,
            network: NetworkPolicy::Network,
            isolation: Default::default(),
            capabilities: vec![],
            features: vec![],
            runtimes: vec![],
        }
    }
}

impl ComputerRequirements {
    pub fn validate(&self) -> crate::Result<()> {
        SessionCapabilities::validate_names(&self.capabilities)?;
        validate_target_features(&self.features)?;
        if self.runtimes.iter().any(|runtime| {
            runtime
                .version
                .as_deref()
                .is_some_and(|version| version.trim().is_empty())
        }) {
            return Err(crate::ComputeError::InvalidWorkload(
                "a computer runtime version cannot be empty".into(),
            ));
        }
        if self.cpu_count == Some(0) {
            return Err(crate::ComputeError::InvalidWorkload(
                "a computer needs at least one CPU".into(),
            ));
        }
        Ok(())
    }
}

/// The computer an environment asks for: desired state, written by
/// operators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputerSpec {
    pub lifecycle: ComputerLifecycle,
    pub requirements: ComputerRequirements,
    /// Constrain placement to this target. Placement chooses when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// An ephemeral computer's lifetime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    /// Increases when the requirements change. A computer running an older
    /// generation is replaced: an explicit lifecycle operation, never an
    /// ordinary change.
    #[serde(default = "first_generation")]
    pub generation: u64,
    /// Set when an operator asked for the computer to be destroyed. The
    /// environment's record stays as evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destroy_requested_at: Option<DateTime<Utc>>,
}

fn network_by_default() -> NetworkPolicy {
    NetworkPolicy::Network
}

fn first_generation() -> u64 {
    1
}

/// Whether a process should run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessDesired {
    #[default]
    Running,
    Stopped,
}

/// What a process is for. Descriptive: every kind is a supervised process
/// inside the computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessKind {
    #[default]
    Application,
    Service,
    Agent,
    Process,
}

impl ProcessKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Application => "application",
            Self::Service => "service",
            Self::Agent => "agent",
            Self::Process => "process",
        }
    }
}

/// A repository checked out in the computer at a revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositorySpec {
    pub name: String,
    pub url: String,
    /// A branch, tag, or commit.
    pub revision: String,
    /// Increases to fetch the revision again (a branch that moved), with
    /// nothing else changed.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub sync: u64,
}

/// A package installed by running a command once per change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageSpec {
    pub name: String,
    pub install: Vec<String>,
    /// Run inside this repository's checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
}

/// A long-running process in the computer: an application, a service such
/// as a database, or an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessSpec {
    pub name: String,
    #[serde(default)]
    pub kind: ProcessKind,
    /// Runtime the target must resolve and prepare before starting this
    /// process. Absent preserves the native/shell command behaviour: the
    /// command is resolved inside the computer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<crate::ProviderRuntimeRequirement>,
    pub command: Vec<String>,
    /// Run inside this repository's checkout, and restart when its
    /// checkout moves to another commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub desired: ProcessDesired,
    /// The port it listens on, published as one of the environment's
    /// endpoints. The process is told it in `$PORT`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Increases to restart it in place, with nothing else changed.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub restart: u64,
    /// The HTTP request that says it is ready to serve. Without one, a
    /// running process is running, never `ready`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness: Option<HttpReadiness>,
    /// Whether Compute restarts it when it exits or fails its readiness.
    #[serde(default, skip_serializing_if = "ProcessRestartPolicy::is_default")]
    pub restart_policy: ProcessRestartPolicy,
    /// How many automatic restarts in a row may fail to recover it before
    /// Compute stops trying (until it changes, or someone asks).
    #[serde(
        default = "default_max_restarts",
        skip_serializing_if = "is_default_max_restarts"
    )]
    pub max_restarts: u32,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// Restarts in a row that may fail before Compute stops restarting.
pub const DEFAULT_MAX_RESTARTS: u32 = 5;
/// The most a process may ask for.
pub const MAX_RESTARTS_LIMIT: u32 = 100;

fn default_max_restarts() -> u32 {
    DEFAULT_MAX_RESTARTS
}

fn is_default_max_restarts(value: &u32) -> bool {
    *value == DEFAULT_MAX_RESTARTS
}

/// When Compute restarts a desired-running process that stopped on its
/// own: it exited, its start failed, or it missed its readiness deadline.
/// A process stopped by its desired state is never restarted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessRestartPolicy {
    /// Never: it stays exited or failed until it changes or someone asks.
    Never,
    /// When it fails: a non-zero or unknown exit status, a failed start, or
    /// a missed readiness deadline. A clean exit (status 0) stays exited.
    OnFailure,
    /// Whenever it stops running.
    #[default]
    Always,
}

impl ProcessRestartPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::OnFailure => "on_failure",
            Self::Always => "always",
        }
    }

    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl std::str::FromStr for ProcessRestartPolicy {
    type Err = crate::ComputeError;

    fn from_str(value: &str) -> crate::Result<Self> {
        match value {
            "never" => Ok(Self::Never),
            "on_failure" | "on-failure" => Ok(Self::OnFailure),
            "always" => Ok(Self::Always),
            other => Err(crate::ComputeError::InvalidWorkload(format!(
                "restart policy {other:?} is not never, on_failure, or always"
            ))),
        }
    }
}

/// An HTTP request, made inside the computer to the process's own port,
/// whose answer says whether the process is ready to serve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpReadiness {
    /// The path requested (`/health`). Redirects are not followed.
    pub path: String,
    /// The port requested; the process's own port when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// The answers that mean ready: a status (`204`) or a class (`2xx`).
    #[serde(default = "default_expect")]
    pub expect: String,
    /// How long one request may take.
    #[serde(default = "default_request_timeout")]
    pub request_timeout_seconds: u64,
    /// How long it may be starting, or unready once ready, before that is a
    /// failure.
    #[serde(default = "default_readiness_deadline")]
    pub deadline_seconds: u64,
}

fn default_expect() -> String {
    "2xx".into()
}

fn default_request_timeout() -> u64 {
    2
}

fn default_readiness_deadline() -> u64 {
    60
}

impl HttpReadiness {
    /// A readiness check of `path` with every default.
    pub fn path(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            port: None,
            expect: default_expect(),
            request_timeout_seconds: default_request_timeout(),
            deadline_seconds: default_readiness_deadline(),
        }
    }

    /// Whether an HTTP status means ready.
    pub fn accepts(&self, status: u16) -> bool {
        match self.expect.as_bytes() {
            [class, b'x', b'x'] => status / 100 == u16::from(class - b'0'),
            _ => self.expect.parse::<u16>() == Ok(status),
        }
    }

    fn validate(&self, process: &str, port: Option<u16>) -> crate::Result<()> {
        let invalid = |message: String| {
            Err(crate::ComputeError::InvalidWorkload(format!(
                "process {process}: readiness {message}"
            )))
        };
        if !self.path.starts_with('/')
            || self.path.len() > 1024
            || self
                .path
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte == b' ')
        {
            return invalid(format!(
                "path {:?} must start with '/' and have no spaces or control characters",
                self.path
            ));
        }
        if self.port.or(port).is_none_or(|port| port == 0) {
            return invalid("needs a port: the process's, or its own".into());
        }
        let class = matches!(self.expect.as_bytes(), [b'1'..=b'5', b'x', b'x']);
        let status = self
            .expect
            .parse::<u16>()
            .is_ok_and(|status| (100..=599).contains(&status));
        if !class && !status {
            return invalid(format!(
                "expects {:?}: a status (200) or a class (2xx)",
                self.expect
            ));
        }
        if !(1..=60).contains(&self.request_timeout_seconds) {
            return invalid("request timeout must be 1 to 60 seconds".into());
        }
        if !(1..=3600).contains(&self.deadline_seconds)
            || self.deadline_seconds < self.request_timeout_seconds
        {
            return invalid(
                "deadline must be 1 to 3600 seconds, and no shorter than a request".into(),
            );
        }
        Ok(())
    }
}

/// A project: software in one of the environment's repositories, with the
/// commands that build, test, and operate it. All of them run inside the
/// computer, in the repository's checkout.
///
/// The build is desired state: it runs whenever the checkout moves to
/// another commit, the build command changes, or the configuration does,
/// before the processes that run from the repository restart. That is a
/// release: a new revision, reconciled in place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectSpec {
    pub name: String,
    pub repository: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub build: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub test: Vec<String>,
    /// Named commands run on request: `migrate`, `lint`, `seed`, ...
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub commands: BTreeMap<String, Vec<String>>,
    /// Named commands that must pass to publish a version (`lint`,
    /// `typecheck`, ...), after the build and the tests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<String>,
}

impl ProjectSpec {
    /// A command by name: `build`, `test`, or one of `commands`.
    pub fn command(&self, name: &str) -> Option<&Vec<String>> {
        match name {
            "build" => Some(&self.build).filter(|command| !command.is_empty()),
            "test" => Some(&self.test).filter(|command| !command.is_empty()),
            other => self.commands.get(other),
        }
    }
}

/// What is known *about* an environment's configuration, never its values
/// (those are `EnvironmentRecord.config`): which variables it has, how each is
/// treated, where each came from, and the generation the configuration last
/// changed at. Configuration is runtime input for the environment's processes;
/// it is not workspace state and never part of a checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigurationState {
    /// The environment's contents generation at the last change to any
    /// variable or its treatment. A process records the generation it started
    /// with, so evidence can say which configuration it used without holding it.
    pub generation: u64,
    pub variables: std::collections::BTreeMap<String, ConfigurationVariable>,
}

/// How one variable is treated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigurationVariable {
    /// A sensitive value is never returned by any surface: only that it is
    /// configured. Everything not known to be public is sensitive.
    pub sensitive: bool,
    /// Where it came from: `.env`, `.env.local`, `cli`, `api`, or `declared`
    /// (set with the environment, or before sources were recorded).
    pub source: String,
}

/// What an environment says belongs in its computer: desired state. Every
/// change is an ordinary authorized operation; the reconciler makes the
/// running computer match it, in place.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentContents {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repositories: Vec<RepositorySpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<PackageSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub processes: Vec<ProcessSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<ProjectSpec>,
    /// Increases with every change, so evidence can name the change it
    /// applied.
    #[serde(default)]
    pub generation: u64,
}

fn valid_name(kind: &str, name: &str) -> crate::Result<()> {
    if name.is_empty()
        || name.len() > 63
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        || name.starts_with('.')
    {
        return Err(crate::ComputeError::InvalidWorkload(format!(
            "{kind} name {name:?} must be 1-63 letters, digits, '.', '-', or '_', not starting with '.'"
        )));
    }
    Ok(())
}

fn valid_command(kind: &str, name: &str, command: &[String]) -> crate::Result<()> {
    if command.is_empty() || command[0].is_empty() || command.iter().any(|part| part.contains('\0'))
    {
        return Err(crate::ComputeError::InvalidWorkload(format!(
            "{kind} {name} needs a command without NUL"
        )));
    }
    Ok(())
}

impl EnvironmentContents {
    pub fn validate(&self) -> crate::Result<()> {
        let invalid = |message: String| Err(crate::ComputeError::InvalidWorkload(message));
        let mut seen = std::collections::BTreeSet::new();
        for repository in &self.repositories {
            valid_name("repository", &repository.name)?;
            if !seen.insert(("repository", repository.name.as_str())) {
                return invalid(format!("repository {} is listed twice", repository.name));
            }
            for (field, value) in [("url", &repository.url), ("revision", &repository.revision)] {
                if value.is_empty()
                    || value.starts_with('-')
                    || value.bytes().any(|byte| byte.is_ascii_control())
                {
                    return invalid(format!(
                        "repository {} has an invalid {field}",
                        repository.name
                    ));
                }
            }
        }
        let known = |repository: &Option<String>, owner: &str| match repository {
            Some(name) if !self.repositories.iter().any(|spec| &spec.name == name) => invalid(
                format!("{owner} names repository {name}, which the environment does not have"),
            ),
            _ => Ok(()),
        };
        for package in &self.packages {
            valid_name("package", &package.name)?;
            if !seen.insert(("package", package.name.as_str())) {
                return invalid(format!("package {} is listed twice", package.name));
            }
            valid_command("package", &package.name, &package.install)?;
            known(&package.repository, &format!("package {}", package.name))?;
        }
        for process in &self.processes {
            valid_name("process", &process.name)?;
            if !seen.insert(("process", process.name.as_str())) {
                return invalid(format!("process {} is listed twice", process.name));
            }
            valid_command("process", &process.name, &process.command)?;
            if let Some(runtime) = &process.runtime
                && runtime
                    .version
                    .as_deref()
                    .is_some_and(|version| version.trim().is_empty())
            {
                return invalid(format!(
                    "process {} has an empty runtime version",
                    process.name
                ));
            }
            known(&process.repository, &format!("process {}", process.name))?;
            if process.port == Some(0) {
                return invalid(format!("process {} has port 0", process.name));
            }
            if let Some(readiness) = &process.readiness {
                readiness.validate(&process.name, process.port)?;
            }
            if process.max_restarts > MAX_RESTARTS_LIMIT {
                return invalid(format!(
                    "process {} may restart at most {MAX_RESTARTS_LIMIT} times in a row",
                    process.name
                ));
            }
            if let Some(port) = process.port
                && let Some(other) = self
                    .processes
                    .iter()
                    .find(|other| other.name != process.name && other.port == Some(port))
            {
                return invalid(format!(
                    "processes {} and {} both listen on port {port}",
                    process.name, other.name
                ));
            }
            for (key, value) in &process.env {
                if key.is_empty() || key.contains('=') || key.contains('\0') || value.contains('\0')
                {
                    return invalid(format!(
                        "process {} has an invalid environment variable {key:?}",
                        process.name
                    ));
                }
            }
        }
        for project in &self.projects {
            valid_name("project", &project.name)?;
            if !seen.insert(("project", project.name.as_str())) {
                return invalid(format!("project {} is listed twice", project.name));
            }
            known(
                &Some(project.repository.clone()),
                &format!("project {}", project.name),
            )?;
            for (command, argv) in [("build", &project.build), ("test", &project.test)]
                .into_iter()
                .filter(|(_, argv)| !argv.is_empty())
                .chain(
                    project
                        .commands
                        .iter()
                        .map(|(name, argv)| (name.as_str(), argv)),
                )
            {
                valid_name("command", command)?;
                valid_command(
                    "project command",
                    &format!("{}/{command}", project.name),
                    argv,
                )?;
            }
            if let Some(check) = project
                .checks
                .iter()
                .find(|check| !project.commands.contains_key(*check))
            {
                return invalid(format!(
                    "project {}: check {check} is not one of its commands",
                    project.name
                ));
            }
            if project.commands.contains_key("build") || project.commands.contains_key("test") {
                return invalid(format!(
                    "project {}: `build` and `test` are fields, not named commands",
                    project.name
                ));
            }
        }
        Ok(())
    }
}

/// The digest of a desired item, so observed state can say which version of
/// it the computer holds.
pub fn fingerprint(value: &impl Serialize) -> String {
    crate::sha256_identity(&serde_json::to_vec(value).expect("desired items serialize"))
}

/// Evidence of one operation run inside the computer: the durable job that
/// did it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationEvidence {
    pub job_id: String,
    pub execution_id: String,
    /// `succeeded`, `failed`, ...
    pub outcome: String,
    pub at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedRepository {
    /// The revision it was asked for.
    pub revision: String,
    /// The commit that revision resolved to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    pub fingerprint: String,
    pub evidence: OperationEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedPackage {
    pub fingerprint: String,
    pub evidence: OperationEvidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    /// A start was decided and recorded; its job has not reported yet. A
    /// controller that restarts finds it here and runs the start, without
    /// deciding (or counting) it again.
    Starting,
    Running,
    Stopped,
    /// It was started and is no longer running.
    Exited,
    /// Its start failed, or it missed its readiness deadline (it may still
    /// be running, unready).
    Failed,
}

impl ProcessState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Exited => "exited",
            Self::Failed => "failed",
        }
    }
}

/// Whether a running process answers its readiness request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessState {
    /// Started, and not yet ready.
    Starting,
    Ready,
    /// It was ready, and no longer answers as ready.
    Unready,
}

impl ReadinessState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Unready => "unready",
        }
    }
}

/// A process's readiness as its last check inside the computer found it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedReadiness {
    pub state: ReadinessState,
    /// When it entered this state: the deadline counts from here.
    pub since: DateTime<Utc>,
    /// What the last request got: `HTTP 200`, `HTTP 503`, `no answer`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The probe job that made the last check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<OperationEvidence>,
}

/// Why a desired-running process stopped running as it should, and what
/// its restart policy decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessFailure {
    /// `exited`, `start_failed`, or `readiness_timeout`.
    pub reason: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// What the restart policy decided.
    pub decision: String,
    pub at: DateTime<Utc>,
    /// The job that established it.
    pub evidence: OperationEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedProcess {
    pub state: ProcessState,
    /// The fingerprint of what runs: the process spec and the commit of
    /// its repository.
    pub fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub evidence: OperationEvidence,
    /// Durable intent copied from the process specification for operator
    /// reality, including across controller restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_runtime: Option<crate::ProviderRuntimeRequirement>,
    /// The target's resolution used for this start. A distribution here is
    /// target-issued evidence; absence identifies an explicit host runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_runtime: Option<crate::RuntimeResolution>,
    /// When the running process was started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness: Option<ObservedReadiness>,
    /// Automatic restarts on this machine, ever.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub restarts: u64,
    /// Automatic restarts in a row that have not yet recovered it: the
    /// bound of `max_restarts`, and the backoff.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub attempts: u32,
    /// When the next automatic restart is due. None: none is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<ProcessFailure>,
    /// The environment's configuration generation this process started with:
    /// which configuration it ran under, without holding any of it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub config_generation: u64,
}

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

impl ObservedProcess {
    /// The process in one word, as every surface shows it: `starting`,
    /// `ready`, `unready`, `running` (no readiness to check), `stopped`,
    /// `exited`, or `failed`.
    pub fn status(&self) -> &'static str {
        match (self.state, &self.readiness) {
            (ProcessState::Running, Some(readiness)) => readiness.state.as_str(),
            (ProcessState::Failed, Some(readiness)) if self.pid.is_some() => {
                // Running, but it missed its readiness deadline.
                match readiness.state {
                    ReadinessState::Ready => "ready",
                    _ => "unready",
                }
            }
            (state, _) => state.as_str(),
        }
    }
}

/// What the computer holds, as Compute last observed it. Every entry names
/// the durable job that produced it.
/// A project's build as it last ran: at which commit, and its evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedBuild {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    pub fingerprint: String,
    pub evidence: OperationEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedContents {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub repositories: BTreeMap<String, ObservedRepository>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub packages: BTreeMap<String, ObservedPackage>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub processes: BTreeMap<String, ObservedProcess>,
    /// Each project's last build.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub builds: BTreeMap<String, ObservedBuild>,
    /// The contents generation the computer last fully matched.
    #[serde(default)]
    pub converged_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<DateTime<Utc>>,
}

/// Where a computer failure happened, and whose it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputerFailure {
    /// `placement`, `provisioning`, `reconciliation`, `stopping`,
    /// `resuming`, `teardown`, `replacement`.
    pub phase: String,
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub at: DateTime<Utc>,
}

/// A session this environment used before it was replaced, kept until its
/// target confirms teardown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetiredSession {
    pub target: String,
    pub session_id: String,
    pub spec_generation: u64,
}

/// One step of a durable operation (a publish, a deployment), as the
/// controller records it: what it is, how it stands, and the job that did it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationStep {
    pub name: String,
    pub status: StepStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
    /// The receipt (`compute.receipt@1`, its `receipt_hash`) the target
    /// issued for the job, once it is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<DateTime<Utc>>,
}

impl OperationStep {
    pub fn pending(name: &str) -> Self {
        Self {
            name: name.into(),
            status: StepStatus::Pending,
            detail: None,
            job_id: None,
            execution_id: None,
            receipt: None,
            at: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Skipped,
}

impl StepStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }

    pub const fn is_done(self) -> bool {
        matches!(self, Self::Succeeded | Self::Skipped)
    }
}

/// What a project needs in a computer, as a version records it: its
/// repository, how it is built and checked, and what runs from it. A version
/// deployed to an environment that lacks any of it brings it along.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectAssembly {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<RepositorySpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<PackageSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub processes: Vec<ProcessSpec>,
}

/// What Compute proposes after inspecting a project's source in a computer:
/// an assembly the user can accept, change, and submit with GO.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectProposal {
    pub name: String,
    /// `node`, `python`, `go`, `rust`, `make`, or none recognised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    pub assembly: ProjectAssembly,
    /// Services the project appears to need (a database, a cache), proposed
    /// but not required.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<ProcessSpec>,
    /// Configuration the project documents, with its documented defaults.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub config: BTreeMap<String, String>,
    /// What Compute found, and what it could not decide.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// The files it looked at.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(name: &str, repository: Option<&str>) -> ProcessSpec {
        ProcessSpec {
            name: name.into(),
            kind: ProcessKind::Application,
            runtime: None,
            command: vec!["./run".into()],
            repository: repository.map(Into::into),
            env: BTreeMap::new(),
            desired: ProcessDesired::Running,
            port: None,
            restart: 0,
            readiness: None,
            restart_policy: Default::default(),
            max_restarts: DEFAULT_MAX_RESTARTS,
        }
    }

    #[test]
    fn contents_are_validated() {
        let mut contents = EnvironmentContents {
            repositories: vec![RepositorySpec {
                name: "app".into(),
                url: "https://example.invalid/app.git".into(),
                revision: "main".into(),
                sync: 0,
            }],
            processes: vec![process("api", Some("app"))],
            ..Default::default()
        };
        contents.validate().unwrap();
        contents.processes.push(process("api", None));
        assert!(contents.validate().is_err(), "duplicate process");
        contents.processes.pop();
        contents.processes.push(process("worker", Some("missing")));
        assert!(contents.validate().is_err(), "unknown repository");
        contents.processes.pop();
        contents.repositories[0].revision = "--upload-pack=evil".into();
        assert!(contents.validate().is_err(), "option injection");
        contents.repositories[0].revision = "main".into();
        contents.processes[0].name = "../x".into();
        assert!(contents.validate().is_err(), "path in a name");
    }

    #[test]
    fn process_runtime_is_optional_persisted_intent() {
        let legacy = process("api", None);
        let legacy_json = serde_json::to_value(&legacy).unwrap();
        assert!(legacy_json.get("runtime").is_none());
        assert_eq!(
            serde_json::from_value::<ProcessSpec>(legacy_json)
                .unwrap()
                .runtime,
            None
        );

        let mut pinned = process("api", None);
        pinned.runtime = Some(crate::ProviderRuntimeRequirement {
            runtime: crate::RuntimeKind::Node,
            version: Some("22.12".into()),
            platform: Some(crate::PlatformIdentity {
                os: "linux".into(),
                architecture: "x86_64".into(),
                runtime_abi: None,
            }),
        });
        let encoded = serde_json::to_vec(&pinned).unwrap();
        assert_eq!(
            serde_json::from_slice::<ProcessSpec>(&encoded).unwrap(),
            pinned
        );

        pinned.runtime.as_mut().unwrap().version = Some(" ".into());
        let contents = EnvironmentContents {
            processes: vec![pinned],
            ..Default::default()
        };
        assert!(contents.validate().is_err(), "empty runtime version");
    }

    #[test]
    fn requirements_reject_unknown_names() {
        let mut requirements = ComputerRequirements {
            features: vec!["kvm".into()],
            capabilities: vec!["persistent_storage".into()],
            ..Default::default()
        };
        requirements.validate().unwrap();
        requirements.features.push("quantum".into());
        assert!(requirements.validate().is_err());
    }

    #[test]
    fn projects_and_ports_are_validated() {
        let project = |name: &str, repository: &str| ProjectSpec {
            name: name.into(),
            repository: repository.into(),
            build: vec!["make".into()],
            test: vec![],
            commands: BTreeMap::from([("migrate".into(), vec!["./migrate".into()])]),
            checks: vec![],
        };
        let mut contents = EnvironmentContents {
            repositories: vec![RepositorySpec {
                name: "app".into(),
                url: "https://example.invalid/app.git".into(),
                revision: "main".into(),
                sync: 0,
            }],
            projects: vec![project("app", "app")],
            processes: vec![process("api", Some("app"))],
            ..Default::default()
        };
        contents.validate().unwrap();
        assert_eq!(contents.projects[0].command("build").unwrap()[0], "make");
        assert!(contents.projects[0].command("test").is_none());
        assert!(contents.projects[0].command("migrate").is_some());
        contents.projects.push(project("web", "missing"));
        assert!(contents.validate().is_err(), "unknown repository");
        contents.projects.pop();
        contents.projects[0]
            .commands
            .insert("build".into(), vec!["x".into()]);
        assert!(contents.validate().is_err(), "build is a field");
        contents.projects[0].commands.remove("build");
        contents.processes[0].port = Some(8080);
        let mut other = process("worker", None);
        other.port = Some(8080);
        contents.processes.push(other);
        assert!(contents.validate().is_err(), "one port, one process");
        contents.processes[1].port = Some(0);
        assert!(contents.validate().is_err(), "port 0");
    }

    #[test]
    fn terminal_statuses_are_final() {
        for status in ComputerStatus::ALL {
            assert_eq!(
                status.is_terminal(),
                matches!(
                    status,
                    ComputerStatus::Destroyed | ComputerStatus::Expired | ComputerStatus::Failed
                )
            );
        }
    }
}
