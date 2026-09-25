//! The data plane: where services run and where their stable endpoints
//! listen, behind an explicit boundary the controller drives.
//!
//! ```text
//!  controller (API, reconcile, releases, state)
//!        │  DataPlane: start · wait · stop · ack · units · routes
//!        ▼
//!  supervisor (in this process, or `compute supervisor` on this node)
//!        ├── service processes (its children)
//!        ├── endpoint listeners and forwarding
//!        └── node-local registry: unit manifests, routes, unacknowledged
//!            results
//! ```
//!
//! [`LocalDataPlane`] is the supervisor. Inside the controller process
//! (tests, embedded use) it shares the controller's fate. As its own
//! process, `compute supervisor`, reached through [`SupervisorClient`] over
//! a node-local socket, it outlives the controller: a controller that is
//! killed, restarted, or upgraded finds its workloads still running and
//! their endpoints still answering, reattaches to them, and collects the
//! results of any that ended while it was away.
//!
//! The registry exists for process recovery only. It is never a second
//! source of desired state: it holds no secrets (a unit's request, which
//! carries its configuration, stays in memory), and the controller decides
//! from durable control state what should run.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use compute_core::ExecutionControl;
use compute_network::{Endpoints, Route};
use compute_provider::{Admission, ExecuteResponse, LocalProvider, ProviderRequest};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::EnvironmentError;

/// The supervisor protocol. A controller refuses a supervisor that speaks
/// another version rather than guessing.
pub const SUPERVISOR_PROTOCOL: u32 = 1;

/// Everything needed to recognize and reattach a running unit — and
/// nothing secret. Its configuration is not here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitManifest {
    /// Unique per start: a restarted unit has a new ID.
    pub unit_id: String,
    pub environment: String,
    pub project: String,
    pub workload: String,
    pub deployment_id: String,
    pub environment_id: String,
    pub project_id: String,
    pub workload_id: String,
    pub revision: String,
    pub runtime: String,
    pub bundle_identity: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependency_identity: Option<String>,
    /// Instance ports, by name.
    #[serde(default)]
    pub ports: BTreeMap<String, u16>,
    pub network: String,
    pub isolation: String,
    pub desired_state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_directory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub started_at: DateTime<Utc>,
}

/// A running process, identified so that a reused PID is never mistaken
/// for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<u64>,
}

/// How a unit ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UnitOutcome {
    Executed {
        response: Box<ExecuteResponse>,
    },
    /// The runtime could not run it; nothing executed. When admission
    /// refused it at execution, the decision says why.
    Failed {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        admission: Option<Box<compute_policy::AdmissionDecision>>,
    },
    /// Its process was left behind by a supervisor that died, and was
    /// stopped; its result is unknown.
    Orphaned {
        message: String,
    },
}

/// A unit as the supervisor sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitStatus {
    pub manifest: UnitManifest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<ProcessIdentity>,
    /// `running`, or `exited` with an outcome not yet acknowledged.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<UnitOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
}

/// An endpoint as the data plane serves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteStatus {
    pub port: u16,
    pub instance_id: String,
    pub target_port: u16,
    pub listening: bool,
    #[serde(default)]
    pub open_connections: usize,
    #[serde(default)]
    pub served_connections: u64,
}

/// Who the supervisor is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataPlaneInfo {
    /// `in_process` or `supervisor`.
    pub kind: String,
    pub protocol: u32,
    pub pid: u32,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_id: Option<String>,
    pub started_at: DateTime<Utc>,
    pub units: usize,
    pub routes: usize,
    /// Units a previous supervisor left behind and this one stopped.
    #[serde(default)]
    pub orphans_stopped: usize,
}

