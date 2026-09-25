//! The application lifecycle: `compute init`, `deploy`, `status`, `logs`,
//! `history`, `rollback`, and `stop`, and the same operations under
//! `compute application` (with `info` and `pack`).
//!
//! An application is a directory with an `[application]` in its
//! `compute.toml`, or a portable application artifact
//! (`compute.application-artifact@1`) as a file or a `file://` or
//! `http(s)://` URL. Operations on a deployed application also take its
//! name.
//!
//! ```text
//! compute deploy APP
//!   → the artifact (a directory is packed into one)
//!   → requirements (its bundle)
//!   → provider discovery and placement (the caller-owned pool)
//!   → the selected provider's Compute daemon: /applications/{name}
//!     (a URL is fetched and verified there, by the provider)
//!   → revision, release, stable endpoint, versions, evidence
//! ```
//!
//! The daemon on the selected provider owns everything after placement;
//! this module decides where and prints what happened. There is no
//! application database here: the application's home is found by asking
//! the pool's providers.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Subcommand};
use compute_core::{
    ApplicationArtifact, ApplicationDescription, ApplicationIdentity, ArtifactReference,
    ComputeError,
};
use compute_environment::client::DaemonClient;
use compute_environment::{
    ApplicationArtifactSource, ApplicationDeployRequest, ApplicationDeploymentState,
    ApplicationDeploymentView, ApplicationRollbackRequest, ApplicationView, EnvironmentError,
};
use compute_placement::{
    EvaluationStatus, PlacementOutcome, PlacementReport, ProviderKind, SubmissionMode,
};
use compute_state::PoolPlacement;
use serde::{Deserialize, Serialize};

use crate::environment_cmd::DaemonLocation;
use crate::{admission, pool};

