//! Services the *distribution* asks Compute Configured to run alongside the
//! control plane.
//!
//! A configured distribution declares them in its profile
//! (`compute.distribution-profile@1`, `managed_services`); base Compute has
//! none, so `compute` on its own is unchanged. For each declared service this
//! module does the four things a supervisor owes it and nothing more:
//!
//! * start the executable the distribution ships,
//! * wait until it actually answers its readiness path,
//! * register (or reconcile) it with the control plane's own service registry,
//! * stop it on shutdown, without touching the state it owns.
//!
//! It never reconstructs the service. The process owns its own state,
//! authentication, gateway and UI; Compute Configured only knows the endpoint
//! and the capability the service declares, and reaches the service over HTTP
//! exactly as any other registered service is reached.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use compute_core::ComputeError;
use serde::{Deserialize, Serialize};

/// Where the installed configured distribution lives. The Homebrew formula
/// sets this to its `libexec`, which is where the profile, the pinned
/// `node_modules` and therefore every managed executable live.
pub const PROFILE_HOME_ENV: &str = "COMPUTE_CONFIGURED_HOME";

/// The one declaration Compute reads, from the distribution profile.
#[derive(Deserialize)]
struct Profile {
    #[serde(default)]
    managed_services: Vec<ManagedService>,
}

/// A service the distribution ships and wants running.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct ManagedService {
    /// Registration name. Deterministic, so re-running reconciles this
    /// service instead of registering a second one.
    pub name: String,
    /// Executable path, relative to the installed distribution root. This is
    /// the npm-installed binary (for example
    /// `node_modules/.bin/appport-services`), never a path into the package's
    /// build output.
    pub executable: String,
    /// Arguments, as given.
    #[serde(default)]
    pub arguments: Vec<String>,
    /// The service's own origin, which is also what Compute registers.
    pub endpoint: String,
    /// The path that answers only once the service is really serving. For an
    /// `AppPort/ui/1` service this is its discovery path.
    pub readiness_path: String,
    /// Capabilities to advertise when registering.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// The pool member the service is attributed to.
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default)]
    pub description: Option<String>,
}

fn default_provider() -> String {
    "local".into()
}

/// What actually happened to one managed service, for the operator's summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    pub service: ManagedService,
    /// `started` when this invocation launched it, `reused` when it was
    /// already running and answered readiness.
    pub outcome: &'static str,
    pub pid: Option<u32>,
}

/// The installed distribution's root, if this is a configured installation.
pub fn distribution_home() -> Option<PathBuf> {
    std::env::var_os(PROFILE_HOME_ENV)
        .map(PathBuf::from)
        .filter(|path| path.join("stack.json").is_file())
}

/// Read the managed services the installed distribution declares. An absent or
/// unreadable profile means "none", so base Compute is never affected.
pub fn declared(home: &Path) -> compute_core::Result<Vec<ManagedService>> {
    let profile = home.join("stack.json");
    let Ok(text) = std::fs::read_to_string(&profile) else {
        return Ok(Vec::new());
    };
    let profile: Profile = serde_json::from_str(&text).map_err(|error| {
        ComputeError::Runtime(format!(
            "{} is not a readable distribution profile: {error}",
            profile.display()
        ))
    })?;
    for service in &profile.managed_services {
        if service.endpoint.trim().is_empty() || service.readiness_path.trim().is_empty() {
            return Err(ComputeError::Runtime(format!(
                "managed service `{}` needs an endpoint and a readiness path",
                service.name
            )));
        }
    }
    Ok(profile.managed_services)
}

/// Where a managed service keeps its pid and log, and the directory it runs in.
/// The directory is only a working directory: whatever the service stores there
/// is the service's own state, owned by the service.
fn layout(home: &Path, service: &ManagedService) -> (PathBuf, PathBuf, PathBuf) {
    let root = home.join("services").join(&service.name);
    let pid = root.join("process.pid");
    let log = root.join("process.log");
    let working = root.join("working");
    (pid, log, working)
}

fn pid_of(path: &Path) -> Option<u32> {
    let recorded = std::fs::read_to_string(path).ok()?;
    recorded.trim().parse().ok()
}

/// Start (or adopt) every service the distribution declares and register it.
///
/// Readiness is a real HTTP answer from the service, so a process that exited,
/// failed to bind, or is still initialising is never reported as healthy and
/// never registered.
pub async fn ensure_running(
    distribution: &Path,
    state_home: &Path,
    control_plane: &str,
    daemon_token_env: &str,
) -> compute_core::Result<Vec<Started>> {
    let services = declared(distribution)?;
    let mut started = Vec::with_capacity(services.len());
    for service in services {
        started.push(
            ensure_one(
                distribution,
                state_home,
                control_plane,
                daemon_token_env,
                service,
            )
            .await?,
        );
    }
    Ok(started)
}

