use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use compute_core::{
    DependencyCapsule, DependencyEntry, EnvironmentVariable, IsolationProfile, Mount,
    NetworkPolicy, PlatformIdentity, ResourceLimits, RuntimeKind, RuntimeSpec,
};
use compute_provider::{
    ComputeProvider, ProviderPolicy, ProviderRequest, RemoteProvider, ServerConfig,
};
use compute_runtime::Compute;

mod admission;
mod certification;
mod direct;
mod distribution;
mod placement_certification;
mod pool;
mod receipt;

#[derive(Parser, Debug)]
#[command(
    name = "compute",
    version,
    about = "Runtime-neutral workload execution"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Run(Box<RunCommand>),
    Bundle(BundleCommand),
    /// Create and verify portable dependency capsules.
    Deps(DepsCommand),
    Inspect(InspectCommand),
    Runtimes(JsonFlag),
    Runtime(RuntimeCommand),
    Capabilities(RuntimeCommand),
    /// Show the versioned isolation profiles and runtime support matrix.
    Isolation(JsonFlag),
    Exec(ExecCommand),
    Doctor(JsonFlag),
    Certify(CertifyCommand),
    /// Build, inspect, or verify a portable Compute distribution.
    Distribution(DistributionCommand),
    /// Inspect or independently verify an execution receipt.
    Receipt(ReceiptCommand),
    Version(JsonFlag),
    /// Execute portable workloads through a remote Compute provider.
    Remote(RemoteCommand),
    /// Discover, inspect, and refresh providers in the caller-owned pool.
    Provider(pool::ProviderCommand),
    /// Evaluate which provider can satisfy a workload. Never executes.
    Placement(pool::PlacementCommand),
    /// Place a workload on a compatible provider and execute or submit it.
    Pool(pool::PoolCommand),
    /// Serve compute.remote@1 with durable filesystem-backed jobs.
    Serve(ServeCommand),
}

#[derive(Args, Debug)]
struct ServeCommand {
    #[arg(long, default_value = "127.0.0.1:8080")]
    listen: std::net::SocketAddr,
    /// Public identity recorded in remote receipts.
    #[arg(long)]
    public_url: Option<String>,
    #[arg(long, default_value = ".compute/jobs")]
    job_store: PathBuf,
    #[arg(long, default_value = "7d", value_parser = parse_retention)]
    job_retention: Duration,
    #[arg(long, default_value_t = 4)]
    max_concurrent_jobs: usize,
    /// Offer only these runtimes (repeatable). Withheld runtimes are neither
    /// advertised nor executed.
    #[arg(long = "allow-runtime")]
    allow_runtimes: Vec<String>,
    /// Offer only these isolation profiles (repeatable).
    #[arg(long = "allow-isolation", value_parser = parse_isolation)]
    allow_isolation: Vec<IsolationProfile>,
    /// Offer only these network policies (repeatable).
    #[arg(long = "allow-network", value_parser = parse_network)]
    allow_network: Vec<NetworkPolicy>,
    /// Largest wall-time limit a workload may request.
    #[arg(long, value_parser = parse_duration)]
    max_timeout: Option<Duration>,
    /// Largest memory limit a workload may request.
    #[arg(long, value_parser = parse_memory)]
    max_memory: Option<u64>,
    /// Execution policy this server enforces: `--policy FILE`, or
    /// `[server.policy] path` in `--config`/compute.toml.
    #[command(flatten)]
    execution_policy: admission::PolicyLocation,
}

#[derive(Args, Debug)]
struct RemoteCommand {
    #[command(subcommand)]
    command: RemoteCommands,
}

#[derive(Subcommand, Debug)]
enum RemoteCommands {
    Run(Box<RemoteArtifactCommand>),
    Inspect(Box<RemoteArtifactCommand>),
    Submit(Box<RemoteArtifactCommand>),
    Status(RemoteJobCommand),
    Result(RemoteJobCommand),
    Wait(RemoteWaitCommand),
    Receipt(RemoteReceiptCommand),
    Artifacts(RemoteJobCommand),
    Cancel(RemoteJobCommand),
    Capabilities(RemoteEndpointCommand),
    Health(RemoteEndpointCommand),
}

