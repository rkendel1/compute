//! The application lifecycle: `compute init`, `deploy`, `status`, `logs`,
//! `history`, `rollback`, and `stop` for a directory with an
//! `[application]` in its `compute.toml`.
//!
//! ```text
//! compute deploy APP
//!   → requirements (compute.toml)
//!   → provider discovery and placement (the caller-owned pool)
//!   → the selected provider's Compute daemon: /applications/{name}
//!   → revision, release, stable endpoint, versions, evidence
//! ```
//!
//! The daemon on the selected provider owns everything after placement;
//! this module decides where and prints what happened. There is no
//! application database here: the application's home is found by asking
//! the pool's providers.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Args;
use compute_core::{ApplicationIdentity, ComputeError};
use compute_environment::client::DaemonClient;
use compute_environment::{
    ApplicationDeployRequest, ApplicationDeploymentState, ApplicationDeploymentView,
    ApplicationRollbackRequest, ApplicationView, EnvironmentError,
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
    pub application: PathBuf,
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
    pub application: PathBuf,
    #[command(flatten)]
    pub daemon: DaemonLocation,
    #[command(flatten)]
    pub location: pool::PoolLocation,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct RollbackCommand {
    pub application: PathBuf,
    /// Deployment ID or application-scoped version.
    pub deployment: String,
    #[command(flatten)]
    pub daemon: DaemonLocation,
    #[command(flatten)]
    pub location: pool::PoolLocation,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone)]
pub struct Definition {
    pub root: PathBuf,
    pub identity: ApplicationIdentity,
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
    Ok(Definition {
        root,
        identity: ApplicationIdentity::new(file.application.name, file.application.port)?,
    })
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
    path: PathBuf,
    provider: Option<String>,
    daemon: DaemonLocation,
    location: pool::PoolLocation,
    json: bool,
) -> compute_core::Result<()> {
    let definition = definition(&path)?;
    let name = definition.identity.name.clone();
    let port = definition.identity.port.ok_or_else(|| {
        ComputeError::InvalidWorkload(
            "deployable applications must declare application.port".into(),
        )
    })?;
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
    let artifact = pool::PlacementArtifact {
        path: Some(definition.root.clone()),
        provider: pinned,
        // Deployment capability is live state: discover it now.
        refresh: true,
        ..pool::PlacementArtifact::default()
    };
    let (_, report, request) = pool::evaluate(
        &location,
        &admission::PolicyLocation::default(),
        &artifact,
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
    let bundle = match &request.artifact {
        compute_provider::ArtifactTransport::Bundle { data } => data.clone(),
        _ => {
            return Err(ComputeError::Runtime(
                "placement did not produce a portable bundle".into(),
            ));
        }
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
                bundle,
                port,
                source: Some(definition.root.display().to_string()),
                placement: Some(placement),
            }),
        )
        .await
        .map_err(|error| provider_error(&host, error))?;
    let deployment = wait_for_release(&host, &name, &started.deployment_id).await?;
    finish(&host, &name, &deployment, json).await
}

pub async fn status(
    path: PathBuf,
    daemon: DaemonLocation,
    location: pool::PoolLocation,
    json: bool,
) -> compute_core::Result<()> {
    let definition = definition(&path)?;
    let (host, view) = locate(&definition.identity.name, &daemon, &location).await?;
    if json {
        print_json(&with_provider(&host, &view));
    } else {
        print_application(&host, &view);
    }
    Ok(())
}

pub async fn stop(
    path: PathBuf,
    daemon: DaemonLocation,
    location: pool::PoolLocation,
    json: bool,
) -> compute_core::Result<()> {
    let definition = definition(&path)?;
    let name = definition.identity.name.clone();
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
        view = application(&host, &name).await?;
    }
    if json {
        print_json(&with_provider(&host, &view));
    } else {
        print_application(&host, &view);
    }
    Ok(())
}

pub async fn logs(command: LogsCommand) -> compute_core::Result<()> {
    let definition = definition(&command.application)?;
    let name = definition.identity.name.clone();
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
    let definition = definition(&command.application)?;
    let name = definition.identity.name.clone();
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
    let definition = definition(&command.application)?;
    let name = definition.identity.name.clone();
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
        let mut value = with_provider(host, &view);
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
    if let Some(active) = &view.active {
        value["version"] = serde_json::json!(active.version);
        value["deployment_id"] = serde_json::json!(active.deployment_id);
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
