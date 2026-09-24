use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use compute_core::{
    EnvironmentVariable, IsolationProfile, Mount, NetworkPolicy, ResourceLimits, RuntimeKind,
    RuntimeSpec,
};
use compute_runtime::Compute;

mod certification;
mod distribution;
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
    Run(RunCommand),
    Bundle(BundleCommand),
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
    #[arg(long)]
    receipt: Option<PathBuf>,
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
                    let plan = compute.plan_bundle_with_isolation(
                        &bundle,
                        command.expected_workload_id.as_deref(),
                        command.expected_bundle_id.as_deref(),
                        command.isolation,
                    )?;
                    print_bundle_plan(&plan, command.json);
                } else {
                    let result = compute
                        .run_bundle_with_isolation(
                            &bundle,
                            command.expected_workload_id.as_deref(),
                            command.expected_bundle_id.as_deref(),
                            command.isolation,
                        )
                        .await?;
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
                    let plan = compute
                        .plan_workload_with_options(
                            &workload,
                            command.expected_workload_id.as_deref(),
                            command.isolation,
                        )
                        .await?;
                    print_workload_plan(&plan, command.json);
                } else {
                    let result = compute
                        .run_workload_with_options(
                            &workload,
                            command.expected_workload_id.as_deref(),
                            command.isolation,
                        )
                        .await?;
                    print_execution_result(result, command.json, command.receipt.as_deref())?;
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
                command.isolation.unwrap_or(IsolationProfile::Process),
                command.memory,
                command.timeout,
                command.stdin,
                command.json,
                command.receipt,
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
        "none" => Ok(NetworkPolicy::None),
        "localhost" => Ok(NetworkPolicy::Localhost),
        "network" => Ok(NetworkPolicy::Network),
        _ => Err("expected one of: none, localhost, network".to_string()),
    }
}

fn parse_isolation(value: &str) -> Result<IsolationProfile, String> {
    value
        .parse()
        .map_err(|error: compute_core::ComputeError| error.to_string())
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
