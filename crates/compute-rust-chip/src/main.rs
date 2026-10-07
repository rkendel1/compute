//! `compute-rust-chip`: the executable behind the `compute-configured-rust-chip` launcher.
//!
//! - `compute-rust-chip serve [chip serve flags]` runs Rust Chip's runtime service with one
//!   isolated Compute session per work. Which target hosts the sessions, and where the project
//!   comes from, is Compute-configured's configuration (the `COMPUTE_RUST_CHIP_*` variables the
//!   launcher sets); the model is Rust Chip's own (`CHIP_PROVIDER`, `CHIP_MODEL`, `CHIP_ENDPOINT`,
//!   ...), passed through untouched.
//! - `compute-rust-chip capability-exec --root <dir>` is the worker Rust Chip runs *inside* a
//!   session: its own executors, acting on the project there.
//!
//! It never starts or calls the npm Chip/Eve agent or the npm FX.

use std::sync::Arc;

use chip_core::Environments;

fn usage() -> i32 {
    eprintln!(
        "usage: compute-rust-chip serve [--host ADDR] [--port PORT] [--max-concurrent-work N] [--max-queued-work N]"
    );
    eprintln!("       compute-rust-chip capability-exec --root <project directory>");
    2
}

fn required(name: &str) -> Result<String, String> {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| format!("{name} is not set"))
}

fn configuration() -> Result<compute_rust_chip::ComputeSessionConfig, String> {
    let mut config = compute_rust_chip::ComputeSessionConfig::new(
        required("COMPUTE_RUST_CHIP_TARGET")?,
        required("COMPUTE_RUST_CHIP_PROJECT")?,
        required("COMPUTE_RUST_CHIP_WORKER")?,
    );
    if let Ok(file) = std::env::var("COMPUTE_RUST_CHIP_TARGET_TOKEN_FILE") {
        let token = std::fs::read_to_string(&file)
            .map_err(|e| format!("cannot read the target token file: {e}"))?;
        config.token = Some(token.trim().to_string());
    }
    if let Ok(path) = std::env::var("COMPUTE_RUST_CHIP_COMMAND_PATH") {
        config.command_environment.insert("PATH".into(), path);
    }
    for name in ["CARGO_HOME", "RUSTUP_HOME"] {
        if let Ok(value) = std::env::var(format!("COMPUTE_RUST_CHIP_{name}")) {
            config.command_environment.insert(name.into(), value);
        }
    }
    if let Ok(n) = std::env::var("COMPUTE_RUST_CHIP_MAX_ENVIRONMENTS") {
        config.max_environments = n
            .parse()
            .map_err(|_| "COMPUTE_RUST_CHIP_MAX_ENVIRONMENTS must be a number".to_string())?;
    }
    Ok(config)
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
        Some("serve") => match configuration() {
            Ok(config) => {
                let provider = compute_rust_chip::ComputeSessionEnvironments::new(config);
                let environments = Arc::new(Environments::new(Arc::new(provider)));
                chip_cli::service::serve_in(&args[2..], Some(environments)).await
            }
            Err(why) => {
                eprintln!("error: {why}; nothing was run");
                3
            }
        },
        _ => usage(),
    };
    std::process::exit(code);
}