#[derive(Args, Debug)]
struct RemoteJobCommand {
    #[arg(long)]
    provider: String,
    job_id: String,
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct RemoteWaitCommand {
    #[arg(long)]
    provider: String,
    job_id: String,
    #[arg(long, default_value = "60s", value_parser = parse_duration)]
    timeout: Duration,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    receipt: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct RemoteReceiptCommand {
    #[arg(long)]
    provider: String,
    job_id: String,
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct RemoteEndpointCommand {
    #[arg(long)]
    provider: String,
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct RemoteArtifactCommand {
    #[arg(required_unless_present = "bundle", conflicts_with = "bundle")]
    path: Option<PathBuf>,
    #[arg(long)]
    bundle: Option<PathBuf>,
    #[arg(long)]
    provider: String,
    #[arg(long)]
    runtime: Option<String>,
    #[arg(long = "env", value_parser = parse_env)]
    env: Vec<EnvironmentVariable>,
    #[arg(long)]
    env_file: Option<PathBuf>,
    #[arg(long = "input")]
    inputs: Vec<PathBuf>,
    #[arg(long = "output")]
    outputs: Vec<PathBuf>,
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long)]
    entrypoint: Option<PathBuf>,
    #[arg(long)]
    deps: Option<PathBuf>,
    #[arg(long)]
    offline: bool,
    #[arg(long, value_parser = parse_network)]
    network: Option<NetworkPolicy>,
    #[arg(long, value_parser = parse_isolation)]
    isolation: Option<IsolationProfile>,
    #[arg(long, value_parser = parse_memory)]
    memory: Option<u64>,
    #[arg(long, value_parser = parse_duration)]
    timeout: Option<Duration>,
    #[arg(long)]
    receipt: Option<PathBuf>,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    explain: bool,
    #[arg(last = true)]
    args: Vec<String>,
    /// Prevent duplicate jobs when retrying a submission.
    #[arg(long)]
    idempotency_key: Option<String>,
}

#[derive(Args, Debug)]
struct DistributionCommand {
    #[command(subcommand)]
    command: DistributionCommands,
}

#[derive(Subcommand, Debug)]
enum DistributionCommands {
    Build {
        #[arg(long, default_value = "dist/compute-distribution")]
        output: PathBuf,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        verify: bool,
        #[arg(long)]
        cache: Option<PathBuf>,
        #[arg(long, hide = true)]
        platform: Option<String>,
        #[arg(long, hide = true)]
        lock: Option<PathBuf>,
        #[arg(long, hide = true)]
        compute_binary: Option<PathBuf>,
    },
    Inspect {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    Verify {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Debug)]
struct JsonFlag {
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct ReceiptCommand {
    #[command(subcommand)]
    command: ReceiptCommands,
}

#[derive(Subcommand, Debug)]
enum ReceiptCommands {
    Inspect {
        receipt: PathBuf,
        #[arg(long)]
        json: bool,
    },
    Verify {
        receipt: PathBuf,
        #[arg(long)]
        distribution: Option<PathBuf>,
        #[arg(long)]
        artifacts: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Debug)]
struct CertifyCommand {
    #[arg(long)]
    json: bool,
    #[arg(long, hide = true)]
    internal_clean_environment: bool,
}

#[derive(Args, Debug)]
struct BundleCommand {
    #[command(subcommand)]
    command: BundleCommands,
}

#[derive(Args, Debug)]
struct DepsCommand {
    #[command(subcommand)]
    command: DepsCommands,
}

#[derive(Subcommand, Debug)]
enum DepsCommands {
    Create {
        #[arg(long)]
        runtime: String,
        #[arg(long)]
        runtime_version: Option<String>,
        /// Directory populated by an external package resolver.
        #[arg(long)]
        resolved: PathBuf,
        #[arg(long)]
        lock: Option<PathBuf>,
        #[arg(long, value_parser = parse_platform)]
        platform: Option<PlatformIdentity>,
        /// SBOM-style package inventory entry NAME=VERSION.
        #[arg(long = "package", value_parser = parse_dependency_entry)]
        packages: Vec<DependencyEntry>,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        json: bool,
    },
    Inspect {
        capsule: PathBuf,
        #[arg(long)]
        json: bool,
    },
    Verify {
        capsule: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum BundleCommands {
    Create {
        #[arg(required_unless_present = "workload", conflicts_with = "workload")]
        path: Option<PathBuf>,
        #[arg(long)]
        workload: Option<PathBuf>,
        #[arg(long)]
        runtime: Option<String>,
        #[arg(long)]
        deps: Option<PathBuf>,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        json: bool,
    },
    Inspect {
        bundle: PathBuf,
        #[arg(long)]
        json: bool,
    },
    Verify {
        bundle: PathBuf,
        #[arg(long)]
        expected_workload_id: Option<String>,
        #[arg(long)]
        expected_bundle_id: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Debug)]
struct InspectCommand {
    #[arg(required_unless_present = "workload", conflicts_with = "workload")]
    path: Option<PathBuf>,
    #[arg(long)]
    workload: Option<PathBuf>,
    #[arg(long)]
    runtime: Option<String>,
    #[arg(long)]
    json: bool,
    /// Refuse to inspect a workload whose canonical identity differs.
    #[arg(long, requires = "workload")]
    expected_workload_id: Option<String>,
}

#[derive(Args, Debug)]
struct RuntimeCommand {
    runtime: String,
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct RunCommand {
    #[arg(
        required_unless_present_any = ["workload", "bundle"],
        conflicts_with_all = ["workload", "bundle"]
    )]
    path: Option<PathBuf>,
    /// Load a versioned portable workload specification.
    #[arg(long)]
    workload: Option<PathBuf>,
    /// Execute a verified portable workload bundle.
    #[arg(long, conflicts_with = "workload")]
    bundle: Option<PathBuf>,
    #[arg(long)]
    runtime: Option<String>,
    #[arg(long = "env", value_parser = parse_env)]
    env: Vec<EnvironmentVariable>,
    #[arg(long)]
    env_file: Option<PathBuf>,
    #[arg(long = "input")]
    inputs: Vec<PathBuf>,
    #[arg(long = "output")]
    outputs: Vec<PathBuf>,
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Select an entrypoint when PATH is a directory.
    #[arg(long)]
    entrypoint: Option<PathBuf>,
    /// Attach a verified compute.deps@1 capsule.
    #[arg(long)]
    deps: Option<PathBuf>,
    /// Require execution to use only already-materialized artifacts.
    #[arg(long)]
    offline: bool,
    #[arg(long = "mount", value_parser = parse_mount)]
    mounts: Vec<Mount>,
    #[arg(long, value_parser = parse_network)]
    network: Option<NetworkPolicy>,
    #[arg(long, value_parser = parse_isolation)]
    isolation: Option<IsolationProfile>,
    #[arg(long, value_parser = parse_memory)]
    memory: Option<u64>,
    #[arg(long, value_parser = parse_duration)]
    timeout: Option<Duration>,
    /// UTF-8 text supplied to the workload; stdin is otherwise an immediate EOF.
    #[arg(long)]
    stdin: Option<String>,
    #[arg(long)]
    json: bool,
    /// Write canonical verifiable execution evidence to this file.
    #[arg(long, conflicts_with_all = ["dry_run", "explain"])]
    receipt: Option<PathBuf>,
    /// Validate and plan a workload specification without executing it.
    #[arg(long, conflicts_with = "explain")]
    dry_run: bool,
    /// Print the generated WorkloadSpec without executing it.
    #[arg(long)]
    explain: bool,
    /// Refuse to execute a workload whose canonical identity differs.
    #[arg(long)]
    expected_workload_id: Option<String>,
    /// Refuse to execute a bundle whose deterministic identity differs.
    #[arg(long, requires = "bundle")]
    expected_bundle_id: Option<String>,
    #[command(flatten)]
    policy: admission::PolicyLocation,
    #[arg(last = true)]
    args: Vec<String>,
}

#[derive(Args, Debug)]
struct ExecCommand {
    #[arg(required = true)]
    issue_description: Vec<String>,
    #[arg(long)]
    runtime: Option<String>,
    #[arg(long = "env", value_parser = parse_env)]
    env: Vec<EnvironmentVariable>,
    #[arg(long = "mount", value_parser = parse_mount)]
    mounts: Vec<Mount>,
    #[arg(long, default_value = "network", value_parser = parse_network)]
    network: NetworkPolicy,
    #[arg(long, default_value = "process", value_parser = parse_isolation)]
    isolation: IsolationProfile,
    #[arg(long, value_parser = parse_memory)]
    memory: Option<u64>,
    #[arg(long, value_parser = parse_duration)]
    timeout: Option<Duration>,
    /// UTF-8 text supplied to the workload; stdin is otherwise an immediate EOF.
    #[arg(long)]
    stdin: Option<String>,
    #[arg(long)]
    json: bool,
    /// Write canonical verifiable execution evidence to this file.
    #[arg(long)]
    receipt: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let compute = Compute::new();

    if let Err(error) = run(cli, compute).await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli, compute: Compute) -> compute_core::Result<()> {
    match cli.command {
        Commands::Run(command) => {
            let command = *command;
            let dependency_capsule = command
                .deps
                .as_deref()
                .map(DependencyCapsule::read)
                .transpose()?;
            let _offline = command.offline;
            let policy_sources = command.policy.sources()?;
            if let Some(bundle) = command.bundle {
                if command.runtime.is_some()
                    || !command.env.is_empty()
                    || command.env_file.is_some()
                    || !command.inputs.is_empty()
                    || !command.outputs.is_empty()
                    || command.cwd.is_some()
                    || command.entrypoint.is_some()
                    || !command.mounts.is_empty()
                    || command.network.is_some()
                    || command.memory.is_some()
                    || command.timeout.is_some()
                    || command.stdin.is_some()
                    || !command.args.is_empty()
                    || command.explain
                {
                    return Err(compute_core::ComputeError::InvalidWorkload(
                        "--bundle cannot be combined with direct execution overrides".into(),
                    ));
                }
                if command.dry_run {
                    let plan = compute.plan_bundle_with_dependencies(
                        &bundle,
                        command.expected_workload_id.as_deref(),
                        command.expected_bundle_id.as_deref(),
                        command.isolation,
                        dependency_capsule,
                    )?;
                    print_bundle_plan(&plan, command.json);
                } else {
                    let loaded = compute.load_bundle(&bundle)?;
                    loaded.require_ids(
                        command.expected_workload_id.as_deref(),
                        command.expected_bundle_id.as_deref(),
                    )?;
                    let admitted = admission::admit_locally(
                        &loaded,
                        dependency_capsule.as_ref(),
                        command.isolation,
                        &policy_sources,
                    )
                    .await?;
                    if !admitted.decision.admitted {
                        admission::deny(&admitted, command.json);
                    }
                    let mut result = compute
                        .run_bundle_with_dependencies(
                            &bundle,
                            command.expected_workload_id.as_deref(),
                            command.expected_bundle_id.as_deref(),
                            command.isolation,
                            dependency_capsule,
                        )
                        .await?;
                    admission::bind(&mut result, &admitted)?;
                    print_execution_result(result, command.json, command.receipt.as_deref())?;
                }
                return Ok(());
            }
            if let Some(workload) = command.workload {
                if command.expected_bundle_id.is_some() {
                    return Err(compute_core::ComputeError::InvalidWorkload(
                        "--expected-bundle-id requires --bundle".into(),
                    ));
                }
                if command.runtime.is_some()
                    || !command.env.is_empty()
                    || command.env_file.is_some()
                    || !command.inputs.is_empty()
                    || !command.outputs.is_empty()
                    || command.cwd.is_some()
                    || command.entrypoint.is_some()
                    || !command.mounts.is_empty()
                    || command.network.is_some()
                    || command.memory.is_some()
                    || command.timeout.is_some()
                    || command.stdin.is_some()
                    || !command.args.is_empty()
                    || command.explain
                {
                    return Err(compute_core::ComputeError::InvalidWorkload(
                        "--workload cannot be combined with direct execution overrides".into(),
                    ));
                }
                if command.dry_run {
                    let plan = compute
                        .plan_workload_with_dependencies(
                            &workload,
                            command.expected_workload_id.as_deref(),
                            command.isolation,
                            dependency_capsule,
                        )
                        .await?;
                    print_workload_plan(&plan, command.json);
                } else {
                    let loaded = compute_core::WorkloadBundle::create(&workload)?;
                    if let Some(expected) = command.expected_workload_id.as_deref() {
                        loaded.workload.require_id(expected)?;
                    }
                    let admitted = admission::admit_locally(
                        &loaded,
                        dependency_capsule.as_ref(),
                        command.isolation,
                        &policy_sources,
                    )
                    .await?;
                    if !admitted.decision.admitted {
                        admission::deny(&admitted, command.json);
                    }
                    let mut result = compute
                        .run_workload_with_dependencies(
                            &workload,
                            command.expected_workload_id.as_deref(),
                            command.isolation,
                            dependency_capsule,
                        )
                        .await?;
                    admission::bind(&mut result, &admitted)?;
                    print_execution_result(result, command.json, command.receipt.as_deref())?;
                }
                return Ok(());
            }
            if command.expected_workload_id.is_some() || command.expected_bundle_id.is_some() {
                return Err(compute_core::ComputeError::InvalidWorkload(
                    "expected identities require --workload or --bundle".into(),
                ));
            }
            if !command.mounts.is_empty() {
                return Err(compute_core::ComputeError::InvalidWorkload(
                    "--mount is not part of portable WorkloadSpec; use declared --input files"
                        .into(),
                ));
            }
            let resolved = direct::resolve(direct::DirectOptions {
                path: command.path.expect("required by clap"),
                runtime: command.runtime,
                args: command.args,
                env: command.env,
                env_file: command.env_file,
                inputs: command.inputs,
                outputs: command.outputs,
                cwd: command.cwd,
                entrypoint: command.entrypoint,
                deps: command.deps,
                network: command.network,
                isolation: command.isolation,
                memory: command.memory,
                timeout: command.timeout,
                defaults: command.policy.defaults()?,
            })?;
            if command.explain {
                print_generated_workload(&resolved, command.json)?;
            } else if command.dry_run {
                let plan = compute.plan_generated_workload_with_dependencies(
                    &resolved.root,
                    resolved.workload,
                    resolved.dependency_capsule,
                )?;
                print_workload_plan(&plan, command.json);
            } else {
                let bundle = compute_core::WorkloadBundle::create_from_with_capsule(
                    resolved.workload.clone(),
                    &resolved.root,
                    resolved.dependency_capsule.clone(),
                )?;
                let admitted =
                    admission::admit_locally(&bundle, None, None, &policy_sources).await?;
                if !admitted.decision.admitted {
                    admission::deny(&admitted, command.json);
                }
                let mut result = compute
                    .run_generated_workload_with_dependencies(
                        &resolved.root,
                        resolved.workload,
                        command.stdin.unwrap_or_default().into_bytes(),
                        resolved.dependency_capsule,
                    )
                    .await?;
                admission::bind(&mut result, &admitted)?;
                print_execution_result(result, command.json, command.receipt.as_deref())?;
            }
        }
        Commands::Bundle(command) => match command.command {
            BundleCommands::Create {
                path,
                workload,
                output,
                runtime,
                deps,
                json,
            } => {
                if output.exists() {
                    return Err(compute_core::ComputeError::InvalidBundle(format!(
                        "refusing to overwrite existing bundle: {}",
                        output.display()
                    )));
                }
                let inspection = if let Some(workload) = workload {
                    if runtime.is_some() {
                        return Err(compute_core::ComputeError::InvalidWorkload(
                            "--runtime cannot override --workload".into(),
                        ));
                    }
                    match deps.as_deref().map(DependencyCapsule::read).transpose()? {
                        Some(capsule) => {
                            compute.create_bundle_with_dependencies(&workload, &output, capsule)?
                        }
                        None => compute.create_bundle(&workload, &output)?,
                    }
                } else {
                    let resolved = direct::resolve(direct::DirectOptions {
                        path: path.expect("required by clap"),
                        runtime,
                        args: vec![],
                        env: vec![],
                        env_file: None,
                        inputs: vec![],
                        outputs: vec![],
                        cwd: None,
                        entrypoint: None,
                        deps,
                        network: None,
                        isolation: None,
                        memory: None,
                        timeout: None,
                        defaults: admission::PolicyLocation::default().defaults()?,
                    })?;
                    compute.create_generated_bundle_with_dependencies(
                        &resolved.root,
                        resolved.workload,
                        &output,
                        resolved.dependency_capsule,
                    )?
                };
                let size_bytes = std::fs::metadata(&output)?.len();
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "bundle_id": inspection.bundle_id,
                            "workload_id": inspection.workload_id,
                            "input_count": inspection.inputs.len(),
                            "size_bytes": size_bytes,
                        }))
                        .unwrap()
                    );
                } else {
                    println!("Bundle: {}", output.display());
                    println!("Bundle ID: {}", inspection.bundle_id);
                    println!("Workload ID: {}", inspection.workload_id);
                    println!("Inputs: {}", inspection.inputs.len());
                    println!("Size: {size_bytes} bytes");
                }
            }
            BundleCommands::Inspect { bundle, json } => {
                let inspection = compute.inspect_bundle(&bundle)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&inspection).unwrap());
                } else {
                    println!("Format: {}@{}", inspection.format, inspection.version);
                    println!("Bundle ID: {}", inspection.bundle_id);
                    println!("Workload ID: {}", inspection.workload_id);
                    println!("Runtime: {}", inspection.runtime);
                    println!("Entrypoint: {}", inspection.entrypoint.display());
                    println!("Inputs: {}", inspection.inputs.len());
                    println!("Outputs: {}", inspection.outputs.len());
                    if let Some(dependencies) = inspection.dependencies {
                        println!("Dependency capsule: {}", dependencies.capsule_id);
                    }
                }
            }
            BundleCommands::Verify {
                bundle,
                expected_workload_id,
                expected_bundle_id,
                json,
            } => match compute.load_bundle(&bundle).and_then(|loaded| {
                loaded.require_ids(
                    expected_workload_id.as_deref(),
                    expected_bundle_id.as_deref(),
                )?;
                loaded.verification()
            }) {
                Ok(verification) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&verification).unwrap());
                    } else {
                        println!("Valid: true");
                        println!("Bundle ID: {}", verification.bundle_id);
                        println!("Workload ID: {}", verification.workload_id);
                    }
                }
                Err(error) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "valid": false,
                                "error": {
                                    "kind": bundle_error_kind(&error),
                                    "message": error.to_string(),
                                }
                            }))
                            .unwrap()
                        );
                    }
                    return Err(error);
                }
            },
        },
        Commands::Deps(command) => match command.command {
            DepsCommands::Create {
                runtime,
                runtime_version,
                resolved,
                lock,
                platform,
                packages,
                output,
                json,
            } => {
                if output.exists() {
                    return Err(compute_core::ComputeError::InvalidDependencyCapsule(
                        format!(
                            "refusing to overwrite existing capsule: {}",
                            output.display()
                        ),
                    ));
                }
                let runtime = runtime.parse::<RuntimeKind>()?;
                let runtime_version = match runtime_version {
                    Some(version) => Some(version),
                    None => compute.runtime(runtime, None).await?.version,
                };
                let capsule = DependencyCapsule::create(
                    &resolved,
                    runtime,
                    runtime_version,
                    platform.unwrap_or_else(PlatformIdentity::current),
                    packages,
                    lock.as_deref(),
                )?;
                capsule.write(&output)?;
                print_dependency_inspection(&capsule.inspection()?, json);
            }
            DepsCommands::Inspect { capsule, json } => {
                let capsule = DependencyCapsule::read(&capsule)?;
                print_dependency_inspection(&capsule.inspection()?, json);
            }
            DepsCommands::Verify { capsule, json } => {
                let capsule = DependencyCapsule::read(&capsule)?;
                capsule.require_compatible(capsule.runtime)?;
                let required = capsule
                    .runtime_version
                    .as_deref()
                    .expect("validated capsule runtime version");
                let runtime = compute.runtime(capsule.runtime, Some(required)).await?;
                if !runtime.available
                    || !runtime.compatible
                    || runtime.version.as_deref() != Some(required)
                {
                    return Err(compute_core::ComputeError::InvalidDependencyCapsule(
                        format!(
                            "dependency capsule runtime {}@{required} is unavailable in this Compute distribution",
                            capsule.runtime
                        ),
                    ));
                }
                print_dependency_inspection(&capsule.inspection()?, json);
            }
        },
        Commands::Inspect(command) => {
            if let Some(workload) = command.workload {
                if command.runtime.is_some() {
                    return Err(compute_core::ComputeError::InvalidWorkload(
                        "--runtime cannot override a workload specification".into(),
                    ));
                }
                let plan = match command.expected_workload_id.as_deref() {
                    Some(expected) => compute.plan_workload_with_id(&workload, expected).await?,
                    None => compute.plan_workload(&workload).await?,
                };
                print_workload_plan(&plan, command.json);
                return Ok(());
            }
            let path = command.path.expect("required by clap");
            if path.exists() {
                let resolved = direct::resolve(direct::DirectOptions {
                    path,
                    runtime: command.runtime,
                    args: vec![],
                    env: vec![],
                    env_file: None,
                    inputs: vec![],
                    outputs: vec![],
                    cwd: None,
                    entrypoint: None,
                    deps: None,
                    network: None,
                    isolation: None,
                    memory: None,
                    timeout: None,
                    defaults: admission::PolicyLocation::default().defaults()?,
                });
                match resolved {
                    Ok(resolved) => print_generated_workload(&resolved, command.json)?,
                    Err(error)
                        if error.to_string().contains("multiple entrypoints detected")
                            || error.to_string().contains("Cannot resolve runtime") =>
                    {
                        print_ambiguous_inspection(&error, command.json);
                    }
                    Err(error) => return Err(error),
                }
                return Ok(());
            }
            let runtime = parse_runtime_spec(command.runtime)?;
            let inspection = compute.inspect_path(&path, runtime)?;
            if command.json {
                println!("{}", serde_json::to_string_pretty(&inspection).unwrap());
            } else {
                println!("Path: {}", inspection.path.display());
                println!("Entrypoint: {}", inspection.entrypoint.display());
                if let Some(runtime) = inspection.runtime {
                    println!("Runtime: {}", runtime.kind);
                }
                if inspection.ambiguous {
                    println!("Runtime selection is ambiguous.");
                    println!("Candidates:");
                    for candidate in inspection.candidates {
                        println!("  {}", candidate);
                    }
                }
            }
        }
        Commands::Runtimes(json_flag) => {
            let inventory = compute.inventory().await;
            if json_flag.json {
                println!("{}", serde_json::to_string_pretty(&inventory).unwrap());
            } else {
                println!("Runtime\tPinned\tDetected\tSource\tAvailable");
                for runtime in inventory.runtimes {
                    println!(
                        "{}\t{}\t{}\t{:?}\t{}",
                        runtime.id,
                        runtime.version,
                        runtime.detected_version.unwrap_or_else(|| "-".to_string()),
                        runtime.source,
                        if runtime.available { "yes" } else { "no" }
                    );
                }
            }
        }
        Commands::Runtime(command) => {
            let kind: RuntimeKind = command.runtime.parse()?;
            let runtime = compute.runtime(kind, None).await?;
            let capabilities = compute.capabilities(kind)?;
            if command.json {
                let mut value = serde_json::to_value(runtime).unwrap();
                value["isolation"] = serde_json::to_value(capabilities.isolation).unwrap();
                println!("{}", serde_json::to_string_pretty(&value).unwrap());
            } else {
                println!("Runtime: {}", runtime.kind);
                println!("Known: {}", runtime.known);
                println!("Installed: {}", runtime.installed);
                println!("Available: {}", runtime.available);
                println!("Compatible: {}", runtime.compatible);
                if let Some(version) = runtime.version {
                    println!("Version: {version}");
                }
                if let Some(executable) = runtime.executable {
                    println!("Executable: {}", executable.display());
                }
                println!("Isolation:");
                println!(
                    "  Process boundary: {}",
                    yes_no(capabilities.isolation.process_boundary)
                );
                println!(
                    "  Filesystem boundary: {}",
                    yes_no(capabilities.isolation.filesystem_boundary)
                );
                println!(
                    "  Network boundary: {}",
                    yes_no(capabilities.isolation.network_boundary)
                );
                println!(
                    "  Timeout enforcement: {}",
                    yes_no(capabilities.isolation.timeout_enforcement)
                );
                println!(
                    "  Memory enforcement: {}",
                    yes_no(capabilities.isolation.memory_enforcement)
                );
            }
        }
        Commands::Capabilities(command) => {
            let kind: RuntimeKind = command.runtime.parse()?;
            let capabilities = compute.capabilities(kind)?;
            if command.json {
                println!("{}", serde_json::to_string_pretty(&capabilities).unwrap());
            } else {
                print_capabilities(&capabilities, "");
            }
        }
        Commands::Isolation(json_flag) => print_isolation_profiles(&compute, json_flag.json),
        Commands::Version(json_flag) => {
            if json_flag.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "name": "compute",
                        "version": env!("CARGO_PKG_VERSION"),
                    })
                );
            } else {
                println!("compute {}", env!("CARGO_PKG_VERSION"));
            }
        }
        Commands::Exec(command) => {
            let mut description = command.issue_description.into_iter();
            let path = PathBuf::from(description.next().expect("required by clap"));
            execute_path(
                &compute,
                path,
                command.runtime,
                description.collect(),
                command.env,
                command.mounts,
                command.network,
                command.isolation,
                command.memory,
                command.timeout,
                command.stdin,
                command.json,
                command.receipt,
            )
            .await?;
        }
        Commands::Doctor(json_flag) => {
            let reports = compute.doctor().await;
            let provenance = distribution::doctor_provenance();
            if json_flag.json {
                let reports = reports
                    .into_iter()
                    .map(|report| {
                        let runtime = report.runtime.as_str();
                        let mut value = serde_json::to_value(report).unwrap();
                        if let Some(item) = provenance.get(runtime) {
                            value["distribution_provenance"] = item.clone();
                        }
                        value
                    })
                    .collect::<Vec<_>>();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({ "runtimes": reports }))
                        .unwrap()
                );
            } else {
                println!("Compute runtime capabilities");
                for report in reports {
                    let availability = if report.availability.available {
                        "available"
                    } else {
                        "unavailable"
                    };
                    println!(
                        "\n{} {} ({availability}, {:?})",
                        report.runtime, report.descriptor.version, report.availability.source
                    );
                    if let Some(detected) = &report.availability.version {
                        println!("  Detected: {detected}");
                    }
                    if let Some(path) = &report.availability.executable {
                        println!("  Path: {}", path.display());
                    }
                    if let Some(remediation) = &report.availability.remediation {
                        println!("  Remediation: {remediation}");
                    }
                    if let Some(item) = provenance.get(report.runtime.as_str()) {
                        println!(
                            "  Artifact: sha256:{}",
                            item["artifact_sha256"].as_str().unwrap_or("unknown")
                        );
                        println!(
                            "  Provenance: {}",
                            item["status"].as_str().unwrap_or("fail").to_uppercase()
                        );
                    }
                    print_capabilities(&report.capabilities, "  ");
                }
            }
        }
        Commands::Certify(command) => {
            if !command.internal_clean_environment {
                return certification::spawn_clean_certification(command.json);
            }
            let report = certification::certify(&compute).await;
            certification::print_report(&report, command.json);
            if !report.passed {
                return Err(compute_core::ComputeError::Runtime(
                    "distribution certification failed".into(),
                ));
            }
        }
        Commands::Distribution(command) => match command.command {
            DistributionCommands::Build {
                output,
                offline,
                verify,
                cache,
                platform,
                lock,
                compute_binary,
            } => distribution::build(distribution::BuildOptions {
                output,
                offline,
                verify,
                cache,
                platform,
                lock,
                compute_binary,
            })?,
            DistributionCommands::Inspect { path, json } => {
                distribution::inspect(&path, json)?;
            }
            DistributionCommands::Verify { path, json } => {
                distribution::verify(&path, json)?;
            }
        },
        Commands::Receipt(command) => match command.command {
            ReceiptCommands::Inspect {
                receipt: path,
                json,
            } => receipt::inspect(&path, json)?,
            ReceiptCommands::Verify {
                receipt: path,
                distribution,
                artifacts,
                json,
            } => {
                receipt::verify(&path, distribution.as_deref(), artifacts.as_deref(), json)?;
            }
        },
        Commands::Serve(command) => {
            let endpoint = command
                .public_url
                .unwrap_or_else(|| format!("http://{}", command.listen));
            eprintln!(
                "Compute provider listening on {} ({})",
                command.listen, endpoint
            );
            let policy = ProviderPolicy {
                runtimes: (!command.allow_runtimes.is_empty())
                    .then(|| {
                        command
                            .allow_runtimes
                            .iter()
                            .map(|value| value.parse::<RuntimeKind>())
                            .collect::<compute_core::Result<_>>()
                    })
                    .transpose()?,
                isolation_profiles: (!command.allow_isolation.is_empty())
                    .then(|| command.allow_isolation.iter().copied().collect()),
                network_policies: (!command.allow_network.is_empty())
                    .then(|| command.allow_network.iter().cloned().collect()),
                max_timeout_ms: command
                    .max_timeout
                    .map(|value| u64::try_from(value.as_millis()).unwrap_or(u64::MAX)),
                max_memory_bytes: command.max_memory,
            };
            let execution_policy = command.execution_policy.server_policy()?;
            if let Some(policy) = &execution_policy {
                eprintln!(
                    "Execution policy: {} ({})",
                    policy.label(),
                    policy.policy_id()
                );
            }
            let mut config = ServerConfig::local_with_policies(endpoint, policy, execution_policy);
            config.job_store = command.job_store;
            config.job_retention = command.job_retention;
            config.max_concurrent_jobs = command.max_concurrent_jobs;
            compute_provider::serve(command.listen, config)
                .await
                .map_err(provider_error)?;
        }
        Commands::Provider(command) => pool::provider(command).await?,
        Commands::Placement(command) => pool::placement(command).await?,
        Commands::Pool(command) => pool::pool(command).await?,
        Commands::Remote(command) => match command.command {
            RemoteCommands::Capabilities(command) => {
                let value = RemoteProvider::new(command.provider)
                    .capabilities()
                    .await
                    .map_err(provider_error)?;
                print_provider_value(&value, command.json);
            }
            RemoteCommands::Health(command) => {
                let value = RemoteProvider::new(command.provider)
                    .health()
                    .await
                    .map_err(provider_error)?;
                print_provider_value(&value, command.json);
            }
            RemoteCommands::Submit(command) => {
                let (provider, request, explain) = remote_request(*command)?;
                if request.3.is_some() {
                    return Err(compute_core::ComputeError::InvalidWorkload(
                        "--receipt is valid for remote run or remote wait".into(),
                    ));
                }
                if let Some(resolved) = explain {
                    print_generated_workload(&resolved, request.1)?;
                } else if request.2 {
                    let response = provider.inspect(request.0).await.map_err(provider_error)?;
                    print_bundle_plan(&response.plan, request.1);
                } else {
                    let submission = provider
                        .submit(request.0, request.4.as_deref())
                        .await
                        .map_err(provider_error)?;
                    print_provider_value(&submission, request.1);
                }
            }
            RemoteCommands::Status(command) => {
                let status = RemoteProvider::new(command.provider)
                    .job_status(&command.job_id)
                    .await
                    .map_err(provider_error)?;
                print_provider_value(&status, command.json);
            }
            RemoteCommands::Result(command) => {
                let result = RemoteProvider::new(command.provider)
                    .job_result(&command.job_id)
                    .await
                    .map_err(provider_error)?;
                print_provider_value(&result, command.json);
            }
            RemoteCommands::Cancel(command) => {
                let status = RemoteProvider::new(command.provider)
                    .cancel_job(&command.job_id)
                    .await
                    .map_err(provider_error)?;
                print_provider_value(&status, command.json);
            }
            RemoteCommands::Receipt(command) => {
                let receipt = RemoteProvider::new(command.provider)
                    .job_receipt(&command.job_id)
                    .await
                    .map_err(provider_error)?;
                if let Some(output) = command.output {
                    std::fs::write(output, receipt.receipt.encoded_bytes()?)?;
                } else {
                    print_provider_value(&receipt.receipt, command.json);
                }
            }
            RemoteCommands::Artifacts(command) => {
                let artifacts = RemoteProvider::new(command.provider)
                    .job_artifacts(&command.job_id)
                    .await
                    .map_err(provider_error)?;
                print_provider_value(&artifacts, command.json);
            }
            RemoteCommands::Wait(command) => remote_wait(command).await?,
            RemoteCommands::Inspect(command) => {
                let (provider, request, explain) = remote_request(*command)?;
                if request.4.is_some() {
                    return Err(compute_core::ComputeError::InvalidWorkload(
                        "--idempotency-key is valid only for remote submit".into(),
                    ));
                }
                if request.3.is_some() {
                    return Err(compute_core::ComputeError::InvalidWorkload(
                        "--receipt is not valid for remote inspect".into(),
                    ));
                }
                if let Some(resolved) = explain {
                    print_generated_workload(&resolved, request.1)?;
                } else {
                    let response = provider.inspect(request.0).await.map_err(provider_error)?;
                    print_bundle_plan(&response.plan, request.1);
                }
            }
            RemoteCommands::Run(command) => {
                let (provider, request, explain) = remote_request(*command)?;
                if request.4.is_some() {
                    return Err(compute_core::ComputeError::InvalidWorkload(
                        "--idempotency-key is valid only for remote submit".into(),
                    ));
                }
                if let Some(resolved) = explain {
                    print_generated_workload(&resolved, request.1)?;
                } else if request.2 {
                    let response = provider.inspect(request.0).await.map_err(provider_error)?;
                    print_bundle_plan(&response.plan, request.1);
                } else {
                    let receipt_path = request.3;
                    let response = provider.execute(request.0).await.map_err(provider_error)?;
                    print_execution_result(response.result, request.1, receipt_path.as_deref())?;
                }
            }
        },
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_path(
        compute: &Compute,
        path: PathBuf,
        runtime: Option<String>,
        args: Vec<String>,
        env: Vec<EnvironmentVariable>,
        mounts: Vec<Mount>,
        network: NetworkPolicy,
        isolation: IsolationProfile,
        memory: Option<u64>,
        timeout: Option<Duration>,
        stdin: Option<String>,
        json: bool,
        receipt_path: Option<PathBuf>,
    ) -> compute_core::Result<()> {
        if !path.exists() && runtime.is_none() && !args.is_empty() {
            if receipt_path.is_some() {
                return Err(compute_core::ComputeError::InvalidReceipt(
                    "an accepted issue description is not an execution and cannot produce a receipt".into(),
                ));
            }
            let description = std::iter::once(path.to_string_lossy().into_owned())
                .chain(args)
                .collect::<Vec<_>>()
                .join(" ");
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "command": "exec",
                        "description": description,
                        "status": "accepted",
                    })
                );
            } else {
                println!("Execution request accepted: {description}");
            }
            return Ok(());
        }
        let mut workload = compute.workload_from_path(
            &path,
            parse_runtime_spec(runtime)?,
            args,
            env,
            mounts,
            network,
            ResourceLimits {
                memory_bytes: memory,
                wall_time: timeout,
                cpu_time: None,
                process_count: None,
                stdout_bytes: None,
                stderr_bytes: None,
            },
            isolation,
        )?;
        workload.stdin = stdin.unwrap_or_default().into_bytes();
        let result = compute.run(workload).await?;
        print_execution_result(result, json, receipt_path.as_deref())?;
        Ok(())
    }

    Ok(())
}

