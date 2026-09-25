//! Product-level application lifecycle over placed durable provider jobs.
//!
//! The provider job and receipt remain authoritative. Application commands
//! discover those records by the identity sealed into each request; there is
//! no second application database.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Args;
use compute_core::{
    ApplicationIdentity, ComputeError, ExecutionJob, JobStatus, ProviderRuntimeRequirement,
    WorkloadBundle,
};
use compute_environment::client::DaemonClient;
use compute_environment::{
    ActualState, DeployRequest, DeploymentStatus, DeploymentView, DesiredState,
    EnvironmentDefinition, EnvironmentError, EnvironmentView, PortSpec, ProjectView, Readiness,
    ReadinessCheck, RestartPolicy, RevisionDefinition, RevisionView, WorkloadDefinition,
    WorkloadKind,
};
use compute_placement::{PlacementOutcome, SubmissionMode, dispatch};
use compute_provider::{ComputeProvider, RemoteProvider};
use serde::{Deserialize, Serialize};

use crate::{admission, pool};

const APPLICATION_ENVIRONMENT: &str = "applications";
const APPLICATION_WORKLOAD: &str = "app";

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
    /// Read the selected historical deployment. Historical logs are
    /// available only while that deployment's instance is retained.
    #[arg(long)]
    pub version: Option<u64>,
    #[command(flatten)]
    pub daemon: crate::environment_cmd::DaemonLocation,
    #[command(flatten)]
    pub location: pool::PoolLocation,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct HistoryCommand {
    pub application: PathBuf,
    #[command(flatten)]
    pub daemon: crate::environment_cmd::DaemonLocation,
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
    pub daemon: crate::environment_cmd::DaemonLocation,
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

#[derive(Debug, Serialize)]
struct ApplicationView {
    application: ApplicationIdentity,
    status: JobStatus,
    provider: String,
    runtime: String,
    runtime_version: Option<String>,
    platform: Option<String>,
    endpoint: Option<String>,
    placement: Option<String>,
    cpu: Option<u32>,
    memory_bytes: Option<u64>,
    network: String,
    job_id: String,
    execution_id: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ApplicationLogs {
    stdout: String,
    stderr: String,
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
        println!("Run: compute run {}", command.path.display());
        println!("Deploy: compute deploy {}", command.path.display());
    }
    Ok(())
}

pub async fn run(
    mut artifact: pool::PlacementArtifact,
    location: pool::PoolLocation,
    policy: admission::PolicyLocation,
) -> compute_core::Result<()> {
    let path = artifact
        .path
        .as_deref()
        .ok_or_else(|| ComputeError::InvalidWorkload("applications require a directory".into()))?;
    let definition = definition(path)?;
    if let Some((provider_id, provider, job)) = active_job(&location, &definition.identity).await? {
        let endpoint = location.application_endpoint(&provider_id)?;
        return print_status(
            &definition,
            &provider_id,
            &provider,
            &job,
            endpoint,
            artifact.json,
        )
        .await;
    }
    artifact.submit = true;
    artifact.path = Some(definition.root.clone());
    let (pool, report, mut request) =
        pool::evaluate(&location, &policy, &artifact, SubmissionMode::Job).await?;
    if report.outcome != PlacementOutcome::Placed {
        if artifact.json {
            print_json(&report);
        }
        return Err(ComputeError::Runtime(
            report
                .failure
                .as_ref()
                .map(|failure| failure.message.clone())
                .unwrap_or_else(|| "no provider can run this application durably".into()),
        ));
    }
    request.execution.application = Some(definition.identity.clone());
    let submission = dispatch::submit(&pool, &report, request, None)
        .await
        .map_err(|error| ComputeError::Runtime(error.to_string()))?;
    let provider = location.remote_provider(&submission.provider_id)?;
    let endpoint = location.application_endpoint(&submission.provider_id)?;
    let mut job = provider
        .job_status(&submission.job.job_id.0)
        .await
        .map_err(crate::provider_error)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !matches!(job.status, JobStatus::Running | JobStatus::Preparing)
        && !job.status.is_terminal()
    {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        job = provider
            .job_status(&submission.job.job_id.0)
            .await
            .map_err(crate::provider_error)?;
    }
    print_status(
        &definition,
        &submission.provider_id,
        &provider,
        &job,
        endpoint,
        artifact.json,
    )
    .await
}

pub async fn deploy(
    path: PathBuf,
    daemon: crate::environment_cmd::DaemonLocation,
    json: bool,
) -> compute_core::Result<()> {
    let definition = definition(&path)?;
    let bundle = application_bundle(&definition)?;
    let bundle_id = bundle.bundle_id()?;
    let client = ensure_deployment_daemon(&daemon).await?;
    ensure_application_environment(&client).await?;
    let revision = format!("artifact-{}", bundle_id.trim_start_matches("sha256:"));
    let port = definition.identity.port.ok_or_else(|| {
        ComputeError::InvalidWorkload(
            "deployable applications must declare application.port".into(),
        )
    })?;
    let revision: RevisionView = client
        .post(
            &format!("/projects/{}/revisions", definition.identity.name),
            Some(&RevisionDefinition {
                revision,
                source: Some(definition.root.display().to_string()),
                workloads: vec![WorkloadDefinition {
                    name: APPLICATION_WORKLOAD.into(),
                    kind: WorkloadKind::Service,
                    bundle: bundle.to_bytes()?,
                    ports: vec![PortSpec {
                        name: "http".into(),
                        port,
                    }],
                    restart: RestartPolicy::OnFailure,
                    desired_state: DesiredState::Running,
                    readiness: Some(Readiness {
                        check: ReadinessCheck::Http,
                        port: Some("http".into()),
                        path: Some("/".into()),
                        task: None,
                        timeout_ms: 60_000,
                        interval_ms: 250,
                    }),
                }],
            }),
        )
        .await
        .map_err(crate::environment_cmd::error)?;
    let deployment: DeploymentView = client
        .post(
            "/deployments",
            Some(&DeployRequest {
                project: definition.identity.name.clone(),
                environment: APPLICATION_ENVIRONMENT.into(),
                revision: Some(revision.revision_id),
                config: None,
                desired_state: Some(DesiredState::Running),
            }),
        )
        .await
        .map_err(crate::environment_cmd::error)?;
    let deployment = wait_for_deployment(&client, deployment.deployment_id).await?;
    print_application_deployment(&daemon, &deployment, None, json);
    if matches!(
        deployment.record.status,
        DeploymentStatus::Failed | DeploymentStatus::RolledBack
    ) {
        return Err(ComputeError::Runtime(
            deployment
                .record
                .failure
                .clone()
                .or(deployment.record.rollback_reason.clone())
                .unwrap_or_else(|| "deployment failed".into()),
        ));
    }
    Ok(())
}

pub async fn status(
    path: PathBuf,
    daemon: crate::environment_cmd::DaemonLocation,
    location: pool::PoolLocation,
    json: bool,
) -> compute_core::Result<()> {
    let definition = definition(&path)?;
    if let Some((project, deployment)) =
        active_deployment(&daemon, &definition.identity.name).await?
    {
        print_application_deployment(&daemon, &deployment, Some(project.actual_state), json);
        return Ok(());
    }
    let (provider_id, provider, job) = latest_job(&location, &definition.identity)
        .await?
        .ok_or_else(|| {
            ComputeError::Runtime(format!(
                "{} has no execution history",
                definition.identity.name
            ))
        })?;
    let endpoint = location.application_endpoint(&provider_id)?;
    print_status(&definition, &provider_id, &provider, &job, endpoint, json).await
}

pub async fn stop(
    path: PathBuf,
    daemon: crate::environment_cmd::DaemonLocation,
    location: pool::PoolLocation,
    json: bool,
) -> compute_core::Result<()> {
    let definition = definition(&path)?;
    if active_deployment(&daemon, &definition.identity.name)
        .await?
        .is_some()
    {
        let client = daemon.client()?;
        let mut project: ProjectView = client
            .post::<(), _>(
                &format!(
                    "/environments/{APPLICATION_ENVIRONMENT}/projects/{}/stop",
                    definition.identity.name
                ),
                None,
            )
            .await
            .map_err(crate::environment_cmd::error)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while project.actual_state != ActualState::Stopped {
            if tokio::time::Instant::now() >= deadline {
                return Err(ComputeError::Runtime(
                    "application did not stop within 30s".into(),
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            project = client
                .get(&format!(
                    "/environments/{APPLICATION_ENVIRONMENT}/projects/{}",
                    definition.identity.name
                ))
                .await
                .map_err(crate::environment_cmd::error)?;
        }
        if json {
            print_json(&project);
        } else {
            println!("{}", definition.identity.name);
            println!("Status      stopped");
            if let Some(deployment) = project.deployment {
                println!("Version     v{}", deployment.version);
                println!("Deployment  {}", deployment.deployment_id);
            }
        }
        return Ok(());
    }
    let (provider_id, provider, mut job) = active_job(&location, &definition.identity)
        .await?
        .ok_or_else(|| {
            ComputeError::Runtime(format!("{} is not running", definition.identity.name))
        })?;
    provider
        .cancel_job(&job.job_id.0)
        .await
        .map_err(crate::provider_error)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !job.status.is_terminal() {
        if tokio::time::Instant::now() >= deadline {
            return Err(ComputeError::Runtime(
                "application did not stop within 30s".into(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        job = provider
            .job_status(&job.job_id.0)
            .await
            .map_err(crate::provider_error)?;
    }
    if json {
        print_json(&serde_json::json!({
            "application": definition.identity,
            "provider": provider_id,
            "job": job,
        }));
    } else {
        println!("{}", definition.identity.name);
        println!("Status      stopped");
        println!("Provider    {provider_id}");
        println!("Job         {}", job.job_id);
        if let Ok(receipt) = provider.job_receipt(&job.job_id.0).await {
            println!("Receipt     {}", receipt.receipt.receipt_hash);
        }
    }
    Ok(())
}

pub async fn logs(command: LogsCommand) -> compute_core::Result<()> {
    let definition = definition(&command.application)?;
    let deployments = application_deployments(&command.daemon, &definition.identity.name).await?;
    if !deployments.is_empty() {
        let selected = match command.version {
            Some(version) => deployments
                .iter()
                .find(|deployment| deployment.record.version == version)
                .ok_or_else(|| ComputeError::Runtime(format!("deployment v{version} not found")))?,
            None => deployments.first().expect("checked"),
        };
        let active = active_deployment(&command.daemon, &definition.identity.name).await?;
        if active
            .as_ref()
            .is_none_or(|(_, deployment)| deployment.deployment_id != selected.deployment_id)
        {
            return Err(ComputeError::Runtime(
                "historical deployment logs are available only while its instance is retained"
                    .into(),
            ));
        }
        let client = command.daemon.client()?;
        let path = format!(
            "/environments/{APPLICATION_ENVIRONMENT}/projects/{}/workloads/{APPLICATION_WORKLOAD}/logs",
            definition.identity.name
        );
        let mut stdout_offset = 0;
        let mut stderr_offset = 0;
        loop {
            let logs: ApplicationLogs = client
                .get(&path)
                .await
                .map_err(crate::environment_cmd::error)?;
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
    let (_, provider, job) = latest_job(&command.location, &definition.identity)
        .await?
        .ok_or_else(|| {
            ComputeError::Runtime(format!(
                "{} has no execution history",
                definition.identity.name
            ))
        })?;
    let mut stdout_offset = 0;
    let mut stderr_offset = 0;
    loop {
        let logs = provider
            .job_logs(&job.job_id.0)
            .await
            .map_err(crate::provider_error)?;
        if command.json {
            print_json(&logs);
            return Ok(());
        }
        print_delta(&logs.stdout, &mut stdout_offset, false)?;
        print_delta(&logs.stderr, &mut stderr_offset, true)?;
        if !command.follow || logs.complete {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

pub async fn history(command: HistoryCommand) -> compute_core::Result<()> {
    let definition = definition(&command.application)?;
    let deployments = application_deployments(&command.daemon, &definition.identity.name).await?;
    if !deployments.is_empty() {
        if command.json {
            print_json(&deployments);
        } else {
            println!("APPLICATION  {}", definition.identity.name);
            println!("VERSION\tSTATUS\tPROVIDER\tCREATED\tDEPLOYMENT");
            for deployment in deployments {
                let provider = deployment
                    .record
                    .workloads
                    .first()
                    .and_then(|workload| workload.provider.as_deref())
                    .unwrap_or("-");
                println!(
                    "v{}\t{}\t{}\t{}\t{}",
                    deployment.record.version,
                    deployment_status(deployment.record.status),
                    provider,
                    deployment.record.created_at.to_rfc3339(),
                    deployment.deployment_id
                );
            }
        }
        return Ok(());
    }
    let jobs = application_jobs(&command.location, &definition.identity).await?;
    if command.json {
        print_json(
            &jobs
                .iter()
                .map(|(provider, _, job)| {
                    serde_json::json!({
                        "provider": provider,
                        "job": job,
                    })
                })
                .collect::<Vec<_>>(),
        );
    } else {
        println!("TIME\tPROVIDER\tRUNTIME\tSTATUS\tJOB");
        for (provider, _, job) in jobs {
            println!(
                "{}\t{}\t{}\t{:?}\t{}",
                job.created_at.to_rfc3339(),
                provider,
                job.request.runtime,
                job.status,
                job.job_id
            );
        }
    }
    Ok(())
}

pub async fn rollback(command: RollbackCommand) -> compute_core::Result<()> {
    let definition = definition(&command.application)?;
    let deployments = application_deployments(&command.daemon, &definition.identity.name).await?;
    let target = if command.deployment.starts_with("dep_") {
        deployments
            .iter()
            .find(|deployment| deployment.deployment_id == command.deployment)
    } else {
        let version = command
            .deployment
            .trim_start_matches('v')
            .parse::<u64>()
            .map_err(|_| {
                ComputeError::InvalidWorkload(
                    "rollback target must be a deployment ID or version".into(),
                )
            })?;
        deployments
            .iter()
            .find(|deployment| deployment.record.version == version)
    }
    .ok_or_else(|| ComputeError::Runtime("rollback deployment not found".into()))?;
    let client = command.daemon.client()?;
    let activation: DeploymentView = client
        .post(
            "/deployments",
            Some(&DeployRequest {
                project: definition.identity.name.clone(),
                environment: APPLICATION_ENVIRONMENT.into(),
                revision: Some(target.record.revision_id.clone()),
                config: Some(target.record.config.clone()),
                desired_state: Some(DesiredState::Running),
            }),
        )
        .await
        .map_err(crate::environment_cmd::error)?;
    let activation = wait_for_deployment(&client, activation.deployment_id).await?;
    print_application_deployment(&command.daemon, &activation, None, command.json);
    Ok(())
}

async fn print_status(
    definition: &Definition,
    provider_id: &str,
    provider: &RemoteProvider,
    job: &ExecutionJob,
    endpoint: Option<String>,
    json: bool,
) -> compute_core::Result<()> {
    let runtime = provider
        .resolve_runtime(ProviderRuntimeRequirement {
            runtime: job.request.runtime,
            version: None,
            platform: None,
        })
        .await
        .ok()
        .and_then(|resolution| resolution.distribution);
    let runtime_version = runtime.as_ref().map(|runtime| runtime.version.clone());
    let platform = runtime.as_ref().map(|runtime| runtime.platform.label());
    let view = ApplicationView {
        application: definition.identity.clone(),
        status: job.status,
        provider: provider_id.into(),
        runtime: job.request.runtime.to_string(),
        runtime_version,
        platform,
        endpoint,
        placement: job
            .placement
            .as_ref()
            .map(|placement| placement.policy.mode.clone()),
        cpu: job.request.requested_execution.resources.cpu_count,
        memory_bytes: job
            .request
            .requested_execution
            .resources
            .memory_required_bytes
            .or(job.request.requested_execution.resources.memory_bytes),
        network: job.request.requested_execution.network.to_string(),
        job_id: job.job_id.to_string(),
        execution_id: job.execution_id.clone(),
        created_at: job.created_at,
    };
    if json {
        print_json(&view);
        return Ok(());
    }
    println!("{}", definition.identity.name);
    println!("Status      {}", status_label(job.status));
    println!("Provider    {provider_id}");
    println!(
        "Runtime     {}{}",
        view.runtime,
        view.runtime_version
            .as_deref()
            .map(|version| format!(" {version}"))
            .unwrap_or_default()
    );
    if let Some(platform) = &view.platform {
        println!("Platform    {platform}");
    }
    if let Some(endpoint) = &view.endpoint {
        println!("Endpoint    {endpoint}");
    }
    println!(
        "Placement   {}",
        view.placement.as_deref().unwrap_or("automatic")
    );
    println!(
        "Resources   {} CPU / {}",
        view.cpu.unwrap_or_default(),
        format_bytes(view.memory_bytes.unwrap_or_default())
    );
    println!("Network     {}", view.network);
    println!(
        "Admission   {}",
        if job.admission.is_some() {
            "admitted"
        } else {
            "pending"
        }
    );
    println!("Job         {}", job.job_id);
    if let Some(execution) = &job.execution_id {
        println!("Execution   {execution}");
    }
    if job.status.is_terminal()
        && let Ok(receipt) = provider.job_receipt(&job.job_id.0).await
    {
        println!("Receipt     {}", receipt.receipt.receipt_hash);
    }
    Ok(())
}

async fn application_jobs(
    location: &pool::PoolLocation,
    application: &ApplicationIdentity,
) -> compute_core::Result<Vec<(String, std::sync::Arc<RemoteProvider>, ExecutionJob)>> {
    let pool = location.pool()?;
    let mut found = Vec::new();
    for member in pool.members() {
        let Some(provider) = &member.jobs else {
            continue;
        };
        let jobs = provider.jobs().await.map_err(crate::provider_error)?;
        found.extend(
            jobs.into_iter()
                .filter(|job| {
                    job.application
                        .as_ref()
                        .is_some_and(|candidate| candidate.id == application.id)
                })
                .map(|job| (member.id.clone(), provider.clone(), job)),
        );
    }
    found.sort_by(|left, right| {
        right
            .2
            .created_at
            .cmp(&left.2.created_at)
            .then_with(|| left.2.job_id.cmp(&right.2.job_id))
    });
    Ok(found)
}

async fn latest_job(
    location: &pool::PoolLocation,
    application: &ApplicationIdentity,
) -> compute_core::Result<Option<(String, std::sync::Arc<RemoteProvider>, ExecutionJob)>> {
    Ok(application_jobs(location, application)
        .await?
        .into_iter()
        .next())
}

async fn active_job(
    location: &pool::PoolLocation,
    application: &ApplicationIdentity,
) -> compute_core::Result<Option<(String, std::sync::Arc<RemoteProvider>, ExecutionJob)>> {
    Ok(application_jobs(location, application)
        .await?
        .into_iter()
        .find(|(_, _, job)| !job.status.is_terminal()))
}

fn application_bundle(definition: &Definition) -> compute_core::Result<WorkloadBundle> {
    let resolved = crate::direct::resolve(crate::direct::DirectOptions {
        path: definition.root.clone(),
        runtime: None,
        args: Vec::new(),
        env: Vec::new(),
        env_file: None,
        inputs: Vec::new(),
        outputs: Vec::new(),
        cwd: None,
        entrypoint: None,
        deps: None,
        network: None,
        isolation: None,
        memory: None,
        timeout: None,
        defaults: admission::PolicyLocation::default().defaults()?,
    })?;
    WorkloadBundle::create_from_with_capsule(
        resolved.workload,
        &resolved.root,
        resolved.dependency_capsule,
    )
}

async fn ensure_application_environment(client: &DaemonClient) -> compute_core::Result<()> {
    match client
        .get::<EnvironmentView>(&format!("/environments/{APPLICATION_ENVIRONMENT}"))
        .await
    {
        Ok(_) => Ok(()),
        Err(EnvironmentError::NotFound(_)) => {
            client
                .post::<_, EnvironmentView>(
                    "/environments",
                    Some(&EnvironmentDefinition {
                        name: APPLICATION_ENVIRONMENT.into(),
                        desired_state: DesiredState::Running,
                        env: Default::default(),
                        policy: None,
                        provider: None,
                    }),
                )
                .await
                .map_err(crate::environment_cmd::error)?;
            Ok(())
        }
        Err(error) => Err(crate::environment_cmd::error(error)),
    }
}

async fn ensure_deployment_daemon(
    daemon: &crate::environment_cmd::DaemonLocation,
) -> compute_core::Result<DaemonClient> {
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
            "could not start the local Compute deployment daemon".into(),
        ));
    }
    let client = daemon.client()?;
    client
        .get::<compute_environment::DaemonStatus>("/status")
        .await
        .map_err(crate::environment_cmd::error)?;
    Ok(client)
}

async fn application_deployments(
    daemon: &crate::environment_cmd::DaemonLocation,
    application: &str,
) -> compute_core::Result<Vec<DeploymentView>> {
    let client = daemon.client()?;
    let query = format!(
        "/deployments?environment={APPLICATION_ENVIRONMENT}&project={application}&limit=50"
    );
    match client.get(&query).await {
        Ok(deployments) => Ok(deployments),
        Err(EnvironmentError::ControllerUnavailable(_) | EnvironmentError::NotFound(_)) => {
            Ok(Vec::new())
        }
        Err(error) => Err(crate::environment_cmd::error(error)),
    }
}

async fn active_deployment(
    daemon: &crate::environment_cmd::DaemonLocation,
    application: &str,
) -> compute_core::Result<Option<(ProjectView, DeploymentView)>> {
    let client = daemon.client()?;
    let project: ProjectView = match client
        .get(&format!(
            "/environments/{APPLICATION_ENVIRONMENT}/projects/{application}"
        ))
        .await
    {
        Ok(project) => project,
        Err(EnvironmentError::ControllerUnavailable(_) | EnvironmentError::NotFound(_)) => {
            return Ok(None);
        }
        Err(error) => return Err(crate::environment_cmd::error(error)),
    };
    let Some(deployment) = project.deployment.as_ref() else {
        return Ok(None);
    };
    let view = client
        .get(&format!("/deployments/{}", deployment.deployment_id))
        .await
        .map_err(crate::environment_cmd::error)?;
    Ok(Some((project, view)))
}

async fn wait_for_deployment(
    client: &DaemonClient,
    deployment_id: String,
) -> compute_core::Result<DeploymentView> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let deployment: DeploymentView = client
            .get(&format!("/deployments/{deployment_id}"))
            .await
            .map_err(crate::environment_cmd::error)?;
        if deployment.record.status.is_terminal() {
            return Ok(deployment);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ComputeError::Runtime(format!(
                "deployment {deployment_id} did not settle within 120s"
            )));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn print_application_deployment(
    daemon: &crate::environment_cmd::DaemonLocation,
    deployment: &DeploymentView,
    actual_state: Option<compute_environment::ActualState>,
    json: bool,
) {
    let workload = deployment.record.workloads.first();
    let endpoint = workload
        .and_then(|workload| workload.endpoints.first())
        .map(|endpoint| application_endpoint(&daemon.endpoint(), endpoint.host));
    let status = actual_state.map_or_else(
        || deployment_status(deployment.record.status),
        |state| state.as_str(),
    );
    if json {
        print_json(&serde_json::json!({
            "application": deployment.record.project,
            "status": status,
            "version": deployment.record.version,
            "endpoint": endpoint,
            "provider": workload.and_then(|workload| workload.provider.as_deref()),
            "deployment_id": deployment.deployment_id,
            "deployment": deployment,
        }));
        return;
    }
    println!("{}", deployment.record.project);
    println!("Status       {status}");
    println!("Version      v{}", deployment.record.version);
    if let Some(endpoint) = endpoint {
        println!("Endpoint     {endpoint}");
    }
    if let Some(provider) = workload.and_then(|workload| workload.provider.as_deref()) {
        println!("Provider     {provider}");
    }
    println!("Deployment   {}", deployment.deployment_id);
    if let Some(instance) = deployment.instances.first() {
        if let Some(started) = instance.record.started_at {
            println!("Started      {}", started.to_rfc3339());
        }
    }
    if let Some(receipt) = &deployment.record.receipt {
        println!("Receipt      {receipt}");
    }
}

fn application_endpoint(daemon_endpoint: &str, port: u16) -> String {
    let (scheme, rest) = daemon_endpoint
        .split_once("://")
        .unwrap_or(("http", daemon_endpoint));
    let authority = rest.split('/').next().unwrap_or(rest);
    let host = authority
        .rsplit_once(':')
        .map_or(authority, |(host, _)| host);
    format!("{scheme}://{host}:{port}")
}

fn deployment_status(status: DeploymentStatus) -> &'static str {
    match status {
        DeploymentStatus::Pending
        | DeploymentStatus::Starting
        | DeploymentStatus::Ready
        | DeploymentStatus::NetworkReady
        | DeploymentStatus::Switching => "deploying",
        DeploymentStatus::Active | DeploymentStatus::Draining | DeploymentStatus::Complete => {
            "running"
        }
        DeploymentStatus::Failed | DeploymentStatus::RolledBack => "failed",
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

fn status_label(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Created | JobStatus::Accepted | JobStatus::Queued => "created",
        JobStatus::WaitingForCapacity => "waiting",
        JobStatus::Reserved | JobStatus::Admitted | JobStatus::Preparing => "deploying",
        JobStatus::Running => "running",
        JobStatus::Succeeded => "completed",
        JobStatus::Cancelled => "stopped",
        JobStatus::Failed | JobStatus::Rejected | JobStatus::TimedOut => "failed",
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes % (1024 * 1024 * 1024) == 0 {
        format!("{} GiB", bytes / (1024 * 1024 * 1024))
    } else if bytes % (1024 * 1024) == 0 {
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
