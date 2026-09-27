//! Environments on a computer: `compute environment create --cpu … `, what
//! belongs in the computer (`repo`, `package`, `process`, `service`,
//! `agent`, `contents`), and using it (`exec`, `connect`, `logs`,
//! `reconcile`, `replace`). Every command is a request to the daemon's API;
//! the daemon decides, records, and reconciles.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Subcommand};
use compute_core::{
    ComputeError, ComputerLifecycle, ComputerRequirements, EnvironmentContents, IsolationProfile,
    NetworkPolicy, PackageSpec, ProcessDesired, ProcessKind, ProcessSpec, ProjectSpec,
    RepositorySpec, SessionCommand,
};
use compute_environment::client::DaemonClient;
use compute_environment::{
    ComputerEnvironmentDefinition, ComputerExec, ComputerJob, ComputerRequest, ComputerView,
    ContentsUpdate, DesiredState, EnvironmentView, LifecycleChange, ProjectCommandRequest,
    ReleaseRequest,
};

use crate::environment_cmd::{error, parse_pair, print_json};

/// The computer an environment asks for. Any of these makes
/// `compute environment create` create an environment on a computer.
#[derive(Args, Debug, Clone, Default)]
pub struct ComputerArgs {
    /// Logical CPUs the computer needs.
    #[arg(long)]
    pub cpu: Option<u32>,
    /// Memory the computer needs (512Mi, 8Gi, ...).
    #[arg(long, value_parser = crate::parse_memory)]
    pub memory: Option<u64>,
    /// Disk the computer needs.
    #[arg(long, value_parser = crate::parse_memory)]
    pub disk: Option<u64>,
    /// Architecture the computer must have (x86_64, arm64).
    #[arg(long = "arch")]
    pub architecture: Option<String>,
    /// Network the computer may use.
    #[arg(long, value_parser = crate::parse_network)]
    pub network: Option<NetworkPolicy>,
    #[arg(long, value_parser = crate::parse_isolation)]
    pub isolation: Option<IsolationProfile>,
    /// A capability the computer must offer (repeatable): terminal,
    /// persistent_storage, public_endpoint, ...
    #[arg(long = "require")]
    pub require: Vec<String>,
    /// A machine feature the target must have (repeatable): kvm,
    /// firecracker, containers, gpu, virtualization.
    #[arg(long = "feature")]
    pub features: Vec<String>,
    /// Keep the computer until it is destroyed (the default).
    #[arg(long, conflicts_with = "ephemeral")]
    pub persistent: bool,
    /// Destroy the computer when its TTL passes.
    #[arg(long)]
    pub ephemeral: bool,
    /// An ephemeral computer's lifetime (30m, 1h, 2d).
    #[arg(long, value_parser = crate::parse_retention, requires = "ephemeral")]
    pub ttl: Option<Duration>,
    /// Constrain placement to this target. Placement chooses otherwise.
    #[arg(long)]
    pub target: Option<String>,
    /// Initial contents (JSON: repositories, packages, processes).
    #[arg(long)]
    pub contents: Option<PathBuf>,
}

impl ComputerArgs {
    pub fn requested(&self) -> bool {
        self.cpu.is_some()
            || self.memory.is_some()
            || self.disk.is_some()
            || self.architecture.is_some()
            || !self.require.is_empty()
            || !self.features.is_empty()
            || self.persistent
            || self.ephemeral
            || self.target.is_some()
            || self.contents.is_some()
    }

    fn requirements(&self) -> ComputerRequirements {
        ComputerRequirements {
            cpu_count: self.cpu,
            memory_bytes: self.memory,
            disk_bytes: self.disk,
            architecture: self.architecture.clone(),
            // A computer is a machine to work on: it reaches the network
            // unless asked not to.
            network: self.network.clone().unwrap_or(NetworkPolicy::Network),
            isolation: self.isolation.unwrap_or_default(),
            capabilities: self.require.clone(),
            features: self.features.clone(),
        }
    }
}

pub async fn create(
    client: &DaemonClient,
    name: String,
    env: BTreeMap<String, String>,
    policy: Option<compute_policy::Policy>,
    stopped: bool,
    computer: ComputerArgs,
) -> compute_core::Result<EnvironmentView> {
    let contents = match &computer.contents {
        Some(path) => serde_json::from_slice::<EnvironmentContents>(&std::fs::read(path)?)?,
        None => EnvironmentContents::default(),
    };
    let definition = ComputerEnvironmentDefinition {
        name,
        desired_state: if stopped {
            DesiredState::Stopped
        } else {
            DesiredState::Running
        },
        env,
        policy,
        computer: ComputerRequest {
            lifecycle: if computer.ephemeral {
                ComputerLifecycle::Ephemeral
            } else {
                ComputerLifecycle::Persistent
            },
            requirements: computer.requirements(),
            target: computer.target.clone(),
            ttl_seconds: computer.ttl.map(|ttl| ttl.as_secs().max(1)),
        },
        contents,
    };
    client
        .post("/environments", Some(&definition))
        .await
        .map_err(error)
}

