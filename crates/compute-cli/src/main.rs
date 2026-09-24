use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use compute_core::{
    EnvironmentVariable, Mount, NetworkPolicy, ResourceLimits, RuntimeKind, RuntimeSpec,
};
use compute_runtime::Compute;

mod certification;
mod distribution;

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
    Run(RunCommand),
    Bundle(BundleCommand),
    Inspect(InspectCommand),
    Runtimes(JsonFlag),
    Runtime(RuntimeCommand),
    Capabilities(RuntimeCommand),
    Exec(ExecCommand),
    Doctor(JsonFlag),
    Certify(CertifyCommand),
    /// Build, inspect, or verify a portable Compute distribution.
    Distribution(DistributionCommand),
    Version(JsonFlag),
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

#[derive(Subcommand, Debug)]
enum BundleCommands {
    Create {
        #[arg(long)]
        workload: PathBuf,
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
    #[arg(long = "mount", value_parser = parse_mount)]
    mounts: Vec<Mount>,
    #[arg(long, value_parser = parse_network)]
    network: Option<NetworkPolicy>,
    #[arg(long, value_parser = parse_memory)]
    memory: Option<u64>,
    #[arg(long, value_parser = parse_duration)]
    timeout: Option<Duration>,
    /// UTF-8 text supplied to the workload; stdin is otherwise an immediate EOF.
    #[arg(long)]
    stdin: Option<String>,
    #[arg(long)]
    json: bool,
    /// Validate and plan a workload specification without executing it.
    #[arg(long)]
    dry_run: bool,
    /// Refuse to execute a workload whose canonical identity differs.
    #[arg(long)]
    expected_workload_id: Option<String>,
    /// Refuse to execute a bundle whose deterministic identity differs.
    #[arg(long, requires = "bundle")]
    expected_bundle_id: Option<String>,
    #[arg(trailing_var_arg = true)]
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
    #[arg(long, value_parser = parse_memory)]
    memory: Option<u64>,
    #[arg(long, value_parser = parse_duration)]
    timeout: Option<Duration>,
    /// UTF-8 text supplied to the workload; stdin is otherwise an immediate EOF.
    #[arg(long)]
    stdin: Option<String>,
    #[arg(long)]
    json: bool,
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
            if let Some(bundle) = command.bundle {
                if command.runtime.is_some()
                    || !command.env.is_empty()
                    || !command.mounts.is_empty()
                    || command.network.is_some()
                    || command.memory.is_some()
                    || command.timeout.is_some()
                    || command.stdin.is_some()
                    || !command.args.is_empty()
                {
                    return Err(compute_core::ComputeError::InvalidWorkload(
                        "--bundle cannot be combined with direct execution overrides".into(),
                    ));
                }
                if command.dry_run {
                    let plan = compute.plan_bundle(
                        &bundle,
                        command.expected_workload_id.as_deref(),
                        command.expected_bundle_id.as_deref(),
                    )?;
                    print_bundle_plan(&plan, command.json);
                } else {
                    let result = compute
                        .run_bundle(
                            &bundle,
                            command.expected_workload_id.as_deref(),
                            command.expected_bundle_id.as_deref(),
                        )
                        .await?;
                    print_execution_result(result, command.json);
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
                    || !command.mounts.is_empty()
                    || command.network.is_some()
                    || command.memory.is_some()
                    || command.timeout.is_some()
                    || command.stdin.is_some()
                    || !command.args.is_empty()
                {
                    return Err(compute_core::ComputeError::InvalidWorkload(
                        "--workload cannot be combined with direct execution overrides".into(),
                    ));
                }
                if command.dry_run {
                    let plan = match command.expected_workload_id.as_deref() {
                        Some(expected) => {
                            compute.plan_workload_with_id(&workload, expected).await?
                        }
                        None => compute.plan_workload(&workload).await?,
                    };
                    print_workload_plan(&plan, command.json);
                } else {
                    let result = match command.expected_workload_id.as_deref() {
                        Some(expected) => compute.run_workload_with_id(&workload, expected).await?,
                        None => compute.run_workload(&workload).await?,
                    };
                    print_execution_result(result, command.json);
                }
                return Ok(());
            }
            if command.dry_run {
                return Err(compute_core::ComputeError::InvalidWorkload(
                    "--dry-run requires --workload".into(),
                ));
            }
            if command.expected_workload_id.is_some() || command.expected_bundle_id.is_some() {
                return Err(compute_core::ComputeError::InvalidWorkload(
                    "expected identities require --workload or --bundle".into(),
                ));
            }
            execute_path(
                &compute,
                command.path.expect("required by clap"),
                command.runtime,
                command.args,
                command.env,
                command.mounts,
                command.network.unwrap_or(NetworkPolicy::Network),
                command.memory,
                command.timeout,
                command.stdin,
                command.json,
            )
            .await?;
        }
        Commands::Bundle(command) => match command.command {
            BundleCommands::Create {
                workload,
                output,
                json,
            } => {
                if output.exists() {
                    return Err(compute_core::ComputeError::InvalidBundle(format!(
                        "refusing to overwrite existing bundle: {}",
                        output.display()
                    )));
                }
                let inspection = compute.create_bundle(&workload, &output)?;
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
            let runtime = parse_runtime_spec(command.runtime)?;
            let inspection =
                compute.inspect_path(&command.path.expect("required by clap"), runtime)?;
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
            if command.json {
                println!("{}", serde_json::to_string_pretty(&runtime).unwrap());
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
                command.memory,
                command.timeout,
                command.stdin,
                command.json,
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
        memory: Option<u64>,
        timeout: Option<Duration>,
        stdin: Option<String>,
        json: bool,
    ) -> compute_core::Result<()> {
        if !path.exists() && runtime.is_none() && !args.is_empty() {
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
        )?;
        workload.stdin = stdin.unwrap_or_default().into_bytes();
        let result = compute.run(workload).await?;
        print_execution_result(result, json);
        Ok(())
    }

    Ok(())
}

fn print_execution_result(result: compute_core::ExecutionResult, json: bool) {
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
    println!("  Resolved runtime: {}", plan.resolved_runtime.kind);
    println!("  Capability compatible: {}", plan.capability_compatible);
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
        "none" => Ok(NetworkPolicy::None),
        "localhost" => Ok(NetworkPolicy::Localhost),
        "network" => Ok(NetworkPolicy::Network),
        _ => Err("expected one of: none, localhost, network".to_string()),
    }
}

fn parse_memory(value: &str) -> Result<u64, String> {
    let normalized = value.trim().to_ascii_lowercase();
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
