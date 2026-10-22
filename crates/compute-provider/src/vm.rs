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
//! **Suspend.** Stopping a session pauses its VM and writes a full snapshot
//! (memory and devices); resuming loads it, so processes, open files, and
//! memory continue where they were. A snapshot that cannot be written or
//! loaded falls back to a stop and a cold boot of the same disk, and the
//! resume says nothing that is not true: the disk is kept either way.

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
}

impl VmSessionProvider {
    pub fn new(image: VmImage, root: impl Into<PathBuf>, client: impl Into<PathBuf>) -> Self {
        Self {
            image,
            root: root.into(),
            client: client.into(),
            egress: None,
            forwarding: Mutex::new(BTreeSet::new()),
        }
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
        let log = std::fs::File::create(directory.join("firecracker.log"))
            .map_err(crate::transport_error)?;
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
                let log = std::fs::read_to_string(self.directory(vm).join("firecracker.log"))
                    .unwrap_or_default();
                let tail = log.lines().rev().take(5).collect::<Vec<_>>();
                return Err(ProviderError::new(
                    ProviderErrorKind::ProviderUnavailable,
                    format!(
                        "the VM did not answer within {}s: {}",
                        BOOT_TIMEOUT.as_secs(),
                        tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
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

    /// Pause the VM and write a full snapshot of it.
    async fn snapshot(&self, vm: &str) -> Result<(), String> {
        let socket = self.directory(vm).join("api.sock");
        api(&socket, "PATCH", "/vm", r#"{"state":"Paused"}"#).await?;
        let body = serde_json::json!({
            "snapshot_type": "Full",
            "snapshot_path": self.directory(vm).join("snapshot").display().to_string(),
            "mem_file_path": self.directory(vm).join("memory").display().to_string(),
        });
        api(&socket, "PUT", "/snapshot/create", &body.to_string()).await
    }

    /// Restore a VM from its snapshot, running.
    async fn restore(&self, vm: &str) -> Result<(), String> {
        self.launch(vm, None).map_err(|error| error.message)?;
        let socket = self.directory(vm).join("api.sock");
        for _ in 0..100 {
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
            "resume_vm": true,
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
    loop {
        let read = tokio::time::timeout(Duration::from_secs(60), stream.read(&mut chunk))
            .await
            .map_err(|_| format!("{method} {path}: no answer"))?
            .map_err(|error| error.to_string())?;
        answer.extend_from_slice(&chunk[..read]);
        if read == 0 || answer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&answer);
    let status = text.split(' ').nth(1).unwrap_or_default();
    if status.starts_with('2') {
        Ok(())
    } else {
        Err(format!(
            "{method} {path}: {}",
            text.lines().next().unwrap_or("no answer")
        ))
    }
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
        if self.running_pid(vm).is_some() {
            let snapshotted = self.snapshot(vm).await;
            if snapshotted.is_err() {
                // A snapshot that failed must not be loaded later.
                let _ = std::fs::remove_file(self.directory(vm).join("snapshot"));
                let _ = std::fs::remove_file(self.directory(vm).join("memory"));
            }
            self.kill(vm);
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
        let directory = self.directory(vm);
        if !directory.join("rootfs.ext4").exists() {
            return Err(ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                "the VM's disk no longer exists; it is not recreated",
            ));
        }
        if self.running_pid(vm).is_some() {
            return Ok(());
        }
        self.ensure_egress(vm);
        let restored = if directory.join("snapshot").exists() {
            let restored = self.restore(vm).await;
            if restored.is_err() {
                self.kill(vm);
                self.ensure_egress(vm);
            }
            restored.is_ok()
        } else {
            false
        };
        if !restored {
            // The disk is kept; memory is not: a cold boot of the same disk.
            self.launch(vm, Some(&directory.join("config.json")))?;
        }
        self.wait_until_answering(vm).await
    }

    async fn destroy(&self, provider_session_id: &str) -> Result<(), ProviderError> {
        let vm = Self::vm(provider_session_id)?;
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
}