type RemotePrepared = (
    RemoteProvider,
    (ProviderRequest, bool, bool, Option<PathBuf>, Option<String>),
    Option<direct::ResolvedDirect>,
);

fn remote_request(command: RemoteArtifactCommand) -> compute_core::Result<RemotePrepared> {
    let RemoteArtifactCommand {
        path,
        bundle,
        provider,
        runtime,
        env,
        env_file,
        inputs,
        outputs,
        cwd,
        entrypoint,
        deps,
        offline: _,
        network,
        isolation,
        memory,
        timeout,
        receipt,
        json,
        dry_run,
        explain,
        args,
        idempotency_key,
    } = command;
    let mut explained = None;
    let bytes = if let Some(bundle) = bundle {
        if runtime.is_some()
            || !env.is_empty()
            || env_file.is_some()
            || !inputs.is_empty()
            || !outputs.is_empty()
            || cwd.is_some()
            || entrypoint.is_some()
            || deps.is_some()
            || network.is_some()
            || memory.is_some()
            || timeout.is_some()
            || !args.is_empty()
        {
            return Err(compute_core::ComputeError::InvalidWorkload(
                "--bundle cannot be combined with direct execution overrides".into(),
            ));
        }
        std::fs::read(bundle)?
    } else {
        let resolved = direct::resolve(direct::DirectOptions {
            path: path.expect("required by clap"),
            runtime,
            args,
            env,
            env_file,
            inputs,
            outputs,
            cwd,
            entrypoint,
            deps,
            network,
            isolation,
            memory,
            timeout,
            defaults: admission::PolicyLocation::default().defaults()?,
        })?;
        let bundle = compute_core::WorkloadBundle::create_from_with_capsule(
            resolved.workload.clone(),
            &resolved.root,
            resolved.dependency_capsule.clone(),
        )?;
        if explain {
            explained = Some(resolved);
        }
        bundle.to_bytes()?
    };
    let mut request = ProviderRequest::bundle(bytes);
    if let compute_provider::ArtifactTransport::Bundle { data } = &request.artifact {
        let bundle = compute_core::WorkloadBundle::from_bytes(data)?;
        request.expected.workload_id = Some(bundle.workload_id()?);
        request.expected.bundle_id = Some(bundle.bundle_id()?);
        request.expected.dependency_id = bundle
            .dependency_capsule
            .as_ref()
            .map(DependencyCapsule::capsule_id)
            .transpose()?;
    }
    request.execution.isolation = isolation;
    Ok((
        RemoteProvider::new(provider),
        (request, json, dry_run, receipt, idempotency_key),
        explained,
    ))
}

