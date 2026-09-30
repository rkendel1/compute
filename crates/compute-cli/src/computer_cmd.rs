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
    ComputeError, ComputerLifecycle, ComputerRequirements, EnvironmentContents, HttpReadiness,
    IsolationProfile, NetworkPolicy, PackageSpec, PlatformIdentity, ProcessDesired, ProcessKind,
    ProcessSpec, ProjectSpec, ProviderRuntimeRequirement, RepositorySpec, RuntimeKind,
    SessionCommand,
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
    /// Ask for the computer a recipe resolves to (`NAME` or `NAME@VERSION`;
    /// see `compute recipe resolve`) instead of stating requirements here.
    /// Only `--target` and `--contents` combine with it.
    #[arg(
        long,
        conflicts_with_all = [
            "cpu", "memory", "disk", "architecture", "network", "isolation", "require",
            "features", "persistent", "ephemeral", "ttl"
        ]
    )]
    pub recipe: Option<String>,
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
            || self.recipe.is_some()
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
            runtimes: vec![],
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
    let (request, policy, recipe) = match &computer.recipe {
        Some(selector) => {
            if policy.is_some() {
                return Err(ComputeError::InvalidWorkload(
                    "a recipe brings its own execution policy; drop --environment-policy".into(),
                ));
            }
            // The recipe is resolved by the control plane, read-only; what
            // it resolves to is sent back as the ordinary request, with the
            // version's identity as evidence. Invalid and unsatisfied are
            // told apart here, before anything is recorded.
            let (recipe, version) = match selector.split_once('@') {
                Some((recipe, version)) => (
                    recipe,
                    Some(version.parse::<u64>().map_err(|_| {
                        ComputeError::InvalidWorkload(format!(
                            "recipe version {version:?} is not a number"
                        ))
                    })?),
                ),
                None => (selector.as_str(), None),
            };
            let resolution =
                crate::recipe_cmd::resolution(client, recipe, version, computer.target.as_deref())
                    .await?;
            if let Some(refusal) = crate::recipe_cmd::unusable(recipe, &resolution) {
                return Err(refusal);
            }
            let resolved = resolution.resolved.expect("a satisfiable recipe resolves");
            let mut request = resolved.computer;
            request.target = computer.target.clone();
            (request, resolved.policy, resolution.recipe)
        }
        None => (
            ComputerRequest {
                lifecycle: if computer.ephemeral {
                    ComputerLifecycle::Ephemeral
                } else {
                    ComputerLifecycle::Persistent
                },
                requirements: computer.requirements(),
                target: computer.target.clone(),
                ttl_seconds: computer.ttl.map(|ttl| ttl.as_secs().max(1)),
            },
            policy,
            None,
        ),
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
        computer: request,
        contents,
        recipe,
    };
    client
        .post("/environments", Some(&definition))
        .await
        .map_err(error)
}

#[derive(Subcommand, Debug)]
pub enum WorkspaceCommands {
    /// Write the computer's workspace to a file and print its digest.
    Export {
        environment: String,
        /// The archive to write.
        #[arg(long, short)]
        output: std::path::PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Seed the computer's empty workspace from an archive, and prove it landed.
    Seed {
        environment: String,
        archive: std::path::PathBuf,
        /// Refuse the archive unless it holds exactly this workspace.
        #[arg(long)]
        digest: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Measure the computer's workspace, and compare it with a digest.
    Verify {
        environment: String,
        /// The digest it should have. Without one, only measure.
        #[arg(long)]
        digest: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Debug)]
#[command(args_conflicts_with_subcommands = true)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: Option<ConfigCommands>,
    pub environment: Option<String>,
    /// Set KEY=VALUE (repeatable).
    #[arg(long = "set", value_parser = parse_pair)]
    pub set: Vec<(String, String)>,
    /// Remove KEY (repeatable).
    #[arg(long)]
    pub unset: Vec<String>,
    /// A variable being set whose value may be shown (repeatable).
    #[arg(long)]
    pub public: Vec<String>,
    /// A variable being set that is sensitive whatever its name says.
    #[arg(long)]
    pub secret: Vec<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Subcommand, Debug)]