/// What the controller needs from wherever workloads run.
#[async_trait]
pub trait DataPlane: Send + Sync {
    async fn info(&self) -> Result<DataPlaneInfo, EnvironmentError>;
    /// Start a unit; returns once its process exists (or it already
    /// ended). A unit ID is started at most once.
    async fn start(
        &self,
        manifest: UnitManifest,
        request: ProviderRequest,
        admission: Admission,
    ) -> Result<Option<ProcessIdentity>, EnvironmentError>;
    /// Wait until a unit ends and return how. The outcome is kept until
    /// [`DataPlane::ack`], so a controller that dies before recording it
    /// finds it again.
    async fn wait(&self, unit_id: &str) -> Result<UnitOutcome, EnvironmentError>;
    async fn stop(&self, unit_id: &str) -> Result<(), EnvironmentError>;
    /// The controller recorded the outcome durably; forget it.
    async fn ack(&self, unit_id: &str) -> Result<(), EnvironmentError>;
    async fn units(&self) -> Result<Vec<UnitStatus>, EnvironmentError>;
    async fn assign(&self, port: u16, route: Route) -> Result<(), EnvironmentError>;
    async fn retain(&self, keep: BTreeSet<u16>) -> Result<(), EnvironmentError>;
    async fn routes(&self) -> Result<Vec<RouteStatus>, EnvironmentError>;
    /// Connections open to, and served by, an instance through endpoints.
    async fn connections(&self, instance_id: &str) -> Result<(usize, u64), EnvironmentError>;
    /// Whether the data plane outlives the controller.
    fn independent(&self) -> bool;
    /// Make sure the data plane is running. Returns whether it had to be
    /// started again: a supervisor that died is replaced, and the new one
    /// reports what the old one left behind.
    async fn recover(&self) -> Result<bool, EnvironmentError>;
    /// Stop the supervisor process itself (not its workloads, which must
    /// be stopped first). A no-op in process.
    async fn shutdown(&self) -> Result<(), EnvironmentError>;
}

struct UnitEntry {
    manifest: UnitManifest,
    control: ExecutionControl,
    process: Option<ProcessIdentity>,
    outcome: watch::Receiver<Option<UnitOutcome>>,
    finished_at: Option<DateTime<Utc>>,
}

/// The supervisor: runs units through the local engine and serves
/// endpoints, optionally recording them on disk so a later supervisor or
/// controller can recover them.
pub struct LocalDataPlane {
    provider: Arc<LocalProvider>,
    endpoints: Endpoints,
    units: Mutex<BTreeMap<String, UnitEntry>>,
    routes: Mutex<BTreeMap<u16, Route>>,
    registry: Option<PathBuf>,
    started_at: DateTime<Utc>,
    orphans_stopped: usize,
    kind: &'static str,
}

impl LocalDataPlane {
    /// In the controller's process: nothing survives it.
    pub fn in_process(provider: Arc<LocalProvider>, endpoints: Endpoints) -> Self {
        Self {
            provider,
            endpoints,
            units: Mutex::default(),
            routes: Mutex::default(),
            registry: None,
            started_at: Utc::now(),
            orphans_stopped: 0,
            kind: "in_process",
        }
    }

