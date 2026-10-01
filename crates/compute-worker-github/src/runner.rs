//! Run one ephemeral GitHub Actions runner job as a Compute execution.
//!
//! The adapter is deliberately thin. Compute's existing execution layer owns
//! the lifecycle: it stages a private workspace, runs the worker script in its
//! own process group, enforces the timeout, honours cancellation, kills the
//! whole group, removes the workspace, and seals a receipt. The adapter only
//! obtains the registration token, describes the workload, and reads back the
//! evidence.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use compute_core::{
    EnvironmentVariable, ExecutionControl, ExecutionRequest, ExecutionResult, ExecutionStatus,
    IsolationProfile, NetworkPolicy, ResourceLimits, RuntimeKind, RuntimeSpec, WorkloadOutput,
};
use compute_runtime::Compute;
use serde::{Deserialize, Serialize};

use crate::error::WorkerError;
use crate::github::{GitHubApi, Repository};
use crate::secret::{Secret, scrub};

/// The recipe that describes the environment a runner needs. It names no
/// repository and holds no credential.
pub const RECIPE_NAME: &str = "github-actions-runner";

/// The environment variable the runner's configuration step reads its
/// registration token from. This is the only place the token is placed.
pub const REGISTRATION_TOKEN_ENV: &str = "ACTIONS_RUNNER_INPUT_TOKEN";

/// The environment variable the CLI reads the GitHub credential from unless
/// told otherwise.
pub const DEFAULT_CREDENTIAL_ENV: &str = "GITHUB_TOKEN";

/// The execution output the worker script writes.
pub const METADATA_OUTPUT: &str = "runner-metadata.json";

pub const DEFAULT_SERVER_URL: &str = "https://github.com";
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);

const SCRIPT: &str = include_str!("runner.sh");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunnerOs {
    Linux,
    Macos,
}

impl RunnerOs {
    /// The name in the runner's release asset (`actions-runner-{os}-…`).
    pub const fn asset(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Macos => "osx",
        }
    }

    pub fn host() -> Option<Self> {
        match std::env::consts::OS {
            "linux" => Some(Self::Linux),
            "macos" => Some(Self::Macos),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunnerArch {
    X64,
    Arm64,
}

impl RunnerArch {
    pub const fn asset(self) -> &'static str {
        match self {
            Self::X64 => "x64",
            Self::Arm64 => "arm64",
        }
    }

    pub fn host() -> Option<Self> {
        match std::env::consts::ARCH {
            "x86_64" => Some(Self::X64),
            "aarch64" => Some(Self::Arm64),
            _ => None,
        }
    }
}

/// A recipe the caller declared: its name and the digest of the spec file
/// (`compute_core::recipe_digest`). Provenance for a reader, not a check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipeEvidence {
    pub name: String,
    pub digest: String,
}

/// What to run. Everything here is workload input: nothing about a
/// repository or a credential belongs in a recipe.
#[derive(Debug, Clone)]
pub struct RunnerSpec {
    pub repository: Repository,
    /// The runner release to use, e.g. `2.331.0`. Pinned, never "latest": a
    /// receipt must say exactly what ran.
    pub runner_version: String,
    /// SHA-256 of the release archive. Required: an archive is never run
    /// unverified.
    pub runner_sha256: String,
    pub os: RunnerOs,
    pub arch: RunnerArch,
    pub labels: Vec<String>,
    /// The runner's name on GitHub. Generated when absent.
    pub name: Option<String>,
    /// Where to fetch the archive. Absent: the release URL for
    /// `runner_version`. HTTPS only (redirects too); a mirror is allowed and
    /// the checksum still decides what runs.
    pub download_url: Option<String>,
    /// An archive already on this host, used instead of downloading. It is
    /// verified against the checksum exactly like a download.
    pub archive_file: Option<PathBuf>,
    /// The GitHub server the runner registers with.
    pub server_url: String,
    pub timeout: Duration,
    /// Run the runner's `installdependencies.sh` (needs privileges).
    pub install_dependencies: bool,
    /// The recipe the caller says this run is for. It is recorded, not
    /// evaluated: nothing checks this host against its requirements.
    pub declared_recipe: Option<RecipeEvidence>,
}

impl RunnerSpec {
    pub fn new(
        repository: Repository,
        runner_version: impl Into<String>,
        runner_sha256: impl Into<String>,
    ) -> Result<Self, WorkerError> {
        let os = RunnerOs::host().ok_or_else(|| {
            WorkerError::InvalidSpec(format!("no runner for host OS {}", std::env::consts::OS))
        })?;
        let arch = RunnerArch::host().ok_or_else(|| {
            WorkerError::InvalidSpec(format!(
                "no runner for host architecture {}",
                std::env::consts::ARCH
            ))
        })?;
        Ok(Self {
            repository,
            runner_version: runner_version.into(),
            runner_sha256: runner_sha256.into(),
            os,
            arch,
            labels: vec![],
            name: None,
            download_url: None,
            archive_file: None,
            server_url: DEFAULT_SERVER_URL.into(),
            timeout: DEFAULT_TIMEOUT,
            install_dependencies: false,
            declared_recipe: None,
        })
    }

