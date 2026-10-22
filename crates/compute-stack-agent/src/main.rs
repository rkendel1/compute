//! `compute-stack-agent`: the executable behind the `compute-configured-stack-agent` launcher.
//!
//! - `compute-stack-agent serve [chip serve flags]` runs stack-agent's runtime service with one
//!   isolated Compute session per work. Which target hosts the sessions, and where the project
//!   comes from, is Compute-configured's configuration (the `COMPUTE_STACK_AGENT_*` variables the
//!   launcher sets); the model is stack-agent's own (`CHIP_PROVIDER`, `CHIP_MODEL`, `CHIP_ENDPOINT`,
//!   ...), passed through untouched.
//! - `compute-stack-agent capability-exec --root <dir>` is the worker stack-agent runs *inside* a
//!   session: its own executors, acting on the project there.
//!
//! `serve` listens on loopback only: the service has no authentication of its own.

use std::sync::Arc;

use chip_core::Environments;

fn usage() -> i32 {
    eprintln!(
        "usage: compute-stack-agent serve [--host ADDR] [--port PORT] [--max-concurrent-work N] [--max-queued-work N]"
    );
    eprintln!("       compute-stack-agent capability-exec --root <project directory>");
    2
}

fn required(name: &str) -> Result<String, String> {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| format!("{name} is not set"))
}

fn configuration() -> Result<compute_stack_agent::ComputeSessionConfig, String> {
    let mut config = compute_stack_agent::ComputeSessionConfig::new(
        required("COMPUTE_STACK_AGENT_TARGET")?,
        required("COMPUTE_STACK_AGENT_PROJECT")?,
        required("COMPUTE_STACK_AGENT_WORKER")?,
    );
    if let Ok(file) = std::env::var("COMPUTE_STACK_AGENT_TARGET_TOKEN_FILE") {
        let token = std::fs::read_to_string(&file)
            .map_err(|e| format!("cannot read the target token file: {e}"))?;
        config.token = Some(token.trim().to_string());
    }
    if let Ok(path) = std::env::var("COMPUTE_STACK_AGENT_COMMAND_PATH") {
        config.command_environment.insert("PATH".into(), path);
    }
    for name in ["CARGO_HOME", "RUSTUP_HOME"] {
        if let Ok(value) = std::env::var(format!("COMPUTE_STACK_AGENT_{name}")) {
            config.command_environment.insert(name.into(), value);
        }
    }
    if let Ok(n) = std::env::var("COMPUTE_STACK_AGENT_MAX_ENVIRONMENTS") {
        config.max_environments = n
            .parse()
            .map_err(|_| "COMPUTE_STACK_AGENT_MAX_ENVIRONMENTS must be a number".to_string())?;
    }
    if let Ok(network) = std::env::var("COMPUTE_STACK_AGENT_NETWORK") {
        // `none` and `localhost` keep a work's capabilities off the network once the project is
        // loaded; they need a target whose session provider can enforce them, and are refused
        // (never weakened) by one that cannot.
        config.network = match network.trim() {
            "none" => compute_core::NetworkPolicy::None,
            "localhost" => compute_core::NetworkPolicy::Localhost,
            "network" => compute_core::NetworkPolicy::Network,
            other => {
                return Err(format!(
                    "COMPUTE_STACK_AGENT_NETWORK must be none, localhost or network, not {other:?}"
                ));
            }
        };
    }
    if let Ok(bytes) = std::env::var("COMPUTE_STACK_AGENT_SESSION_MEMORY") {
        let bytes: u64 = bytes
            .trim()
            .parse()
            .ok()
            .filter(|bytes| *bytes > 0)
            .ok_or_else(|| {
                "COMPUTE_STACK_AGENT_SESSION_MEMORY must be a number of bytes".to_string()
            })?;
        config.resources.memory_bytes = Some(bytes);
    }
    Ok(config)
}

/// The `serve` arguments, with durable agent state on by default. Embedded, stack-agent persists
/// works only in a database it is given; without one, a work interrupted by a crash would be lost
/// without a trace. `--state-db FILE` or `--no-state` from the operator win; otherwise the works
/// go to `COMPUTE_STACK_AGENT_STATE_DB`, or `stack-agent/works.feltdb` under `COMPUTE_HOME`
/// (default `~/.compute`).
fn durable(args: &[String]) -> Result<Vec<String>, String> {
    let mut args = args.to_vec();
    if args
        .iter()
        .any(|a| a == "--state-db" || a.starts_with("--state-db=") || a == "--no-state")
    {
        return Ok(args);
    }
    let path = match std::env::var_os("COMPUTE_STACK_AGENT_STATE_DB") {
        Some(path) => std::path::PathBuf::from(path),
        None => {
            let home = std::env::var_os("COMPUTE_HOME")
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME")
                        .map(|home| std::path::Path::new(&home).join(".compute"))
                })
                .ok_or("neither COMPUTE_HOME nor HOME is set, so there is nowhere to keep works")?;
            home.join("stack-agent").join("works.feltdb")
        }
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    args.extend(["--state-db".into(), path.display().to_string()]);
    Ok(args)
}

/// Whether `serve` arguments keep the service on loopback. stack-agent's service is a local
/// trusted-client interface (no authentication), so a wider listener is refused here rather than
/// left to a flag.
fn loopback_only(args: &[String]) -> bool {
    let mut host = None;
    let mut i = 0;
    while i < args.len() {
        if let Some(value) = args[i].strip_prefix("--host=") {
            host = Some(value.to_owned());
        } else if args[i] == "--host" {
            host = args.get(i + 1).cloned();
            i += 1;
        }
        i += 1;
    }
    host.is_none_or(|host| {
        host == "localhost"
            || host
                .parse::<core::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code = match args.get(1).map(String::as_str) {
        Some(chip_remote_env::WORKER_SUBCOMMAND) => {
            match (args.get(2).map(String::as_str), args.get(3)) {
                (Some("--root"), Some(root)) if args.len() == 4 => {
                    chip_remote_env::worker::run(std::path::Path::new(root)).await
                }
                _ => usage(),
            }
        }
        Some("serve") if !loopback_only(&args[2..]) => {
            eprintln!(
                "error: the stack-agent service has no authentication, so it listens on loopback only; --host must be 127.0.0.1, ::1 or localhost"
            );
            2
        }
        Some("serve") => {
            match configuration().and_then(|config| Ok((config, durable(&args[2..])?))) {
                Ok((config, serve_args)) => {
                    let provider = compute_stack_agent::ComputeSessionEnvironments::new(config);
                    let environments = Arc::new(Environments::new(Arc::new(provider)));
                    chip_cli::service::serve_in(&serve_args, Some(environments)).await
                }
                Err(why) => {
                    eprintln!("error: {why}; nothing was run");
                    3
                }
            }
        }
        _ => usage(),
    };
    std::process::exit(code);
}