async fn ensure_one(
    distribution: &Path,
    state_home: &Path,
    control_plane: &str,
    daemon_token_env: &str,
    service: ManagedService,
) -> compute_core::Result<Started> {
    let (pid_file, log, working) = layout(state_home, &service);

    // Already running and answering? Adopt it, so repeated starts converge on
    // one process and one registration.
    if let Some(pid) = pid_of(&pid_file)
        && alive(pid)
        && ready(&service).await
    {
        register(control_plane, daemon_token_env, &service).await?;
        return Ok(Started {
            service,
            outcome: "reused",
            pid: Some(pid),
        });
    }
    // A stale pid file (the process is gone) is cleared before we start.
    let _ = std::fs::remove_file(&pid_file);

    let executable = distribution.join(&service.executable);
    if !executable.is_file() {
        return Err(ComputeError::Runtime(format!(
            "the configured distribution does not ship `{}`: {} is missing, so the managed service `{}` cannot run. Reinstall compute-configured.",
            service.executable,
            executable.display(),
            service.name
        )));
    }

    std::fs::create_dir_all(&working)?;
    let mut command = std::process::Command::new(&executable);
    command
        .args(&service.arguments)
        // The service resolves its own state, configuration and authority from
        // this directory. Compute Configured only chooses where it runs; it
        // never reads or writes the state the service keeps there.
        .current_dir(&working)
        // The service must never inherit a Compute credential.
        .env_remove("COMPUTE_DAEMON_TOKEN")
        .env_remove("COMPUTE_DAEMON");

    let pid = crate::launch_cmd::detached(&mut command, &log)?;
    std::fs::write(&pid_file, pid.to_string())?;

    if !crate::launch_cmd::wait_ready(
        &service.endpoint,
        &service.readiness_path,
        &service.name,
        &log,
    )
    .await
    {
        // The process never served its readiness path. Stop it rather than leave
        // a half-started child behind, and do not register it.
        stop_pid(pid);
        let _ = std::fs::remove_file(&pid_file);
        return Err(ComputeError::Runtime(format!(
            "the managed service `{}` did not become ready at {}{}; see {}",
            service.name,
            service.endpoint,
            service.readiness_path,
            log.display()
        )));
    }

    register(control_plane, daemon_token_env, &service).await?;
    Ok(Started {
        service,
        outcome: "started",
        pid: Some(pid),
    })
}

/// Register the service with the control plane's own registry. Registering the
/// same name again updates that record in place, so this is the reconciliation
/// step and never creates a second entry.
async fn register(
    control_plane: &str,
    daemon_token_env: &str,
    service: &ManagedService,
) -> compute_core::Result<()> {
    #[derive(Serialize)]
    struct Definition<'a> {
        name: &'a str,
        capabilities: &'a [String],
        provider: &'a str,
        endpoint: &'a str,
        description: Option<&'a str>,
    }

    let mut client = compute_environment::client::DaemonClient::new(control_plane)
        .map_err(crate::environment_cmd::error)?;
    if let Ok(token) = std::env::var(daemon_token_env) {
        client = client.with_bearer_token(token);
    }
    let definition = Definition {
        name: &service.name,
        capabilities: &service.capabilities,
        provider: &service.provider,
        endpoint: &service.endpoint,
        description: service.description.as_deref(),
    };
    client
        .post::<_, serde_json::Value>("/services", Some(&definition))
        .await
        .map_err(|error| {
            ComputeError::Runtime(format!(
                "the managed service `{}` started but could not be registered with the control plane: {error}. Its state is untouched; fix the control plane and run this again to reconcile.",
                service.name
            ))
        })?;
    Ok(())
}

/// Stop every managed service this distribution declared. Their state is left
/// exactly where it is, so the next start reuses it.
pub fn stop_all(distribution: &Path, state_home: &Path) -> compute_core::Result<Vec<String>> {
    let mut stopped = Vec::new();
    for service in declared(distribution)? {
        let (pid_file, _, _) = layout(state_home, &service);
        let Some(pid) = pid_of(&pid_file) else {
            continue;
        };
        if alive(pid) {
            stop_pid(pid);
        }
        let _ = std::fs::remove_file(&pid_file);
        stopped.push(service.name);
    }
    Ok(stopped)
}

