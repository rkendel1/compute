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
    DaemonStatus, DeployRequest, DeploymentStatus, DeploymentView, DesiredState,
    EnvironmentDefinition, EnvironmentError, EnvironmentSummary, EnvironmentView, EventRecord,
    ExecutionRecord, ExecutionView, ProjectDetail, ProjectSummary, ProjectView, PromoteRequest,
    ProviderRecord, ReceiptRecord, RevisionView, ServiceDefinition, ServiceRecord, WorkloadView,
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
    pub fn endpoint(&self) -> String {
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
    /// Node-local data: artifact cache, logs, and the daemon lock (and,
    /// for the file backend, control state).
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
    /// How often the reconciler rereads desired state, in milliseconds.
    #[arg(long, default_value_t = 5000)]
    pub reconcile_interval_ms: u64,
    #[command(flatten)]
    pub state: crate::control_state::StateOptions,
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
    let backend = command.state.open(&command.state_dir).await?;
    let mut config = compute_environment::DaemonConfig::new(
        &command.state_dir,
        backend.state,
        backend.artifacts,
    );
    config.port_range = (low, high);
    config.reconcile_interval = Duration::from_millis(command.reconcile_interval_ms.max(100));
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
            "Compute daemon {} listening on http://{} (control state: {} {}; UI: http://{}/ui/)",
            status.instance_id,
            command.listen,
            status.state.kind,
            status.state.location,
            command.listen
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
        .arg(&command.port_range)
        .arg("--reconcile-interval-ms")
        .arg(command.reconcile_interval_ms.to_string())
        .args(command.state.arguments());
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
        println!("Node directory: {}", status.state_dir);
        println!(
            "Control state: {} ({}){}",
            status.state.kind,
            status.state.location,
            if status.state_available {
                String::new()
            } else {
                format!(
                    " — UNAVAILABLE: {}",
                    status.state_error.clone().unwrap_or_default()
                )
            }
        );
        println!("Artifacts: {}", status.artifacts);
        if let Some(at) = status.last_reconciled_at {
            println!("Last reconciled: {}", at.to_rfc3339());
        }
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
                println!("ENVIRONMENT\tPROJECTS\tWORKLOADS\tSTATE\tHEALTH\tPROVIDER");
                for environment in environments {
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}",
                        environment.name,
                        environment.project_count,
                        environment.workload_count,
                        environment.actual_state.as_str(),
                        label(&environment.health),
                        environment.provider
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
    /// List projects: every project across environments, or the projects
    /// in one environment.
    List {
        #[arg(long)]
        environment: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Register a revision from compute.project.toml and deploy it to an
    /// environment.
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
    /// Register an immutable revision without deploying it.
    Push {
        name: String,
        /// Directory containing compute.project.toml. Defaults to ./NAME.
        #[arg(long)]
        source: Option<PathBuf>,
        #[arg(long)]
        revision: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// A project's registered revisions, newest first.
    Revisions {
        name: String,
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
    /// Inspect a project in one environment, or across all of them.
    Inspect {
        name: String,
        #[arg(long)]
        environment: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Status {
        name: String,
        #[arg(long)]
        environment: Option<String>,
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
    /// Recent executions of a project in an environment.
    Executions {
        name: String,
        #[arg(long)]
        environment: String,
        #[arg(long)]
        json: bool,
    },
    /// Receipt references of a project in an environment.
    Receipts {
        name: String,
        #[arg(long)]
        environment: String,
        #[arg(long)]
        json: bool,
    },
}

/// Load compute.project.toml from `source` (default ./NAME) as a revision.
fn load_revision(
    name: &str,
    source: Option<PathBuf>,
    revision: Option<String>,
) -> compute_core::Result<compute_environment::ProjectDefinition> {
    let source = source.unwrap_or_else(|| PathBuf::from(name));
    let mut definition = compute_environment::manifest::load_project(&source).map_err(error)?;
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
    Ok(definition)
}

pub async fn project(command: ProjectCommand) -> compute_core::Result<()> {
    let client = command.daemon.client()?;
    match command.command {
        ProjectCommands::List {
            environment: Some(environment),
            json,
        } => {
            let projects: Vec<ProjectView> = client
                .get(&format!("/environments/{environment}/projects"))
                .await
                .map_err(error)?;
            if json {
                print_json(&projects);
            } else {
                println!("PROJECT\tREVISION\tDESIRED\tACTUAL\tHEALTH\tDEPLOYMENT");
                for project in projects {
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}",
                        project.name,
                        project.revision,
                        label(&project.desired_state),
                        project.actual_state.as_str(),
                        label(&project.health),
                        project
                            .deployment
                            .as_ref()
                            .map_or("-", |deployment| deployment.status.as_str())
                    );
                }
            }
        }
        ProjectCommands::List {
            environment: None,
            json,
        } => {
            let projects: Vec<ProjectSummary> = client.get("/projects").await.map_err(error)?;
            if json {
                print_json(&projects);
            } else {
                println!("PROJECT\tREVISIONS\tENVIRONMENTS");
                for project in projects {
                    println!(
                        "{}\t{}\t{}",
                        project.name,
                        project.revision_count,
                        project
                            .environments
                            .iter()
                            .map(|placement| format!(
                                "{}@{} ({})",
                                placement.environment,
                                placement.revision,
                                placement.actual_state.as_str()
                            ))
                            .collect::<Vec<_>>()
                            .join(", ")
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
            let mut definition = load_revision(&name, source, revision)?;
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
        ProjectCommands::Push {
            name,
            source,
            revision,
            json,
        } => {
            let definition = load_revision(&name, source, revision)?;
            let view: RevisionView = client
                .post(
                    &format!("/projects/{name}/revisions"),
                    Some(&definition.revision_definition()),
                )
                .await
                .map_err(error)?;
            if json {
                print_json(&view);
            } else {
                println!(
                    "{name} revision {} ({}): {}",
                    view.revision, view.revision_id, view.revision_digest
                );
            }
        }
        ProjectCommands::Revisions { name, json } => {
            let revisions: Vec<RevisionView> = client
                .get(&format!("/projects/{name}/revisions"))
                .await
                .map_err(error)?;
            if json {
                print_json(&revisions);
            } else {
                println!("REVISION\tID\tDIGEST\tCREATED");
                for revision in revisions {
                    println!(
                        "{}\t{}\t{}\t{}",
                        revision.revision,
                        revision.revision_id,
                        revision.revision_digest,
                        revision.created_at.to_rfc3339()
                    );
                }
            }
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
            environment: Some(environment),
            json,
        }
        | ProjectCommands::Status {
            name,
            environment: Some(environment),
            json,
        } => {
            let view: ProjectView = client
                .get(&format!("/environments/{environment}/projects/{name}"))
                .await
                .map_err(error)?;
            print_project(&environment, &view, json);
        }
        ProjectCommands::Inspect {
            name,
            environment: None,
            json,
        }
        | ProjectCommands::Status {
            name,
            environment: None,
            json,
        } => {
            let detail: ProjectDetail = client
                .get(&format!("/projects/{name}"))
                .await
                .map_err(error)?;
            if json {
                print_json(&detail);
            } else {
                let summary = &detail.summary;
                println!("Project: {} ({})", summary.name, summary.project_id);
                println!("Revisions: {}", summary.revision_count);
                println!("\nENVIRONMENT\tREVISION\tDESIRED\tACTUAL\tHEALTH\tDEPLOYMENT");
                for placement in &summary.environments {
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}",
                        placement.environment,
                        placement.revision,
                        label(&placement.desired_state),
                        placement.actual_state.as_str(),
                        label(&placement.health),
                        placement
                            .deployment
                            .as_ref()
                            .map_or("-", |deployment| deployment.status.as_str())
                    );
                }
            }
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
        ProjectCommands::Executions {
            name,
            environment,
            json,
        } => {
            let executions: Vec<ExecutionRecord> = client
                .get(&format!(
                    "/environments/{environment}/projects/{name}/executions"
                ))
                .await
                .map_err(error)?;
            if json {
                print_json(&executions);
            } else {
                println!("EXECUTION\tWORKLOAD\tSTATUS\tEXIT\tSTARTED");
                for execution in executions {
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        execution.execution_id,
                        execution.workload,
                        execution.status,
                        execution
                            .exit_code
                            .map_or_else(|| "-".into(), |code| code.to_string()),
                        execution.started_at.to_rfc3339()
                    );
                }
            }
        }
        ProjectCommands::Receipts {
            name,
            environment,
            json,
        } => {
            let receipts: Vec<ReceiptRecord> = client
                .get(&format!(
                    "/environments/{environment}/projects/{name}/receipts"
                ))
                .await
                .map_err(error)?;
            if json {
                print_json(&receipts);
            } else {
                println!("RECEIPT\tEXECUTION\tADMISSION");
                for receipt in receipts {
                    println!(
                        "{}\t{}\t{}",
                        receipt.receipt_id,
                        receipt.execution_id,
                        receipt.admission_id.as_deref().unwrap_or("-")
                    );
                }
            }
        }
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
    if let Some(deployment) = &view.deployment {
        println!(
            "Deployment: {} ({})",
            deployment.deployment_id,
            deployment.status.as_str()
        );
    }
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
            let view: ExecutionView = client
                .post::<(), _>(&format!("{base}/run"), None)
                .await
                .map_err(error)?;
            print_execution(&view, target.json);
            let record = &view.record;
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
    let record: ExecutionView = command
        .daemon
        .client()?
        .get(&format!("/executions/{}", command.execution_id))
        .await
        .map_err(error)?;
    print_execution(&record, command.json);
    Ok(())
}

fn print_execution(view: &ExecutionView, json: bool) {
    if json {
        print_json(view);
        return;
    }
    if !view.stdout.is_empty() {
        print!("{}", view.stdout);
    }
    if !view.stderr.is_empty() {
        eprint!("{}", view.stderr);
    }
    let record = &view.record;
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

// ---- compute deploy / promote / deployment ------------------------------------

#[derive(Args, Debug)]
pub struct DeployCommand {
    pub project: String,
    #[arg(long)]
    pub environment: String,
    /// A registered revision label or ID. With --source, the revision to
    /// register. Defaults to the latest registered revision.
    #[arg(long)]
    pub revision: Option<String>,
    /// Register the revision in this directory's compute.project.toml
    /// first.
    #[arg(long)]
    pub source: Option<PathBuf>,
    /// Replace the project's configuration in this environment.
    #[arg(long = "set", value_parser = parse_pair)]
    pub env: Vec<(String, String)>,
    /// Wait until the deployment is healthy or has failed.
    #[arg(long)]
    pub wait: bool,
    #[command(flatten)]
    pub daemon: DaemonLocation,
    #[arg(long)]
    pub json: bool,
}

pub async fn deploy(command: DeployCommand) -> compute_core::Result<()> {
    let client = command.daemon.client()?;
    let mut config = None;
    let revision = match &command.source {
        Some(source) => {
            let definition = load_revision(
                &command.project,
                Some(source.clone()),
                command.revision.clone(),
            )?;
            if !definition.env.is_empty() {
                config = Some(definition.env.clone());
            }
            let view: RevisionView = client
                .post(
                    &format!("/projects/{}/revisions", command.project),
                    Some(&definition.revision_definition()),
                )
                .await
                .map_err(error)?;
            Some(view.revision_id)
        }
        None => command.revision.clone(),
    };
    if !command.env.is_empty() {
        config
            .get_or_insert_with(BTreeMap::new)
            .extend(command.env.clone());
    }
    let request = DeployRequest {
        project: command.project.clone(),
        environment: command.environment.clone(),
        revision,
        config,
        desired_state: None,
    };
    let deployment: DeploymentView = client
        .post("/deployments", Some(&request))
        .await
        .map_err(error)?;
    finish_deployment(&client, deployment, command.wait, command.json).await
}

#[derive(Args, Debug)]
pub struct PromoteCommand {
    pub project: String,
    #[arg(long)]
    pub from: String,
    #[arg(long)]
    pub to: String,
    /// Promote even when the source deployment is not healthy.
    #[arg(long)]
    pub allow_unhealthy: bool,
    #[arg(long)]
    pub wait: bool,
    #[command(flatten)]
    pub daemon: DaemonLocation,
    #[arg(long)]
    pub json: bool,
}

pub async fn promote(command: PromoteCommand) -> compute_core::Result<()> {
    let client = command.daemon.client()?;
    let request = PromoteRequest {
        project: command.project,
        from: command.from,
        to: command.to,
        allow_unhealthy: command.allow_unhealthy,
    };
    let deployment: DeploymentView = client
        .post("/deployments/promote", Some(&request))
        .await
        .map_err(error)?;
    finish_deployment(&client, deployment, command.wait, command.json).await
}

/// Report a deployment, optionally following it to a settled state. A
/// failed deployment exits non-zero: no silent deployment.
async fn finish_deployment(
    client: &DaemonClient,
    mut deployment: DeploymentView,
    wait: bool,
    json: bool,
) -> compute_core::Result<()> {
    if wait {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
        let mut last = None;
        loop {
            let status = deployment.record.status;
            if !json && last != Some(status) {
                eprintln!("{}: {}", deployment.deployment_id, status.as_str());
                last = Some(status);
            }
            if matches!(
                status,
                DeploymentStatus::Healthy
                    | DeploymentStatus::Failed
                    | DeploymentStatus::Stopped
                    | DeploymentStatus::Superseded
            ) || tokio::time::Instant::now() > deadline
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
            deployment = client
                .get(&format!("/deployments/{}", deployment.deployment_id))
                .await
                .map_err(error)?;
        }
    }
    print_deployment(&deployment, json);
    if deployment.record.status == DeploymentStatus::Failed {
        std::process::exit(1);
    }
    Ok(())
}

fn print_deployment(view: &DeploymentView, json: bool) {
    if json {
        print_json(view);
        return;
    }
    let record = &view.record;
    println!(
        "Deployment {}: {} {} → {} ({})",
        view.deployment_id,
        record.project,
        record.revision,
        record.environment,
        record.status.as_str()
    );
    println!(
        "Revision: {} ({})",
        record.revision_id, record.revision_digest
    );
    if let Some(from) = &record.promoted_from {
        println!("Promoted from: {from}");
    }
    if let Some(previous) = &record.previous {
        println!("Replaces: {previous}");
    }
    if let Some(failure) = &record.failure {
        println!("Failure: {failure}");
    }
    println!("\nWORKLOAD\tKIND\tADMITTED\tPROVIDER\tADMISSION");
    for workload in &record.workloads {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            workload.name,
            workload.kind.as_str(),
            workload.admitted,
            workload.provider.as_deref().unwrap_or("-"),
            workload.admission_id.as_deref().unwrap_or("-")
        );
        for reason in &workload.reasons {
            println!("  {reason}");
        }
    }
}

#[derive(Args, Debug)]
pub struct DeploymentCommand {
    #[command(subcommand)]
    pub command: DeploymentCommands,
    #[command(flatten)]
    pub daemon: DaemonLocation,
}

#[derive(Subcommand, Debug)]
pub enum DeploymentCommands {
    /// Deployments, newest first.
    List {
        #[arg(long)]
        environment: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    Inspect {
        deployment: String,
        #[arg(long)]
        json: bool,
    },
}

pub async fn deployment(command: DeploymentCommand) -> compute_core::Result<()> {
    let client = command.daemon.client()?;
    match command.command {
        DeploymentCommands::List {
            environment,
            project,
            limit,
            json,
        } => {
            let mut query = format!("/deployments?limit={limit}");
            if let Some(environment) = environment {
                query.push_str(&format!("&environment={environment}"));
            }
            if let Some(project) = project {
                query.push_str(&format!("&project={project}"));
            }
            let deployments: Vec<DeploymentView> = client.get(&query).await.map_err(error)?;
            if json {
                print_json(&deployments);
            } else {
                println!("DEPLOYMENT\tPROJECT\tENVIRONMENT\tREVISION\tSTATUS\tCREATED");
                for deployment in deployments {
                    let record = &deployment.record;
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}",
                        deployment.deployment_id,
                        record.project,
                        record.environment,
                        record.revision,
                        record.status.as_str(),
                        record.created_at.to_rfc3339()
                    );
                }
            }
        }
        DeploymentCommands::Inspect { deployment, json } => {
            let view: DeploymentView = client
                .get(&format!("/deployments/{deployment}"))
                .await
                .map_err(error)?;
            print_deployment(&view, json);
        }
    }
    Ok(())
}

// ---- compute events -------------------------------------------------------------

#[derive(Args, Debug)]
pub struct EventsCommand {
    /// Only events after this sequence.
    #[arg(long)]
    pub after: Option<u64>,
    #[arg(long)]
    pub environment: Option<String>,
    #[arg(long)]
    pub project: Option<String>,
    #[arg(long)]
    pub deployment: Option<String>,
    #[arg(long, default_value_t = 50)]
    pub limit: usize,
    /// Keep printing events as they are recorded.
    #[arg(long)]
    pub follow: bool,
    #[command(flatten)]
    pub daemon: DaemonLocation,
    #[arg(long)]
    pub json: bool,
}

fn print_event(event: &EventRecord, json: bool) {
    if json {
        println!("{}", serde_json::to_string(event).expect("serializable"));
    } else {
        println!(
            "{}\t{}\t{}\t{}",
            event.sequence,
            event.at.to_rfc3339(),
            event.kind,
            event.message
        );
    }
}

pub async fn events(command: EventsCommand) -> compute_core::Result<()> {
    let client = command.daemon.client()?;
    let mut query = format!("limit={}", command.limit);
    for (name, value) in [
        ("environment", &command.environment),
        ("project", &command.project),
        ("deployment", &command.deployment),
    ] {
        if let Some(value) = value {
            query.push_str(&format!("&{name}={value}"));
        }
    }
    if let Some(after) = command.after {
        query.push_str(&format!("&after={after}"));
    }
    let events: Vec<EventRecord> = client
        .get(&format!("/events?{query}"))
        .await
        .map_err(error)?;
    let mut last = command.after.unwrap_or_default();
    for event in &events {
        last = last.max(event.sequence);
        print_event(event, command.json);
    }
    if !command.follow {
        return Ok(());
    }
    // Follow the event stream the UI uses.
    let endpoint = command.daemon.endpoint();
    let authority = endpoint
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or_default()
        .to_string();
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(&authority).await?;
    stream
        .write_all(
            format!(
                "GET /events/stream?after={last}&{query} HTTP/1.1\r\nHost: {authority}\r\nAccept: text/event-stream\r\n\r\n"
            )
            .as_bytes(),
        )
        .await?;
    let mut lines = tokio::io::BufReader::new(stream).lines();
    while let Some(line) = lines.next_line().await? {
        if let Some(data) = line.strip_prefix("data: ")
            && let Ok(event) = serde_json::from_str::<EventRecord>(data)
        {
            print_event(&event, command.json);
        }
    }
    Ok(())
}

// ---- compute service --------------------------------------------------------------

#[derive(Args, Debug)]
pub struct ServiceCommand {
    #[command(subcommand)]
    pub command: ServiceCommands,
    #[command(flatten)]
    pub daemon: DaemonLocation,
}

#[derive(Subcommand, Debug)]
pub enum ServiceCommands {
    /// Shared services registered with this control plane.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Register a shared service and the capabilities it provides.
    Register {
        name: String,
        /// A capability it provides, such as llm.generate@1. Repeatable.
        #[arg(long = "capability")]
        capabilities: Vec<String>,
        #[arg(long, default_value = "local")]
        provider: String,
        #[arg(long)]
        environment: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        workload: Option<String>,
        #[arg(long)]
        endpoint: Option<String>,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Remove {
        name: String,
    },
    /// The daemon's provider pool as recorded in control state.
    Providers {
        #[arg(long)]
        json: bool,
    },
}

pub async fn service(command: ServiceCommand) -> compute_core::Result<()> {
    let client = command.daemon.client()?;
    match command.command {
        ServiceCommands::List { json } => {
            let services: Vec<ServiceRecord> = client.get("/services").await.map_err(error)?;
            if json {
                print_json(&services);
            } else {
                println!("SERVICE\tPROVIDER\tCAPABILITIES\tENDPOINT");
                for service in services {
                    println!(
                        "{}\t{}\t{}\t{}",
                        service.name,
                        service.provider,
                        service.capabilities.join(","),
                        service.endpoint.as_deref().unwrap_or("-")
                    );
                }
            }
        }
        ServiceCommands::Register {
            name,
            capabilities,
            provider,
            environment,
            project,
            workload,
            endpoint,
            description,
            json,
        } => {
            let definition = ServiceDefinition {
                name,
                capabilities,
                provider,
                environment,
                project,
                workload,
                endpoint,
                description,
            };
            let service: ServiceRecord = client
                .post("/services", Some(&definition))
                .await
                .map_err(error)?;
            if json {
                print_json(&service);
            } else {
                println!(
                    "Registered {} ({})",
                    service.name,
                    service.capabilities.join(", ")
                );
            }
        }
        ServiceCommands::Remove { name } => {
            let _: serde_json::Value = client
                .delete(&format!("/services/{name}"))
                .await
                .map_err(error)?;
            println!("Removed {name}");
        }
        ServiceCommands::Providers { json } => {
            let providers: Vec<ProviderRecord> = client.get("/providers").await.map_err(error)?;
            if json {
                print_json(&providers);
            } else {
                println!("PROVIDER\tKIND\tPRIORITY\tENDPOINT");
                for provider in providers {
                    println!(
                        "{}\t{}\t{}\t{}",
                        provider.provider_id,
                        provider.kind,
                        provider.priority,
                        provider.endpoint.as_deref().unwrap_or("-")
                    );
                }
            }
        }
    }
    Ok(())
}
