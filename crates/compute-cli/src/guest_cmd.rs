//! `compute guest-exec`: the client a microVM session's job runs on the
//! host to enter its VM, as `docker exec` is for a container. It connects
//! to the guest agent through Firecracker's vsock bridge, sends the command,
//! and relays its output and exit status.
//!
//! An interactive command (`--tty`, `--interactive`) also listens on a
//! private Unix socket (`--attach`): each connection to it sends control
//! frames (input, resize, signal, end of input), which are relayed to the
//! guest. Output is never sent there: it is this process's own output, the
//! job's durable log, which clients read from an offset. Reconnecting is
//! connecting again; it never starts the command again.

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
    /// Run the command on a terminal of ROWSxCOLS.
    #[arg(long, value_parser = parse_size)]
    tty: Option<compute_guest::TerminalSize>,
    /// Keep the command's standard input open for input sent while it runs.
    #[arg(long)]
    interactive: bool,
    /// Accept control frames for the command on this Unix socket (created
    /// owner-only, removed when the command ends).
    #[arg(long)]
    attach: Option<PathBuf>,
    #[arg(last = true, required = true)]
    command: Vec<String>,
}

pub(crate) fn parse_size(value: &str) -> Result<compute_guest::TerminalSize, String> {
    let (rows, cols) = value
        .split_once('x')
        .ok_or_else(|| format!("{value:?} is not ROWSxCOLS"))?;
    let parse = |part: &str| {
        part.parse::<u16>()
            .ok()
            .filter(|size| (1..=1000).contains(size))
            .ok_or_else(|| format!("{value:?} is not ROWSxCOLS"))
    };
    Ok(compute_guest::TerminalSize {
        rows: parse(rows)?,
        cols: parse(cols)?,
    })
}

/// The attach socket's file, removed when the command ends.
struct Listening(PathBuf);

impl Drop for Listening {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Accept attach connections; relay their control frames to `controls`.
fn listen(
    path: &std::path::Path,
    controls: tokio::sync::mpsc::Sender<compute_guest::Request>,
) -> compute_core::Result<Listening> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(path);
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let controls = controls.clone();
            tokio::spawn(async move {
                while let Ok(Some(request)) =
                    compute_guest::read_frame::<_, compute_guest::Request>(&mut stream).await
                {
                    let control = matches!(
                        request,
                        compute_guest::Request::Input { .. }
                            | compute_guest::Request::Resize { .. }
                            | compute_guest::Request::Signal { .. }
                            | compute_guest::Request::CloseInput
                    );
                    // Anything else ends this connection, never the command.
                    if !control || controls.send(request).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    Ok(Listening(path.to_path_buf()))
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
    let interactive = command.tty.is_some() || command.interactive;
    let request = compute_guest::Request::Exec {
        argv: command.command,
        env,
        cwd: command.cwd,
        stdin,
        terminal: command.tty,
        interactive: command.interactive,
    };
    let ran = if interactive {
        let (sender, controls) = tokio::sync::mpsc::channel(64);
        let _listening = match &command.attach {
            Some(path) => Some(listen(path, sender.clone())?),
            None => None,
        };
        compute_guest::exec_client_controlled(
            stream,
            &request,
            &mut tokio::io::stdout(),
            &mut tokio::io::stderr(),
            controls,
        )
        .await
    } else {
        compute_guest::exec_client(
            stream,
            &request,
            &mut tokio::io::stdout(),
            &mut tokio::io::stderr(),
        )
        .await
    };
    let code =
        ran.map_err(|error| ComputeError::Runtime(format!("the VM ended the command: {error}")))?;
    std::process::exit(code);
}