#[derive(Subcommand, Debug)]
pub enum ComputerCommands {
    /// Show an environment's computer: desired against observed contents.
    Computer {
        environment: String,
        #[arg(long)]
        json: bool,
    },
    /// Run a command in the environment's computer as a durable job.
    Exec(ExecArgs),
    /// Get connection details for the environment's computer.
    Connect {
        environment: String,
        #[arg(long)]
        json: bool,
    },
    /// Every job the computer ran, or one process's log.
    Logs {
        environment: String,
        #[arg(long)]
        process: Option<String>,
        #[arg(long, default_value_t = 200)]
        lines: usize,
        #[arg(long)]
        json: bool,
    },
    /// Check every process now, and retry what failed.
    Reconcile {
        environment: String,
        #[arg(long)]
        json: bool,
    },
    /// Replace the computer with one that meets new requirements. The only
    /// change that provisions a new machine.
    Replace {
        environment: String,
        #[command(flatten)]
        computer: ComputerArgs,
        #[arg(long)]
        json: bool,
    },
    /// Repositories checked out in the computer.
    #[command(subcommand)]
    Repo(RepoCommands),
    /// Packages installed in the computer.
    #[command(subcommand)]
    Package(PackageCommands),
    /// Processes running in the computer: applications, services, agents.
    #[command(subcommand)]
    Process(ProcessCommands),
    /// Add a service (a long-running process such as a database).
    Service {
        #[command(subcommand)]
        command: KindCommands,
    },
    /// Start an agent in the computer.
    Agent {
        #[command(subcommand)]
        command: KindCommands,
    },
    /// Show or replace the computer's desired contents at once.
    #[command(subcommand)]
    Contents(ContentsCommands),
    /// Projects: software in a repository, with the commands that build,
    /// test, and operate it inside the computer.
    #[command(subcommand)]
    Project(ProjectCommands),
    /// Build a project inside the environment's computer.
    Build(ProjectRunArgs),
    /// Test a project inside the environment's computer.
    Test(ProjectRunArgs),
    /// Run one of a project's named commands inside the environment's
    /// computer.
    Run {
        environment: String,
        project: String,
        command: String,
        #[arg(long = "env", value_parser = parse_pair)]
        env: Vec<(String, String)>,
        #[arg(long)]
        json: bool,
    },
    /// Release a revision of a project: the computer checks it out, builds
    /// it, and restarts what runs from it, in place. No redeployment.
    Release(ReleaseArgs),
    /// Show or change the configuration every process, build, and command
    /// sees. What depends on it restarts in place.
    Config {
        environment: String,
        /// Set KEY=VALUE (repeatable).
        #[arg(long = "set", value_parser = parse_pair)]
        set: Vec<(String, String)>,
        /// Remove KEY (repeatable).
        #[arg(long)]
        unset: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Inspect a project's source in the computer and propose how to run
    /// it: runtime, dependencies, build, tests, start command, ports,
    /// services, configuration. Nothing changes.
    Propose {
        environment: String,
        /// A Git URL, or a folder the computer can read that is a Git
        /// repository.
        #[arg(long)]
        url: String,
        #[arg(long)]
        revision: Option<String>,
        #[arg(long)]
        name: Option<String>,
    },
    /// Change how long the environment lives, in place: kept until
    /// destroyed, or temporary.
    Lifetime {
        environment: String,
        /// Keep it until it is destroyed.
        #[arg(long, conflicts_with = "temporary")]
        keep: bool,
        /// Let it expire, and its record remain, after --ttl (default 1h).
        #[arg(long)]
        temporary: bool,
        #[arg(long, value_parser = crate::parse_retention, requires = "temporary")]
        ttl: Option<Duration>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Debug)]
pub struct ProjectRunArgs {
    environment: String,
    /// The project. Optional when only one project has the command.
    project: Option<String>,
    #[arg(long = "env", value_parser = parse_pair)]
    env: Vec<(String, String)>,
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
pub struct ReleaseArgs {
    pub environment: String,
    pub project: String,
    #[arg(long)]
    pub revision: String,
    /// Return once the change is recorded, without waiting for the computer.
    #[arg(long)]
    pub no_wait: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Subcommand, Debug)]
pub enum ProjectCommands {
    /// Add (or change) a project in one of the environment's repositories.
    Add {
        environment: String,
        name: String,
        #[arg(long)]
        repository: String,
        /// The build, run by `sh -c` in the checkout whenever it changes.
        #[arg(long)]
        build: Option<String>,
        /// The tests, run by `sh -c` on request.
        #[arg(long)]
        test: Option<String>,
        /// A named command, NAME=COMMAND, run by `sh -c` on request
        /// (repeatable).
        #[arg(long = "command", value_parser = parse_pair)]
        commands: Vec<(String, String)>,
        /// A named command that must pass to publish a version
        /// (repeatable).
        #[arg(long = "check")]
        checks: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    Remove {
        environment: String,
        name: String,
        #[arg(long)]
        json: bool,
    },
}

fn shell(command: &str) -> Vec<String> {
    vec!["sh".into(), "-c".into(), command.into()]
}

#[derive(Args, Debug)]
pub struct ExecArgs {
    environment: String,
    #[arg(long = "env", value_parser = parse_pair)]
    env: Vec<(String, String)>,
    #[arg(long, value_parser = crate::parse_duration)]
    timeout: Option<Duration>,
    /// Return the durable job and execution IDs without waiting.
    #[arg(long)]
    detach: bool,
    #[arg(long)]
    json: bool,
    #[arg(last = true, required = true)]
    command: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub enum RepoCommands {
    /// Add a repository, checked out at a revision.
    Add(RepoArgs),
    /// Move a repository to another revision (or URL).
    Update(RepoArgs),
    /// Fetch a repository's revision again: a branch that moved.
    Pull {
        environment: String,
        name: String,
        #[arg(long)]
        json: bool,
    },
    Remove {
        environment: String,
        name: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Debug)]
pub struct RepoArgs {
    environment: String,
    name: String,
    #[arg(long)]
    url: String,
    #[arg(long, default_value = "main")]
    revision: String,
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand, Debug)]
pub enum PackageCommands {
    /// Install a package by running a command (again whenever it changes).
    Install {
        environment: String,
        name: String,
        /// Run inside this repository's checkout.
        #[arg(long)]
        repository: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    Remove {
        environment: String,
        name: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ProcessCommands {
    /// Add (or change) a process and start it.
    Add(ProcessArgs),
    Start {
        environment: String,
        name: String,
        #[arg(long)]
        json: bool,
    },
    Stop {
        environment: String,
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Restart it in place.
    Restart {
        environment: String,
        name: String,
        #[arg(long)]
        json: bool,
    },
    Remove {
        environment: String,
        name: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum KindCommands {
    /// Add (or change) it and start it.
    #[command(alias = "start")]
    Add(ProcessArgs),
}

#[derive(Args, Debug)]
pub struct ProcessArgs {
    environment: String,
    name: String,
    /// Run inside this repository's checkout, restarting when it moves.
    #[arg(long)]
    repository: Option<String>,
    #[arg(long = "env", value_parser = parse_pair)]
    env: Vec<(String, String)>,
    /// Add it stopped.
    #[arg(long)]
    stopped: bool,
    /// The port it listens on: published as an endpoint, and given to it
    /// as $PORT.
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    json: bool,
    #[arg(last = true, required = true)]
    command: Vec<String>,
}

impl ProcessArgs {
    fn spec(&self, kind: ProcessKind) -> ProcessSpec {
        ProcessSpec {
            name: self.name.clone(),
            kind,
            command: self.command.clone(),
            repository: self.repository.clone(),
            env: self.env.iter().cloned().collect(),
            desired: if self.stopped {
                ProcessDesired::Stopped
            } else {
                ProcessDesired::Running
            },
            port: self.port,
            restart: 0,
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum ContentsCommands {
    Show {
        environment: String,
    },
    /// Replace the contents with a JSON document. With
    /// `--expected-generation`, only if nobody changed them since.
    Apply {
        environment: String,
        file: PathBuf,
        #[arg(long)]
        expected_generation: Option<u64>,
        #[arg(long)]
        json: bool,
    },
}

pub async fn run(client: &DaemonClient, command: ComputerCommands) -> compute_core::Result<()> {
    match command {
        ComputerCommands::Computer { environment, json } => {
            let view: ComputerView = client
                .get(&format!("/environments/{environment}/computer"))
                .await
                .map_err(error)?;
            print_computer(&view, json);
        }
        ComputerCommands::Exec(args) => exec(client, args).await?,
        ComputerCommands::Connect { environment, json } => {
            let grant: compute_core::SessionConnectionGrant = client
                .post::<(), _>(&format!("/environments/{environment}/connect"), None)
                .await
                .map_err(error)?;
            if json {
                print_json(&grant);
            } else {
                println!("Mode:        {}", grant.connection.mode.as_str());
                if let Some(address) = &grant.connection.address {
                    println!("Address:     {address}");
                }
                if !grant.command.is_empty() {
                    println!("Command:     {}", grant.command.join(" "));
                }
                if !grant.credentials.is_empty() {
                    println!("Credentials: issued (use --json to read them)");
                }
                println!(
                    "Commands in this environment: compute environment exec {environment} -- <command>"
                );
            }
        }
        ComputerCommands::Logs {
            environment,
            process,
            lines,
            json,
        } => {
            let mut path = format!("/environments/{environment}/logs?limit={lines}");
            if let Some(process) = &process {
                path.push_str(&format!("&process={process}"));
            }
            let logs: serde_json::Value = client.get(&path).await.map_err(error)?;
            if json {
                print_json(&logs);
            } else if process.is_some() {
                print!("{}", logs["log"].as_str().unwrap_or_default());
            } else {
                for execution in logs["executions"].as_array().into_iter().flatten() {
                    println!(
                        "== {} {} [{}] {}",
                        execution["job_id"].as_str().unwrap_or_default(),
                        execution["execution_id"].as_str().unwrap_or_default(),
                        execution["purpose"].as_str().unwrap_or_default(),
                        execution["status"].as_str().unwrap_or_default(),
                    );
                    print!("{}", execution["stdout"].as_str().unwrap_or_default());
                    eprint!("{}", execution["stderr"].as_str().unwrap_or_default());
                }
            }
        }
        ComputerCommands::Reconcile { environment, json } => {
            let view: ComputerView = client
                .post::<(), _>(&format!("/environments/{environment}/reconcile"), None)
                .await
                .map_err(error)?;
            print_computer(&view, json);
        }
        ComputerCommands::Replace {
            environment,
            computer,
            json,
        } => {
            let view: ComputerView = client
                .post(
                    &format!("/environments/{environment}/replace"),
                    Some(&computer.requirements()),
                )
                .await
                .map_err(error)?;
            print_computer(&view, json);
        }
        ComputerCommands::Repo(command) => match command {
            RepoCommands::Add(args) | RepoCommands::Update(args) => {
                let view: ComputerView = client
                    .post(
                        &format!("/environments/{}/repositories", args.environment),
                        Some(&RepositorySpec {
                            name: args.name.clone(),
                            url: args.url.clone(),
                            revision: args.revision.clone(),
                            sync: 0,
                        }),
                    )
                    .await
                    .map_err(error)?;
                print_computer(&view, args.json);
            }
            RepoCommands::Pull {
                environment,
                name,
                json,
            } => {
                let view: ComputerView = client
                    .get(&format!("/environments/{environment}/computer"))
                    .await
                    .map_err(error)?;
                let mut repository = view
                    .desired
                    .repositories
                    .iter()
                    .find(|repository| repository.name == name)
                    .cloned()
                    .ok_or_else(|| ComputeError::Runtime(format!("no repository {name}")))?;
                repository.sync += 1;
                let view: ComputerView = client
                    .post(
                        &format!("/environments/{environment}/repositories"),
                        Some(&repository),
                    )
                    .await
                    .map_err(error)?;
                print_computer(&view, json);
            }
            RepoCommands::Remove {
                environment,
                name,
                json,
            } => remove(client, &environment, "repositories", &name, json).await?,
        },
        ComputerCommands::Package(command) => match command {
            PackageCommands::Install {
                environment,
                name,
                repository,
                json,
                command,
            } => {
                let view: ComputerView = client
                    .post(
                        &format!("/environments/{environment}/packages"),
                        Some(&PackageSpec {
                            name,
                            install: command,
                            repository,
                        }),
                    )
                    .await
                    .map_err(error)?;
                print_computer(&view, json);
            }
            PackageCommands::Remove {
                environment,
                name,
                json,
            } => remove(client, &environment, "packages", &name, json).await?,
        },
        ComputerCommands::Process(command) => match command {
            ProcessCommands::Add(args) => add_process(client, &args, ProcessKind::Process).await?,
            ProcessCommands::Start {
                environment,
                name,
                json,
            } => process_action(client, &environment, &name, "start", json).await?,
            ProcessCommands::Stop {
                environment,
                name,
                json,
            } => process_action(client, &environment, &name, "stop", json).await?,
            ProcessCommands::Restart {
                environment,
                name,
                json,
            } => process_action(client, &environment, &name, "restart", json).await?,
            ProcessCommands::Remove {
                environment,
                name,
                json,
            } => remove(client, &environment, "processes", &name, json).await?,
        },
        ComputerCommands::Service {
            command: KindCommands::Add(args),
        } => add_process(client, &args, ProcessKind::Service).await?,
        ComputerCommands::Agent {
            command: KindCommands::Add(args),
        } => add_process(client, &args, ProcessKind::Agent).await?,
        ComputerCommands::Contents(command) => match command {
            ContentsCommands::Show { environment } => {
                let view: ComputerView = client
                    .get(&format!("/environments/{environment}/computer"))
                    .await
                    .map_err(error)?;
                print_json(&view.desired);
            }
            ContentsCommands::Apply {
                environment,
                file,
                expected_generation,
                json,
            } => {
                let contents: EnvironmentContents = serde_json::from_slice(&std::fs::read(file)?)?;
                let view: ComputerView = client
                    .post(
                        &format!("/environments/{environment}/contents"),
                        Some(&ContentsUpdate {
                            contents,
                            expected_generation,
                            config: None,
                            lifecycle: None,
                        }),
                    )
                    .await
                    .map_err(error)?;
                print_computer(&view, json);
            }
        },
        ComputerCommands::Project(command) => match command {
            ProjectCommands::Add {
                environment,
                name,
                repository,
                build,
                test,
                commands,
                checks,
                json,
            } => {
                let view: ComputerView = client
                    .post(
                        &format!("/environments/{environment}/projects"),
                        Some(&ProjectSpec {
                            name,
                            repository,
                            build: build.as_deref().map(shell).unwrap_or_default(),
                            test: test.as_deref().map(shell).unwrap_or_default(),
                            commands: commands
                                .iter()
                                .map(|(name, command)| (name.clone(), shell(command)))
                                .collect(),
                            checks,
                        }),
                    )
                    .await
                    .map_err(error)?;
                print_computer(&view, json);
            }
            ProjectCommands::Remove {
                environment,
                name,
                json,
            } => remove(client, &environment, "projects", &name, json).await?,
        },
        ComputerCommands::Build(args) => project_command(client, args, "build").await?,
        ComputerCommands::Test(args) => project_command(client, args, "test").await?,
        ComputerCommands::Run {
            environment,
            project,
            command,
            env,
            json,
        } => {
            let submitted: ComputerExec = client
                .post(
                    &format!("/environments/{environment}/run"),
                    Some(&ProjectCommandRequest {
                        project,
                        command,
                        env: env.into_iter().collect(),
                        timeout: None,
                    }),
                )
                .await
                .map_err(error)?;
            wait_job(client, &environment, &submitted, json).await?;
        }
        ComputerCommands::Release(args) => release(client, &args).await?,
        ComputerCommands::Config {
            environment,
            set,
            unset,
            json,
        } => {
            let view: ComputerView = client
                .get(&format!("/environments/{environment}/computer"))
                .await
                .map_err(error)?;
            if set.is_empty() && unset.is_empty() {
                if json {
                    print_json(&view.config.keys().collect::<Vec<_>>());
                } else {
                    for key in view.config.keys() {
                        println!("{key}");
                    }
                }
                return Ok(());
            }
            let mut config = view.config.clone();
            for key in &unset {
                config.remove(key);
            }
            config.extend(set);
            let view: ComputerView = client
                .post(
                    &format!("/environments/{environment}/config"),
                    Some(&config),
                )
                .await
                .map_err(error)?;
            print_computer(&view, json);
        }
        ComputerCommands::Propose {
            environment,
            url,
            revision,
            name,
        } => {
            let proposal: compute_core::ProjectProposal = client
                .post(
                    &format!("/environments/{environment}/propose"),
                    Some(&compute_environment::ProposeRequest {
                        url,
                        revision,
                        name,
                    }),
                )
                .await
                .map_err(error)?;
            print_json(&proposal);
        }
        ComputerCommands::Lifetime {
            environment,
            keep,
            temporary,
            ttl,
            json,
        } => {
            if keep == temporary {
                return Err(ComputeError::Runtime("choose --keep or --temporary".into()));
            }
            let view: ComputerView = client
                .post(
                    &format!("/environments/{environment}/lifecycle"),
                    Some(&LifecycleChange {
                        lifecycle: if keep {
                            ComputerLifecycle::Persistent
                        } else {
                            ComputerLifecycle::Ephemeral
                        },
                        ttl_seconds: ttl.map(|ttl| ttl.as_secs().max(1)),
                    }),
                )
                .await
                .map_err(error)?;
            print_computer(&view, json);
        }
    }
    Ok(())
}

/// Build or test a project: the project named, or the only one that has
/// the command.
async fn project_command(
    client: &DaemonClient,
    args: ProjectRunArgs,
    command: &str,
) -> compute_core::Result<()> {
    let project = match args.project {
        Some(project) => project,
        None => {
            let view: ComputerView = client
                .get(&format!("/environments/{}/computer", args.environment))
                .await
                .map_err(error)?;
            let candidates = view
                .desired
                .projects
                .iter()
                .filter(|project| project.command(command).is_some())
                .map(|project| project.name.clone())
                .collect::<Vec<_>>();
            match candidates.as_slice() {
                [project] => project.clone(),
                [] => {
                    return Err(ComputeError::Runtime(format!(
                        "no project in {} has a {command} command",
                        args.environment
                    )));
                }
                _ => {
                    return Err(ComputeError::Runtime(format!(
                        "more than one project can {command}: name one of {}",
                        candidates.join(", ")
                    )));
                }
            }
        }
    };
    let submitted: ComputerExec = client
        .post(
            &format!("/environments/{}/run", args.environment),
            Some(&ProjectCommandRequest {
                project,
                command: command.into(),
                env: args.env.into_iter().collect(),
                timeout: None,
            }),
        )
        .await
        .map_err(error)?;
    wait_job(client, &args.environment, &submitted, args.json).await
}

/// Release a revision of a project, and follow the computer until it holds
/// it: the same machine, changed in place.
pub async fn release(client: &DaemonClient, args: &ReleaseArgs) -> compute_core::Result<()> {
    let before: ComputerView = client
        .get(&format!("/environments/{}/computer", args.environment))
        .await
        .map_err(error)?;
    let view: ComputerView = client
        .post(
            &format!("/environments/{}/release", args.environment),
            Some(&ReleaseRequest {
                project: args.project.clone(),
                revision: args.revision.clone(),
                expected_generation: None,
            }),
        )
        .await
        .map_err(error)?;
    if args.no_wait {
        print_computer(&view, args.json);
        return Ok(());
    }
    let generation = view.desired.generation;
    let deadline = std::time::Instant::now() + Duration::from_secs(60 * 60);
    let view = loop {
        let view: ComputerView = client
            .get(&format!("/environments/{}/computer", args.environment))
            .await
            .map_err(error)?;
        let settled = view.observed.converged_generation >= generation && view.converged;
        let failed = view
            .failure
            .as_ref()
            .is_some_and(|failure| failure.phase == "reconciliation" || !failure.retryable);
        if settled || failed || view.status.is_terminal() {
            break view;
        }
        if std::time::Instant::now() > deadline {
            return Err(ComputeError::Runtime(format!(
                "{} did not settle on the release",
                args.environment
            )));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    if args.json {
        print_json(&view);
    } else {
        let same = before.machine.as_ref().map(|machine| &machine.session_id)
            == view.machine.as_ref().map(|machine| &machine.session_id);
        println!(
            "Released {} {} to {}: {}{}",
            args.project,
            args.revision,
            args.environment,
            if view.converged { "running" } else { "failed" },
            if same {
                ", on the same machine (changed in place)"
            } else {
                ""
            }
        );
        print_computer(&view, false);
    }
    if !view.converged {
        std::process::exit(1);
    }
    Ok(())
}

async fn add_process(
    client: &DaemonClient,
    args: &ProcessArgs,
    kind: ProcessKind,
) -> compute_core::Result<()> {
    let view: ComputerView = client
        .post(
            &format!("/environments/{}/processes", args.environment),
            Some(&args.spec(kind)),
        )
        .await
        .map_err(error)?;
    print_computer(&view, args.json);
    Ok(())
}

async fn process_action(
    client: &DaemonClient,
    environment: &str,
    name: &str,
    action: &str,
    json: bool,
) -> compute_core::Result<()> {
    let view: ComputerView = client
        .post::<(), _>(
            &format!("/environments/{environment}/processes/{name}/{action}"),
            None,
        )
        .await
        .map_err(error)?;
    print_computer(&view, json);
    Ok(())
}

async fn remove(
    client: &DaemonClient,
    environment: &str,
    kind: &str,
    name: &str,
    json: bool,
) -> compute_core::Result<()> {
    let view: ComputerView = client
        .delete(&format!("/environments/{environment}/{kind}/{name}"))
        .await
        .map_err(error)?;
    print_computer(&view, json);
    Ok(())
}

async fn exec(client: &DaemonClient, args: ExecArgs) -> compute_core::Result<()> {
    let mut command = SessionCommand::new(args.command.clone());
    command.env = args.env.iter().cloned().collect();
    command.timeout = args.timeout;
    let submitted: ComputerExec = client
        .post(
            &format!("/environments/{}/exec", args.environment),
            Some(&command),
        )
        .await
        .map_err(error)?;
    if args.detach {
        print_json(&submitted);
        return Ok(());
    }
    wait_job(client, &args.environment, &submitted, args.json).await
}

/// Follow a job in the environment's computer to its end, print its
/// output, and exit with its exit code.
async fn wait_job(
    client: &DaemonClient,
    environment: &str,
    submitted: &ComputerExec,
    json: bool,
) -> compute_core::Result<()> {
    let path = format!("/environments/{environment}/jobs/{}", submitted.job_id);
    let mut delay = Duration::from_millis(50);
    let job = loop {
        let job: ComputerJob = client.get(&path).await.map_err(error)?;
        if job.job.status.is_terminal() {
            break job;
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(1));
    };
    let Some(result) = job.result else {
        return Err(ComputeError::Runtime(format!(
            "job {} ended {:?} without a result: {}",
            job.job.job_id,
            job.job.status,
            job.job.failure.unwrap_or_default()
        )));
    };
    if json {
        print_json(&result);
    } else {
        print!("{}", result.result.stdout.text);
        eprint!("{}", result.result.stderr.text);
        eprintln!(
            "\njob: {}\nexecution {}: {}",
            submitted.job_id,
            result.result.execution_id,
            serde_json::to_string(&result.result.status)
                .unwrap_or_default()
                .trim_matches('"')
        );
    }
    if let Some(code) = result.result.exit_code.filter(|code| *code != 0) {
        std::process::exit(code);
    }
    Ok(())
}

fn size(bytes: u64) -> String {
    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;
    if bytes >= GIB && bytes.is_multiple_of(GIB) {
        format!("{} GiB", bytes / GIB)
    } else if bytes >= MIB && bytes.is_multiple_of(MIB) {
        format!("{} MiB", bytes / MIB)
    } else {
        format!("{bytes} bytes")
    }
}

/// The computer as one screen: where it runs, and what it holds against
/// what it should hold.
pub fn print_computer(view: &ComputerView, json: bool) {
    if json {
        print_json(view);
        return;
    }
    println!(
        "Computer:    {} ({}){}",
        view.status,
        view.lifecycle.as_str(),
        view.target
            .as_ref()
            .map(|target| format!(" on {target}"))
            .unwrap_or_default()
    );
    println!(
        "Environment: {} ({})",
        view.environment, view.environment_id
    );
    println!("Owner:       {}", view.owner);
    if let Some(machine) = &view.machine {
        println!(
            "Machine:     {}{}",
            machine.resource.as_deref().unwrap_or(&machine.session_id),
            machine
                .provider_kind
                .as_ref()
                .map(|kind| format!(" ({kind} on {})", machine.target))
                .unwrap_or_default()
        );
        println!("Session:     {}", machine.session_id);
    }
    if view.requested_lifecycle != view.lifecycle {
        println!(
            "Lifetime:    becoming {}",
            view.requested_lifecycle.as_str()
        );
    }
    let requirements = &view.requirements;
    let mut needs = vec![];
    if let Some(cpu) = requirements.cpu_count {
        needs.push(format!("{cpu} CPU"));
    }
    if let Some(memory) = requirements.memory_bytes {
        needs.push(size(memory));
    }
    if let Some(architecture) = &requirements.architecture {
        needs.push(architecture.clone());
    }
    needs.push(format!("network {}", requirements.network));
    needs.extend(requirements.features.iter().cloned());
    println!("Needs:       {}", needs.join(", "));
    if view.running_generation != view.spec_generation {
        println!(
            "Replacing:   generation {} → {}",
            view.running_generation, view.spec_generation
        );
    }
    if let Some(expires_at) = view.expires_at {
        println!("Expires:     {}", expires_at.to_rfc3339());
    }
    println!(
        "Contents:    generation {}, {}",
        view.desired.generation,
        if view.converged {
            "in place"
        } else {
            "reconciling"
        }
    );
    let yes = |value: bool| if value { "✓" } else { " " };
    if !view.desired.repositories.is_empty() {
        println!("\nRepositories");
        for repository in &view.desired.repositories {
            let observed = view.observed.repositories.get(&repository.name);
            println!(
                "  {:<16} {:<12} {:<12} {} {}",
                repository.name,
                repository.revision,
                observed
                    .and_then(|seen| seen.commit.as_deref())
                    .map(|commit| &commit[..commit.len().min(12)])
                    .unwrap_or("-"),
                yes(
                    observed.is_some_and(|seen| seen.evidence.outcome == "succeeded"
                        && seen.revision == repository.revision)
                ),
                observed
                    .and_then(|seen| seen.evidence.error.as_deref())
                    .unwrap_or_default()
            );
        }
    }
    if !view.desired.packages.is_empty() {
        println!("\nPackages");
        for package in &view.desired.packages {
            let observed = view.observed.packages.get(&package.name);
            println!(
                "  {:<16} {:<10} {}",
                package.name,
                observed.map_or("pending", |seen| seen.evidence.outcome.as_str()),
                package.install.join(" ")
            );
        }
    }
    if !view.desired.projects.is_empty() {
        println!("\nProjects");
        for project in &view.desired.projects {
            let built = view.observed.builds.get(&project.name);
            let mut commands = vec![];
            if !project.build.is_empty() {
                commands.push("build".to_owned());
            }
            if !project.test.is_empty() {
                commands.push("test".to_owned());
            }
            commands.extend(project.commands.keys().cloned());
            println!(
                "  {:<16} {:<12} {:<10} {}",
                project.name,
                project.repository,
                built.map_or(
                    if project.build.is_empty() {
                        "-"
                    } else {
                        "pending"
                    },
                    |seen| seen.evidence.outcome.as_str()
                ),
                commands.join(", ")
            );
        }
    }
    if !view.desired.processes.is_empty() {
        println!("\nProcesses");
        for process in &view.desired.processes {
            let observed = view.observed.processes.get(&process.name);
            println!(
                "  {:<16} {:<12} {:<9} {:<8} {}",
                process.name,
                process.kind.as_str(),
                observed.map_or("pending", |seen| seen.state.as_str()),
                observed
                    .and_then(|seen| seen.pid)
                    .map(|pid| pid.to_string())
                    .unwrap_or_default(),
                process.command.join(" ")
            );
        }
    }
    if !view.endpoints.is_empty() {
        println!("\nEndpoints");
        for endpoint in &view.endpoints {
            println!(
                "  {:<16} {:<6} {} {}",
                endpoint.process,
                endpoint.port,
                endpoint.url.as_deref().unwrap_or("-"),
                if endpoint.serving { "serving" } else { "" }
            );
        }
    }
    if !view.config.is_empty() {
        println!(
            "\nConfiguration: {}",
            view.config.keys().cloned().collect::<Vec<_>>().join(", ")
        );
    }
    if let Some(failure) = &view.failure {
        println!(
            "\nFailure:     {} in {} ({}{}): {}",
            failure.code,
            failure.phase,
            if failure.retryable {
                "retryable"
            } else {
                "not retryable"
            },
            failure
                .target
                .as_ref()
                .map(|target| format!(", target {target}"))
                .unwrap_or_default(),
            failure.message
        );
    }
}

// ---- compute target ---------------------------------------------------------

#[derive(Args, Debug)]
pub struct TargetCommand {
    #[command(subcommand)]
    command: TargetCommands,
    #[command(flatten)]
    daemon: crate::environment_cmd::DaemonLocation,
}

#[derive(Subcommand, Debug)]
enum TargetCommands {
    /// The computers and infrastructure the daemon can place environments on.
    List {
        #[arg(long)]
        json: bool,
    },
    /// The control planes a target trusts, on the target's machine: its
    /// `compute serve --credentials` file.
    Credential {
        #[command(subcommand)]
        command: TargetCredentialCommands,
    },
}

#[derive(Subcommand, Debug)]
enum TargetCredentialCommands {
    /// Issue a credential that lets a control plane control this target.
    /// The token is shown (or written to --token-file) once; the target
    /// keeps only its verifier.
    Issue {
        /// The target's trust file.
        #[arg(long, default_value = ".compute/target-credentials.json")]
        credentials: PathBuf,
        /// The control plane the credential identifies. What it creates on
        /// the target belongs to this identity.
        #[arg(long)]
        control_plane: String,
        /// Write the token here (owner-only) instead of printing it: the
        /// file a pool member's `token_file` names.
        #[arg(long)]
        token_file: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// The credentials a target trusts: never a secret.
    List {
        #[arg(long, default_value = ".compute/target-credentials.json")]
        credentials: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Revoke a credential: the target refuses it at the next request.
    Revoke {
        credential_id: String,
        #[arg(long, default_value = ".compute/target-credentials.json")]
        credentials: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

fn credential_error(error: compute_provider::ProviderError) -> ComputeError {
    ComputeError::InvalidWorkload(error.message)
}

fn target_credential(command: TargetCredentialCommands) -> compute_core::Result<()> {
    use compute_provider::TargetCredentials;
    match command {
        TargetCredentialCommands::Issue {
            credentials,
            control_plane,
            token_file,
            json,
        } => {
            let mut trusted =
                TargetCredentials::load_or_default(&credentials).map_err(credential_error)?;
            let (record, token) = trusted.issue(&control_plane).map_err(credential_error)?;
            trusted.save(&credentials).map_err(credential_error)?;
            if let Some(path) = &token_file {
                compute_provider::credentials::write_token_file(path, &token)?;
            }
            if json {
                let mut value = serde_json::json!({
                    "credential_id": record.credential_id,
                    "control_plane": record.control_plane,
                    "created_at": record.created_at,
                });
                match &token_file {
                    Some(path) => value["token_file"] = serde_json::json!(path),
                    None => value["token"] = serde_json::json!(token),
                }
                print_json(&value);
            } else {
                println!(
                    "Issued {} for control plane {}",
                    record.credential_id, record.control_plane
                );
                match &token_file {
                    Some(path) => println!("Token written to {}", path.display()),
                    None => println!("Token (shown once): {token}"),
                }
            }
        }
        TargetCredentialCommands::List { credentials, json } => {
            let trusted =
                TargetCredentials::load_or_default(&credentials).map_err(credential_error)?;
            let rows = trusted
                .credentials
                .iter()
                .map(|record| {
                    serde_json::json!({
                        "credential_id": record.credential_id,
                        "control_plane": record.control_plane,
                        "created_at": record.created_at,
                        "revoked_at": record.revoked_at,
                        "status": if record.active() { "active" } else { "revoked" },
                    })
                })
                .collect::<Vec<_>>();
            if json {
                print_json(&rows);
            } else {
                println!("CREDENTIAL\tCONTROL PLANE\tSTATUS\tCREATED");
                for record in &trusted.credentials {
                    println!(
                        "{}\t{}\t{}\t{}",
                        record.credential_id,
                        record.control_plane,
                        if record.active() { "active" } else { "revoked" },
                        record.created_at.to_rfc3339()
                    );
                }
            }
        }
        TargetCredentialCommands::Revoke {
            credential_id,
            credentials,
            json,
        } => {
            let mut trusted = TargetCredentials::load(&credentials).map_err(credential_error)?;
            let record = trusted.revoke(&credential_id).map_err(credential_error)?;
            trusted.save(&credentials).map_err(credential_error)?;
            if json {
                print_json(&serde_json::json!({
                    "credential_id": record.credential_id,
                    "control_plane": record.control_plane,
                    "revoked_at": record.revoked_at,
                    "status": "revoked",
                }));
            } else {
                println!("Revoked {}", record.credential_id);
            }
        }
    }
    Ok(())
}

pub async fn target(command: TargetCommand) -> compute_core::Result<()> {
    match command.command {
        TargetCommands::Credential { command } => return target_credential(command),
        TargetCommands::List { json } => {
            let client = command.daemon.client()?;
            let targets: Vec<compute_placement::ComputeTarget> =
                client.get("/targets").await.map_err(error)?;
            if json {
                print_json(&targets);
                return Ok(());
            }
            println!(
                "TARGET\tHOSTS COMPUTERS\tPLATFORM\tCPU\tMEMORY\tFEATURES\tHEALTH\tAUTHENTICATION"
            );
            for target in targets {
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    target.target_id,
                    if target.hosts_computers { "yes" } else { "no" },
                    target
                        .platform
                        .as_ref()
                        .map(|platform| platform.label())
                        .unwrap_or_default(),
                    target
                        .resources
                        .as_ref()
                        .map(|resources| resources.capacity.cpu_count.to_string())
                        .unwrap_or_default(),
                    target
                        .resources
                        .as_ref()
                        .map(|resources| size(resources.capacity.memory_bytes))
                        .unwrap_or_default(),
                    target.features.join(","),
                    target.health,
                    match (target.authentication.as_deref(), target.credential) {
                        (Some("credential"), true) => "credential".to_owned(),
                        (Some(other), _) => other.to_owned(),
                        (None, true) => "credential (unconfirmed)".to_owned(),
                        (None, false) => "-".to_owned(),
                    }
                );
            }
        }
    }
    Ok(())
}