    /// The supervisor process: units, routes, and unacknowledged outcomes
    /// are recorded under `registry`. A previous supervisor's running
    /// processes cannot be supervised (they are not this process's
    /// children), so they are stopped and reported as orphaned; its routes
    /// are listened on again at once; its unacknowledged outcomes are kept.
    pub async fn supervisor(
        provider: Arc<LocalProvider>,
        endpoints: Endpoints,
        registry: PathBuf,
    ) -> Result<Self, EnvironmentError> {
        std::fs::create_dir_all(registry.join("units"))?;
        std::fs::create_dir_all(registry.join("outcomes"))?;
        let mut plane = Self {
            provider,
            endpoints,
            units: Mutex::default(),
            routes: Mutex::default(),
            registry: Some(registry.clone()),
            started_at: Utc::now(),
            orphans_stopped: 0,
            kind: "supervisor",
        };
        // Routes first: endpoints answer as soon as possible.
        if let Ok(bytes) = std::fs::read(registry.join("routes.json"))
            && let Ok(routes) = serde_json::from_slice::<Vec<RouteStatus>>(&bytes)
        {
            for route in routes {
                let route_value = Route {
                    instance_id: route.instance_id,
                    target_port: route.target_port,
                };
                if plane
                    .endpoints
                    .assign(route.port, route_value.clone())
                    .await
                    .is_ok()
                {
                    plane
                        .routes
                        .lock()
                        .expect("routes")
                        .insert(route.port, route_value);
                }
            }
        }
        // Outcomes a controller never acknowledged.
        let mut recovered = BTreeMap::new();
        for entry in std::fs::read_dir(registry.join("outcomes"))?.flatten() {
            if let Ok(bytes) = std::fs::read(entry.path())
                && let Ok(status) = serde_json::from_slice::<UnitStatus>(&bytes)
            {
                recovered.insert(status.manifest.unit_id.clone(), status);
            }
        }
        // Units a previous supervisor was running.
        for entry in std::fs::read_dir(registry.join("units"))?.flatten() {
            let path = entry.path();
            let Some(status) = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<UnitStatus>(&bytes).ok())
            else {
                let _ = std::fs::remove_file(&path);
                continue;
            };
            let _ = std::fs::remove_file(&path);
            if recovered.contains_key(&status.manifest.unit_id) {
                continue;
            }
            let message = match &status.process {
                Some(process) if same_process(process) => {
                    terminate_group(process.pid).await;
                    plane.orphans_stopped += 1;
                    format!(
                        "process {} was left by a supervisor that stopped; it was stopped so it cannot run twice",
                        process.pid
                    )
                }
                _ => "its process ended while no supervisor was running".to_string(),
            };
            let orphaned = UnitStatus {
                state: "exited".into(),
                outcome: Some(UnitOutcome::Orphaned { message }),
                finished_at: Some(Utc::now()),
                ..status
            };
            plane.write_outcome(&orphaned);
            recovered.insert(orphaned.manifest.unit_id.clone(), orphaned);
        }
        {
            let mut units = plane.units.lock().expect("units");
            for (unit_id, status) in recovered {
                let (_, receiver) = watch::channel(status.outcome.clone());
                units.insert(
                    unit_id,
                    UnitEntry {
                        manifest: status.manifest,
                        control: ExecutionControl::new(),
                        process: status.process,
                        outcome: receiver,
                        finished_at: status.finished_at,
                    },
                );
            }
        }
        Ok(plane)
    }

    pub fn provider(&self) -> &Arc<LocalProvider> {
        &self.provider
    }

    fn status_of(entry: &UnitEntry) -> UnitStatus {
        let outcome = entry.outcome.borrow().clone();
        UnitStatus {
            manifest: entry.manifest.clone(),
            process: entry.process.clone(),
            state: if outcome.is_some() {
                "exited"
            } else {
                "running"
            }
            .into(),
            outcome,
            finished_at: entry.finished_at,
        }
    }

    fn unit_path(&self, unit_id: &str) -> Option<PathBuf> {
        self.registry.as_ref().map(|root| {
            root.join("units")
                .join(format!("{}.json", file_name(unit_id)))
        })
    }

    fn outcome_path(&self, unit_id: &str) -> Option<PathBuf> {
        self.registry.as_ref().map(|root| {
            root.join("outcomes")
                .join(format!("{}.json", file_name(unit_id)))
        })
    }

    fn write_unit(&self, status: &UnitStatus) {
        if let Some(path) = self.unit_path(&status.manifest.unit_id) {
            write_json(&path, status);
        }
    }

    fn write_outcome(&self, status: &UnitStatus) {
        if let Some(path) = self.outcome_path(&status.manifest.unit_id) {
            write_json(&path, status);
        }
        if let Some(path) = self.unit_path(&status.manifest.unit_id) {
            let _ = std::fs::remove_file(path);
        }
    }

    fn write_routes(&self) {
        let Some(root) = &self.registry else { return };
        let routes = self
            .routes
            .lock()
            .expect("routes")
            .iter()
            .map(|(port, route)| RouteStatus {
                port: *port,
                instance_id: route.instance_id.clone(),
                target_port: route.target_port,
                listening: true,
                open_connections: 0,
                served_connections: 0,
            })
            .collect::<Vec<_>>();
        write_json(&root.join("routes.json"), &routes);
    }
}