/// Ask a process to stop, then make sure it did.
fn stop_pid(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if !alive(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

/// Whether a recorded pid is still one of our processes. A pid we no longer own
/// is stale and is treated as "not running" rather than signalled, so a recycled
/// pid belonging to something else is never killed.
fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, 0) == 0
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Ask a service's readiness path over HTTP. A successful answer means the
/// process is serving, not merely spawned.
async fn ready(service: &ManagedService) -> bool {
    let Ok(client) = compute_environment::client::DaemonClient::new(&service.endpoint) else {
        return false;
    };
    client
        .get::<serde_json::Value>(&service.readiness_path)
        .await
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The configured distribution the release pipeline assembles. These tests
    /// read the *shipped* profile, so a distribution that stops declaring a
    /// runnable service fails here rather than at a user's machine.
    fn configured_distribution() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../compatibility/published-stack")
            .canonicalize()
            .expect("the configured distribution fixture exists")
    }

    #[test]
    fn the_configured_distribution_declares_appport_services_with_its_ui_capability() {
        let services = declared(&configured_distribution()).expect("the shipped profile parses");
        assert_eq!(
            services.len(),
            1,
            "the configured profile declares one service"
        );

        let service = &services[0];
        assert_eq!(service.name, "appport-services");
        assert_eq!(
            service.capabilities,
            vec!["AppPort/ui/1".to_owned()],
            "registration advertises the AppPort UI capability"
        );
        // Discovery, not an authenticated page, is what readiness means: a
        // management operation refused for want of an identity is not a failure.
        assert_eq!(service.readiness_path, "/v1/ui");
        assert_eq!(service.provider, "this-machine");
    }

    #[test]
    fn the_declared_executable_is_the_installed_one_and_exists() {
        let service = &declared(&configured_distribution()).unwrap()[0];
        let executable = configured_distribution().join(&service.executable);
        assert!(
            executable.is_file(),
            "the distribution must ship {} (found {})",
            service.executable,
            executable.display()
        );
        // The npm-installed bin, never a path into the package's build output.
        assert!(
            service.executable.starts_with("node_modules/.bin/"),
            "expected an npm-installed executable, got {}",
            service.executable
        );
        assert!(
            !service.executable.contains("dist/"),
            "Compute Configured must not invoke the package's internal build path"
        );
    }

    #[test]
    fn the_managed_port_is_declared_once_and_does_not_collide_with_compute() {
        let service = &declared(&configured_distribution()).unwrap()[0];
        let port = service.endpoint.rsplit(':').next().unwrap();
        assert_ne!(port, "8787", "Compute's control plane keeps 8787");
        assert_ne!(port, "8788", "Compute's local computer host keeps 8788");
        // The registered endpoint and the launched port come from one value.
        let flag = service
            .arguments
            .iter()
            .position(|value| value == "--port")
            .expect("the host is told its port");
        assert_eq!(service.arguments[flag + 1], port);
        assert_eq!(service.arguments[0], "serve");
    }

    #[test]
    fn base_compute_declares_nothing() {
        // Base Compute ships no configured profile, so it starts no managed
        // service: the feature is additive.
        let empty = tempfile::tempdir().unwrap();
        assert!(declared(empty.path()).unwrap().is_empty());
    }

    #[test]
    fn a_service_without_a_readiness_path_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("stack.json"),
            r#"{"managed_services":[{"name":"x","executable":"node_modules/.bin/x",
                 "endpoint":"http://127.0.0.1:1","readiness_path":"  "}]}"#,
        )
        .unwrap();
        let error = declared(dir.path()).unwrap_err().to_string();
        assert!(error.contains("readiness"), "{error}");
    }

    #[test]
    fn a_malformed_profile_is_reported_rather_than_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("stack.json"), "not json at all").unwrap();
        assert!(declared(dir.path()).is_err());
    }

    #[test]
    fn a_stale_pid_is_not_a_live_process() {
        // A pid that is recorded but gone must read as not running, so a
        // recycled pid belonging to someone else is never signalled.
        let dir = tempfile::tempdir().unwrap();
        let service = ManagedService {
            name: "x".into(),
            executable: "node_modules/.bin/x".into(),
            arguments: vec![],
            endpoint: "http://127.0.0.1:1".into(),
            readiness_path: "/v1/ui".into(),
            capabilities: vec![],
            provider: "local".into(),
            description: None,
        };
        let (pid_file, _, _) = layout(dir.path(), &service);
        std::fs::create_dir_all(pid_file.parent().unwrap()).unwrap();
        std::fs::write(&pid_file, "4294967294").unwrap();
        assert!(!alive(pid_of(&pid_file).unwrap()));
    }

    #[test]
    fn the_profile_the_release_ships_is_the_one_the_tests_read() {
        // Guards the fixture path: if the distribution moves, this fails loudly
        // instead of silently checking nothing.
        let distribution = configured_distribution();
        assert!(distribution.join("stack.json").is_file());
    }
}
