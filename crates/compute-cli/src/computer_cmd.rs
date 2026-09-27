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
    NetworkPolicy, PackageSpec, ProcessDesired, ProcessKind, ProcessSpec, RepositorySpec,
    SessionCommand,
};
use compute_environment::client::DaemonClient;
use compute_environment::{
    ComputerEnvironmentDefinition, ComputerExec, ComputerJob, ComputerRequest, ComputerView,
    ContentsUpdate, DesiredState, EnvironmentView,
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
                        }),
                    )
                    .await
                    .map_err(error)?;
                print_computer(&view, args.json);
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
                        }),
                    )
                    .await
                    .map_err(error)?;
                print_computer(&view, json);
            }
        },
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
    let path = format!(
        "/environments/{}/jobs/{}",
        args.environment, submitted.job_id
    );
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
    if args.json {
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
    println!("Owner:       {}", view.owner);
    if let Some(kind) = &view.provider_kind {
        println!("Provider:    {kind}");
    }
    if let Some(session) = &view.session_id {
        println!("Session:     {session}");
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
}

pub async fn target(command: TargetCommand) -> compute_core::Result<()> {
    let client = command.daemon.client()?;
    match command.command {
        TargetCommands::List { json } => {
            let targets: Vec<compute_placement::ComputeTarget> =
                client.get("/targets").await.map_err(error)?;
            if json {
                print_json(&targets);
                return Ok(());
            }
            println!("TARGET\tHOSTS COMPUTERS\tPLATFORM\tCPU\tMEMORY\tFEATURES\tHEALTH");
            for target in targets {
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}",
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
                    target.health
                );
            }
        }
    }
    Ok(())
}