    pub fn download_url(&self) -> String {
        self.download_url.clone().unwrap_or_else(|| {
            format!(
                "https://github.com/actions/runner/releases/download/v{v}/actions-runner-{os}-{arch}-{v}.tar.gz",
                v = self.runner_version,
                os = self.os.asset(),
                arch = self.arch.asset(),
            )
        })
    }

    /// Every value reaches a shell environment, so each is restricted to a
    /// character set that needs no quoting.
    pub fn validate(&self) -> Result<(), WorkerError> {
        let invalid = |message: &str| Err(WorkerError::InvalidSpec(message.into()));
        let version_ok = !self.runner_version.is_empty()
            && self
                .runner_version
                .chars()
                .all(|c| c.is_ascii_digit() || c == '.')
            && !self.runner_version.starts_with('.')
            && !self.runner_version.ends_with('.');
        if !version_ok {
            return invalid("the runner version must look like 2.331.0");
        }
        if self.runner_sha256.len() != 64
            || !self.runner_sha256.chars().all(|c| c.is_ascii_hexdigit())
        {
            return invalid("the runner checksum must be a 64-digit hex SHA-256");
        }
        let safe = |value: &str| {
            !value.is_empty()
                && value.len() <= 64
                && value
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        if self.labels.iter().any(|label| !safe(label)) {
            return invalid("a label may only contain letters, digits, `-`, `_`, and `.`");
        }
        if self.name.as_deref().is_some_and(|name| !safe(name)) {
            return invalid("the runner name may only contain letters, digits, `-`, `_`, and `.`");
        }
        if !self.server_url.starts_with("https://")
            || self
                .server_url
                .chars()
                .any(|c| !(c.is_ascii_alphanumeric() || matches!(c, ':' | '/' | '.' | '-' | '_')))
        {
            return invalid("the server URL must be an https:// URL");
        }
        if let Some(url) = &self.download_url
            && (!url.starts_with("https://")
                || url.chars().any(|c| c.is_whitespace() || c.is_control()))
        {
            return invalid("the download URL must be an https:// URL");
        }
        if let Some(path) = &self.archive_file
            && (!path.is_absolute() || !path.is_file())
        {
            return invalid("the archive file must be an absolute path to an existing file");
        }
        if self.download_url.is_some() && self.archive_file.is_some() {
            return invalid("give a download URL or an archive file, not both");
        }
        if self.timeout.is_zero() {
            return invalid("the timeout must be greater than zero");
        }
        Ok(())
    }
}

/// What the worker script reports about the run.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ScriptMetadata {
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
    stage: String,
    work_dir: String,
    job_name: String,
    job_result: String,
    host: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerIdentity {
    pub version: String,
    pub os: RunnerOs,
    pub arch: RunnerArch,
    pub name: String,
    pub ephemeral: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobEvidence {
    /// What the runner logged. Best-effort: absent when the runner never took
    /// a job (a registration failure, a timeout while idle).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupEvidence {
    /// Whether the execution's private workspace is gone. `None` when the
    /// script never reported one (it did not start).
    pub workspace_removed: Option<bool>,
}

/// The adapter's evidence for one runner job. It sits beside Compute's own
/// receipt (the execution receipt, which this references by hash and which
/// seals the metadata file as an output); it never replaces it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerReport {
    pub worker: String,
    pub repository: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declared_recipe: Option<RecipeEvidence>,
    pub runner: RunnerIdentity,
    /// The execution environment: the runtime and the host that ran it.
    pub environment: String,
    pub execution_id: String,
    pub receipt_hash: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub status: ExecutionStatus,
    pub exit_code: Option<i32>,
    /// The worker script's last stage; says where a failure happened.
    pub stage: String,
    pub job: JobEvidence,
    pub cleanup: CleanupEvidence,
}

pub struct RunnerRun {
    pub report: RunnerReport,
    /// The execution result, with credentials scrubbed from its text. Its
    /// `receipt` is Compute's execution receipt.
    pub result: ExecutionResult,
}

pub struct RunnerWorker<A> {
    api: A,
    compute: Compute,
}

impl<A: GitHubApi> RunnerWorker<A> {
    pub fn new(api: A, compute: Compute) -> Self {
        Self { api, compute }
    }