fn file_name(unit_id: &str) -> String {
    unit_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn write_json(path: &Path, value: &impl Serialize) {
    let Ok(bytes) = serde_json::to_vec(value) else {
        return;
    };
    let temporary = path.with_extension("tmp");
    let written = {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(&temporary)
            .and_then(|mut file| std::io::Write::write_all(&mut file, &bytes))
    };
    if written.is_ok() {
        let _ = std::fs::rename(&temporary, path);
    }
}

#[async_trait]
impl DataPlane for LocalDataPlane {
    async fn info(&self) -> Result<DataPlaneInfo, EnvironmentError> {
        Ok(DataPlaneInfo {
            kind: self.kind.into(),
            protocol: SUPERVISOR_PROTOCOL,
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").into(),
            build_id: compute_core::compute_executable_identity().map(str::to_owned),
            started_at: self.started_at,
            units: self.units.lock().expect("units").len(),
            routes: self.routes.lock().expect("routes").len(),
            orphans_stopped: self.orphans_stopped,
        })
    }

    async fn start(
        &self,
        manifest: UnitManifest,
        request: ProviderRequest,
        admission: Admission,
    ) -> Result<Option<ProcessIdentity>, EnvironmentError> {
        let unit_id = manifest.unit_id.clone();
        let mut control = ExecutionControl::new();
        if let Some(directory) = &manifest.log_directory {
            control = control.with_log_directory(directory);
        }
        let (sender, receiver) = watch::channel(None);
        {
            let mut units = self.units.lock().expect("units");
            if units.contains_key(&unit_id) {
                return Err(EnvironmentError::Conflict(format!(
                    "unit {unit_id} was already started"
                )));
            }
            units.insert(
                unit_id.clone(),
                UnitEntry {
                    manifest: manifest.clone(),
                    control: control.clone(),
                    process: None,
                    outcome: receiver.clone(),
                    finished_at: None,
                },
            );
        }
        self.write_unit(&UnitStatus {
            manifest: manifest.clone(),
            process: None,
            state: "running".into(),
            outcome: None,
            finished_at: None,
        });
        let provider = self.provider.clone();
        let run_control = control.clone();
        tokio::spawn(async move {
            let outcome = match provider
                .execute_controlled(request, admission, &run_control)
                .await
            {
                Ok(response) => UnitOutcome::Executed {
                    response: Box::new(response),
                },
                Err(error) => UnitOutcome::Failed {
                    message: if error.admission.is_some() {
                        error.message.clone()
                    } else {
                        error.to_string()
                    },
                    admission: error.admission,
                },
            };
            let _ = sender.send(Some(outcome));
        });
        // Return once the process exists, or the unit already ended.
        let mut outcome = receiver;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        let process = loop {
            if let Some(pid) = control.process_id() {
                break Some(ProcessIdentity {
                    pid,
                    boot_id: boot_id(),
                    start_time: start_time(pid),
                });
            }
            if outcome.borrow().is_some() || tokio::time::Instant::now() >= deadline {
                break None;
            }
            let _ = tokio::time::timeout(Duration::from_millis(20), outcome.changed()).await;
        };
        let status = {
            let mut units = self.units.lock().expect("units");
            let entry = units.get_mut(&unit_id).expect("inserted");
            entry.process = process.clone();
            Self::status_of(entry)
        };
        if status.outcome.is_none() {
            self.write_unit(&status);
        }
        // Record the outcome as soon as it exists, whether or not anyone
        // is waiting.
        let registry = self.registry.clone();
        let mut outcome = outcome.clone();
        let written = status.clone();
        let root = registry;
        tokio::spawn(async move {
            while outcome.borrow().is_none() {
                if outcome.changed().await.is_err() {
                    return;
                }
            }
            if let Some(root) = root {
                let finished = UnitStatus {
                    state: "exited".into(),
                    outcome: outcome.borrow().clone(),
                    finished_at: Some(Utc::now()),
                    ..written
                };
                write_json(
                    &root
                        .join("outcomes")
                        .join(format!("{}.json", file_name(&finished.manifest.unit_id))),
                    &finished,
                );
                let _ = std::fs::remove_file(
                    root.join("units")
                        .join(format!("{}.json", file_name(&finished.manifest.unit_id))),
                );
            }
        });
        Ok(process)
    }

    async fn wait(&self, unit_id: &str) -> Result<UnitOutcome, EnvironmentError> {
        let mut receiver = self
            .units
            .lock()
            .expect("units")
            .get(unit_id)
            .map(|entry| entry.outcome.clone())
            .ok_or_else(|| EnvironmentError::NotFound(format!("unit {unit_id}")))?;
        loop {
            if let Some(outcome) = receiver.borrow().clone() {
                if let Some(entry) = self.units.lock().expect("units").get_mut(unit_id) {
                    entry.finished_at.get_or_insert_with(Utc::now);
                }
                return Ok(outcome);
            }
            if receiver.changed().await.is_err() {
                return Err(EnvironmentError::RuntimeUnavailable(format!(
                    "unit {unit_id} ended without an outcome"
                )));
            }
        }
    }

    async fn stop(&self, unit_id: &str) -> Result<(), EnvironmentError> {
        if let Some(entry) = self.units.lock().expect("units").get(unit_id) {
            entry.control.cancel();
        }
        Ok(())
    }

    async fn ack(&self, unit_id: &str) -> Result<(), EnvironmentError> {
        let removed = {
            let mut units = self.units.lock().expect("units");
            match units.get(unit_id) {
                Some(entry) if entry.outcome.borrow().is_some() => units.remove(unit_id),
                _ => None,
            }
        };
        if removed.is_some()
            && let Some(path) = self.outcome_path(unit_id)
        {
            let _ = std::fs::remove_file(path);
        }
        Ok(())
    }

    async fn units(&self) -> Result<Vec<UnitStatus>, EnvironmentError> {
        Ok(self
            .units
            .lock()
            .expect("units")
            .values()
            .map(Self::status_of)
            .collect())
    }

    async fn assign(&self, port: u16, route: Route) -> Result<(), EnvironmentError> {
        self.endpoints.assign(port, route.clone()).await?;
        let changed = self
            .routes
            .lock()
            .expect("routes")
            .insert(port, route.clone())
            != Some(route);
        if changed {
            self.write_routes();
        }
        Ok(())
    }

    async fn retain(&self, keep: BTreeSet<u16>) -> Result<(), EnvironmentError> {
        self.endpoints.retain(&keep);
        let changed = {
            let mut routes = self.routes.lock().expect("routes");
            let before = routes.len();
            routes.retain(|port, _| keep.contains(port));
            routes.len() != before
        };
        if changed {
            self.write_routes();
        }
        Ok(())
    }

    async fn routes(&self) -> Result<Vec<RouteStatus>, EnvironmentError> {
        let listening = self.endpoints.ports().into_iter().collect::<BTreeSet<_>>();
        Ok(self
            .routes
            .lock()
            .expect("routes")
            .iter()
            .map(|(port, route)| RouteStatus {
                port: *port,
                instance_id: route.instance_id.clone(),
                target_port: route.target_port,
                listening: listening.contains(port) && self.endpoints.route(*port).is_some(),
                open_connections: self.endpoints.open_connections(&route.instance_id),
                served_connections: self.endpoints.served_connections(&route.instance_id),
            })
            .collect())
    }

    async fn connections(&self, instance_id: &str) -> Result<(usize, u64), EnvironmentError> {
        Ok((
            self.endpoints.open_connections(instance_id),
            self.endpoints.served_connections(instance_id),
        ))
    }

    fn independent(&self) -> bool {
        false
    }

    async fn recover(&self) -> Result<bool, EnvironmentError> {
        Ok(false)
    }

    async fn shutdown(&self) -> Result<(), EnvironmentError> {
        Ok(())
    }
}

// ---- Process identity -------------------------------------------------------

#[cfg(target_os = "linux")]
pub(crate) fn boot_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|id| id.trim().to_string())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn boot_id() -> Option<String> {
    None
}

