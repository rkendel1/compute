//! `compute start | stop | status`, `compute environment`, `compute
//! project`, `compute workload`, and `compute execution`.
//!
//! Apart from `compute start`, which runs the daemon, every command is a
//! client of the Compute API: the CLI and the UI use the same lifecycle
//! implementation.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Subcommand};
use compute_core::ComputeError;
use compute_environment::client::{DEFAULT_ENDPOINT, DaemonClient};
use compute_environment::{
    DaemonStatus, DesiredState, EnvironmentDefinition, EnvironmentError, EnvironmentSummary,
    EnvironmentView, ExecutionRecord, ProjectView, WorkloadView,
};

use crate::admission::load_policy;

#[derive(Args, Debug, Clone)]
pub struct DaemonLocation {
    /// Compute API endpoint. Defaults to $COMPUTE_DAEMON, then
    /// http://127.0.0.1:8787.
    #[arg(long = "daemon", global = true)]
    pub endpoint: Option<String>,
    /// Environment variable holding the API bearer token.
    #[arg(long, global = true, default_value = "COMPUTE_DAEMON_TOKEN")]
    pub token_env: String,
}

impl DaemonLocation {
    fn endpoint(&self) -> String {
        self.endpoint
            .clone()
            .or_else(|| std::env::var("COMPUTE_DAEMON").ok())
            .unwrap_or_else(|| DEFAULT_ENDPOINT.into())
    }

    fn client(&self) -> compute_core::Result<DaemonClient> {
        let mut client = DaemonClient::new(&self.endpoint()).map_err(error)?;
        if let Ok(token) = std::env::var(&self.token_env) {
            client = client.with_bearer_token(token);
        }
        Ok(client)
    }
}

fn error(error: EnvironmentError) -> ComputeError {
    ComputeError::Runtime(error.to_string())
}

fn print_json(value: &impl serde::Serialize) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("API values are serializable")
    );
}

fn label(value: &impl serde::Serialize) -> String {
    crate::pool::enum_label(value)
}

fn parse_pair(value: &str) -> Result<(String, String), String> {
    value
        .split_once('=')
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .ok_or_else(|| "expected KEY=VALUE".into())
}

// ---- compute start / stop / status ------------------------------------------

