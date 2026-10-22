//! A container per session, through a Docker-compatible CLI (`docker`,
//! `podman`, or anything that speaks the same commands).
//!
//! The adapter only translates the session contract into container
//! commands. Compute keeps the session's identity, owner, lifecycle, and
//! evidence; every command a session runs is still a durable job on this
//! node, which enters the container with `<runtime> exec`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use compute_core::{
    NetworkPolicy, SessionCapabilities, SessionCommand, SessionConnection, SessionConnectionMode,
};
use sha2::{Digest, Sha256};

use crate::sessions::{
    EnvironmentState, ProviderConnection, ProvisionRequest, ProvisionedSession, SessionEnvironment,
    SessionProvider, shell_request,
};
use crate::{ProviderError, ProviderErrorKind, ProviderRequest};

/// Where a container's workspace is mounted.
pub const CONTAINER_WORKSPACE: &str = "/workspace";

pub struct ContainerSessionProvider {
    runtime: String,
    image: String,
    /// Host directories mounted as each container's workspace.
    root: PathBuf,
    /// The OCI runtime containers run under (`runsc` for gVisor), when not
    /// the engine's default.
    oci_runtime: Option<String>,
}

/// The capabilities a session container keeps: enough for a package
/// manager running as root to install and own files, nothing that reaches
/// the network stack, devices, or the kernel.
pub const CONTAINER_CAPABILITIES: &[&str] = &[
    "CHOWN",
    "DAC_OVERRIDE",
    "FOWNER",
    "SETUID",
    "SETGID",
    "KILL",
];

/// The most processes one session container may hold.
pub const CONTAINER_PIDS_LIMIT: u32 = 4096;

/// Variables the container client itself reads. Their values stay on its
/// command line so a session cannot redirect the client that enters it.
const CLIENT_VARIABLES: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "XDG_RUNTIME_DIR",
    "XDG_CONFIG_HOME",
];

fn client_variable(key: &str) -> bool {
    CLIENT_VARIABLES.contains(&key)
        || key.starts_with("DOCKER_")
        || key.starts_with("CONTAINER_")
        || key.starts_with("CONTAINERS_")
        || key.starts_with("PODMAN_")
}

impl ContainerSessionProvider {
    /// `runtime` is the container CLI (resolved on `PATH` when not a path);
    /// `image` is what every session container starts from.
    pub fn new(
        runtime: impl Into<String>,
        image: impl Into<String>,
        root: impl Into<PathBuf>,
    ) -> Self {
        let runtime = runtime.into();
        let runtime = which::which(&runtime)
            .map(|path| path.display().to_string())
            .unwrap_or(runtime);
        Self {
            runtime,
            image: image.into(),
            root: root.into(),
            oci_runtime: None,
        }
    }

    /// Runs every session container under `runtime` (for example `runsc`).
    pub fn with_oci_runtime(mut self, runtime: impl Into<String>) -> Self {
        self.oci_runtime = Some(runtime.into());
        self
    }

