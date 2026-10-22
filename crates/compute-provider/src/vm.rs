//! A Firecracker microVM per session: its own kernel, nothing shared with
//! the node but what the VM's devices expose.
//!
//! ```text
//!   session job (on this node) ── compute guest-exec ──▶ vsock bridge (UDS)
//!                                                         │
//!   microVM: kernel + its own copy of the rootfs ◀───────┘ compute-guest (PID 1)
//!            no network interface; optional egress over vsock to the gateway
//! ```
//!
//! The adapter translates the session contract into a VM, as the container
//! adapter does into a container. Compute keeps the session's identity,
//! owner, lifecycle, and evidence; every command is still a durable job on
//! this node, which enters the VM through its vsock bridge.
//!
//! **Network.** The VM has no network interface. With an egress gateway
//! configured, a session that asks for `network` gets one route out: the
//! guest agent forwards `127.0.0.1:3128` to the host over vsock, and this
//! adapter connects that to the gateway, which enforces its allowlist and
//! adds credentials. Without a gateway, only `none` is offered.
//!
//! **Suspend** (`memory_suspend`). Stopping a session pauses its VM and
//! writes a full snapshot: guest memory, vCPU and device state. The
//! snapshot, the memory file, and the VM's disk are flushed to stable
//! storage, then a suspension record naming them is written; only then is
//! the VMM ended and the stop reported. Resuming loads that snapshot paused,
//! retires the record (the guest is about to change its disk, so the
//! snapshot must never be loaded again), and continues the guest; it is
//! reported only once the guest agent answers. Processes, their PIDs, tmpfs,
//! and the page cache continue where they were. Connections over vsock (a
//! command's own stream) do not survive; the commands started over them do.
//!
//! Nothing is substituted. A snapshot that cannot be written leaves the VM
//! running, unchanged, and the stop fails. A VM without a valid suspension
//! record (its VMM ended some other way, the snapshot is damaged, or another
//! VMM version wrote it) is not resumed and not cold-booted in its place:
//! the resume fails and says why. A stop or resume interrupted part way is
//! completed by the next attempt.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use compute_core::{
    NetworkPolicy, SessionCapabilities, SessionCommand, SessionConnection, SessionConnectionMode,
};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::sessions::{
    EnvironmentState, ProviderConnection, ProvisionRequest, ProvisionedSession, SessionEnvironment,
    SessionProvider, shell_request,
};
use crate::{ProviderError, ProviderErrorKind, ProviderRequest};

/// Where a VM's workspace is, inside the guest.
pub const VM_WORKSPACE: &str = "/workspace";

/// The guest port (and host vsock port) egress travels on.
const EGRESS_PORT: u32 = 3128;

/// How long a VM may take to boot to a ping.
const BOOT_TIMEOUT: Duration = Duration::from_secs(30);

/// The record that a VM is suspended with its memory.
const SUSPENSION: &str = "suspension.json";
const SUSPENSION_FORMAT: &str = "compute.vm-suspension@1";

/// What a suspension consists of, written once all of it is on stable
/// storage. A resume loads the snapshot only if all of it still matches.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Suspension {
    format: String,
    /// The VMM that wrote the snapshot; another version may not load it.
    backend: String,
    snapshot_sha256: String,
    snapshot_bytes: u64,
    memory_bytes: u64,
    suspended_at: chrono::DateTime<chrono::Utc>,
}