/// Field 22 of `/proc/<pid>/stat`: the start time in clock ticks.
#[cfg(target_os = "linux")]
pub(crate) fn start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 2..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn start_time(_pid: u32) -> Option<u64> {
    None
}

/// Whether this identity still names the same live process: same boot,
/// same start time. A reused PID never matches.
pub(crate) fn same_process(process: &ProcessIdentity) -> bool {
    process.boot_id.is_some()
        && process.boot_id == boot_id()
        && process.start_time.is_some()
        && process.start_time == start_time(process.pid)
}

#[cfg(unix)]
async fn terminate_group(pid: u32) {
    let Ok(group) = i32::try_from(pid) else {
        return;
    };
    // SAFETY: signalling a process group whose leader was verified, by
    // boot ID and start time, to be the recorded process.
    if unsafe { libc::kill(-group, libc::SIGTERM) } != 0 {
        return;
    }
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        // SAFETY: probing for existence with signal 0.
        if unsafe { libc::kill(-group, 0) } != 0 {
            return;
        }
    }
    // SAFETY: as above.
    unsafe { libc::kill(-group, libc::SIGKILL) };
}

#[cfg(not(unix))]
async fn terminate_group(_pid: u32) {}

// ---- The supervisor process and its client -----------------------------------

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Call {
    Info,
    Start {
        manifest: UnitManifest,
        request: Box<ProviderRequest>,
        admission: Box<Admission>,
    },
    Wait {
        unit_id: String,
    },
    Stop {
        unit_id: String,
    },
    Ack {
        unit_id: String,
    },
    Units,
    Assign {
        port: u16,
        instance_id: String,
        target_port: u16,
    },
    Retain {
        ports: BTreeSet<u16>,
    },
    Routes,
    Connections {
        instance_id: String,
    },
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
struct Reply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    value: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<(String, String)>,
}

