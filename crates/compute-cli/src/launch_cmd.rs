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

pub async fn up(command: UpCommand) -> compute_core::Result<()> {
    let listen = setting(command.listen.clone(), "COMPUTE_LISTEN", "127.0.0.1:8787");
    let target_listen = setting(
        command.target_listen.clone(),
        "COMPUTE_TARGET_LISTEN",
        "127.0.0.1:8788",
    );
    let no_browser = command.no_browser || std::env::var_os("COMPUTE_NO_BROWSER").is_some();
    let home = home()?;
    std::fs::create_dir_all(&home)?;
    let exe = std::env::current_exe()?;
    // This machine's computer host: a target like any other.
    let host = home.join("computers");
    std::fs::create_dir_all(&host)?;
    if !answers(&target_listen) {
        let mut serve = std::process::Command::new(&exe);
        serve
            .args(["serve", "--listen", &target_listen, "--public-url"])
            .arg(format!("http://{}", target_listen))
            .arg("--job-store")
            .arg(host.join("jobs"))
            .arg("--session-store")
            .arg(host.join("sessions"));
        if command.containers {
            serve.args(["--session-provider", "container"]);
        }
        let log = host.join("host.log");
        let pid = detached(&mut serve, &log)?;
        std::fs::write(host.join("host.pid"), pid.to_string())?;
        wait_for(&target_listen, "this machine's computer host", &log)?;
    }
    let pool = home.join("pool.toml");
    std::fs::write(
        &pool,
        format!(
            "# Written by `compute`: the targets computers are placed on.\n\
             [providers.this-machine]\nkind = \"remote\"\nendpoint = \"http://{}\"\n",
            target_listen
        ),
    )?;
    let url = format!("http://{}", listen);
    if !answers(&listen) {
        let state = home.join("control-plane");
        let output = std::process::Command::new(&exe)
            .args(["start", "--detach", "--listen", &listen])
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

pub async fn down(command: DownCommand) -> compute_core::Result<()> {
    let listen = setting(command.listen, "COMPUTE_LISTEN", "127.0.0.1:8787");
    let home = home()?;
    if answers(&listen) {
        let _ = std::process::Command::new(std::env::current_exe()?)
            .arg("stop")
            .env("COMPUTE_DAEMON", format!("http://{}", listen))
            .status();
    }
    let pid_file = home.join("computers").join("host.pid");
    if let Ok(pid) = std::fs::read_to_string(&pid_file)
        && let Ok(pid) = pid.trim().parse::<i32>()
    {
        #[cfg(unix)]
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        let _ = std::fs::remove_file(&pid_file);
    }
    println!(
        "Compute is stopped. Its state is kept in {}.",
        home.display()
    );
    Ok(())
}