async fn remote_wait(command: RemoteWaitCommand) -> compute_core::Result<()> {
    let provider = RemoteProvider::new(command.provider);
    let deadline = tokio::time::Instant::now() + command.timeout;
    let mut delay = Duration::from_millis(100);
    loop {
        let status = provider
            .job_status(&command.job_id)
            .await
            .map_err(provider_error)?;
        if status.status.is_terminal() {
            match provider.job_result(&command.job_id).await {
                Ok(result) => {
                    print_execution_result(result.result, command.json, command.receipt.as_deref())?
                }
                Err(_)
                    if matches!(
                        status.status,
                        compute_core::JobStatus::Cancelled | compute_core::JobStatus::Rejected
                    ) =>
                {
                    print_provider_value(&status, command.json);
                }
                Err(error) => return Err(provider_error(error)),
            }
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(compute_core::ComputeError::Runtime(format!(
                "client wait timed out after {}ms; job {} continues running",
                command.timeout.as_millis(),
                command.job_id
            )));
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(2));
    }
}

fn provider_error(error: compute_provider::ProviderError) -> compute_core::ComputeError {
    compute_core::ComputeError::Runtime(error.to_string())
}

fn print_provider_value(value: &impl serde::Serialize, _json: bool) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("provider response is serializable")
    );
}