pub enum ConfigCommands {
    /// Import `.env` files as configuration: parsed and validated whole, then
    /// applied as one new generation. The files are not copied into the
    /// workspace, and no value is printed.
    Import {
        environment: String,
        /// `.env` files, applied in the order given (a later file overrides).
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// A variable whose value may be shown (repeatable).
        #[arg(long)]
        public: Vec<String>,
        /// A variable that is sensitive whatever its name says (repeatable).
        #[arg(long)]
        secret: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// List the variables the workspace's `.env` files ask for or provide
    /// (names only; nothing is imported).
    Discover {
        environment: String,
        #[arg(long)]
        json: bool,
    },
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
    /// Fork the environment: a new environment on a new computer, from this
    /// one's workspace files, declared contents, and policy (never its
    /// configuration values, sessions, or machine).
    Fork {
        environment: String,
        /// The new environment's name.
        name: String,
        /// Constrain placement of the new computer to one target.
        #[arg(long)]
        target: Option<String>,
        /// Also copy configuration values (they may be credentials).
        #[arg(long)]
        copy_config: bool,
        #[arg(long)]
        json: bool,
    },
    /// Capture a checkpoint: immutable, verified, portable workspace state,
    /// held as a durable artifact. Not a machine snapshot: no process,
    /// memory, session, or credential is captured.
    Checkpoint {
        environment: String,
        /// The checkpoint this one is derived from (recorded as lineage).
        #[arg(long)]
        parent: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Restore a checkpoint into a new environment on a new computer: its
    /// workspace, verified, with the declared state of the environment it came
    /// from. Restores no process, session, credential, or machine.
    Restore {
        checkpoint: String,
        /// The new environment's name.
        name: String,
        /// Constrain placement of the new computer to one target.
        #[arg(long)]
        target: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List an environment's checkpoints, or show one and validate its
    /// artifact.
    Checkpoints {
        environment: String,
        /// Show this checkpoint and validate its artifact.
        checkpoint: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Workspace state: export a computer's workspace, seed an empty one,
    /// verify one against a digest. See docs/workspace.md.
    #[command(subcommand)]
    Workspace(WorkspaceCommands),
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
    /// The environment's configuration: the runtime inputs its processes,
    /// builds, and commands are given. Shows what is configured (never a
    /// sensitive value); `--set`/`--unset` change it; `import` reads `.env`
    /// files; `discover` lists what the workspace's `.env` files ask for. What
    /// depends on a change restarts in place.
    Config(ConfigArgs),
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
    /// Runtime the target must resolve for this process (`node`, `python`,
    /// `jvm`, `dotnet`, ...). Without this, the command keeps using the
    /// computer's ordinary PATH.
    #[arg(long)]
    runtime: Option<RuntimeKind>,
    /// Version or constraint for a pinned runtime distribution.
    #[arg(long, requires = "runtime")]
    runtime_version: Option<String>,
    /// Architecture required by the runtime (`x86_64`, `arm64`).
    #[arg(long, requires_all = ["runtime", "runtime_os"])]
    runtime_architecture: Option<String>,
    /// Operating system required by the runtime (`linux`, `macos`). Must be
    /// explicit because the CLI machine is not necessarily the target.
    #[arg(long, requires_all = ["runtime", "runtime_architecture"])]
    runtime_os: Option<String>,
    /// Add it stopped.
    #[arg(long)]
    stopped: bool,
    /// The port it listens on: published as an endpoint, and given to it
    /// as $PORT.
    #[arg(long)]
    port: Option<u16>,
    /// Ready when a GET of this path, made inside the computer, answers as
    /// expected (`/health`). Without it, a running process is never
    /// `ready`, only `running`.
    #[arg(long = "ready-path")]
    ready_path: Option<String>,
    /// The port the readiness request goes to (default: --port).
    #[arg(long = "ready-port", requires = "ready_path")]
    ready_port: Option<u16>,
    /// The answers that mean ready: a status (`204`) or a class (`2xx`).
    #[arg(long = "ready-expect", requires = "ready_path", default_value = "2xx")]
    ready_expect: String,
    /// How long one readiness request may take, in seconds.
    #[arg(long = "ready-timeout", requires = "ready_path", default_value_t = 2)]
    ready_timeout: u64,
    /// How long it may take to become ready (or be unready) before that is
    /// a failure, in seconds.
    #[arg(long = "ready-deadline", requires = "ready_path", default_value_t = 60)]
    ready_deadline: u64,
    /// When Compute restarts it after it exits, fails to start, or misses
    /// its readiness deadline: never, on-failure, or always. A process
    /// stopped on purpose is never restarted.
    #[arg(long = "restart", default_value = "always")]
    restart_policy: compute_core::ProcessRestartPolicy,
    /// Automatic restarts in a row that may fail before Compute stops.
    #[arg(long = "max-restarts", default_value_t = compute_core::DEFAULT_MAX_RESTARTS)]
    max_restarts: u32,
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
            runtime: self.runtime.map(|runtime| ProviderRuntimeRequirement {
                runtime,
                version: self.runtime_version.clone(),
                platform: self
                    .runtime_architecture
                    .as_ref()
                    .zip(self.runtime_os.as_ref())
                    .map(|(architecture, os)| PlatformIdentity {
                        os: os.clone(),
                        architecture: architecture.clone(),
                        runtime_abi: None,
                    }),
            }),
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
            readiness: self.ready_path.clone().map(|path| HttpReadiness {
                path,
                port: self.ready_port,
                expect: self.ready_expect.clone(),
                request_timeout_seconds: self.ready_timeout,
                deadline_seconds: self.ready_deadline,
            }),
            restart_policy: self.restart_policy,
            max_restarts: self.max_restarts,
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

async fn workspace(client: &DaemonClient, command: WorkspaceCommands) -> compute_core::Result<()> {
    use compute_environment::*;
    match command {
        WorkspaceCommands::Export {
            environment,
            output,
            json,
        } => {
            let export: WorkspaceExport = client
                .post::<(), _>(
                    &format!("/environments/{environment}/workspace/export"),
                    None,
                )
                .await
                .map_err(error)?;
            std::fs::write(&output, &export.archive)?;
            if json {
                print_json(&serde_json::json!({
                    "digest": export.digest, "archive_digest": export.archive_digest,
                    "files": export.files, "directories": export.directories,
                    "bytes": export.bytes, "job_id": export.job_id, "archive": output,
                }));
            } else {
                println!("Workspace {}", export.digest);
                println!(
                    "Wrote {} ({} files, {} empty directories, {} bytes of content)",
                    output.display(),
                    export.files,
                    export.directories,
                    export.bytes
                );
            }
        }
        WorkspaceCommands::Seed {
            environment,
            archive,
            digest,
            json,
        } => {
            let seed: WorkspaceSeed = client
                .post(
                    &format!("/environments/{environment}/workspace/seed"),
                    Some(&WorkspaceSeedRequest {
                        archive: std::fs::read(&archive)?,
                        digest,
                    }),
                )
                .await
                .map_err(error)?;
            if json {
                print_json(&seed);
            } else {
                println!(
                    "Seeded {environment}: {} files, workspace {} (verified inside the computer)",
                    seed.files, seed.digest
                );
            }
        }
        WorkspaceCommands::Verify {
            environment,
            digest,
            json,
        } => {
            let result: WorkspaceVerification = client
                .post(
                    &format!("/environments/{environment}/workspace/verify"),
                    Some(&WorkspaceVerifyRequest { digest }),
                )
                .await
                .map_err(error)?;
            if json {
                print_json(&result);
            } else {
                println!("Workspace {}", result.digest);
                match &result.expected {
                    Some(expected) if result.verified => println!("Matches {expected}"),
                    Some(expected) => println!("MISMATCH: expected {expected}"),
                    None => {}
                }
            }
            if result.expected.is_some() && !result.verified {
                std::process::exit(1);
            }
        }
    }
    Ok(())
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
            // Without requirement flags the computer is replaced with one that
            // meets the requirements it already has.
            let requirements = if computer.requested() {
                computer.requirements()
            } else {
                let current: ComputerView = client
                    .get(&format!("/environments/{environment}/computer"))
                    .await
                    .map_err(error)?;
                current.requirements
            };
            let view: ComputerView = client
                .post(
                    &format!("/environments/{environment}/replace"),
                    Some(&requirements),
                )
                .await
                .map_err(error)?;
            print_computer(&view, json);
        }
        ComputerCommands::Fork {
            environment,
            name,
            target,
            copy_config,
            json,
        } => {
            let report: compute_environment::ForkReport = client
                .post(
                    &format!("/environments/{environment}/fork"),
                    Some(&compute_environment::ForkRequest {
                        name,
                        target,
                        copy_config,
                    }),
                )
                .await
                .map_err(error)?;
            if json {
                print_json(&report);
            } else {
                println!("Forked {} into {}", report.source, report.environment);
                println!(
                    "Seeded:   {} files, {} bytes, workspace {} (verified inside the new computer)",
                    report.files, report.bytes, report.workspace
                );
                for (name, (from, to)) in &report.repositories {
                    println!(
                        "Repo:     {name} {} -> {}",
                        from.as_deref().unwrap_or("-"),
                        to.as_deref().unwrap_or("-")
                    );
                }
                if !report.omitted_config.is_empty() {
                    println!(
                        "Config:   {} not copied (use --copy-config)",
                        report.omitted_config.join(", ")
                    );
                }
                print_computer(&report.computer, false);
            }
        }
        ComputerCommands::Checkpoint {
            environment,
            parent,
            json,
        } => {
            let report: compute_environment::CheckpointReport = client
                .post(
                    &format!("/environments/{environment}/checkpoint"),
                    Some(&compute_environment::CheckpointRequest { parent }),
                )
                .await
                .map_err(error)?;
            if json {
                print_json(&report);
            } else {
                println!(
                    "Checkpoint {} of {}",
                    report.checkpoint_id, report.environment
                );
                println!(
                    "Workspace: {} ({} files, {} empty directories)",
                    report.workspace, report.files, report.directories
                );
                println!(
                    "Artifact:  {} ({}, {} bytes)",
                    report.artifact, report.format, report.size
                );
                if let Some(parent) = &report.parent {
                    println!("Parent:    {parent}");
                }
                println!(
                    "Verified:  {} (read back from the artifact store and validated); operation job {} succeeded{}",
                    report.verified,
                    report.job_id,
                    if report.existing {
                        "; this state was already checkpointed"
                    } else {
                        ""
                    }
                );
            }
        }
        ComputerCommands::Restore {
            checkpoint,
            name,
            target,
            json,
        } => {
            let report: compute_environment::RestoreReport = client
                .post(
                    &format!("/checkpoints/{checkpoint}/restore"),
                    Some(&compute_environment::RestoreRequest { name, target }),
                )
                .await
                .map_err(error)?;
            if json {
                print_json(&report);
            } else {
                println!(
                    "Restored checkpoint {} into {}",
                    report.checkpoint_id, report.environment
                );
                println!(
                    "From:        {} (declared state {})",
                    report.source, report.declared_state
                );
                println!(
                    "Environment: {}  Computer: {}",
                    report.environment_id, report.computer_id
                );
                println!(
                    "Workspace:   {} ({} files, {} bytes), verified inside the new computer: {}",
                    report.workspace, report.files, report.bytes, report.workspace_verified
                );
                if !report.omitted_config.is_empty() {
                    println!(
                        "Config:      {} not restored",
                        report.omitted_config.join(", ")
                    );
                }
                if !report.configuration_required.is_empty() {
                    println!(
                        "Configure:   {} (no values are restored: compute environment config)",
                        report
                            .configuration_required
                            .iter()
                            .map(|variable| variable.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                println!("Operation:   {} durable jobs succeeded", report.jobs.len());
                print_computer(&report.computer, false);
            }
        }
        ComputerCommands::Checkpoints {
            environment,
            checkpoint,
            json,
        } => match checkpoint {
            Some(checkpoint) => {
                let view: compute_environment::CheckpointView = client
                    .get(&format!(
                        "/environments/{environment}/checkpoints/{checkpoint}"
                    ))
                    .await
                    .map_err(error)?;
                if json {
                    print_json(&view);
                } else {
                    let record = &view.checkpoint;
                    println!(
                        "Checkpoint {} of {}",
                        record.checkpoint_id, record.environment
                    );
                    println!("Workspace: {}", record.workspace_digest);
                    println!(
                        "Artifact:  {} ({}, {} bytes)",
                        record.artifact_id, record.format, record.size
                    );
                    println!(
                        "Valid:     {}{}",
                        view.valid.unwrap_or(false),
                        view.invalid_reason
                            .map(|why| format!(" ({why})"))
                            .unwrap_or_default()
                    );
                }
            }
            None => {
                let views: Vec<compute_environment::CheckpointView> = client
                    .get(&format!("/environments/{environment}/checkpoints"))
                    .await
                    .map_err(error)?;
                if json {
                    print_json(&views);
                } else if views.is_empty() {
                    println!("No checkpoints.");
                } else {
                    for view in views {
                        let record = view.checkpoint;
                        println!(
                            "{}  {}  {} files  {}",
                            record.checkpoint_id,
                            record.workspace_digest,
                            record.files,
                            record.created_at
                        );
                    }
                }
            }
        },
        ComputerCommands::Workspace(command) => workspace(client, command).await?,
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
        ComputerCommands::Config(args) => configuration(client, args).await?,
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
    // Observed reality first; what the environment asks for beside it.
    let reality = &view.reality;
    println!(
        "Computer:    {} (desired {}; {}){}",
        reality.observed,
        reality.desired,
        view.lifecycle.as_str(),
        view.target
            .as_ref()
            .map(|target| format!(" on {target}"))
            .unwrap_or_default()
    );
    if !reality.explanation.is_empty() {
        println!("             {}", reality.explanation);
    }
    if let Some(at) = reality.confirmed_at {
        println!("Confirmed:   {} by its target", at.to_rfc3339());
    }
    if let Some(since) = reality.since {
        println!("Since:       {}", since.to_rfc3339());
    }
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
        println!(
            "  {:<16} {:<12} {:<8} {:<10} {:<8} {:<8} COMMAND",
            "NAME", "KIND", "DESIRED", "PROCESS", "PID", "RESTARTS"
        );
        for process in &view.desired.processes {
            let reality = view.reality.processes.get(&process.name);
            println!(
                "  {:<16} {:<12} {:<8} {:<10} {:<8} {:<8} {}",
                process.name,
                process.kind.as_str(),
                reality.map_or("running", |reality| reality.desired.as_str()),
                reality.map_or("pending", |reality| reality.process.as_str()),
                reality
                    .and_then(|reality| reality.pid)
                    .map(|pid| pid.to_string())
                    .unwrap_or_else(|| "-".into()),
                reality.map_or(0, |reality| reality.restarts),
                process.command.join(" ")
            );
            let Some(reality) = reality else { continue };
            if let (Some(check), Some(readiness)) = (&process.readiness, &reality.readiness) {
                println!(
                    "    readiness: {readiness} (GET {} expects {}{})",
                    check.path,
                    check.expect,
                    reality
                        .readiness_detail
                        .as_ref()
                        .map(|detail| format!("; last: {detail}"))
                        .unwrap_or_default()
                );
            }
            println!(
                "    restart: {} ({} in a row of at most {}{})",
                reality.restart_policy.as_str(),
                reality.attempts,
                reality.max_restarts,
                reality
                    .next_restart_at
                    .map(|at| format!("; next at {}", at.to_rfc3339()))
                    .unwrap_or_default()
            );
            if let Some(failure) = &reality.last_failure {
                println!(
                    "    last failure: {} ({}, {}): {}",
                    failure.message,
                    failure.reason,
                    failure.at.to_rfc3339(),
                    failure.decision
                );
            }
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

async fn configuration(client: &DaemonClient, args: ConfigArgs) -> compute_core::Result<()> {
    use compute_environment::*;
    match args.command {
        Some(ConfigCommands::Import {
            environment,
            files,
            public,
            secret,
            json,
        }) => {
            let mut sent = vec![];
            for path in files {
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| {
                        compute_core::ComputeError::Runtime(format!(
                            "{} is not a file",
                            path.display()
                        ))
                    })?
                    .to_owned();
                let content = std::fs::read_to_string(&path)?;
                sent.push(ConfigFile { name, content });
            }
            let report: ConfigImportReport = client
                .post(
                    &format!("/environments/{environment}/config/import"),
                    Some(&ConfigImportRequest {
                        files: sent,
                        public,
                        secret,
                    }),
                )
                .await
                .map_err(error)?;
            if json {
                print_json(&report);
            } else {
                if report.unchanged {
                    println!(
                        "Nothing changed: {} already holds these variables (configuration generation {}).",
                        report.environment, report.generation
                    );
                } else {
                    println!(
                        "Imported {} variables into {} from {} (configuration generation {}).",
                        report.imported.len(),
                        report.environment,
                        report.files.join(", "),
                        report.generation
                    );
                }
                for variable in &report.imported {
                    println!(
                        "  {:<28} {:<10} {:<10} {}",
                        variable.name,
                        if variable.changed {
                            "imported"
                        } else {
                            "unchanged"
                        },
                        if variable.sensitive {
                            "secret"
                        } else {
                            "non-secret"
                        },
                        variable.source
                    );
                }
                for skipped in &report.skipped {
                    println!("  {:<28} skipped: {}", skipped.name, skipped.reason);
                }
                if !report.overridden.is_empty() {
                    println!(
                        "  defined in more than one file (the later won): {}",
                        report.overridden.join(", ")
                    );
                }
            }
        }
        Some(ConfigCommands::Discover { environment, json }) => {
            let found: ConfigurationDiscovery = client
                .post::<(), _>(
                    &format!("/environments/{environment}/config/discover"),
                    None,
                )
                .await
                .map_err(error)?;
            if json {
                print_json(&found);
            } else if found.files.is_empty() {
                println!("No .env files in {}'s workspace.", found.environment);
            } else {
                println!("Environment: {}", found.environment);
                for file in &found.files {
                    println!("  {} ({})", file.path, file.kind);
                }
                println!("Variables:");
                for variable in &found.variables {
                    println!(
                        "  {:<28} {:<10} {:<10} {}",
                        variable.name,
                        variable.status,
                        if variable.sensitive {
                            "secret"
                        } else {
                            "non-secret"
                        },
                        variable.files.join(", ")
                    );
                }
            }
        }
        None => {
            let environment = args.environment.ok_or_else(|| {
                compute_core::ComputeError::Runtime(
                    "name the environment: compute environment config NAME".into(),
                )
            })?;
            let view: ConfigurationView = if args.set.is_empty() && args.unset.is_empty() {
                client
                    .get(&format!("/environments/{environment}/config"))
                    .await
                    .map_err(error)?
            } else {
                client
                    .post(
                        &format!("/environments/{environment}/config/change"),
                        Some(&ConfigChange {
                            set: args.set.into_iter().collect(),
                            unset: args.unset,
                            public: args.public,
                            secret: args.secret,
                            source: Some("cli".into()),
                        }),
                    )
                    .await
                    .map_err(error)?
            };
            if args.json {
                print_json(&view);
            } else {
                println!(
                    "Environment: {}  (configuration generation {})",
                    view.environment, view.generation
                );
                if view.variables.is_empty() {
                    println!("  No configuration.");
                }
                for variable in &view.variables {
                    println!(
                        "  {:<28} {:<10} {:<10} {}{}",
                        variable.name,
                        if variable.configured {
                            "configured"
                        } else {
                            "missing"
                        },
                        if variable.sensitive {
                            "secret"
                        } else {
                            "non-secret"
                        },
                        variable.source,
                        variable
                            .value
                            .as_ref()
                            .map(|value| format!("  = {value}"))
                            .unwrap_or_default()
                    );
                }
            }
        }
    }
    Ok(())
}