/// The node-local socket a supervisor serves in `state_dir`.
pub fn socket_path(state_dir: &Path) -> PathBuf {
    state_dir.join("supervisor.sock")
}

/// Serve a supervisor on its socket until asked to shut down. Only this
/// user can connect: the socket is created with mode 0600.
#[cfg(unix)]
pub async fn serve_supervisor(
    plane: Arc<LocalDataPlane>,
    socket: PathBuf,
) -> Result<(), EnvironmentError> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let _ = std::fs::remove_file(&socket);
    let listener = tokio::net::UnixListener::bind(&socket)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    }
    let (stop, mut stopped) = watch::channel(false);
    loop {
        let (stream, _) = tokio::select! {
            accepted = listener.accept() => accepted?,
            _ = stopped.changed() => break,
        };
        let plane = plane.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            let (reader, mut writer) = stream.into_split();
            let mut lines = tokio::io::BufReader::new(reader).lines();
            let Ok(Some(line)) = lines.next_line().await else {
                return;
            };
            let reply = match serde_json::from_str::<Call>(&line) {
                Ok(Call::Shutdown) => {
                    let _ = stop.send(true);
                    Ok(serde_json::Value::Null)
                }
                Ok(call) => dispatch(&plane, call).await,
                Err(error) => Err(EnvironmentError::Invalid(error.to_string())),
            };
            let reply = match reply {
                Ok(value) => Reply {
                    value: Some(value),
                    error: None,
                },
                Err(error) => Reply {
                    value: None,
                    error: Some((error.kind().into(), error.message())),
                },
            };
            if let Ok(mut bytes) = serde_json::to_vec(&reply) {
                bytes.push(b'\n');
                let _ = writer.write_all(&bytes).await;
            }
        });
    }
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

