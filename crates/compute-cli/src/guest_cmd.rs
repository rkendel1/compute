//! `compute guest-exec`: the client a microVM session's job runs on the
//! host to enter its VM, as `docker exec` is for a container. It connects
//! to the guest agent through Firecracker's vsock bridge, sends the command,
//! and relays its output and exit status.

use std::path::PathBuf;

use clap::Args;
use compute_core::ComputeError;

#[derive(Args, Debug)]
pub struct GuestExecCommand {
    /// Firecracker's vsock bridge socket for the VM.
    #[arg(long)]
    uds: PathBuf,
    #[arg(long, default_value_t = compute_guest::GUEST_PORT)]
    port: u32,
    /// The working directory inside the guest.
    #[arg(long, default_value = "/workspace")]
    cwd: String,
    /// A variable to pass, by name: its value is read from this process's
    /// environment, so it never appears on a command line.
    #[arg(long = "env")]
    env: Vec<String>,
    /// Send this process's standard input to the command.
    #[arg(long)]
    stdin: bool,
    #[arg(last = true, required = true)]
    command: Vec<String>,
}

pub async fn guest_exec(command: GuestExecCommand) -> compute_core::Result<()> {
    let mut env = std::collections::BTreeMap::from([(
        "COMPUTE_SESSION_WORKSPACE".to_owned(),
        command.cwd.clone(),
    )]);
    for name in &command.env {
        if let Ok(value) = std::env::var(name) {
            env.insert(name.clone(), value);
        }
    }
    let mut stdin = Vec::new();
    if command.stdin {
        std::io::Read::read_to_end(&mut std::io::stdin(), &mut stdin)?;
    }
    let stream = compute_guest::connect_firecracker(&command.uds, command.port)
        .await
        .map_err(|error| ComputeError::Runtime(format!("the VM did not answer: {error}")))?;
    let request = compute_guest::Request::Exec {
        argv: command.command,
        env,
        cwd: command.cwd,
        stdin,
    };
    let code = compute_guest::exec_client(
        stream,
        &request,
        &mut tokio::io::stdout(),
        &mut tokio::io::stderr(),
    )
    .await
    .map_err(|error| ComputeError::Runtime(format!("the VM ended the command: {error}")))?;
    std::process::exit(code);
}