fn print_execution_result(
    result: compute_core::ExecutionResult,
    json: bool,
    receipt_path: Option<&std::path::Path>,
) -> compute_core::Result<()> {
    if let Some(path) = receipt_path {
        let receipt = result.receipt.as_ref().ok_or_else(|| {
            compute_core::ComputeError::InvalidReceipt("execution did not produce a receipt".into())
        })?;
        std::fs::write(path, receipt.encoded_bytes()?)?;
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&result).unwrap());
    } else {
        if !result.stdout.text.is_empty() {
            print!("{}", result.stdout.text);
        }
        if !result.stderr.text.is_empty() {
            eprint!("{}", result.stderr.text);
        }
        eprintln!(
            "\nexecution {}: {}",
            result.execution_id,
            serde_json::to_string(&result.status)
                .unwrap()
                .trim_matches('"')
        );
    }
    if !matches!(result.status, compute_core::ExecutionStatus::Completed)
        || result.exit_code.is_some_and(|code| code != 0)
    {
        std::process::exit(result.exit_code.filter(|code| *code != 0).unwrap_or(1));
    }
    Ok(())
}

fn print_generated_workload(
    resolved: &direct::ResolvedDirect,
    json: bool,
) -> compute_core::Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "source": "direct",
                "configuration": resolved.config.as_ref().map(|path| {
                    path.file_name().unwrap_or_default().to_string_lossy()
                }),
                "workload": resolved.workload,
                "workload_id": resolved.workload.workload_id()?,
            }))
            .unwrap()
        );
        return Ok(());
    }
    println!("Resolved Workload");
    println!("Runtime: {}", resolved.workload.runtime);
    println!("Entrypoint: {}", resolved.workload.entrypoint.display());
    println!("Arguments: {}", resolved.workload.args.len());
    println!("Isolation: {}", resolved.workload.isolation.profile);
    println!("Network: {}", resolved.workload.network);
    println!("Inputs: {}", resolved.workload.inputs.len());
    println!("Outputs: {}", resolved.workload.outputs.len());
    println!("Workload ID: {}", resolved.workload.workload_id()?);
    Ok(())
}