fn sync_path(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

fn file_sha256(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

/// What a microVM session is made from.
#[derive(Debug, Clone)]
pub struct VmImage {
    /// The `firecracker` executable.
    pub firecracker: PathBuf,
    /// An uncompressed kernel (`vmlinux`).
    pub kernel: PathBuf,
    /// An ext4 root filesystem whose `/sbin/compute-guest` is the guest
    /// agent; each session boots its own copy.
    pub rootfs: PathBuf,
}

pub struct VmSessionProvider {
    image: VmImage,
    /// One directory per VM: its disk, socket, snapshot, and log.
    root: PathBuf,
    /// The `compute` executable whose `guest-exec` a job runs.
    client: PathBuf,
    egress: Option<SocketAddr>,
    forwarding: Mutex<BTreeSet<String>>,
    /// One lifecycle operation per VM at a time. A caller and the session
    /// sweep can both drive one session: two suspends of one VM would each
    /// take the other's snapshot for a stale one.
    lifecycle: Mutex<BTreeMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    /// The VMM and what every VM boots from, measured once.
    identity: VmIdentity,
}

#[derive(Debug, Clone, Default)]
struct VmIdentity {
    backend: String,
    kernel: Option<String>,
    rootfs: Option<String>,
}

impl VmIdentity {
    fn measure(image: &VmImage) -> Self {
        let digest = |path: &Path| -> Option<String> {
            let mut file = std::fs::File::open(path).ok()?;
            let mut hasher = Sha256::new();
            std::io::copy(&mut file, &mut hasher).ok()?;
            Some(format!("sha256:{:x}", hasher.finalize()))
        };
        let version = std::process::Command::new(&image.firecracker)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .output()
            .ok()
            .and_then(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .next()
                    .map(|line| line.trim().to_owned())
            })
            .filter(|line| !line.is_empty())
            .unwrap_or_else(|| "Firecracker (version unknown)".into());
        Self {
            backend: version,
            kernel: digest(&image.kernel),
            rootfs: digest(&image.rootfs),
        }
    }
}

impl VmSessionProvider {
    pub fn new(image: VmImage, root: impl Into<PathBuf>, client: impl Into<PathBuf>) -> Self {
        Self {
            identity: VmIdentity::measure(&image),
            image,
            root: root.into(),
            client: client.into(),
            egress: None,
            forwarding: Mutex::new(BTreeSet::new()),
            lifecycle: Mutex::new(BTreeMap::new()),
        }
    }

    /// Hold `vm`'s lifecycle lock; another provision, stop, resume, or
    /// destroy of it waits, then finds the state this one left.
    async fn exclusive(&self, vm: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = self
            .lifecycle
            .lock()
            .expect("lifecycle")
            .entry(vm.to_owned())
            .or_default()
            .clone();
        lock.lock_owned().await
    }

    /// Sessions that ask for network reach it only through this egress
    /// gateway.
    pub fn with_egress(mut self, gateway: SocketAddr) -> Self {
        self.egress = Some(gateway);
        self
    }

    fn vm(provider_session_id: &str) -> Result<&str, ProviderError> {
        let valid = provider_session_id
            .strip_prefix("compute-vm-")
            .is_some_and(|digest| {
                digest.len() == 24
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            });
        if valid {
            Ok(provider_session_id)
        } else {
            Err(ProviderError::new(
                ProviderErrorKind::UnknownSession,
                "malformed VM identity",
            ))
        }
    }

    fn directory(&self, vm: &str) -> PathBuf {
        self.root.join(vm)
    }

    fn running_pid(&self, vm: &str) -> Option<i32> {
        let pid = std::fs::read_to_string(self.directory(vm).join("pid"))
            .ok()?
            .trim()
            .parse::<i32>()
            .ok()?;
        // SAFETY: signal 0 only checks that the process exists.
        (unsafe { libc::kill(pid, 0) } == 0).then_some(pid)
    }

    /// Start Firecracker for `vm`, detached, with or without a boot config.
    fn launch(&self, vm: &str, config: Option<&Path>) -> Result<(), ProviderError> {
        let directory = self.directory(vm);
        for stale in ["api.sock", "vsock.sock"] {
            let _ = std::fs::remove_file(directory.join(stale));
        }
        // Appended, never truncated: a failed snapshot's evidence survives
        // the restore or boot that follows it.
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join("firecracker.log"))
            .map_err(crate::transport_error)?;
        let _ = std::io::Write::write_all(
            &mut log,
            format!(
                "== compute: {} at {}\n",
                if config.is_some() { "boot" } else { "restore" },
                chrono::Utc::now().to_rfc3339()
            )
            .as_bytes(),
        );
        let mut command = std::process::Command::new(&self.image.firecracker);
        command
            .arg("--api-sock")
            .arg(directory.join("api.sock"))
            .current_dir(&directory)
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone().map_err(crate::transport_error)?)
            .stderr(log);
        if let Some(config) = config {
            command.arg("--config-file").arg(config);
        }
        {
            use std::os::unix::process::CommandExt;
            // The VM outlives the request that started it.
            command.process_group(0);
        }
        let mut child = command.spawn().map_err(|error| {
            ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                format!("firecracker {}: {error}", self.image.firecracker.display()),
            )
        })?;
        std::fs::write(directory.join("pid"), child.id().to_string())
            .map_err(crate::transport_error)?;
        // Reaped when it ends, so an ended VM is never mistaken for a
        // running one.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }

    async fn wait_until_answering(&self, vm: &str) -> Result<(), ProviderError> {
        let uds = self.directory(vm).join("vsock.sock");
        let deadline = tokio::time::Instant::now() + BOOT_TIMEOUT;
        loop {
            if let Ok(stream) =
                compute_guest::connect_firecracker(&uds, compute_guest::GUEST_PORT).await
                && compute_guest::ping(stream).await.is_ok()
            {
                return Ok(());
            }
            if self.running_pid(vm).is_none() || tokio::time::Instant::now() > deadline {
                return Err(ProviderError::new(
                    ProviderErrorKind::ProviderUnavailable,
                    format!(
                        "the VM did not answer within {}s: {}",
                        BOOT_TIMEOUT.as_secs(),
                        self.log_tail(vm)
                    ),
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Carry the guest's egress connections to the gateway.
    fn ensure_egress(&self, vm: &str) {
        let Some(gateway) = self.egress else { return };
        let mut forwarding = self.forwarding.lock().expect("forwarding");
        if !forwarding.insert(vm.to_owned()) {
            return;
        }
        // Firecracker connects a guest's connection to host port P to the
        // Unix socket `<vsock uds>_P`.
        let path = self.directory(vm).join(format!("vsock.sock_{EGRESS_PORT}"));
        let _ = std::fs::remove_file(&path);
        let Ok(listener) = tokio::net::UnixListener::bind(&path) else {
            forwarding.remove(vm);
            return;
        };
        tokio::spawn(async move {
            while let Ok((mut inbound, _)) = listener.accept().await {
                tokio::spawn(async move {
                    if let Ok(mut outbound) = tokio::net::TcpStream::connect(gateway).await {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                });
            }
        });
    }

    fn forget_egress(&self, vm: &str) {
        self.forwarding.lock().expect("forwarding").remove(vm);
    }

    fn kill(&self, vm: &str) {
        if let Some(pid) = self.running_pid(vm) {
            // SAFETY: SIGKILL to the Firecracker process this adapter started.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
            for _ in 0..50 {
                if self.running_pid(vm).is_none() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let _ = std::fs::remove_file(self.directory(vm).join("pid"));
        self.forget_egress(vm);
    }

    /// The last lines the VMM wrote, for an error that names its cause.
    fn log_tail(&self, vm: &str) -> String {
        let log =
            std::fs::read_to_string(self.directory(vm).join("firecracker.log")).unwrap_or_default();
        let tail = log.lines().rev().take(5).collect::<Vec<_>>();
        tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
    }

    /// The VM's suspension record, if it is present and everything it names
    /// is still intact and loadable by this VMM.
    fn suspension(&self, vm: &str) -> Result<Suspension, String> {
        let directory = self.directory(vm);
        let bytes = std::fs::read(directory.join(SUSPENSION)).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                "it was not suspended (no memory snapshot)".to_owned()
            } else {
                format!("its suspension record: {error}")
            }
        })?;
        let record: Suspension = serde_json::from_slice(&bytes)
            .map_err(|error| format!("its suspension record is not valid: {error}"))?;
        if record.format != SUSPENSION_FORMAT {
            return Err(format!("its suspension record is {}", record.format));
        }
        if record.backend != self.identity.backend {
            return Err(format!(
                "it was suspended by {}, and this node runs {}",
                record.backend, self.identity.backend
            ));
        }
        let size = |name: &str| std::fs::metadata(directory.join(name)).map(|meta| meta.len());
        if size("memory").ok() != Some(record.memory_bytes) {
            return Err("its memory file is missing or not the size it was written".into());
        }
        if size("snapshot").ok() != Some(record.snapshot_bytes)
            || file_sha256(&directory.join("snapshot")).ok().as_deref()
                != Some(record.snapshot_sha256.as_str())
        {
            return Err("its device snapshot is missing or changed".into());
        }
        Ok(record)
    }

    fn write_suspension(&self, vm: &str, record: &Suspension) -> std::io::Result<()> {
        let directory = self.directory(vm);
        let staged = directory.join(format!("{SUSPENSION}.tmp"));
        {
            let mut file = std::fs::File::create(&staged)?;
            std::io::Write::write_all(&mut file, &serde_json::to_vec_pretty(record)?)?;
            file.sync_all()?;
        }
        std::fs::rename(&staged, directory.join(SUSPENSION))?;
        sync_path(&directory)
    }

    fn retire_suspension(&self, vm: &str) -> std::io::Result<()> {
        let directory = self.directory(vm);
        match std::fs::remove_file(directory.join(SUSPENSION)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        sync_path(&directory)
    }

    /// Pause the running VM, write a full snapshot, put it and the disk on
    /// stable storage, record the suspension, and end the VMM. On any
    /// failure before the record, the VM is continued and left running.
    async fn suspend(&self, vm: &str) -> Result<(), ProviderError> {
        let directory = self.directory(vm);
        let socket = directory.join("api.sock");
        for stale in ["snapshot", "memory"] {
            let _ = std::fs::remove_file(directory.join(stale));
        }
        api(&socket, "PATCH", "/vm", r#"{"state":"Paused"}"#)
            .await
            .map_err(|error| {
                ProviderError::new(
                    ProviderErrorKind::ProviderUnavailable,
                    format!("the VM could not be paused: {error}"),
                )
            })?;
        let written = async {
            let body = serde_json::json!({
                "snapshot_type": "Full",
                "snapshot_path": directory.join("snapshot").display().to_string(),
                "mem_file_path": directory.join("memory").display().to_string(),
            });
            api(&socket, "PUT", "/snapshot/create", &body.to_string()).await?;
            // Only what is on disk counts, whatever the VMM answered.
            for name in ["snapshot", "memory"] {
                if !directory.join(name).is_file() {
                    return Err(format!(
                        "the VMM reported the snapshot written, but {} is not a file",
                        directory.join(name).display()
                    ));
                }
            }
            let stable = |name: &str| {
                sync_path(&directory.join(name))
                    .map_err(|error| format!("flushing {name}: {error}"))
            };
            for name in ["snapshot", "memory", "rootfs.ext4"] {
                stable(name)?;
            }
            let size = |name: &str| {
                std::fs::metadata(directory.join(name))
                    .map(|meta| meta.len())
                    .map_err(|error| format!("{name}: {error}"))
            };
            let record = Suspension {
                format: SUSPENSION_FORMAT.into(),
                backend: self.identity.backend.clone(),
                snapshot_sha256: file_sha256(&directory.join("snapshot"))
                    .map_err(|error| format!("snapshot: {error}"))?,
                snapshot_bytes: size("snapshot")?,
                memory_bytes: size("memory")?,
                suspended_at: chrono::Utc::now(),
            };
            self.write_suspension(vm, &record)
                .map_err(|error| format!("recording the suspension: {error}"))
        }
        .await;
        if let Err(error) = written {
            let _ = self.retire_suspension(vm);
            for partial in ["snapshot", "memory"] {
                let _ = std::fs::remove_file(directory.join(partial));
            }
            // Nothing was lost yet: the VM continues as it was.
            let continued = api(&socket, "PATCH", "/vm", r#"{"state":"Resumed"}"#).await;
            return Err(ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                match continued {
                    Ok(()) => format!(
                        "the VM could not be suspended: {error}; it is still running, unchanged \
                         (VMM log: {})",
                        self.log_tail(vm)
                    ),
                    Err(again) => format!(
                        "the VM could not be suspended: {error}; and it could not be continued: \
                         {again} (it is paused; VMM log: {})",
                        self.log_tail(vm)
                    ),
                },
            ));
        }
        self.kill(vm);
        Ok(())
    }

    /// Start a VMM and load the VM's snapshot into it, paused.
    async fn load(&self, vm: &str) -> Result<(), String> {
        self.launch(vm, None).map_err(|error| error.message)?;
        let socket = self.directory(vm).join("api.sock");
        for _ in 0..250 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let body = serde_json::json!({
            "snapshot_path": self.directory(vm).join("snapshot").display().to_string(),
            "mem_backend": {
                "backend_type": "File",
                "backend_path": self.directory(vm).join("memory").display().to_string(),
            },
            "resume_vm": false,
        });
        api(&socket, "PUT", "/snapshot/load", &body.to_string()).await
    }

    fn boot_config(&self, vm: &str, request: &ProvisionRequest) -> serde_json::Value {
        let directory = self.directory(vm);
        let mut boot_args =
            "console=ttyS0 reboot=k panic=1 pci=off quiet init=/sbin/compute-guest".to_owned();
        if request.network != NetworkPolicy::None {
            boot_args.push_str(&format!(" compute.forward={EGRESS_PORT}:{EGRESS_PORT}"));
        }
        serde_json::json!({
            "boot-source": {
                "kernel_image_path": self.image.kernel.display().to_string(),
                "boot_args": boot_args,
            },
            "drives": [{
                "drive_id": "rootfs",
                "path_on_host": directory.join("rootfs.ext4").display().to_string(),
                "is_root_device": true,
                "is_read_only": false,
            }],
            "machine-config": {
                "vcpu_count": request.resources.cpu_count.unwrap_or(1).clamp(1, 32),
                "mem_size_mib": request
                    .resources
                    .memory_bytes
                    .map(|bytes| (bytes / (1024 * 1024)).max(128))
                    .unwrap_or(512),
            },
            "vsock": {
                "guest_cid": 3,
                "uds_path": directory.join("vsock.sock").display().to_string(),
            },
        })
    }
}

/// One request to Firecracker's API socket; `Ok` for a 2xx answer.
async fn api(socket: &Path, method: &str, path: &str, body: &str) -> Result<(), String> {
    let mut stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|error| format!("the VM's API: {error}"))?;
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    let mut answer = Vec::new();
    let mut chunk = [0_u8; 4096];
    // The head, then as much body as it declares.
    let complete = |answer: &[u8]| -> bool {
        let Some(end) = answer.windows(4).position(|window| window == b"\r\n\r\n") else {
            return false;
        };
        let head = String::from_utf8_lossy(&answer[..end]).to_ascii_lowercase();
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        answer.len() >= end + 4 + length
    };
    loop {
        let read = tokio::time::timeout(Duration::from_secs(120), stream.read(&mut chunk))
            .await
            .map_err(|_| format!("{method} {path}: no answer"))?
            .map_err(|error| error.to_string())?;
        answer.extend_from_slice(&chunk[..read]);
        if read == 0 || complete(&answer) {
            break;
        }
    }
    let text = String::from_utf8_lossy(&answer);
    let status = text.split(' ').nth(1).unwrap_or_default();
    if status.starts_with('2') {
        return Ok(());
    }
    // Firecracker explains a refusal in the body's `fault_message`.
    let fault = text
        .split_once("\r\n\r\n")
        .and_then(|(_, body)| serde_json::from_str::<serde_json::Value>(body).ok())
        .and_then(|body| body["fault_message"].as_str().map(str::to_owned));
    Err(format!(
        "{method} {path}: {}{}",
        text.lines().next().unwrap_or("no answer"),
        fault.map(|fault| format!(": {fault}")).unwrap_or_default()
    ))
}

fn provisioned(vm: &str, capabilities: SessionCapabilities) -> ProvisionedSession {
    ProvisionedSession {
        provider_session_id: vm.to_owned(),
        connection: SessionConnection {
            mode: SessionConnectionMode::Exec,
            address: None,
            port: None,
            details: BTreeMap::from([("vm".into(), vm.to_owned())]),
        },
        endpoints: vec![],
        capabilities,
    }
}

#[async_trait]
impl SessionProvider for VmSessionProvider {
    fn kind(&self) -> String {
        "microvm".into()
    }

    fn capabilities(&self) -> SessionCapabilities {
        SessionCapabilities {
            exec: true,
            terminal: false,
            filesystem: true,
            network: self.egress.is_some(),
            public_endpoint: false,
            persistent_storage: false,
            suspend: true,
            resume: true,
            claim: true,
            // Ending the VM ends every process in it.
            process_tree_termination: true,
            memory_suspend: true,
        }
    }

    async fn provision(
        &self,
        request: &ProvisionRequest,
    ) -> Result<ProvisionedSession, ProviderError> {
        if !request.endpoints.is_empty() {
            return Err(crate::unsupported(&self.kind(), "endpoints"));
        }
        if request.network != NetworkPolicy::None && self.egress.is_none() {
            return Err(crate::unsupported(
                &self.kind(),
                "network without an egress gateway (a VM has no other route out)",
            ));
        }
        let vm = format!(
            "compute-vm-{}",
            &format!(
                "{:x}",
                Sha256::digest(format!("vm:{}", request.session_id).as_bytes())
            )[..24]
        );
        let mut capabilities = self.capabilities();
        capabilities.network = request.network != NetworkPolicy::None;
        let _exclusive = self.exclusive(&vm).await;
        // Idempotent per session: a VM that runs is this session's.
        if self.running_pid(&vm).is_some() {
            self.ensure_egress(&vm);
            return Ok(provisioned(&vm, capabilities));
        }
        let directory = self.directory(&vm);
        std::fs::create_dir_all(&directory).map_err(crate::transport_error)?;
        let disk = directory.join("rootfs.ext4");
        if !disk.exists() {
            let staged = directory.join("rootfs.ext4.tmp");
            std::fs::copy(&self.image.rootfs, &staged).map_err(crate::transport_error)?;
            std::fs::rename(&staged, &disk).map_err(crate::transport_error)?;
        }
        let config = directory.join("config.json");
        std::fs::write(
            &config,
            serde_json::to_vec_pretty(&self.boot_config(&vm, request))
                .map_err(crate::transport_error)?,
        )
        .map_err(crate::transport_error)?;
        self.ensure_egress(&vm);
        self.launch(&vm, Some(&config))?;
        if let Err(error) = self.wait_until_answering(&vm).await {
            self.kill(&vm);
            return Err(error);
        }
        Ok(provisioned(&vm, capabilities))
    }

    async fn inspect(&self, provider_session_id: &str) -> Result<EnvironmentState, ProviderError> {
        let vm = Self::vm(provider_session_id)?;
        Ok(if self.running_pid(vm).is_some() {
            EnvironmentState::Ready
        } else if self.directory(vm).join("rootfs.ext4").exists() {
            EnvironmentState::Stopped
        } else {
            EnvironmentState::Missing
        })
    }

    async fn exec(
        &self,
        environment: &SessionEnvironment,
        command: &SessionCommand,
    ) -> Result<ProviderRequest, ProviderError> {
        let vm = Self::vm(&environment.provider_session_id)?;
        if environment.network != NetworkPolicy::None {
            self.ensure_egress(vm);
        }
        // The job runs on this node and enters the VM; values travel in the
        // job's own sealed environment and are passed by name.
        let mut arguments = vec![
            self.client.display().to_string(),
            "guest-exec".into(),
            "--uds".into(),
            self.directory(vm).join("vsock.sock").display().to_string(),
            "--cwd".into(),
            VM_WORKSPACE.into(),
        ];
        // `guest-exec` sets COMPUTE_SESSION_WORKSPACE to the working
        // directory itself.
        let mut carried = BTreeMap::from([("HOME".to_owned(), VM_WORKSPACE.to_owned())]);
        if environment.network != NetworkPolicy::None {
            for name in ["HTTPS_PROXY", "HTTP_PROXY", "https_proxy", "http_proxy"] {
                carried.insert(name.into(), format!("http://127.0.0.1:{EGRESS_PORT}"));
            }
        }
        for (key, value) in &command.env {
            if key.starts_with("COMPUTE_SESSION_") {
                return Err(ProviderError::new(
                    ProviderErrorKind::ArtifactInvalid,
                    format!("{key} is set by Compute"),
                ));
            }
            carried.insert(key.clone(), value.clone());
        }
        for name in carried.keys() {
            arguments.extend(["--env".into(), name.clone()]);
        }
        // The guest learns its session from the job's own variable.
        arguments.extend(["--env".into(), "COMPUTE_SESSION_ID".into()]);
        if command.stdin.is_some() {
            arguments.push("--stdin".into());
        }
        arguments.push("--".into());
        arguments.extend(command.command.iter().cloned());
        let mut direct = SessionCommand::new(arguments);
        direct.timeout = command.timeout;
        direct.env = carried;
        direct.stdin = command.stdin.clone();
        direct.output_bytes = command.output_bytes;
        // The client is a local process; the VM's own lack of a network
        // interface is the boundary.
        let mut client = environment.clone();
        client.network = NetworkPolicy::Network;
        shell_request(&client, &direct)
    }

    async fn connect(
        &self,
        environment: &SessionEnvironment,
    ) -> Result<ProviderConnection, ProviderError> {
        Self::vm(&environment.provider_session_id)?;
        Ok(ProviderConnection {
            connection: None,
            command: vec![
                "compute".into(),
                "session".into(),
                "exec".into(),
                environment.session_id.to_string(),
                "--".into(),
            ],
            credentials: BTreeMap::new(),
            expires_at: None,
        })
    }

    async fn stop(&self, provider_session_id: &str) -> Result<(), ProviderError> {
        let vm = Self::vm(provider_session_id)?;
        let _exclusive = self.exclusive(vm).await;
        if self.suspension(vm).is_ok() {
            // Already suspended: a repeated stop, or one interrupted after
            // its record was written (the VM has been paused since).
            self.kill(vm);
        } else if self.running_pid(vm).is_some() {
            self.suspend(vm).await?;
        } else {
            return Err(ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                "the VM is not running and was not suspended: its memory is gone, so there is \
                 nothing to suspend (its disk is kept until the session is destroyed)",
            ));
        }
        if self.running_pid(vm).is_some() {
            return Err(ProviderError::new(
                ProviderErrorKind::TerminationFailed,
                "the VM is still running after it was stopped",
            ));
        }
        Ok(())
    }

    async fn resume(&self, provider_session_id: &str) -> Result<(), ProviderError> {
        let vm = Self::vm(provider_session_id)?;
        let _exclusive = self.exclusive(vm).await;
        let directory = self.directory(vm);
        if !directory.join("rootfs.ext4").exists() {
            return Err(ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                "the VM's disk no longer exists; it is not recreated",
            ));
        }
        if self.running_pid(vm).is_some() {
            if directory.join(SUSPENSION).exists() {
                // Paused since its snapshot and never continued (a stop or a
                // resume that was interrupted): load it again.
                self.kill(vm);
            } else {
                // A resume interrupted after the record was retired: the
                // loaded VM only needs to be continued.
                let _ = api(
                    &directory.join("api.sock"),
                    "PATCH",
                    "/vm",
                    r#"{"state":"Resumed"}"#,
                )
                .await;
                self.ensure_egress(vm);
                return self.wait_until_answering(vm).await;
            }
        }
        let record = self.suspension(vm).map_err(|why| {
            ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                format!(
                    "the VM cannot be resumed with its memory: {why}; it is not cold-booted in \
                     its place"
                ),
            )
        })?;
        self.ensure_egress(vm);
        if let Err(error) = self.load(vm).await {
            self.kill(vm);
            return Err(ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                format!(
                    "the VM's snapshot could not be loaded: {error} ({})",
                    self.log_tail(vm)
                ),
            ));
        }
        // From here the guest runs and changes its disk: the snapshot must
        // never be loaded again.
        if let Err(error) = self.retire_suspension(vm) {
            self.kill(vm);
            return Err(crate::transport_error(error));
        }
        if let Err(error) = api(
            &directory.join("api.sock"),
            "PATCH",
            "/vm",
            r#"{"state":"Resumed"}"#,
        )
        .await
        {
            // The guest never ran: the suspension is still whole.
            self.kill(vm);
            let _ = self.write_suspension(vm, &record);
            return Err(ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                format!("the restored VM could not be continued: {error}"),
            ));
        }
        self.wait_until_answering(vm).await?;
        for spent in ["snapshot", "memory"] {
            let _ = std::fs::remove_file(directory.join(spent));
        }
        Ok(())
    }

    async fn destroy(&self, provider_session_id: &str) -> Result<(), ProviderError> {
        let vm = Self::vm(provider_session_id)?;
        let _exclusive = self.exclusive(vm).await;
        self.kill(vm);
        if self.running_pid(vm).is_some() {
            return Err(ProviderError::new(
                ProviderErrorKind::TerminationFailed,
                "the VM is still running after it was ended",
            ));
        }
        match std::fs::remove_dir_all(self.directory(vm)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(crate::transport_error(error)),
        }
    }

    async fn claim(&self, provider_session_id: &str) -> Result<(), ProviderError> {
        Self::vm(provider_session_id).map(|_| ())
    }

    fn contains_network(&self) -> bool {
        true
    }

    fn substrate(&self, provider_session_id: &str) -> Option<compute_core::SubstrateEvidence> {
        let vm = Self::vm(provider_session_id).ok()?;
        let mut evidence = compute_core::SubstrateEvidence::new("microvm");
        evidence.backend = Some(self.identity.backend.clone());
        for (name, digest) in [
            ("kernel", &self.identity.kernel),
            ("rootfs", &self.identity.rootfs),
        ] {
            if let Some(digest) = digest {
                evidence.artifacts.insert(name.into(), digest.clone());
            }
        }
        // What this VM was booted with, read back from its own config.
        if let Ok(config) = std::fs::read(self.directory(vm).join("config.json"))
            && let Ok(config) = serde_json::from_slice::<serde_json::Value>(&config)
        {
            let machine = &config["machine-config"];
            if let Some(vcpus) = machine["vcpu_count"].as_u64() {
                evidence.limits.insert("vcpus".into(), vcpus.to_string());
            }
            if let Some(mib) = machine["mem_size_mib"].as_u64() {
                evidence
                    .limits
                    .insert("memory_bytes".into(), (mib * 1024 * 1024).to_string());
            }
            let forwarded = config["boot-source"]["boot_args"]
                .as_str()
                .is_some_and(|args| args.contains("compute.forward="));
            evidence.limits.insert(
                "network".into(),
                if forwarded {
                    "egress-gateway-only"
                } else {
                    "none"
                }
                .into(),
            );
        }
        Some(evidence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VM: &str = "compute-vm-0123456789abcdef01234567";

    /// A provider whose VMM does not exist: nothing here may launch one.
    fn provider(root: &Path) -> VmSessionProvider {
        VmSessionProvider::new(
            VmImage {
                firecracker: root.join("no-firecracker"),
                kernel: root.join("no-kernel"),
                rootfs: root.join("no-rootfs"),
            },
            root.join("vms"),
            root.join("compute"),
        )
    }

    /// A suspended VM's files, as `suspend` leaves them.
    fn suspended(provider: &VmSessionProvider) -> Suspension {
        let directory = provider.directory(VM);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("rootfs.ext4"), b"disk").unwrap();
        std::fs::write(directory.join("snapshot"), b"vcpu and device state").unwrap();
        std::fs::write(directory.join("memory"), vec![7_u8; 4096]).unwrap();
        let record = Suspension {
            format: SUSPENSION_FORMAT.into(),
            backend: provider.identity.backend.clone(),
            snapshot_sha256: file_sha256(&directory.join("snapshot")).unwrap(),
            snapshot_bytes: 21,
            memory_bytes: 4096,
            suspended_at: chrono::Utc::now(),
        };
        provider.write_suspension(VM, &record).unwrap();
        record
    }

    #[test]
    fn memory_suspend_is_declared() {
        let root = tempfile::tempdir().unwrap();
        let capabilities = provider(root.path()).capabilities();
        assert!(capabilities.memory_suspend && capabilities.suspend && capabilities.resume);
    }

    #[test]
    fn a_whole_suspension_is_loadable() {
        let root = tempfile::tempdir().unwrap();
        let provider = provider(root.path());
        let record = suspended(&provider);
        assert_eq!(provider.suspension(VM), Ok(record));
    }

    #[test]
    fn a_damaged_or_foreign_suspension_is_not() {
        let root = tempfile::tempdir().unwrap();
        let provider = provider(root.path());
        let directory = provider.directory(VM);

        suspended(&provider);
        std::fs::write(directory.join("memory"), vec![7_u8; 100]).unwrap();
        assert!(provider.suspension(VM).unwrap_err().contains("memory file"));

        suspended(&provider);
        std::fs::write(directory.join("snapshot"), b"vcpu and device stat!").unwrap();
        assert!(
            provider
                .suspension(VM)
                .unwrap_err()
                .contains("device snapshot")
        );

        let mut record = suspended(&provider);
        record.backend = "Firecracker v0.0.1".into();
        provider.write_suspension(VM, &record).unwrap();
        assert!(
            provider
                .suspension(VM)
                .unwrap_err()
                .contains("suspended by")
        );

        suspended(&provider);
        provider.retire_suspension(VM).unwrap();
        assert!(
            provider
                .suspension(VM)
                .unwrap_err()
                .contains("not suspended")
        );
    }

    #[tokio::test]
    async fn a_vm_without_its_memory_is_neither_resumed_nor_cold_booted() {
        let root = tempfile::tempdir().unwrap();
        let provider = provider(root.path());
        suspended(&provider);
        provider.retire_suspension(VM).unwrap();
        let error = provider.resume(VM).await.unwrap_err();
        assert!(
            error.message.contains("not cold-booted in its place"),
            "{error:?}"
        );
        // Nothing was launched: the VMM does not even exist.
        assert!(provider.running_pid(VM).is_none());
        assert!(!provider.directory(VM).join("pid").exists());
    }

    #[tokio::test]
    async fn a_vm_that_ended_without_suspension_cannot_be_stopped_as_suspended() {
        let root = tempfile::tempdir().unwrap();
        let provider = provider(root.path());
        std::fs::create_dir_all(provider.directory(VM)).unwrap();
        let error = provider.stop(VM).await.unwrap_err();
        assert!(error.message.contains("memory is gone"), "{error:?}");
    }

    #[tokio::test]
    async fn lifecycle_operations_on_one_vm_are_serialized() {
        let root = tempfile::tempdir().unwrap();
        let provider = std::sync::Arc::new(provider(root.path()));
        let held = provider.exclusive(VM).await;
        let waiting = {
            let provider = provider.clone();
            tokio::spawn(async move { provider.stop(VM).await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !waiting.is_finished(),
            "a stop waits for the operation in flight"
        );
        // Another VM is not held up.
        let other = "compute-vm-ffffffffffffffffffffffff";
        drop(provider.exclusive(other).await);
        drop(held);
        // It then sees the state the other left (here: nothing to suspend).
        assert!(waiting.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn a_repeated_stop_of_a_suspended_vm_succeeds() {
        let root = tempfile::tempdir().unwrap();
        let provider = provider(root.path());
        let record = suspended(&provider);
        provider.stop(VM).await.unwrap();
        provider.stop(VM).await.unwrap();
        assert_eq!(provider.suspension(VM), Ok(record));
    }
}