    fn container(provider_session_id: &str) -> Result<&str, ProviderError> {
        let valid = provider_session_id
            .strip_prefix("compute-")
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
                "malformed container identity",
            ))
        }
    }

    async fn run(&self, arguments: &[String]) -> Result<std::process::Output, ProviderError> {
        tokio::process::Command::new(&self.runtime)
            .args(arguments)
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .map_err(|error| {
                ProviderError::new(
                    ProviderErrorKind::ProviderUnavailable,
                    format!("container runtime {}: {error}", self.runtime),
                )
            })
    }

    async fn checked(&self, arguments: &[String]) -> Result<String, ProviderError> {
        let output = self.run(arguments).await?;
        if !output.status.success() {
            return Err(ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                format!(
                    "{} {} failed: {}",
                    self.runtime,
                    arguments.first().map(String::as_str).unwrap_or_default(),
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    /// The container's state, or `None` when it does not exist.
    async fn state(&self, container: &str) -> Result<Option<String>, ProviderError> {
        let output = self
            .run(&[
                "inspect".into(),
                "--format".into(),
                "{{.State.Status}}".into(),
                container.into(),
            ])
            .await?;
        if output.status.success() {
            Ok(Some(
                String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            ))
        } else {
            Ok(None)
        }
    }

    fn workspace(&self, container: &str) -> PathBuf {
        self.root.join(container)
    }
}

fn provisioned(container: &str, capabilities: SessionCapabilities) -> ProvisionedSession {
    ProvisionedSession {
        provider_session_id: container.to_owned(),
        connection: SessionConnection {
            mode: SessionConnectionMode::Exec,
            address: None,
            port: None,
            details: BTreeMap::from([("container".into(), container.to_owned())]),
        },
        endpoints: vec![],
        capabilities,
    }
}

#[async_trait]
impl SessionProvider for ContainerSessionProvider {
    fn kind(&self) -> String {
        "container".into()
    }

    fn capabilities(&self) -> SessionCapabilities {
        SessionCapabilities {
            exec: true,
            terminal: false,
            filesystem: true,
            network: true,
            public_endpoint: false,
            persistent_storage: false,
            suspend: true,
            resume: true,
            claim: true,
            // The engine ends every process in a container it stops or
            // removes; the provider confirms the container's state after.
            process_tree_termination: true,
            memory_suspend: false,
            fork: false,
        }
    }

    async fn provision(
        &self,
        request: &ProvisionRequest,
    ) -> Result<ProvisionedSession, ProviderError> {
        if !request.endpoints.is_empty() {
            return Err(crate::unsupported(&self.kind(), "endpoints"));
        }
        let container = format!(
            "compute-{}",
            &format!(
                "{:x}",
                Sha256::digest(format!("container:{}", request.session_id).as_bytes())
            )[..24]
        );
        let mut capabilities = self.capabilities();
        capabilities.network = request.network != NetworkPolicy::None;
        // Idempotent per session: a container that exists is this session's.
        match self.state(&container).await?.as_deref() {
            Some("running") => return Ok(provisioned(&container, capabilities)),
            Some(_) => {
                self.checked(&["start".into(), container.clone()]).await?;
                return Ok(provisioned(&container, capabilities));
            }
            None => {}
        }
        let workspace = self.workspace(&container);
        std::fs::create_dir_all(&workspace).map_err(crate::transport_error)?;
        let mut arguments = vec![
            "run".to_owned(),
            "--detach".into(),
            "--name".into(),
            container.clone(),
            "--label".into(),
            format!("compute.session={}", request.session_id),
            "--volume".into(),
            format!("{}:{CONTAINER_WORKSPACE}", workspace.display()),
            "--workdir".into(),
            CONTAINER_WORKSPACE.into(),
            // An init reaps the processes every exec leaves behind.
            "--init".into(),
            "--cap-drop".into(),
            "ALL".into(),
            "--security-opt".into(),
            "no-new-privileges".into(),
            "--pids-limit".into(),
            CONTAINER_PIDS_LIMIT.to_string(),
        ];
        for capability in CONTAINER_CAPABILITIES {
            arguments.extend(["--cap-add".into(), (*capability).into()]);
        }
        if let Some(runtime) = &self.oci_runtime {
            arguments.extend(["--runtime".into(), runtime.clone()]);
        }
        if let Some(cpu) = request.resources.cpu_count {
            arguments.extend(["--cpus".into(), cpu.to_string()]);
        }
        if let Some(memory) = request.resources.memory_bytes {
            arguments.extend(["--memory".into(), format!("{memory}b")]);
        }
        if request.network == NetworkPolicy::None {
            arguments.extend(["--network".into(), "none".into()]);
        }
        arguments.extend([self.image.clone(), "sleep".into(), "infinity".into()]);
        self.checked(&arguments).await?;
        Ok(provisioned(&container, capabilities))
    }

    async fn inspect(&self, provider_session_id: &str) -> Result<EnvironmentState, ProviderError> {
        let container = Self::container(provider_session_id)?;
        Ok(match self.state(container).await?.as_deref() {
            None => EnvironmentState::Missing,
            Some("running") => EnvironmentState::Ready,
            Some("created" | "restarting") => EnvironmentState::Provisioning,
            Some(_) => EnvironmentState::Stopped,
        })
    }

    async fn exec(
        &self,
        environment: &SessionEnvironment,
        command: &SessionCommand,
    ) -> Result<ProviderRequest, ProviderError> {
        let container = Self::container(&environment.provider_session_id)?;
        // The job runs on this node and enters the container.
        let mut arguments = vec![
            self.runtime.clone(),
            "exec".into(),
            "--workdir".into(),
            CONTAINER_WORKSPACE.into(),
            "--env".into(),
            format!("HOME={CONTAINER_WORKSPACE}"),
            "--env".into(),
            format!("COMPUTE_SESSION_WORKSPACE={CONTAINER_WORKSPACE}"),
            "--env".into(),
            format!("COMPUTE_SESSION_ID={}", environment.session_id),
        ];
        // Values travel in the job's own environment, which is sealed at
        // rest, and the client copies them in by name: they never appear on
        // a command line another process on this node can read.
        let mut carried = BTreeMap::new();
        for (key, value) in &command.env {
            if key.starts_with("COMPUTE_SESSION_") {
                return Err(ProviderError::new(
                    ProviderErrorKind::ArtifactInvalid,
                    format!("{key} is set by Compute"),
                ));
            }
            if client_variable(key) {
                arguments.extend(["--env".into(), format!("{key}={value}")]);
            } else {
                arguments.extend(["--env".into(), key.clone()]);
                carried.insert(key.clone(), value.clone());
            }
        }
        if command.stdin.is_some() {
            // The client's standard input is the command's.
            arguments.push("--interactive".into());
        }
        arguments.push(container.to_owned());
        arguments.extend(command.command.iter().cloned());
        let mut direct = SessionCommand::new(arguments);
        direct.timeout = command.timeout;
        direct.env = carried;
        direct.stdin = command.stdin.clone();
        direct.output_bytes = command.output_bytes;
        // The job on this node is the engine's client, which needs the
        // engine, not the session's network; the container's own `--network`
        // is the boundary (and a host process cannot enforce `none`).
        let mut client = environment.clone();
        client.network = NetworkPolicy::Network;
        shell_request(&client, &direct)
    }

    async fn connect(
        &self,
        environment: &SessionEnvironment,
    ) -> Result<ProviderConnection, ProviderError> {
        Self::container(&environment.provider_session_id)?;
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
        let container = Self::container(provider_session_id)?;
        self.checked(&["stop".into(), container.into()]).await?;
        if self.state(container).await?.as_deref() == Some("running") {
            return Err(ProviderError::new(
                ProviderErrorKind::TerminationFailed,
                "the container is still running after it was stopped",
            ));
        }
        Ok(())
    }

    async fn resume(&self, provider_session_id: &str) -> Result<(), ProviderError> {
        let container = Self::container(provider_session_id)?;
        if self.state(container).await?.is_none() {
            return Err(ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                "the container no longer exists; it is not recreated",
            ));
        }
        self.checked(&["start".into(), container.into()])
            .await
            .map(|_| ())
    }

    async fn destroy(&self, provider_session_id: &str) -> Result<(), ProviderError> {
        let container = Self::container(provider_session_id)?;
        if self.state(container).await?.is_some() {
            self.checked(&["rm".into(), "--force".into(), container.into()])
                .await?;
            if self.state(container).await?.is_some() {
                return Err(ProviderError::new(
                    ProviderErrorKind::TerminationFailed,
                    "the container still exists after it was removed",
                ));
            }
        }
        remove_workspace(&self.workspace(container))
    }

    async fn claim(&self, provider_session_id: &str) -> Result<(), ProviderError> {
        Self::container(provider_session_id).map(|_| ())
    }

    fn contains_network(&self) -> bool {
        true
    }

    fn substrate(&self, provider_session_id: &str) -> Option<compute_core::SubstrateEvidence> {
        Self::container(provider_session_id).ok()?;
        let mut evidence = compute_core::SubstrateEvidence::new("container");
        let engine = Path::new(&self.runtime)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.runtime.clone());
        evidence.backend = Some(match &self.oci_runtime {
            Some(runtime) => format!("{engine} (OCI runtime {runtime})"),
            None => engine,
        });
        // A reference, not a digest: the engine resolves it.
        evidence
            .artifacts
            .insert("image_reference".into(), self.image.clone());
        evidence
            .limits
            .insert("pids".into(), CONTAINER_PIDS_LIMIT.to_string());
        evidence
            .limits
            .insert("capabilities".into(), CONTAINER_CAPABILITIES.join(","));
        evidence
            .limits
            .insert("no_new_privileges".into(), "true".into());
        Some(evidence)
    }
}

fn remove_workspace(path: &Path) -> Result<(), ProviderError> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(crate::transport_error(error)),
    }
}