#[derive(Args, Debug)]
pub struct StartCommand {
    #[arg(long, default_value = "127.0.0.1:8787")]
    pub listen: std::net::SocketAddr,
    /// Where environments, bundles, logs, and receipts are kept.
    #[arg(long, default_value = ".compute/daemon")]
    pub state_dir: PathBuf,
    /// Daemon-wide execution policy, intersected with every environment's.
    #[arg(long)]
    pub policy: Option<PathBuf>,
    /// Provider pool for tasks (TOML). `local` is always this daemon.
    #[arg(long)]
    pub pool_config: Option<PathBuf>,
    /// Host ports available for logical port bindings, as LOW-HIGH.
    #[arg(long, default_value = "20000-29999")]
    pub port_range: String,
    /// Environment variable holding the token required for mutations.
    /// Without it, the API accepts every request; bind to localhost.
    #[arg(long)]
    pub require_token_env: Option<String>,
    /// Run in the background and return once the API answers.
    #[arg(long)]
    pub detach: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct DaemonCommand {
    #[command(flatten)]
    pub daemon: DaemonLocation,
    #[arg(long)]
    pub json: bool,
}

/// Mutations require the configured token; reads are open to anyone who
/// can reach the API.
struct TokenAuthorizer {
    token: String,
}

#[async_trait::async_trait]
impl compute_provider::ProviderAuthorizer for TokenAuthorizer {
    async fn authorize(
        &self,
        operation: compute_provider::ProviderOperation,
        authorization: Option<&str>,
    ) -> Result<(), compute_provider::ProviderError> {
        if operation == compute_provider::ProviderOperation::EnvironmentRead
            || authorization == Some(format!("Bearer {}", self.token).as_str())
        {
            Ok(())
        } else {
            Err(compute_provider::ProviderError::new(
                compute_provider::ProviderErrorKind::Unauthorized,
                "this daemon requires a bearer token for changes",
            ))
        }
    }
}

pub async fn start(command: StartCommand) -> compute_core::Result<()> {
    if command.detach {
        return detach(&command);
    }
    let (low, high) = command
        .port_range
        .split_once('-')
        .and_then(|(low, high)| Some((low.parse::<u16>().ok()?, high.parse::<u16>().ok()?)))
        .filter(|(low, high)| low <= high)
        .ok_or_else(|| ComputeError::InvalidWorkload("--port-range must be LOW-HIGH".into()))?;
    let mut config = compute_environment::DaemonConfig::new(&command.state_dir);
    config.port_range = (low, high);
    config.policy = command.policy.as_deref().map(load_policy).transpose()?;
    config.pool = command
        .pool_config
        .as_deref()
        .map(compute_placement::PoolConfig::load)
        .transpose()
        .map_err(|error| ComputeError::InvalidWorkload(error.to_string()))?;
    let authorizer: std::sync::Arc<dyn compute_provider::ProviderAuthorizer> =
        match &command.require_token_env {
            Some(name) => std::sync::Arc::new(TokenAuthorizer {
                token: std::env::var(name)
                    .map_err(|_| ComputeError::InvalidWorkload(format!("{name} is not set")))?,
            }),
            None => std::sync::Arc::new(compute_provider::AllowAllAuthorizer),
        };
    let listener = tokio::net::TcpListener::bind(command.listen).await?;
    let daemon = compute_environment::Daemon::start(config)
        .await
        .map_err(error)?;
    let status = daemon.status().await;
    if command.json {
        print_json(&status);
    } else {
        eprintln!(
            "Compute daemon {} listening on http://{} (state: {})",
            status.instance_id,
            command.listen,
            command.state_dir.display()
        );
    }
    let server: tokio::task::JoinHandle<std::io::Result<()>> = tokio::spawn(
        compute_environment::api::serve(listener, daemon.clone(), authorizer),
    );
    tokio::select! {
        result = server => match result {
            Ok(outcome) => outcome.map_err(ComputeError::Io)?,
            Err(join) => return Err(ComputeError::Runtime(join.to_string())),
        },
        _ = tokio::signal::ctrl_c() => {
            daemon.shutdown().await;
        }
    }
    Ok(())
}

/// Start the daemon as a background process of this executable and wait
/// until its API answers. No service manager is required.
fn detach(command: &StartCommand) -> compute_core::Result<()> {
    std::fs::create_dir_all(&command.state_dir)?;
    let log = std::fs::File::create(command.state_dir.join("daemon.log"))?;
    let mut child = std::process::Command::new(std::env::current_exe()?);
    child
        .arg("start")
        .arg("--listen")
        .arg(command.listen.to_string())
        .arg("--state-dir")
        .arg(&command.state_dir)
        .arg("--port-range")
        .arg(&command.port_range);
    if let Some(policy) = &command.policy {
        child.arg("--policy").arg(policy);
    }
    if let Some(pool) = &command.pool_config {
        child.arg("--pool-config").arg(pool);
    }
    if let Some(name) = &command.require_token_env {
        child.arg("--require-token-env").arg(name);
    }
    child
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        child.process_group(0);
    }
    let spawned = child.spawn()?;
    let endpoint = format!("http://{}", command.listen);
    let client = DaemonClient::new(&endpoint).map_err(error)?;
    let runtime = tokio::runtime::Handle::current();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let status: Result<DaemonStatus, _> =
            tokio::task::block_in_place(|| runtime.block_on(client.get("/status")));
        if let Ok(status) = status {
            if command.json {
                print_json(&status);
            } else {
                println!(
                    "Compute daemon {} started (pid {}) at {endpoint}",
                    status.instance_id,
                    spawned.id()
                );
            }
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(ComputeError::Runtime(format!(
                "the daemon did not answer at {endpoint}; see {}",
                command.state_dir.join("daemon.log").display()
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub async fn stop(command: DaemonCommand) -> compute_core::Result<()> {
    let client = command.daemon.client()?;
    let _: serde_json::Value = client
        .post::<(), _>("/shutdown", None)
        .await
        .map_err(error)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while client.get::<DaemonStatus>("/status").await.is_ok() {
        if tokio::time::Instant::now() >= deadline {
            return Err(ComputeError::Runtime("the daemon did not stop".into()));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if command.json {
        print_json(&serde_json::json!({ "stopped": true }));
    } else {
        println!("Compute daemon stopped; desired state is kept for the next start");
    }
    Ok(())
}

pub async fn status(command: DaemonCommand) -> compute_core::Result<()> {
    let status: DaemonStatus = command
        .daemon
        .client()?
        .get("/status")
        .await
        .map_err(error)?;
    if command.json {
        print_json(&status);
    } else {
        println!("Compute daemon: running");
        println!("Instance: {}", status.instance_id);
        println!("Version: {}", status.version);
        println!("PID: {}", status.pid);
        println!("Started: {}", status.started_at.to_rfc3339());
        println!("State: {}", status.state_dir);
        println!("Environments: {}", status.environments);
        println!("Running services: {}", status.running_services);
    }
    Ok(())
}

// ---- compute environment -----------------------------------------------------

#[derive(Args, Debug)]
pub struct EnvironmentCommand {
    #[command(subcommand)]
    pub command: EnvironmentCommands,
    #[command(flatten)]
    pub daemon: DaemonLocation,
}

#[derive(Subcommand, Debug)]
pub enum EnvironmentCommands {
    /// List environments.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Create an environment.
    Create {
        name: String,
        /// Environment execution policy (compute.policy@1 JSON).
        #[arg(long = "environment-policy")]
        policy: Option<PathBuf>,
        /// Pin workloads to a provider in the daemon's pool.
        #[arg(long)]
        provider: Option<String>,
        #[arg(long = "set", value_parser = parse_pair)]
        env: Vec<(String, String)>,
        /// Create without starting.
        #[arg(long)]
        stopped: bool,
        #[arg(long)]
        json: bool,
    },
    /// Create or update an environment and its projects from a manifest.
    Apply {
        manifest: PathBuf,
        #[arg(long)]
        json: bool,
    },
    Inspect {
        environment: String,
        #[arg(long)]
        json: bool,
    },
    Status {
        environment: String,
        #[arg(long)]
        json: bool,
    },
    /// Start an environment, or `PROJECT/ENVIRONMENT`.
    Start {
        target: String,
        #[arg(long)]
        json: bool,
    },
    /// Stop an environment, or `PROJECT/ENVIRONMENT`.
    Stop {
        target: String,
        #[arg(long)]
        json: bool,
    },
    /// Restart an environment, or `PROJECT/ENVIRONMENT`.
    Restart {
        target: String,
        #[arg(long)]
        json: bool,
    },
    /// Stop and delete an environment and all of its state.
    Destroy {
        environment: String,
        #[arg(long)]
        json: bool,
    },
}

pub async fn environment(command: EnvironmentCommand) -> compute_core::Result<()> {
    let client = command.daemon.client()?;
    match command.command {
        EnvironmentCommands::List { json } => {
            let environments: Vec<EnvironmentSummary> =
                client.get("/environments").await.map_err(error)?;
            if json {
                print_json(&environments);
            } else {
                println!("ENVIRONMENT\tPROJECTS\tSTATE\tHEALTH");
                for environment in environments {
                    println!(
                        "{}\t{}\t{}\t{}",
                        environment.name,
                        environment.project_count,
                        environment.actual_state.as_str(),
                        label(&environment.health)
                    );
                }
            }
        }
        EnvironmentCommands::Create {
            name,
            policy,
            provider,
            env,
            stopped,
            json,
        } => {
            let definition = EnvironmentDefinition {
                name,
                desired_state: if stopped {
                    DesiredState::Stopped
                } else {
                    DesiredState::Running
                },
                env: env.into_iter().collect::<BTreeMap<_, _>>(),
                policy: policy.as_deref().map(load_policy).transpose()?,
                provider,
            };
            let view: EnvironmentView = client
                .post("/environments", Some(&definition))
                .await
                .map_err(error)?;
            print_environment(&view, json);
        }
        EnvironmentCommands::Apply { manifest, json } => {
            let (definition, projects) =
                compute_environment::manifest::load_environment(&manifest).map_err(error)?;
            let name = definition.name.clone();
            match client
                .get::<EnvironmentView>(&format!("/environments/{name}"))
                .await
            {
                Ok(_) => {
                    let action = match definition.desired_state {
                        DesiredState::Running => "start",
                        DesiredState::Stopped => "stop",
                    };
                    let _: EnvironmentView = client
                        .post::<(), _>(&format!("/environments/{name}/{action}"), None)
                        .await
                        .map_err(error)?;
                }
                Err(EnvironmentError::NotFound(_)) => {
                    let _: EnvironmentView = client
                        .post("/environments", Some(&definition))
                        .await
                        .map_err(error)?;
                }
                Err(other) => return Err(error(other)),
            }
            for project in projects {
                let _: ProjectView = client
                    .post(&format!("/environments/{name}/projects"), Some(&project))
                    .await
                    .map_err(error)?;
            }
            let view: EnvironmentView = client
                .get(&format!("/environments/{name}"))
                .await
                .map_err(error)?;
            print_environment(&view, json);
        }
        EnvironmentCommands::Inspect { environment, json }
        | EnvironmentCommands::Status { environment, json } => {
            let view: EnvironmentView = client
                .get(&format!("/environments/{environment}"))
                .await
                .map_err(error)?;
            print_environment(&view, json);
        }
        EnvironmentCommands::Start { target, json } => {
            lifecycle(&client, &target, "start", json).await?;
        }
        EnvironmentCommands::Stop { target, json } => {
            lifecycle(&client, &target, "stop", json).await?;
        }
        EnvironmentCommands::Restart { target, json } => {
            lifecycle(&client, &target, "restart", json).await?;
        }
        EnvironmentCommands::Destroy { environment, json } => {
            let value: serde_json::Value = client
                .delete(&format!("/environments/{environment}"))
                .await
                .map_err(error)?;
            if json {
                print_json(&value);
            } else {
                println!("Destroyed environment {environment}");
            }
        }
    }
    Ok(())
}

/// `ENVIRONMENT` or `PROJECT/ENVIRONMENT`.
async fn lifecycle(
    client: &DaemonClient,
    target: &str,
    action: &str,
    json: bool,
) -> compute_core::Result<()> {
    match target.split_once('/') {
        Some((project, environment)) => {
            let view: ProjectView = client
                .post::<(), _>(
                    &format!("/environments/{environment}/projects/{project}/{action}"),
                    None,
                )
                .await
                .map_err(error)?;
            print_project(environment, &view, json);
        }
        None => {
            let view: EnvironmentView = client
                .post::<(), _>(&format!("/environments/{target}/{action}"), None)
                .await
                .map_err(error)?;
            print_environment(&view, json);
        }
    }
    Ok(())
}

fn print_environment(view: &EnvironmentView, json: bool) {
    if json {
        print_json(view);
        return;
    }
    println!("Environment: {} ({})", view.name, view.environment_id);
    println!(
        "State: desired {}, actual {}, health {}",
        label(&view.desired_state),
        view.actual_state.as_str(),
        label(&view.health)
    );
    println!("Policy: {}", view.policy_id);
    if let Some(provider) = &view.provider {
        println!("Provider: {provider}");
    }
    println!("Disk: {} bytes", view.disk_bytes);
    println!("\nPROJECT\tREVISION\tDESIRED\tACTUAL\tHEALTH");
    for project in &view.projects {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            project.name,
            project.revision,
            label(&project.desired_state),
            project.actual_state.as_str(),
            label(&project.health)
        );
    }
}

// ---- compute project ----------------------------------------------------------

#[derive(Args, Debug)]
pub struct ProjectCommand {
    #[command(subcommand)]
    pub command: ProjectCommands,
    #[command(flatten)]
    pub daemon: DaemonLocation,
}

#[derive(Subcommand, Debug)]
pub enum ProjectCommands {
    List {
        #[arg(long)]
        environment: String,
        #[arg(long)]
        json: bool,
    },
    /// Add a project to an environment, or deploy a new revision of it.
    Add {
        name: String,
        #[arg(long)]
        environment: String,
        /// Directory containing compute.project.toml. Defaults to ./NAME.
        #[arg(long)]
        source: Option<PathBuf>,
        #[arg(long)]
        revision: Option<String>,
        #[arg(long = "set", value_parser = parse_pair)]
        env: Vec<(String, String)>,
        /// Add without starting.
        #[arg(long)]
        stopped: bool,
        #[arg(long)]
        json: bool,
    },
    Remove {
        name: String,
        #[arg(long)]
        environment: String,
        #[arg(long)]
        json: bool,
    },
    Inspect {
        name: String,
        #[arg(long)]
        environment: String,
        #[arg(long)]
        json: bool,
    },
    Status {
        name: String,
        #[arg(long)]
        environment: String,
        #[arg(long)]
        json: bool,
    },
    Start {
        name: String,
        #[arg(long)]
        environment: String,
        #[arg(long)]
        json: bool,
    },
    Stop {
        name: String,
        #[arg(long)]
        environment: String,
        #[arg(long)]
        json: bool,
    },
    Restart {
        name: String,
        #[arg(long)]
        environment: String,
        #[arg(long)]
        json: bool,
    },
}

pub async fn project(command: ProjectCommand) -> compute_core::Result<()> {
    let client = command.daemon.client()?;
    match command.command {
        ProjectCommands::List { environment, json } => {
            let projects: Vec<ProjectView> = client
                .get(&format!("/environments/{environment}/projects"))
                .await
                .map_err(error)?;
            if json {
                print_json(&projects);
            } else {
                println!("PROJECT\tREVISION\tDESIRED\tACTUAL\tHEALTH");
                for project in projects {
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        project.name,
                        project.revision,
                        label(&project.desired_state),
                        project.actual_state.as_str(),
                        label(&project.health)
                    );
                }
            }
        }
        ProjectCommands::Add {
            name,
            environment,
            source,
            revision,
            env,
            stopped,
            json,
        } => {
            let source = source.unwrap_or_else(|| PathBuf::from(&name));
            let mut definition =
                compute_environment::manifest::load_project(&source).map_err(error)?;
            if definition.name != name {
                return Err(ComputeError::InvalidWorkload(format!(
                    "{} declares project {}, not {name}",
                    source.display(),
                    definition.name
                )));
            }
            if let Some(revision) = revision {
                definition.revision = revision;
            }
            definition.env.extend(env);
            if stopped {
                definition.desired_state = DesiredState::Stopped;
            }
            let view: ProjectView = client
                .post(
                    &format!("/environments/{environment}/projects"),
                    Some(&definition),
                )
                .await
                .map_err(error)?;
            print_project(&environment, &view, json);
        }
        ProjectCommands::Remove {
            name,
            environment,
            json,
        } => {
            let value: serde_json::Value = client
                .delete(&format!("/environments/{environment}/projects/{name}"))
                .await
                .map_err(error)?;
            if json {
                print_json(&value);
            } else {
                println!("Removed {name} from {environment}");
            }
        }
        ProjectCommands::Inspect {
            name,
            environment,
            json,
        }
        | ProjectCommands::Status {
            name,
            environment,
            json,
        } => {
            let view: ProjectView = client
                .get(&format!("/environments/{environment}/projects/{name}"))
                .await
                .map_err(error)?;
            print_project(&environment, &view, json);
        }
        ProjectCommands::Start {
            name,
            environment,
            json,
        } => lifecycle(&client, &format!("{name}/{environment}"), "start", json).await?,
        ProjectCommands::Stop {
            name,
            environment,
            json,
        } => lifecycle(&client, &format!("{name}/{environment}"), "stop", json).await?,
        ProjectCommands::Restart {
            name,
            environment,
            json,
        } => lifecycle(&client, &format!("{name}/{environment}"), "restart", json).await?,
    }
    Ok(())
}

fn print_project(environment: &str, view: &ProjectView, json: bool) {
    if json {
        print_json(view);
        return;
    }
    println!(
        "{} / {}",
        view.name.to_uppercase(),
        environment.to_uppercase()
    );
    println!("Project ID: {}", view.project_id);
    println!("Revision: {} ({})", view.revision, view.revision_digest);
    println!(
        "State: desired {}, actual {}, health {}",
        label(&view.desired_state),
        view.actual_state.as_str(),
        label(&view.health)
    );
    println!("\nWORKLOAD\tKIND\tDESIRED\tACTUAL\tHEALTH\tPORTS\tEXECUTION");
    for workload in &view.workloads {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            workload.name,
            workload.kind.as_str(),
            label(&workload.desired_state),
            workload.actual_state.as_str(),
            label(&workload.health),
            if workload.ports.is_empty() {
                "-".to_string()
            } else {
                workload
                    .ports
                    .iter()
                    .map(|port| format!("{}:{}→{}", port.name, port.logical, port.host))
                    .collect::<Vec<_>>()
                    .join(",")
            },
            workload.execution_id.as_deref().unwrap_or("-")
        );
    }
}

// ---- compute workload / execution ---------------------------------------------

#[derive(Args, Debug)]
pub struct WorkloadCommand {
    #[command(subcommand)]
    pub command: WorkloadCommands,
    #[command(flatten)]
    pub daemon: DaemonLocation,
}

#[derive(Args, Debug)]
pub struct WorkloadTarget {
    /// `PROJECT/WORKLOAD`
    pub target: String,
    #[arg(long)]
    pub environment: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Subcommand, Debug)]
pub enum WorkloadCommands {
    Inspect(WorkloadTarget),
    Start(WorkloadTarget),
    Stop(WorkloadTarget),
    Restart(WorkloadTarget),
    /// Run a task to completion.
    Run(WorkloadTarget),
    /// Show the most recent output of a workload.
    Logs(WorkloadTarget),
}

pub async fn workload(command: WorkloadCommand) -> compute_core::Result<()> {
    let client = command.daemon.client()?;
    let (action, target) = match command.command {
        WorkloadCommands::Inspect(target) => ("inspect", target),
        WorkloadCommands::Start(target) => ("start", target),
        WorkloadCommands::Stop(target) => ("stop", target),
        WorkloadCommands::Restart(target) => ("restart", target),
        WorkloadCommands::Run(target) => ("run", target),
        WorkloadCommands::Logs(target) => ("logs", target),
    };
    let (project, workload) = target
        .target
        .split_once('/')
        .ok_or_else(|| ComputeError::InvalidWorkload("the workload is PROJECT/WORKLOAD".into()))?;
    let base = format!(
        "/environments/{}/projects/{project}/workloads/{workload}",
        target.environment
    );
    match action {
        "inspect" => {
            let view: WorkloadView = client.get(&base).await.map_err(error)?;
            print_workload(&view, target.json);
        }
        "run" => {
            let record: ExecutionRecord = client
                .post::<(), _>(&format!("{base}/run"), None)
                .await
                .map_err(error)?;
            print_execution(&record, target.json);
            if record.exit_code.is_some_and(|code| code != 0) || record.status != "completed" {
                std::process::exit(record.exit_code.filter(|code| *code != 0).unwrap_or(1));
            }
        }
        "logs" => {
            let logs: serde_json::Value =
                client.get(&format!("{base}/logs")).await.map_err(error)?;
            if target.json {
                print_json(&logs);
            } else {
                print!("{}", logs["stdout"].as_str().unwrap_or_default());
                eprint!("{}", logs["stderr"].as_str().unwrap_or_default());
            }
        }
        action => {
            let view: WorkloadView = client
                .post::<(), _>(&format!("{base}/{action}"), None)
                .await
                .map_err(error)?;
            print_workload(&view, target.json);
        }
    }
    Ok(())
}

fn print_workload(view: &WorkloadView, json: bool) {
    if json {
        print_json(view);
        return;
    }
    println!(
        "Workload: {} ({}, {})",
        view.name,
        view.kind.as_str(),
        view.workload_id
    );
    println!(
        "State: desired {}, actual {}, health {}",
        label(&view.desired_state),
        view.actual_state.as_str(),
        label(&view.health)
    );
    println!("Runtime: {}", view.runtime);
    println!("Bundle: {}", view.bundle_id);
    if let Some(execution) = &view.execution_id {
        println!("Execution: {execution}");
    }
    for port in &view.ports {
        println!(
            "Port {}: logical {} → host {}",
            port.name, port.logical, port.host
        );
    }
    if let Some(provider) = &view.placement.provider {
        println!("Provider: {provider}");
    }
    if let Some(policy) = &view.evidence.policy_id {
        println!("Policy: {policy}");
    }
    if let Some(admission) = &view.evidence.admission_id {
        println!("Admission: {admission}");
    }
    for receipt in &view.evidence.receipt_ids {
        println!("Receipt: {receipt}");
    }
    if let Some(error) = &view.error {
        println!("Error: {error}");
    }
}

#[derive(Args, Debug)]
pub struct ExecutionCommand {
    pub execution_id: String,
    #[command(flatten)]
    pub daemon: DaemonLocation,
    #[arg(long)]
    pub json: bool,
}

pub async fn execution(command: ExecutionCommand) -> compute_core::Result<()> {
    let record: ExecutionRecord = command
        .daemon
        .client()?
        .get(&format!("/executions/{}", command.execution_id))
        .await
        .map_err(error)?;
    print_execution(&record, command.json);
    Ok(())
}

fn print_execution(record: &ExecutionRecord, json: bool) {
    if json {
        print_json(record);
        return;
    }
    if !record.stdout.is_empty() {
        print!("{}", record.stdout);
    }
    if !record.stderr.is_empty() {
        eprint!("{}", record.stderr);
    }
    eprintln!(
        "\nexecution {} ({}/{}/{}): {}{}",
        record.execution_id,
        record.environment,
        record.project,
        record.workload,
        record.status,
        record
            .exit_code
            .map(|code| format!(", exit {code}"))
            .unwrap_or_default()
    );
}
