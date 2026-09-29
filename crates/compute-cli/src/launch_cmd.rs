//! `compute` (and `compute up`): the control plane, on this machine, in one
//! command.
//!
//! ```text
//! compute
//!   ├── this machine's computer host   compute serve   (127.0.0.1:8788)  computers run here
//!   ├── the control plane              compute start   (127.0.0.1:8787)  the authority, the UI
//!   └── opens http://127.0.0.1:8787/
//! ```
//!
//! The control plane never runs software itself: computers do, on the
//! computer host, as they would on any other target. Everything lives in
//! `$COMPUTE_HOME` (default `~/.compute`); running `compute` again reuses what
//! is already running.
//!
//! The computer host trusts only this control plane: `compute` issues it a
//! target credential (`computers/credentials.json` holds the verifier, the
//! control plane's `control-plane/targets/this-machine.token` the token)
//! and the pool presents it on every request. The control plane's
//! identity (`identity`) stays the same across restarts, so what it owns on
//! the host stays its own.
//!
//! Control state is whatever `[state]` (or `--state`) says; without one it
//! is a file on this machine, which `compute` states as local development.
//! Production control planes use FeltDB.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::Args;
use compute_core::ComputeError;

#[derive(Args, Debug, Default)]
pub struct UpCommand {
    /// Where the control plane answers ($COMPUTE_LISTEN, else
    /// 127.0.0.1:8787).
    #[arg(long)]
    pub listen: Option<String>,
    /// Where this machine's computer host answers ($COMPUTE_TARGET_LISTEN,
    /// else 127.0.0.1:8788).
    #[arg(long)]
    pub target_listen: Option<String>,
    /// Run computers in containers (docker or podman) instead of private
    /// workspaces.
    #[arg(long)]
    pub containers: bool,
    /// Don't open a browser (also when $COMPUTE_NO_BROWSER is set).
    #[arg(long)]
    pub no_browser: bool,
    /// How often the controller reconciles, in milliseconds.
    #[arg(long, default_value_t = 1000)]
    pub reconcile_interval_ms: u64,
    /// The control plane's durable state; see `compute start --help`.
    #[command(flatten)]
    pub state: crate::control_state::StateOptions,
}

#[derive(Args, Debug)]
pub struct DownCommand {
    #[arg(long)]
    pub listen: Option<String>,
}

fn setting(value: Option<String>, variable: &str, default: &str) -> String {
    value
        .or_else(|| std::env::var(variable).ok())
        .unwrap_or_else(|| default.to_owned())
}

fn home() -> compute_core::Result<PathBuf> {
    if let Ok(home) = std::env::var("COMPUTE_HOME") {
        return Ok(PathBuf::from(home));
    }
    let base = std::env::var("HOME")
        .map_err(|_| ComputeError::Runtime("set HOME or COMPUTE_HOME".into()))?;
    Ok(PathBuf::from(base).join(".compute"))
}

fn answers(address: &str) -> bool {
    use std::net::ToSocketAddrs;
    address
        .to_socket_addrs()
        .ok()
        .and_then(|mut addresses| addresses.next())
        .is_some_and(|address| {
            std::net::TcpStream::connect_timeout(&address, Duration::from_millis(300)).is_ok()
        })
}

