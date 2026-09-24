use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use compute_core::{
    EnvironmentVariable, Mount, NetworkPolicy, ResourceLimits, RuntimeKind, RuntimeSpec,
};
use compute_runtime::Compute;

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
    Inspect(PathCommand),
    Runtimes(JsonFlag),
    Runtime(RuntimeCommand),
    Version(JsonFlag),
}

#[derive(Args, Debug)]
struct JsonFlag {
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct PathCommand {
    path: PathBuf,
    #[arg(long)]
    runtime: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct RuntimeCommand {
    runtime: String,
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct RunCommand {
    path: PathBuf,
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
    #[arg(long)]
    json: bool,
    #[arg(last = true)]
    args: Vec<String>,
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
            let runtime = parse_runtime_spec(command.runtime)?;
            let workload = compute.workload_from_path(
                &command.path,
                runtime,
                command.args,
                command.env,
                command.mounts,
                command.network,
                ResourceLimits {
                    memory_bytes: command.memory,
                    wall_time: command.timeout,
                    cpu_time: None,
                    process_count: None,
                    stdout_bytes: None,
                    stderr_bytes: None,
                },
            )?;
            let result = compute.run(workload).await?;
            if command.json {
                println!("{}", serde_json::to_string_pretty(&result).unwrap());
            } else {
                if !result.stdout.text.is_empty() {
                    print!("{}", result.stdout.text);
                }
                if !result.stderr.text.is_empty() {
                    eprint!("{}", result.stderr.text);
                }
            }
            if !matches!(result.status, compute_core::ExecutionStatus::Completed) {
                std::process::exit(result.exit_code.unwrap_or(1));
            }
        }
        Commands::Inspect(command) => {
            let runtime = parse_runtime_spec(command.runtime)?;
            let inspection = compute.inspect_path(&command.path, runtime)?;
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
            let runtimes = compute.runtimes().await;
            if json_flag.json {
                println!("{}", serde_json::to_string_pretty(&runtimes).unwrap());
            } else {
                println!("Runtime\tVersion\tAvailable");
                for runtime in runtimes {
                    println!(
                        "{}\t{}\t{}",
                        runtime.kind,
                        runtime.version.unwrap_or_else(|| "-".to_string()),
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
    }

    Ok(())
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