fn print_dependency_inspection(inspection: &compute_core::DependencyCapsuleInspection, json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(inspection).unwrap());
        return;
    }
    println!("Dependency Capsule");
    println!("ID: {}", inspection.capsule_id);
    println!("Runtime: {}", inspection.runtime);
    if let Some(version) = &inspection.runtime_version {
        println!("Runtime version: {version}");
    }
    println!("Platform: {}", inspection.platform.label());
    println!("Files: {}", inspection.file_count);
    println!("Size: {} bytes", inspection.size_bytes);
    println!("Dependencies: {}", inspection.dependency_count);
    if let Some(lock) = &inspection.lock_identity {
        println!("Lock identity: {lock}");
    }
    println!("Valid: {}", if inspection.valid { "yes" } else { "no" });
}

fn print_ambiguous_inspection(error: &compute_core::ComputeError, json: bool) {
    if json {
        let candidates = if error.to_string().contains("Cannot resolve runtime") {
            vec!["node", "bun", "deno"]
        } else {
            Vec::new()
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "source": "direct",
                "project": { "type": "unknown" },
                "resolution": "ambiguous",
                "candidates": candidates,
                "diagnostic": error.to_string(),
            }))
            .unwrap()
        );
    } else {
        println!("Project");
        println!("  Type: unknown");
        println!("Resolution: ambiguous");
        println!("{error}");
    }
}