fn wait_for(address: &str, what: &str, log: &std::path::Path) -> compute_core::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !answers(address) {
        if Instant::now() > deadline {
            return Err(ComputeError::Runtime(format!(
                "{what} did not start at {address}; see {}",
                log.display()
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn detached(
    command: &mut std::process::Command,
    log: &std::path::Path,
) -> compute_core::Result<u32> {
    let file = std::fs::File::create(log)?;
    command
        .stdin(std::process::Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    Ok(command.spawn()?.id())
}

/// Ensure this control plane has a local computer host and return the pool
/// that names it. This is shared by `compute` and a development
/// `compute start`: both are one user-facing start operation, even though
/// the computer host remains a separate authority internally.
pub(crate) fn ensure_local_host(
    home: &std::path::Path,
    target_listen: &str,
    containers: bool,
) -> compute_core::Result<PathBuf> {
    std::fs::create_dir_all(home)?;
    let exe = std::env::current_exe()?;
    let host = home.join("computers");
    std::fs::create_dir_all(&host)?;
    let control_plane = control_plane_identity(home)?;
    let credentials = host.join("credentials.json");
    let token_file = home
        .join("control-plane")
        .join("targets")
        .join("this-machine.token");
    ensure_target_credential(&credentials, &token_file, &control_plane)?;
    if !answers(target_listen) {
        let mut serve = std::process::Command::new(&exe);
        serve
            .args(["serve", "--listen", target_listen, "--public-url"])
            .arg(format!("http://{target_listen}"))
            .arg("--job-store")
            .arg(host.join("jobs"))
            .arg("--session-store")
            .arg(host.join("sessions"))
            .arg("--credentials")
            .arg(&credentials);
        if containers {
            serve.args(["--session-provider", "container"]);
        }
        let log = host.join("host.log");
        let pid = detached(&mut serve, &log)?;
        std::fs::write(host.join("host.pid"), pid.to_string())?;
        wait_for(target_listen, "this machine's computer host", &log)?;
    }
    let pool = home.join("pool.toml");
    std::fs::write(
        &pool,
        format!(
            "# Written by Compute: the local computer host and the credential\n\
             # this control plane presents to it.\n\
             [providers.this-machine]\nkind = \"remote\"\nendpoint = \"http://{target_listen}\"\ntoken_file = {:?}\n",
            token_file.display().to_string()
        ),
    )?;
    Ok(pool)
}

/// The private endpoint of a managed local host. Pick it once and persist it
/// so controller restarts find the same target without imposing a public
/// port convention or colliding with another local control plane.
pub(crate) fn managed_local_host_endpoint(home: &std::path::Path) -> compute_core::Result<String> {
    let host = home.join("computers");
    std::fs::create_dir_all(&host)?;
    let path = host.join("listen");
    if let Ok(endpoint) = std::fs::read_to_string(&path) {
        let endpoint = endpoint.trim();
        if endpoint.parse::<std::net::SocketAddr>().is_ok() {
            return Ok(endpoint.to_owned());
        }
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let endpoint = listener.local_addr()?.to_string();
    drop(listener);
    std::fs::write(path, format!("{endpoint}\n"))?;
    Ok(endpoint)
}

/// Stop a computer host previously created by [`ensure_local_host`]. Its
/// durable session and workspace records stay under `home` for the next
/// start.
pub(crate) fn stop_local_host(home: &std::path::Path) {
    let pid_file = home.join("computers").join("host.pid");
    if let Ok(pid) = std::fs::read_to_string(&pid_file)
        && let Ok(pid) = pid.trim().parse::<i32>()
    {
        #[cfg(unix)]
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        let _ = std::fs::remove_file(pid_file);
    }
}

pub async fn up(command: UpCommand) -> compute_core::Result<()> {
    let listen = setting(command.listen.clone(), "COMPUTE_LISTEN", "127.0.0.1:8787");
    let target_listen = setting(
        command.target_listen.clone(),
        "COMPUTE_TARGET_LISTEN",
        "127.0.0.1:8788",
    );
    let no_browser = command.no_browser || std::env::var_os("COMPUTE_NO_BROWSER").is_some();
    let home = home()?;
    let exe = std::env::current_exe()?;
    let pool = ensure_local_host(&home, &target_listen, command.containers)?;
    let control_plane = control_plane_identity(&home)?;
    let url = format!("http://{}", listen);
    if !answers(&listen) {
        let state = home.join("control-plane");
        let output = std::process::Command::new(&exe)
            .args(["start", "--detach", "--listen", &listen])
            .args(command.state.arguments())
            .arg("--state-dir")
            .arg(&state)
            .arg("--pool-config")
            .arg(&pool)
            .args([
                "--reconcile-interval-ms",
                &command.reconcile_interval_ms.to_string(),
            ])
            .output()?;
        if !output.status.success() {
            return Err(ComputeError::Runtime(format!(
                "the control plane did not start: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )));
        }
    }
    println!("Compute is running: {url}/");
    println!(
        "  computers run on this machine's computer host ({})",
        target_listen
    );
    println!("  this control plane ({control_plane}) authenticates to it with a target credential");
    println!("  control state: {}", durability_note(&command.state));
    println!("  state: {}", home.display());
    println!("  stop it with `compute down`");
    if !no_browser {
        for opener in ["xdg-open", "open"] {
            if std::process::Command::new(opener)
                .arg(format!("{url}/"))
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .is_ok()
            {
                break;
            }
        }
    }
    Ok(())
}

/// This control plane's identity: generated once, kept in `$COMPUTE_HOME`.
fn control_plane_identity(home: &std::path::Path) -> compute_core::Result<String> {
    let path = home.join("identity");
    if let Ok(identity) = std::fs::read_to_string(&path) {
        let identity = identity.trim().to_owned();
        if compute_provider::credentials::validate_control_plane(&identity).is_ok() {
            return Ok(identity);
        }
    }
    let mut bytes = [0_u8; 8];
    {
        use std::io::Read;
        std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    }
    let identity = format!(
        "cp-{}",
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    std::fs::write(&path, format!("{identity}\n"))?;
    Ok(identity)
}

/// Make sure the computer host trusts this control plane, and that the
/// control plane holds a token it accepts. A token the host no longer
/// accepts (revoked, or its trust file replaced) is replaced by a new
/// credential; nothing falls back to an open host.
fn ensure_target_credential(
    credentials: &std::path::Path,
    token_file: &std::path::Path,
    control_plane: &str,
) -> compute_core::Result<()> {
    let invalid = |error: compute_provider::ProviderError| ComputeError::Runtime(error.message);
    let mut trusted =
        compute_provider::TargetCredentials::load_or_default(credentials).map_err(invalid)?;
    if let Ok(token) = compute_provider::credentials::read_token_file(token_file)
        && trusted
            .authenticate(Some(&format!("Bearer {token}")))
            .is_ok_and(|identity| identity == control_plane)
    {
        return Ok(());
    }
    let (_, token) = trusted.issue(control_plane).map_err(invalid)?;
    trusted.save(credentials).map_err(invalid)?;
    compute_provider::credentials::write_token_file(token_file, &token)?;
    Ok(())
}

/// What the control plane's durable state is, said plainly.
fn durability_note(state: &crate::control_state::StateOptions) -> String {
    match state.backend_name() {
        Ok(backend) if backend == "feltdb" => "FeltDB (production durable authority)".into(),
        Ok(backend) if backend == "memory" => "memory (ephemeral: lost when it stops)".into(),
        Ok(backend) => format!(
            "{backend} on this machine (local development; production control planes use FeltDB: [state] backend = \"feltdb\")"
        ),
        Err(error) => error.to_string(),
    }
}

pub async fn down(command: DownCommand) -> compute_core::Result<()> {
    let listen = setting(command.listen, "COMPUTE_LISTEN", "127.0.0.1:8787");
    let home = home()?;
    if answers(&listen) {
        let _ = std::process::Command::new(std::env::current_exe()?)
            .arg("stop")
            .env("COMPUTE_DAEMON", format!("http://{}", listen))
            .status();
    }
    stop_local_host(&home);
    println!(
        "Compute is stopped. Its state is kept in {}.",
        home.display()
    );
    Ok(())
}
