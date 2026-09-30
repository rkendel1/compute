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
        }
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
        ];
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
        for (key, value) in &command.env {
            if key.starts_with("COMPUTE_SESSION_") {
                return Err(ProviderError::new(
                    ProviderErrorKind::ArtifactInvalid,
                    format!("{key} is set by Compute"),
                ));
            }
            arguments.extend(["--env".into(), format!("{key}={value}")]);
        }
        arguments.push(container.to_owned());
        arguments.extend(command.command.iter().cloned());
        let mut direct = SessionCommand::new(arguments);
        direct.timeout = command.timeout;
        shell_request(environment, &direct)
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
}

fn remove_workspace(path: &Path) -> Result<(), ProviderError> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(crate::transport_error(error)),
    }
}