fn print_workload_plan(plan: &compute_core::WorkloadPlan, json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(plan).unwrap());
        return;
    }
    println!("Workload specification {}", plan.workload.version);
    println!("  Workload ID: {}", plan.workload_id);
    println!(
        "  Capability: {}@{}",
        plan.capability.name, plan.capability.version
    );
    println!("  Runtime: {}", plan.workload.runtime);
    println!("  Entrypoint: {}", plan.workload.entrypoint.display());
    println!("  Arguments: {}", plan.workload.args.len());
    println!("  Network: {}", plan.workload.network);
    println!("  Inputs: {}", plan.input_preparation.len());
    println!("  Output root: {}", plan.output_root.display());
    println!("  Dependencies required: {}", plan.dependencies.required);
    if let Some(capsule) = &plan.dependencies.capsule_id {
        println!("  Dependency capsule: {capsule}");
        println!("  Dependency available: {}", plan.dependencies.available);
    }
    println!("  Resolved runtime: {}", plan.resolved_runtime.kind);
    println!("  Capability compatible: {}", plan.capability_compatible);
    println!("  Requested isolation: {}", plan.isolation.requested);
    println!(
        "  Isolation result: {}",
        if plan.isolation.compatible {
            "ACCEPTED"
        } else {
            "REJECTED"
        }
    );
    if let Some(evidence) = &plan.isolation.evidence {
        println!("  Effective isolation: {}", evidence.effective);
        println!("  Filesystem: {:?}", evidence.filesystem);
        println!("  Network boundary: {:?}", evidence.network);
        println!("  Environment: {:?}", evidence.environment);
        println!("  Resources: {:?}", evidence.resources);
    }
    if let Some(reason) = &plan.isolation.reason {
        println!("  Isolation reason: {} ({})", reason.message, reason.code);
    }
    if let Some(error) = &plan.capability_error {
        println!("  Capability error: {error}");
    }
}

fn print_bundle_plan(plan: &compute_core::BundleWorkloadPlan, json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(plan).unwrap());
        return;
    }
    println!("Bundle verified: {}", plan.bundle_verification.valid);
    println!("Bundle ID: {}", plan.bundle_verification.bundle_id);
    println!("Workload ID: {}", plan.bundle_verification.workload_id);
    print_workload_plan(&plan.plan, false);
}

fn bundle_error_kind(error: &compute_core::ComputeError) -> &'static str {
    let message = error.to_string().to_ascii_lowercase();
    if message.contains("version") || message.contains("format") {
        "invalid_format_version"
    } else if message.contains("workload identity mismatch") {
        "workload_identity_mismatch"
    } else if message.contains("bundle identity") || message.contains("metadata mismatch") {
        "bundle_identity_mismatch"
    } else if message.contains("duplicate") {
        "duplicate_input"
    } else if message.contains("missing") {
        "missing_input"
    } else if message.contains("unexpected") {
        "unexpected_input"
    } else if message.contains("path") || message.contains("traversal") {
        "invalid_input_path"
    } else {
        "invalid_bundle"
    }
}

fn print_capabilities(capabilities: &compute_core::RuntimeCapabilities, indent: &str) {
    let show = |name: &str, capability: &compute_core::Capability| {
        println!(
            "{indent}{name:24} {}",
            if capability.supported {
                "supported"
            } else {
                "unsupported"
            }
        );
    };
    println!("{indent}Execution");
    show("  stdin", &capabilities.stdin);
    show("  stdout", &capabilities.stdout);
    show("  stderr", &capabilities.stderr);
    show("  timeout", &capabilities.timeout);
    show("  cancellation", &capabilities.cancellation);
    println!("{indent}Isolation");
    show("  environment", &capabilities.environment);
    show("  filesystem", &capabilities.filesystem_isolation);
    show("  artifacts", &capabilities.artifacts);
    println!(
        "{indent}  process boundary         {}",
        yes_no(capabilities.isolation.process_boundary)
    );
    println!(
        "{indent}  filesystem boundary      {}",
        yes_no(capabilities.isolation.filesystem_boundary)
    );
    println!(
        "{indent}  network boundary         {}",
        yes_no(capabilities.isolation.network_boundary)
    );
    println!("{indent}Network");
    for policy in [
        NetworkPolicy::None,
        NetworkPolicy::Localhost,
        NetworkPolicy::Network,
    ] {
        if let Some(capability) = capabilities.network.get(&policy) {
            show(&format!("  {policy}"), capability);
        }
    }
    println!("{indent}Resources");
    show("  stdout limit", &capabilities.stdout_limit);
    show("  stderr limit", &capabilities.stderr_limit);
    show("  memory limit", &capabilities.memory_limit);
    show("  CPU limit", &capabilities.cpu_limit);
    show("  process limit", &capabilities.process_limit);
}