    /// Provision (stage) → register → run one job → collect → clean up.
    /// Cleanup is the runtime's: it happens on success, command failure,
    /// timeout, and cancellation alike.
    pub async fn run(
        &self,
        spec: &RunnerSpec,
        credential: &Secret,
        control: &ExecutionControl,
    ) -> Result<RunnerRun, WorkerError> {
        spec.validate()?;
        if credential.is_empty() {
            return Err(WorkerError::MissingCredential(
                DEFAULT_CREDENTIAL_ENV.into(),
            ));
        }
        let token = self
            .api
            .registration_token(&spec.repository, credential)
            .await?;
        let secrets = [credential, &token];

        let name = spec
            .name
            .clone()
            .unwrap_or_else(|| generated_name(&spec.repository));
        let script_dir = tempfile::tempdir()
            .map_err(|error| WorkerError::Execution(scrub(&error.to_string(), &secrets)))?;
        let script = script_dir.path().join("runner.sh");
        std::fs::write(&script, SCRIPT)
            .map_err(|error| WorkerError::Execution(scrub(&error.to_string(), &secrets)))?;

        let request = request(spec, &name, script, &token);
        let mut result = self
            .compute
            .run_controlled(request, control)
            .await
            .map_err(|error| WorkerError::Execution(scrub(&error.to_string(), &secrets)))?;
        drop(script_dir);
        scrub_result(&mut result, &secrets);

        let metadata = result
            .outputs
            .iter()
            .find(|output| output.path == Path::new(METADATA_OUTPUT))
            .and_then(|output| serde_json::from_slice::<ScriptMetadata>(&output.data).ok())
            .unwrap_or_default();
        let workspace_removed = (!metadata.work_dir.is_empty())
            .then(|| Path::new(&metadata.work_dir))
            .filter(|path| path.is_absolute())
            .map(|path| !path.exists());
        let non_empty = |value: &str| (!value.is_empty()).then(|| value.to_owned());
        let receipt = result.receipt.as_ref();
        let report = RunnerReport {
            worker: "github-actions".into(),
            repository: spec.repository.to_string(),
            declared_recipe: spec.declared_recipe.clone(),
            runner: RunnerIdentity {
                version: spec.runner_version.clone(),
                os: spec.os,
                arch: spec.arch,
                name,
                ephemeral: true,
            },
            environment: format!(
                "{} on {}",
                result.runtime,
                non_empty(&metadata.host).unwrap_or_default()
            )
            .trim_end()
            .to_owned(),
            execution_id: result.execution_id.clone(),
            receipt_hash: receipt.and_then(|receipt| {
                serde_json::to_value(&receipt.receipt_hash)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
            }),
            started_at: metadata
                .started_at
                .or_else(|| receipt.and_then(|receipt| receipt.started_at)),
            finished_at: metadata
                .finished_at
                .or_else(|| receipt.and_then(|receipt| receipt.finished_at)),
            status: result.status.clone(),
            exit_code: result.exit_code,
            stage: metadata.stage,
            job: JobEvidence {
                name: non_empty(&metadata.job_name),
                result: non_empty(&metadata.job_result),
            },
            cleanup: CleanupEvidence { workspace_removed },
        };
        Ok(RunnerRun { report, result })
    }
}

/// The workload: the worker script under the `shell` runtime, network on (it
/// must reach GitHub), one declared output, a wall-time limit. The registration
/// token is the single secret and travels only in the process environment,
/// which receipts record by name.
fn request(spec: &RunnerSpec, name: &str, script: PathBuf, token: &Secret) -> ExecutionRequest {
    let var = |key: &str, value: String| EnvironmentVariable {
        key: key.into(),
        value,
    };
    let mut env = vec![
        var(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into()),
        ),
        var(
            "RUNNER_REPO_URL",
            format!(
                "{}/{}",
                spec.server_url.trim_end_matches('/'),
                spec.repository
            ),
        ),
        var("RUNNER_VERSION", spec.runner_version.clone()),
        var("RUNNER_DOWNLOAD_URL", spec.download_url()),
        var(
            "RUNNER_ARCHIVE_FILE",
            spec.archive_file
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
        ),
        var("RUNNER_SHA256", spec.runner_sha256.to_ascii_lowercase()),
        var("RUNNER_NAME", name.into()),
        var("RUNNER_LABELS", spec.labels.join(",")),
        var(
            "RUNNER_INSTALL_DEPENDENCIES",
            if spec.install_dependencies { "1" } else { "0" }.into(),
        ),
    ];
    env.push(var(REGISTRATION_TOKEN_ENV, token.expose().to_owned()));
    ExecutionRequest {
        runtime: RuntimeSpec {
            kind: RuntimeKind::Shell,
            version: None,
        },
        entrypoint: script,
        args: vec![],
        stdin: vec![],
        env,
        inputs: vec![],
        outputs: vec![WorkloadOutput {
            path: METADATA_OUTPUT.into(),
            required: false,
        }],
        mounts: vec![],
        network: NetworkPolicy::Network,
        resources: ResourceLimits {
            wall_time: Some(spec.timeout),
            ..ResourceLimits::default()
        },
        isolation: IsolationProfile::Process,
        host_isolation: Default::default(),
        dependencies: None,
    }
}

fn generated_name(repository: &Repository) -> String {
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or_default();
    let digest = compute_core::sha256_identity(
        format!("{repository}/{nanos}/{}", std::process::id()).as_bytes(),
    );
    let digest = digest.trim_start_matches("sha256:");
    format!("compute-{}", &digest[..10])
}

fn scrub_result(result: &mut ExecutionResult, secrets: &[&Secret]) {
    result.stdout.text = scrub(&result.stdout.text, secrets);
    result.stderr.text = scrub(&result.stderr.text, secrets);
    if let Some(error) = &mut result.error {
        error.message = scrub(&error.message, secrets);
    }
}