#[derive(Args, Debug)]
pub struct InitCommand {
    pub path: PathBuf,
    #[arg(long, default_value = "node")]
    pub runtime: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct LogsCommand {
    /// The application: its directory, artifact, or name.
    pub application: String,
    #[arg(long)]
    pub follow: bool,
    /// Read one version's output. Only the active version's instance keeps
    /// its logs.
    #[arg(long)]
    pub version: Option<u64>,
    #[command(flatten)]
    pub daemon: DaemonLocation,
    #[command(flatten)]
    pub location: pool::PoolLocation,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct HistoryCommand {
    /// The application: its directory, artifact, or name.
    pub application: String,
    #[command(flatten)]
    pub daemon: DaemonLocation,
    #[command(flatten)]
    pub location: pool::PoolLocation,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct RollbackCommand {
    /// The application: its directory, artifact, or name.
    pub application: String,
    /// Deployment ID or application-scoped version.
    pub deployment: String,
    #[command(flatten)]
    pub daemon: DaemonLocation,
    #[command(flatten)]
    pub location: pool::PoolLocation,
    #[arg(long)]
    pub json: bool,
}

/// `compute application …`: the application lifecycle as one resource, for
/// people and agents.
#[derive(Args, Debug)]
pub struct ApplicationCommand {
    #[command(subcommand)]
    pub command: ApplicationCommands,
}

#[derive(Subcommand, Debug)]
pub enum ApplicationCommands {
    /// Describe an application directory or artifact: identity, runtime,
    /// requirements, environment contract. Contacts no provider.
    Info(InfoCommand),
    /// Package an application directory as a portable artifact.
    Pack(PackCommand),
    /// Place the application on a provider that can host it and release a
    /// new version there.
    Deploy(DeployCommand),
    /// The application's state on the provider that hosts it.
    Status(TargetCommand),
    /// The active version's output.
    Logs(LogsCommand),
    /// Every version, newest first.
    History(HistoryCommand),
    /// Deploy an earlier version again, as the next version.
    Rollback(RollbackCommand),
    /// Stop serving. Versions and evidence remain.
    Stop(TargetCommand),
}

#[derive(Args, Debug)]
pub struct InfoCommand {
    /// An application directory, artifact file, or `file://`/`http(s)://`
    /// artifact URL.
    pub application: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct PackCommand {
    /// The application directory.
    pub path: PathBuf,
    /// Where to write the artifact. Defaults to `NAME.capp` here.
    #[arg(long, short)]
    pub output: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct DeployCommand {
    /// An application directory, artifact file, or `file://`/`http(s)://`
    /// artifact URL. A URL is fetched by the provider.
    pub application: String,
    /// The provider to deploy to (`auto`, `provider:<id>`, or a pool
    /// provider ID). Placement chooses by default.
    #[arg(long)]
    pub provider: Option<String>,
    /// The application's configuration (`NAME=VALUE`, repeatable),
    /// replacing the current one. Omitted, the current one is kept.
    #[arg(long = "set", value_parser = crate::environment_cmd::parse_pair)]
    pub env: Vec<(String, String)>,
    #[command(flatten)]
    pub daemon: DaemonLocation,
    #[command(flatten)]
    pub location: pool::PoolLocation,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct TargetCommand {
    /// The application: its directory, artifact, or name.
    pub application: String,
    #[command(flatten)]
    pub daemon: DaemonLocation,
    #[command(flatten)]
    pub location: pool::PoolLocation,
    #[arg(long)]
    pub json: bool,
}

pub async fn command(command: ApplicationCommand) -> compute_core::Result<()> {
    match command.command {
        ApplicationCommands::Info(command) => info(command).await,
        ApplicationCommands::Pack(command) => pack_command(command),
        ApplicationCommands::Deploy(command) => {
            deploy(
                command.application,
                command.provider,
                command.env,
                command.daemon,
                command.location,
                command.json,
            )
            .await
        }
        ApplicationCommands::Status(command) => {
            status(
                command.application,
                command.daemon,
                command.location,
                command.json,
            )
            .await
        }
        ApplicationCommands::Logs(command) => logs(command).await,
        ApplicationCommands::History(command) => history(command).await,
        ApplicationCommands::Rollback(command) => rollback(command).await,
        ApplicationCommands::Stop(command) => {
            stop(
                command.application,
                command.daemon,
                command.location,
                command.json,
            )
            .await
        }
    }
}

#[derive(Debug, Clone)]
pub struct Definition {
    pub root: PathBuf,
    pub identity: ApplicationIdentity,
    pub description: ApplicationDescription,
}

#[derive(Debug, Deserialize)]
struct ApplicationFile {
    application: ApplicationSection,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplicationSection {
    name: String,
    #[serde(default)]
    port: Option<u16>,
    /// The developer's label for this build.
    #[serde(default)]
    version: Option<String>,
    /// Environment names the deployment's configuration must supply.
    #[serde(default)]
    required_env: BTreeSet<String>,
    /// Capability names the application offers.
    #[serde(default)]
    capabilities: BTreeSet<String>,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ApplicationLogs {
    stdout: String,
    stderr: String,
}

/// A provider in the pool that hosts deployments, and its daemon.
struct Host {
    provider_id: String,
    kind: ProviderKind,
    client: DaemonClient,
}

pub fn definition(path: &Path) -> compute_core::Result<Definition> {
    let root = if path.is_dir() {
        path.to_path_buf()
    } else if path.file_name().is_some_and(|name| name == "compute.toml") {
        path.parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()
    } else {
        return Err(ComputeError::InvalidWorkload(format!(
            "application path is not a directory: {}",
            path.display()
        )));
    };
    let root = std::fs::canonicalize(&root).map_err(|error| {
        ComputeError::InvalidWorkload(format!(
            "cannot open application {}: {error}",
            root.display()
        ))
    })?;
    let config = root.join("compute.toml");
    let text = std::fs::read_to_string(&config).map_err(|error| {
        ComputeError::InvalidWorkload(format!("cannot read {}: {error}", config.display()))
    })?;
    let file: ApplicationFile = toml::from_str(&text).map_err(|error| {
        ComputeError::InvalidWorkload(format!(
            "invalid application in {}: {error}",
            config.display()
        ))
    })?;
    let application = file.application;
    Ok(Definition {
        root,
        identity: ApplicationIdentity::new(application.name, application.port)?,
        description: ApplicationDescription {
            version: application.version,
            required_env: application.required_env,
            capabilities: application.capabilities,
            metadata: application.metadata,
        },
    })
}

/// What a command was given: an application directory, an artifact (a
/// file, or a URL the provider will fetch), or a deployed application's
/// name.
enum Target {
    Directory(Definition),
    Artifact {
        artifact: Box<ApplicationArtifact>,
        bytes: Vec<u8>,
        /// Set for a URL: the provider fetches this, pinned to the digest
        /// of the bytes placed here.
        reference: Option<ArtifactReference>,
        source: String,
    },
    Name(String),
}

impl Target {
    fn name(&self) -> &str {
        match self {
            Self::Directory(definition) => &definition.identity.name,
            Self::Artifact { artifact, .. } => &artifact.manifest.application.name,
            Self::Name(name) => name,
        }
    }
}

fn is_url(value: &str) -> bool {
    ["file://", "http://", "https://"]
        .iter()
        .any(|scheme| value.starts_with(scheme))
}

/// Whether `value` names something `compute deploy` deploys as an
/// application: a directory with `[application]`, an artifact file, or an
/// artifact URL.
pub fn is_deployable(value: &str) -> bool {
    let path = Path::new(value);
    is_url(value)
        || is_application(path)
        || (path.is_file()
            && std::fs::read(path).is_ok_and(|bytes| ApplicationArtifact::sniff(&bytes)))
}

async fn target(value: &str) -> compute_core::Result<Target> {
    if is_url(value) {
        let url = value.to_owned();
        let bytes =
            tokio::task::spawn_blocking(move || compute_core::application_artifact::fetch(&url))
                .await
                .map_err(|error| ComputeError::Runtime(error.to_string()))??;
        let artifact = ApplicationArtifact::from_bytes(&bytes)?;
        let reference = ArtifactReference {
            url: value.to_owned(),
            digest: compute_core::sha256_identity(&bytes),
        };
        return Ok(Target::Artifact {
            artifact: Box::new(artifact),
            bytes,
            reference: Some(reference),
            source: value.to_owned(),
        });
    }
    let path = Path::new(value);
    if path.is_dir() || path.file_name().is_some_and(|name| name == "compute.toml") {
        return Ok(Target::Directory(definition(path)?));
    }
    if path.is_file() {
        let bytes = std::fs::read(path)?;
        if !ApplicationArtifact::sniff(&bytes) {
            return Err(ComputeError::InvalidWorkload(format!(
                "{value} is not an application artifact (compute.application-artifact@1)"
            )));
        }
        let artifact = ApplicationArtifact::from_bytes(&bytes)?;
        return Ok(Target::Artifact {
            artifact: Box::new(artifact),
            bytes,
            reference: None,
            source: std::fs::canonicalize(path)?.display().to_string(),
        });
    }
    ApplicationIdentity::new(value, None).map_err(|_| {
        ComputeError::InvalidWorkload(format!(
            "{value} is not an application directory, artifact, URL, or name"
        ))
    })?;
    Ok(Target::Name(value.to_owned()))
}

/// The name of the application `value` refers to.
async fn target_name(value: &str) -> compute_core::Result<String> {
    Ok(target(value).await?.name().to_owned())
}

/// Package an application directory as a portable artifact: its canonical
/// bundle, built exactly as `compute run` and `compute deploy` build it,
/// and its manifest.
pub fn pack(definition: &Definition) -> compute_core::Result<ApplicationArtifact> {
    if definition.identity.port.is_none() {
        return Err(ComputeError::InvalidWorkload(
            "deployable applications must declare application.port".into(),
        ));
    }
    let (bundle, _, _) = pool::prepare(
        &pool::PlacementArtifact {
            path: Some(definition.root.clone()),
            ..pool::PlacementArtifact::default()
        },
        &admission::PolicyLocation::default(),
    )?;
    ApplicationArtifact::new(
        definition.identity.clone(),
        definition.description.clone(),
        &bundle,
    )
}

fn pack_command(command: PackCommand) -> compute_core::Result<()> {
    let definition = definition(&command.path)?;
    let artifact = pack(&definition)?;
    let bytes = artifact.to_bytes()?;
    let output = command
        .output
        .unwrap_or_else(|| PathBuf::from(format!("{}.capp", definition.identity.name)));
    std::fs::write(&output, &bytes)?;
    let artifact_id = artifact.artifact_id()?;
    if command.json {
        print_json(&serde_json::json!({
            "artifact_id": artifact_id,
            "path": output,
            "size": bytes.len(),
            "manifest": artifact.manifest,
        }));
    } else {
        println!("Packed {}", artifact.manifest.application.name);
        println!("Artifact:    {artifact_id}");
        println!("Path:        {}", output.display());
    }
    Ok(())
}

async fn info(command: InfoCommand) -> compute_core::Result<()> {
    let (artifact, source) = match target(&command.application).await? {
        Target::Directory(definition) => {
            let source = definition.root.display().to_string();
            (pack(&definition)?, source)
        }
        Target::Artifact {
            artifact, source, ..
        } => (*artifact, source),
        Target::Name(name) => {
            return Err(ComputeError::InvalidWorkload(format!(
                "{name}: give an application directory or artifact; `compute application status {name}` describes a deployed application"
            )));
        }
    };
    let artifact_id = artifact.artifact_id()?;
    let manifest = &artifact.manifest;
    if command.json {
        print_json(&serde_json::json!({
            "artifact_id": artifact_id,
            "source": source,
            "manifest": manifest,
        }));
        return Ok(());
    }
    println!("Application: {}", manifest.application.name);
    println!("ID:          {}", manifest.application.id);
    if let Some(version) = &manifest.version {
        println!("Version:     {version}");
    }
    println!(
        "Runtime:     {}{}",
        manifest.runtime.name,
        manifest
            .runtime
            .version
            .as_deref()
            .map(|version| format!(" {version}"))
            .unwrap_or_default()
    );
    println!("Entrypoint:  {}", manifest.entrypoint.display());
    if let Some(port) = manifest.application.port {
        println!("Port:        {port}");
    }
    if !manifest.env.required.is_empty() {
        println!(
            "Requires:    {}",
            manifest
                .env
                .required
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !manifest.capabilities.is_empty() {
        println!(
            "Offers:      {}",
            manifest
                .capabilities
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    println!("Artifact:    {artifact_id}");
    println!("Source:      {source}");
    Ok(())
}

pub fn is_application(path: &Path) -> bool {
    let config = if path.is_dir() {
        path.join("compute.toml")
    } else {
        path.to_path_buf()
    };
    std::fs::read_to_string(config)
        .ok()
        .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
        .and_then(|value| value.get("application").cloned())
        .is_some()
}

pub fn init(command: InitCommand) -> compute_core::Result<()> {
    if command.path.exists() && command.path.read_dir()?.next().is_some() {
        return Err(ComputeError::InvalidWorkload(format!(
            "application directory is not empty: {}",
            command.path.display()
        )));
    }
    let name = command
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| ComputeError::InvalidWorkload("application path needs a name".into()))?;
    let identity = ApplicationIdentity::new(name, Some(3000))?;
    std::fs::create_dir_all(&command.path)?;
    let (entrypoint, source) = match command.runtime.as_str() {
        "node" => (
            "server.js",
            "const http = require('http');\nconst port = Number(process.env.PORT || 3000);\nhttp.createServer((req, res) => { res.end('Hello from Compute\\n'); }).listen(port, '0.0.0.0', () => console.log(`listening on ${port}`));\n",
        ),
        "python" => (
            "main.py",
            "import os\nfrom http.server import HTTPServer, BaseHTTPRequestHandler\nclass Handler(BaseHTTPRequestHandler):\n    def do_GET(self):\n        self.send_response(200); self.end_headers(); self.wfile.write(b'Hello from Compute\\n')\nHTTPServer(('0.0.0.0', int(os.environ.get('PORT', '3000'))), Handler).serve_forever()\n",
        ),
        other => {
            return Err(ComputeError::InvalidWorkload(format!(
                "compute init currently supports node or python, not {other}"
            )));
        }
    };
    let version = if command.runtime == "node" {
        ">=24"
    } else {
        ">=3.12"
    };
    let manifest = format!(
        "[application]\nname = {name:?}\nport = 3000\n\n[runtime]\nname = {:?}\nversion = {version:?}\n\n[run]\nentrypoint = {entrypoint:?}\n\n[resources]\ncpu = 1\nmemory = \"512MiB\"\n\n[network]\nmode = \"network\"\n\n[placement]\npolicy = \"auto\"\n",
        command.runtime
    );
    std::fs::write(command.path.join("compute.toml"), manifest)?;
    std::fs::write(command.path.join(entrypoint), source)?;
    if command.json {
        print_json(&serde_json::json!({
            "application": identity,
            "path": command.path,
            "entrypoint": entrypoint,
        }));
    } else {
        println!("Initialized application {}", identity.name);
        println!("Path: {}", command.path.display());
        println!("Deploy: compute deploy {}", command.path.display());
    }
    Ok(())
}

/// Deploy a new version: place the application on a provider that can
/// satisfy its requirements and host it, then release it there.
pub async fn deploy(
    application: String,
    provider: Option<String>,
    env: Vec<(String, String)>,
    daemon: DaemonLocation,
    location: pool::PoolLocation,
    json: bool,
) -> compute_core::Result<()> {
    let target = target(&application).await?;
    // A directory is packed into the artifact it deploys, so every
    // deployment is of an artifact, with its identity as evidence.
    let (artifact, bytes, reference, source, directory) = match target {
        Target::Directory(definition) => {
            let artifact = pack(&definition)?;
            let bytes = artifact.to_bytes()?;
            let source = definition.root.display().to_string();
            (artifact, bytes, None, source, Some(definition.root))
        }
        Target::Artifact {
            artifact,
            bytes,
            reference,
            source,
        } => (*artifact, bytes, reference, source, None),
        Target::Name(name) => {
            return Err(ComputeError::InvalidWorkload(format!(
                "{name}: deploy an application directory, artifact, or artifact URL"
            )));
        }
    };
    let name = artifact.manifest.application.name.clone();
    // An application lives on one provider. A new version goes where the
    // application already is: moving it is not something deploy does
    // silently.
    let home = find_home(&name, &daemon, &location).await?;
    let requested = provider.filter(|value| value != "auto");
    let pinned = match (&home, &requested) {
        (Some(home), Some(requested))
            if requested.strip_prefix("provider:").unwrap_or(requested) != home.provider_id =>
        {
            return Err(ComputeError::InvalidWorkload(format!(
                "{name} is deployed on {home}, where its versions, endpoint, and evidence live. Compute does not move an application between providers; deploy it to {home}",
                home = home.provider_id
            )));
        }
        (Some(home), _) => Some(home.provider_id.clone()),
        (None, requested) => requested.clone(),
    };
    // Placement evaluates the bundle the provider will run. A directory
    // keeps its compute.toml placement preferences.
    let staged = tempfile::NamedTempFile::new()?;
    let placement_artifact = match directory {
        Some(root) => pool::PlacementArtifact {
            path: Some(root),
            provider: pinned,
            // Deployment capability is live state: discover it now.
            refresh: true,
            ..pool::PlacementArtifact::default()
        },
        None => {
            std::fs::write(staged.path(), artifact.bundle_bytes())?;
            pool::PlacementArtifact {
                bundle: Some(staged.path().to_path_buf()),
                provider: pinned,
                refresh: true,
                ..pool::PlacementArtifact::default()
            }
        }
    };
    let (_, report, _) = pool::evaluate(
        &location,
        &admission::PolicyLocation::default(),
        &placement_artifact,
        SubmissionMode::Deployment,
    )
    .await?;
    let Some(selected) = report
        .selected
        .as_ref()
        .filter(|_| report.outcome == PlacementOutcome::Placed)
    else {
        if json {
            print_json(&report);
        }
        return Err(ComputeError::Runtime(explain_no_provider(&name, &report)));
    };
    let host = match home {
        Some(home) => home,
        None => Host {
            client: match selected.provider_kind {
                ProviderKind::Local => ensure_local_daemon(&daemon).await?,
                ProviderKind::Remote => remote_client(&location, &selected.provider_id)?,
            },
            provider_id: selected.provider_id.clone(),
            kind: selected.provider_kind,
        },
    };
    let placement = PoolPlacement {
        placement_id: report.placement_id.clone(),
        provider_id: selected.provider_id.clone(),
        selection_mode: serde_json::to_value(report.selection_mode)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default(),
    };
    let started: ApplicationDeploymentView = host
        .client
        .post(
            &format!("/applications/{name}/deployments"),
            Some(&ApplicationDeployRequest {
                artifact: Some(match reference {
                    Some(reference) => ApplicationArtifactSource::Reference(reference),
                    None => ApplicationArtifactSource::Inline { data: bytes },
                }),
                bundle: vec![],
                port: None,
                env: (!env.is_empty()).then(|| env.into_iter().collect()),
                source: Some(source),
                placement: Some(placement),
            }),
        )
        .await
        .map_err(|error| provider_error(&host, error))?;
    let deployment = wait_for_release(&host, &name, &started.deployment_id).await?;
    finish(&host, &name, &deployment, json).await
}

pub async fn status(
    application: String,
    daemon: DaemonLocation,
    location: pool::PoolLocation,
    json: bool,
) -> compute_core::Result<()> {
    let name = target_name(&application).await?;
    let (host, view) = locate(&name, &daemon, &location).await?;
    if json {
        print_json(&with_provider(&host, &view));
    } else {
        print_application(&host, &view);
    }
    Ok(())
}

pub async fn stop(
    application: String,
    daemon: DaemonLocation,
    location: pool::PoolLocation,
    json: bool,
) -> compute_core::Result<()> {
    let name = target_name(&application).await?;
    let (host, _) = locate(&name, &daemon, &location).await?;
    let mut view: ApplicationView = host
        .client
        .post::<(), _>(&format!("/applications/{name}/stop"), None)
        .await
        .map_err(|error| provider_error(&host, error))?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while view.status != "stopped" {
        if tokio::time::Instant::now() >= deadline {
            return Err(ComputeError::Runtime(format!(
                "{name} did not stop within 30s"
            )));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        view = self::application(&host, &name).await?;
    }
    if json {
        print_json(&with_provider(&host, &view));
    } else {
        print_application(&host, &view);
    }
    Ok(())
}

pub async fn logs(command: LogsCommand) -> compute_core::Result<()> {
    let name = target_name(&command.application).await?;
    let (host, view) = locate(&name, &command.daemon, &command.location).await?;
    if let Some(version) = command.version
        && view
            .active
            .as_ref()
            .is_none_or(|active| active.version != version)
    {
        return Err(ComputeError::Runtime(format!(
            "{name} v{version} is not the active version; only the active version's output is kept"
        )));
    }
    let mut stdout_offset = 0;
    let mut stderr_offset = 0;
    loop {
        let logs: ApplicationLogs = host
            .client
            .get(&format!("/applications/{name}/logs"))
            .await
            .map_err(|error| provider_error(&host, error))?;
        if command.json {
            print_json(&logs);
            return Ok(());
        }
        print_delta(&logs.stdout, &mut stdout_offset, false)?;
        print_delta(&logs.stderr, &mut stderr_offset, true)?;
        if !command.follow {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

pub async fn history(command: HistoryCommand) -> compute_core::Result<()> {
    let name = target_name(&command.application).await?;
    let (host, view) = locate(&name, &command.daemon, &command.location).await?;
    if command.json {
        print_json(&view.deployments);
        return Ok(());
    }
    println!("{name} on {}", host.provider_id);
    println!("VERSION\tSTATE\tCREATED\tNOTE\tDEPLOYMENT");
    for deployment in &view.deployments {
        let note = deployment
            .rollback_of
            .map(|version| format!("rollback to v{version}"))
            .or_else(|| deployment.failure.clone())
            .unwrap_or_else(|| "-".into());
        println!(
            "v{}\t{}\t{}\t{}\t{}",
            deployment.version,
            deployment.state.as_str(),
            deployment.created_at.to_rfc3339(),
            note,
            deployment.deployment_id
        );
    }
    Ok(())
}

pub async fn rollback(command: RollbackCommand) -> compute_core::Result<()> {
    let name = target_name(&command.application).await?;
    let (host, _) = locate(&name, &command.daemon, &command.location).await?;
    let started: ApplicationDeploymentView = host
        .client
        .post(
            &format!("/applications/{name}/rollback"),
            Some(&ApplicationRollbackRequest {
                target: command.deployment,
                placement: None,
            }),
        )
        .await
        .map_err(|error| provider_error(&host, error))?;
    let deployment = wait_for_release(&host, &name, &started.deployment_id).await?;
    finish(&host, &name, &deployment, command.json).await
}

/// Print a finished release, and fail when it did not become active.
async fn finish(
    host: &Host,
    name: &str,
    deployment: &ApplicationDeploymentView,
    json: bool,
) -> compute_core::Result<()> {
    let view = application(host, name).await?;
    if json {
        // This release, whatever is active now: identity, where, what,
        // and its evidence.
        let mut value = with_provider(host, &view);
        value["application_id"] = serde_json::json!(view.application.id);
        value["deployment_id"] = serde_json::json!(deployment.deployment_id);
        value["version"] = serde_json::json!(deployment.version);
        value["runtime"] = serde_json::json!(deployment.runtime);
        value["runtime_version"] = serde_json::json!(deployment.runtime_version);
        value["receipt"] = serde_json::json!(deployment.receipt);
        value["artifact"] = serde_json::json!(deployment.artifact);
        value["deployment"] = serde_json::to_value(deployment)?;
        print_json(&value);
    } else {
        print_release(host, &view, deployment);
    }
    if !deployment.active {
        return Err(ComputeError::Runtime(format!(
            "{name} v{} did not become active: {}",
            deployment.version,
            deployment
                .failure
                .clone()
                .unwrap_or_else(|| deployment.state.as_str().into())
        )));
    }
    Ok(())
}

async fn wait_for_release(
    host: &Host,
    name: &str,
    deployment_id: &str,
) -> compute_core::Result<ApplicationDeploymentView> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    loop {
        let deployment: ApplicationDeploymentView = host
            .client
            .get(&format!("/applications/{name}/deployments/{deployment_id}"))
            .await
            .map_err(|error| provider_error(host, error))?;
        if deployment.state != ApplicationDeploymentState::Deploying {
            return Ok(deployment);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ComputeError::Runtime(format!(
                "{name} v{} did not finish releasing within 180s",
                deployment.version
            )));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn application(host: &Host, name: &str) -> compute_core::Result<ApplicationView> {
    host.client
        .get(&format!("/applications/{name}"))
        .await
        .map_err(|error| provider_error(host, error))
}

/// The provider an application is deployed on, and its view there.
async fn locate(
    name: &str,
    daemon: &DaemonLocation,
    location: &pool::PoolLocation,
) -> compute_core::Result<(Host, ApplicationView)> {
    let host = find_home(name, daemon, location).await?.ok_or_else(|| {
        ComputeError::Runtime(format!(
            "{name} is not deployed on any provider in the pool; deploy it with `compute deploy`"
        ))
    })?;
    let view = application(&host, name).await?;
    Ok((host, view))
}

/// Ask each provider that could host deployments whether the application
/// is there. A local daemon that is not running hosts nothing that runs; a
/// remote provider that cannot be reached might, so it fails closed.
async fn find_home(
    name: &str,
    daemon: &DaemonLocation,
    location: &pool::PoolLocation,
) -> compute_core::Result<Option<Host>> {
    let mut found = vec![];
    let mut unreachable = vec![];
    for (provider_id, config) in location.pool()?.configs() {
        let client = match config.kind {
            ProviderKind::Local => daemon.client()?,
            ProviderKind::Remote => remote_client(location, &provider_id)?,
        };
        match client
            .get::<ApplicationView>(&format!("/applications/{name}"))
            .await
        {
            Ok(_) => found.push(Host {
                provider_id,
                kind: config.kind,
                client,
            }),
            Err(EnvironmentError::ControllerUnavailable(_))
                if config.kind == ProviderKind::Remote =>
            {
                unreachable.push(provider_id)
            }
            // Not there, not a daemon, or no local daemon running.
            Err(_) => {}
        }
    }
    match found.len() {
        0 if !unreachable.is_empty() => Err(ComputeError::Runtime(format!(
            "cannot tell where {name} is deployed: {} cannot be reached",
            unreachable.join(", ")
        ))),
        0 => Ok(None),
        1 => Ok(found.pop()),
        _ => Err(ComputeError::Runtime(format!(
            "{name} is deployed on more than one provider ({}); stop all but one",
            found
                .iter()
                .map(|host| host.provider_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// The daemon of a remote provider, authenticated with the pool's
/// credential for it.
fn remote_client(
    location: &pool::PoolLocation,
    provider_id: &str,
) -> compute_core::Result<DaemonClient> {
    let configs = location.pool()?.configs();
    let config = configs.get(provider_id).ok_or_else(|| {
        ComputeError::InvalidWorkload(format!("provider {provider_id} is not in the pool"))
    })?;
    let endpoint = config.endpoint.as_deref().ok_or_else(|| {
        ComputeError::InvalidWorkload(format!("provider {provider_id} has no endpoint"))
    })?;
    let mut client = DaemonClient::new(endpoint).map_err(crate::environment_cmd::error)?;
    if let Some(name) = &config.token_env {
        let token = std::env::var(name).map_err(|_| {
            ComputeError::InvalidWorkload(format!(
                "provider {provider_id}: environment variable {name} is not set"
            ))
        })?;
        client = client.with_bearer_token(token);
    }
    Ok(client)
}

/// This machine's daemon, started in the background when none runs.
async fn ensure_local_daemon(daemon: &DaemonLocation) -> compute_core::Result<DaemonClient> {
    let client = daemon.client()?;
    match client
        .get::<compute_environment::DaemonStatus>("/status")
        .await
    {
        Ok(_) => return Ok(client),
        Err(EnvironmentError::ControllerUnavailable(_)) if daemon.endpoint.is_none() => {}
        Err(error) => return Err(crate::environment_cmd::error(error)),
    }
    let status = std::process::Command::new(std::env::current_exe()?)
        .args(["start", "--detach", "--insecure"])
        .status()?;
    if !status.success() {
        return Err(ComputeError::Runtime(
            "could not start the local Compute daemon".into(),
        ));
    }
    let client = daemon.client()?;
    client
        .get::<compute_environment::DaemonStatus>("/status")
        .await
        .map_err(crate::environment_cmd::error)?;
    Ok(client)
}

fn provider_error(host: &Host, error: EnvironmentError) -> ComputeError {
    let kind = match host.kind {
        ProviderKind::Local => "local",
        ProviderKind::Remote => "remote",
    };
    ComputeError::Runtime(format!("provider {} ({kind}): {error}", host.provider_id))
}

/// Why no provider can take this application, in terms of what it asks
/// for and what each provider lacks, and what to change.
fn explain_no_provider(name: &str, report: &PlacementReport) -> String {
    let requirements = &report.requirements;
    let mut lines = vec![
        format!("No compatible provider found for {name}."),
        String::new(),
    ];
    lines.push(format!("{name} requires:"));
    lines.push(format!(
        "  runtime:      {}{}",
        requirements.runtime.kind,
        requirements
            .runtime
            .version
            .as_deref()
            .map(|version| format!(" {version}"))
            .unwrap_or_default()
    ));
    if let Some(architecture) = &requirements.architecture {
        lines.push(format!("  architecture: {architecture}"));
    }
    if let Some(cpu) = requirements.resources.cpu_count {
        lines.push(format!("  cpu:          {cpu}"));
    }
    if let Some(memory) = requirements.resources.memory_bytes {
        lines.push(format!("  memory:       {}", format_bytes(memory)));
    }
    lines.push(format!("  network:      {}", requirements.network));
    lines.push("  deployment:   a provider that hosts applications".into());
    lines.push(String::new());
    if report.providers.is_empty() {
        lines.push(
            report
                .failure
                .as_ref()
                .map(|failure| failure.message.clone())
                .unwrap_or_else(|| "The pool has no providers.".into()),
        );
    } else {
        lines.push("Rejected providers:".into());
    }
    for provider in &report.providers {
        lines.push(format!("  {}", provider.provider_id));
        match provider.status {
            EvaluationStatus::Compatible if provider.capacity_reasons.is_empty() => {
                lines.push("    compatible, but not selected".into());
            }
            EvaluationStatus::ProviderUnavailable
            | EvaluationStatus::CapabilitiesUnknown
            | EvaluationStatus::CapabilitiesInvalid => {
                lines.push(format!(
                    "    unreachable: {}",
                    provider
                        .error
                        .as_ref()
                        .map(|error| error.message.clone())
                        .unwrap_or_else(|| "capabilities could not be established".into())
                ));
            }
            _ => {}
        }
        for reason in provider.reasons.iter().chain(&provider.capacity_reasons) {
            lines.push(format!(
                "    {}: requires {}, provider offers {}{}",
                reason.dimension,
                plain(&reason.required),
                plain(&reason.available),
                reason
                    .detail
                    .as_deref()
                    .map(|detail| format!(" ({detail})"))
                    .unwrap_or_default()
            ));
        }
    }
    lines.push(String::new());
    lines.push(
        "Change the requirements in compute.toml, or add a provider that satisfies them to the pool (a Compute daemon: `compute start`)."
            .into(),
    );
    lines.join("\n")
}

fn plain(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(items) => items.iter().map(plain).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

fn with_provider(host: &Host, view: &ApplicationView) -> serde_json::Value {
    let mut value = serde_json::to_value(view).unwrap_or_default();
    value["provider"] = serde_json::json!(host.provider_id);
    value["application_id"] = serde_json::json!(view.application.id);
    if let Some(active) = &view.active {
        value["version"] = serde_json::json!(active.version);
        value["deployment_id"] = serde_json::json!(active.deployment_id);
        value["runtime"] = serde_json::json!(active.runtime);
        value["receipt"] = serde_json::json!(active.receipt);
        value["artifact"] = serde_json::json!(active.artifact);
    }
    value
}

fn print_application(host: &Host, view: &ApplicationView) {
    println!("Application: {}", view.application.name);
    println!("Status:      {}", view.status);
    if let Some(active) = &view.active {
        println!("Version:     v{}", active.version);
    }
    println!("Provider:    {}", host.provider_id);
    if let Some(active) = &view.active {
        print_runtime(active);
    }
    if let Some(endpoint) = &view.endpoint {
        println!("Endpoint:    {endpoint}");
    }
    if let Some(deploying) = &view.deploying {
        println!("Deploying:   v{}", deploying.version);
    }
}

fn print_release(host: &Host, view: &ApplicationView, deployment: &ApplicationDeploymentView) {
    println!("Application: {}", view.application.name);
    println!("Version:     v{}", deployment.version);
    if let Some(version) = deployment.rollback_of {
        println!("Rollback:    to the code of v{version}");
    }
    println!("Provider:    {}", host.provider_id);
    print_runtime(deployment);
    if let Some(endpoint) = &view.endpoint {
        println!("Endpoint:    {endpoint}");
    }
    println!(
        "Status:      {}",
        if deployment.active {
            view.status.as_str()
        } else {
            deployment.state.as_str()
        }
    );
    if let Some(failure) = &deployment.failure {
        println!("Failure:     {failure}");
    }
}

fn print_runtime(deployment: &ApplicationDeploymentView) {
    if let Some(runtime) = &deployment.runtime {
        println!(
            "Runtime:     {runtime}{}",
            deployment
                .runtime_version
                .as_deref()
                .map(|version| format!(" {version}"))
                .unwrap_or_default()
        );
    }
}

fn print_delta(value: &str, offset: &mut usize, stderr: bool) -> compute_core::Result<()> {
    if *offset > value.len() || !value.is_char_boundary(*offset) {
        *offset = 0;
    }
    let delta = &value[*offset..];
    if stderr {
        eprint!("{delta}");
        std::io::stderr().flush()?;
    } else {
        print!("{delta}");
        std::io::stdout().flush()?;
    }
    *offset = value.len();
    Ok(())
}

fn format_bytes(bytes: u64) -> String {
    if bytes.is_multiple_of(1024 * 1024 * 1024) {
        format!("{} GiB", bytes / (1024 * 1024 * 1024))
    } else if bytes.is_multiple_of(1024 * 1024) {
        format!("{} MiB", bytes / (1024 * 1024))
    } else {
        format!("{bytes} bytes")
    }
}

fn print_json(value: &impl Serialize) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("serializable output")
    );
}