fn print_isolation_profiles(compute: &Compute, json: bool) {
    let profiles = IsolationProfile::ALL
        .into_iter()
        .map(|profile| {
            serde_json::json!({
                "profile": profile,
                "description": profile.description(),
            })
        })
        .collect::<Vec<_>>();
    let runtimes = RuntimeKind::ALL
        .into_iter()
        .map(|runtime| {
            let isolation = compute
                .capabilities(runtime)
                .expect("registered runtime")
                .isolation;
            let stronger = isolation.filesystem_boundary
                && isolation.network_boundary
                && isolation.environment_boundary
                && isolation.timeout_enforcement;
            let stronger_support = if !stronger {
                "no"
            } else if runtime == RuntimeKind::Deno {
                "conditional"
            } else {
                "yes"
            };
            serde_json::json!({
                "runtime": runtime,
                "process": if isolation.process_boundary { "yes" } else { "no" },
                "sandboxed": stronger_support,
                "strict": stronger_support,
                "capabilities": isolation,
            })
        })
        .collect::<Vec<_>>();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "version": compute_core::ISOLATION_MODEL_VERSION,
                "profiles": profiles,
                "runtimes": runtimes,
            }))
            .unwrap()
        );
        return;
    }
    println!(
        "Isolation Profiles (model {})",
        compute_core::ISOLATION_MODEL_VERSION
    );
    for profile in IsolationProfile::ALL {
        println!("{}\n  {}", profile, profile.description());
    }
    println!("\nRuntime\tProcess\tSandboxed\tStrict");
    for item in runtimes {
        println!(
            "{}\t{}\t{}\t{}",
            item["runtime"].as_str().unwrap_or("unknown"),
            item["process"].as_str().unwrap_or("no"),
            item["sandboxed"].as_str().unwrap_or("no"),
            item["strict"].as_str().unwrap_or("no"),
        );
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn parse_runtime_spec(value: Option<String>) -> compute_core::Result<Option<RuntimeSpec>> {
    match value {
        Some(value) => Ok(Some(RuntimeSpec::new(value, None)?)),
        None => Ok(None),
    }
}

fn parse_env(value: &str) -> Result<EnvironmentVariable, String> {
    let Some((key, val)) = value.split_once('=') else {
        return Err("expected KEY=VALUE".to_string());
    };
    Ok(EnvironmentVariable {
        key: key.to_string(),
        value: val.to_string(),
    })
}

fn parse_mount(value: &str) -> Result<Mount, String> {
    let Some((host_path, execution_path)) = value.split_once(':') else {
        return Err("expected host:path".to_string());
    };
    Ok(Mount {
        host_path: PathBuf::from(host_path),
        execution_path: PathBuf::from(execution_path),
    })
}

fn parse_network(value: &str) -> Result<NetworkPolicy, String> {
    match value {
        "none" | "disabled" => Ok(NetworkPolicy::None),
        "localhost" => Ok(NetworkPolicy::Localhost),
        "network" => Ok(NetworkPolicy::Network),
        _ => Err("expected one of: none, localhost, network".to_string()),
    }
}

fn parse_platform(value: &str) -> Result<PlatformIdentity, String> {
    let (os, architecture) = value
        .split_once('-')
        .ok_or_else(|| "expected platform like linux-x86_64".to_string())?;
    if os.is_empty() || architecture.is_empty() {
        return Err("expected platform like linux-x86_64".into());
    }
    Ok(PlatformIdentity {
        os: os.into(),
        architecture: architecture.into(),
        runtime_abi: None,
    })
}

fn parse_dependency_entry(value: &str) -> Result<DependencyEntry, String> {
    let (name, version) = value
        .split_once('=')
        .ok_or_else(|| "expected dependency metadata NAME=VERSION".to_string())?;
    if name.is_empty() || version.is_empty() {
        return Err("dependency name and version must be non-empty".into());
    }
    Ok(DependencyEntry {
        name: name.into(),
        version: version.into(),
        source: "resolved".into(),
        file_count: 0,
        sha256: compute_core::sha256_identity(value.as_bytes()),
        license: None,
    })
}

fn parse_isolation(value: &str) -> Result<IsolationProfile, String> {
    value
        .parse()
        .map_err(|error: compute_core::ComputeError| error.to_string())
}

fn parse_memory(value: &str) -> Result<u64, String> {
    let normalized = value.trim().to_ascii_lowercase();
    if let Some(raw) = normalized.strip_suffix("mb") {
        let mib = raw.parse::<u64>().map_err(|error| error.to_string())?;
        return Ok(mib * 1024 * 1024);
    }
    if let Some(raw) = normalized.strip_suffix("gb") {
        let gib = raw.parse::<u64>().map_err(|error| error.to_string())?;
        return Ok(gib * 1024 * 1024 * 1024);
    }
    if let Some(raw) = normalized.strip_suffix('m') {
        let mib = raw.parse::<u64>().map_err(|error| error.to_string())?;
        return Ok(mib * 1024 * 1024);
    }
    if let Some(raw) = normalized.strip_suffix('g') {
        let gib = raw.parse::<u64>().map_err(|error| error.to_string())?;
        return Ok(gib * 1024 * 1024 * 1024);
    }
    normalized.parse::<u64>().map_err(|error| error.to_string())
}

fn parse_duration(value: &str) -> Result<Duration, String> {
    let normalized = value.trim().to_ascii_lowercase();
    if let Some(raw) = normalized.strip_suffix("ms") {
        let millis = raw.parse::<u64>().map_err(|error| error.to_string())?;
        return Ok(Duration::from_millis(millis));
    }
    if let Some(raw) = normalized.strip_suffix('s') {
        let seconds = raw.parse::<u64>().map_err(|error| error.to_string())?;
        return Ok(Duration::from_secs(seconds));
    }
    if let Some(raw) = normalized.strip_suffix('m') {
        let minutes = raw.parse::<u64>().map_err(|error| error.to_string())?;
        return Ok(Duration::from_secs(minutes * 60));
    }
    Err("expected a duration like 250ms, 10s, or 1m".to_string())
}

fn parse_retention(value: &str) -> Result<Duration, String> {
    let normalized = value.trim().to_ascii_lowercase();
    if let Some(raw) = normalized.strip_suffix('d') {
        return raw
            .parse::<u64>()
            .map(|days| Duration::from_secs(days * 24 * 60 * 60))
            .map_err(|error| error.to_string());
    }
    if let Some(raw) = normalized.strip_suffix('h') {
        return raw
            .parse::<u64>()
            .map(|hours| Duration::from_secs(hours * 60 * 60))
            .map_err(|error| error.to_string());
    }
    parse_duration(value)
}