#[cfg(unix)]
async fn dispatch(
    plane: &LocalDataPlane,
    call: Call,
) -> Result<serde_json::Value, EnvironmentError> {
    fn value(value: impl Serialize) -> serde_json::Value {
        serde_json::to_value(value).unwrap_or_default()
    }
    Ok(match call {
        Call::Info => value(&plane.info().await?),
        Call::Start {
            manifest,
            request,
            admission,
        } => value(&plane.start(manifest, *request, *admission).await?),
        Call::Wait { unit_id } => value(&plane.wait(&unit_id).await?),
        Call::Stop { unit_id } => value(&plane.stop(&unit_id).await?),
        Call::Ack { unit_id } => value(&plane.ack(&unit_id).await?),
        Call::Units => value(&plane.units().await?),
        Call::Assign {
            port,
            instance_id,
            target_port,
        } => value(
            &plane
                .assign(
                    port,
                    Route {
                        instance_id,
                        target_port,
                    },
                )
                .await?,
        ),
        Call::Retain { ports } => value(&plane.retain(ports).await?),
        Call::Routes => value(&plane.routes().await?),
        Call::Connections { instance_id } => value(&plane.connections(&instance_id).await?),
        Call::Shutdown => serde_json::Value::Null,
    })
}

/// How to start the node's supervisor: this executable's `supervisor`
/// command, in its own session so it outlives whoever started it.
#[derive(Debug, Clone)]
pub struct Launcher {
    pub executable: PathBuf,
    pub state_dir: PathBuf,
    pub endpoint_address: std::net::IpAddr,
}

impl Launcher {
    fn launch(&self) -> Result<u32, EnvironmentError> {
        std::fs::create_dir_all(&self.state_dir)?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.state_dir.join("supervisor.log"))?;
        let mut child = std::process::Command::new(&self.executable);
        child
            .arg("supervisor")
            .arg("--state-dir")
            .arg(&self.state_dir)
            .arg("--endpoint-address")
            .arg(self.endpoint_address.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // SAFETY: setsid in the child before exec only detaches it from
            // this session and terminal.
            unsafe {
                child.pre_exec(|| {
                    if libc::setsid() < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        Ok(child.spawn()?.id())
    }
}

/// A controller's handle on its node's supervisor process.
pub struct SupervisorClient {
    socket: PathBuf,
    launcher: Option<Launcher>,
}

impl SupervisorClient {
    pub fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            launcher: None,
        }
    }

    /// The node's supervisor: the running one when it answers, otherwise a
    /// new one started with `launcher`. A supervisor speaking another
    /// protocol is never taken over.
    pub async fn ensure(
        launcher: Launcher,
    ) -> Result<(Self, DataPlaneInfo, bool), EnvironmentError> {
        let client = Self {
            socket: socket_path(&launcher.state_dir),
            launcher: Some(launcher),
        };
        let (info, started) = client.connect_or_launch().await?;
        Ok((client, info, started))
    }

    async fn connect_or_launch(&self) -> Result<(DataPlaneInfo, bool), EnvironmentError> {
        match self.info().await {
            Ok(info) if info.protocol == SUPERVISOR_PROTOCOL => return Ok((info, false)),
            Ok(info) => {
                return Err(EnvironmentError::Invalid(format!(
                    "the supervisor on this node (pid {}) speaks protocol {}, not {SUPERVISOR_PROTOCOL}; refusing to take it over",
                    info.pid, info.protocol
                )));
            }
            Err(_) => {}
        }
        let Some(launcher) = &self.launcher else {
            return Err(EnvironmentError::RuntimeUnavailable(format!(
                "no supervisor answers at {}",
                self.socket.display()
            )));
        };
        let pid = launcher.launch()?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(info) = self.info().await {
                return Ok((info, true));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(EnvironmentError::RuntimeUnavailable(format!(
                    "the supervisor (pid {pid}) did not answer; see {}",
                    launcher.state_dir.join("supervisor.log").display()
                )));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[cfg(unix)]
    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        call: &Call,
    ) -> Result<T, EnvironmentError> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let stream = tokio::net::UnixStream::connect(&self.socket)
            .await
            .map_err(|error| {
                EnvironmentError::RuntimeUnavailable(format!(
                    "the supervisor at {} is unreachable: {error}",
                    self.socket.display()
                ))
            })?;
        let (reader, mut writer) = stream.into_split();
        let mut bytes = serde_json::to_vec(call)?;
        bytes.push(b'\n');
        writer.write_all(&bytes).await?;
        let line = tokio::io::BufReader::new(reader)
            .lines()
            .next_line()
            .await?
            .ok_or_else(|| {
                EnvironmentError::RuntimeUnavailable("the supervisor closed the connection".into())
            })?;
        let reply: Reply = serde_json::from_str(&line)?;
        if let Some((kind, message)) = reply.error {
            return Err(match kind.as_str() {
                "not_found" => EnvironmentError::NotFound(message),
                "conflict" => EnvironmentError::Conflict(message),
                "invalid" => EnvironmentError::Invalid(message),
                _ => EnvironmentError::RuntimeUnavailable(message),
            });
        }
        Ok(serde_json::from_value(
            reply.value.unwrap_or(serde_json::Value::Null),
        )?)
    }

    #[cfg(not(unix))]
    async fn call<T: serde::de::DeserializeOwned>(&self, _: &Call) -> Result<T, EnvironmentError> {
        Err(EnvironmentError::RuntimeUnavailable(
            "the supervisor needs a Unix host".into(),
        ))
    }
}

#[async_trait]
impl DataPlane for SupervisorClient {
    async fn info(&self) -> Result<DataPlaneInfo, EnvironmentError> {
        self.call(&Call::Info).await
    }

    async fn start(
        &self,
        manifest: UnitManifest,
        request: ProviderRequest,
        admission: Admission,
    ) -> Result<Option<ProcessIdentity>, EnvironmentError> {
        self.call(&Call::Start {
            manifest,
            request: Box::new(request),
            admission: Box::new(admission),
        })
        .await
    }

    async fn wait(&self, unit_id: &str) -> Result<UnitOutcome, EnvironmentError> {
        self.call(&Call::Wait {
            unit_id: unit_id.into(),
        })
        .await
    }

    async fn stop(&self, unit_id: &str) -> Result<(), EnvironmentError> {
        self.call(&Call::Stop {
            unit_id: unit_id.into(),
        })
        .await
    }

    async fn ack(&self, unit_id: &str) -> Result<(), EnvironmentError> {
        self.call(&Call::Ack {
            unit_id: unit_id.into(),
        })
        .await
    }

    async fn units(&self) -> Result<Vec<UnitStatus>, EnvironmentError> {
        self.call(&Call::Units).await
    }

    async fn assign(&self, port: u16, route: Route) -> Result<(), EnvironmentError> {
        self.call(&Call::Assign {
            port,
            instance_id: route.instance_id,
            target_port: route.target_port,
        })
        .await
    }

    async fn retain(&self, keep: BTreeSet<u16>) -> Result<(), EnvironmentError> {
        self.call(&Call::Retain { ports: keep }).await
    }

    async fn routes(&self) -> Result<Vec<RouteStatus>, EnvironmentError> {
        self.call(&Call::Routes).await
    }

    async fn connections(&self, instance_id: &str) -> Result<(usize, u64), EnvironmentError> {
        self.call(&Call::Connections {
            instance_id: instance_id.into(),
        })
        .await
    }

    fn independent(&self) -> bool {
        true
    }

    async fn recover(&self) -> Result<bool, EnvironmentError> {
        self.connect_or_launch().await.map(|(_, started)| started)
    }

    async fn shutdown(&self) -> Result<(), EnvironmentError> {
        self.call::<serde_json::Value>(&Call::Shutdown)
            .await
            .map(|_| ())
    }
}
