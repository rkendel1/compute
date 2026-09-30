//! Compute sessions on a provider: the session contract that environment
//! providers implement, the built-in workspace provider, and the durable
//! session manager.
//!
//! The manager owns the lifecycle. A provider only provisions, reaches, and
//! tears down environments; it never decides who may use them, and its
//! identifiers never authorize anything. Every command run in a session is an
//! ordinary durable job, accepted through the same path as a remote
//! submission, so sessions add no second execution lifecycle.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use compute_core::{
    BundleInput, ComputeSession, ExecutionJob, IsolationProfile, IsolationRequirement, JobId,
    JobStatus, NetworkPolicy, ResourceLimits, RuntimeKind, SESSION_VERSION, SessionCapabilities,
    SessionCommand, SessionConnection, SessionConnectionGrant, SessionConnectionMode,
    SessionEndpoint, SessionEndpointRequest, SessionEvent, SessionExecSubmission, SessionExecution,
    SessionExecutionLogs, SessionFailure, SessionId, SessionLogs, SessionOwnership, SessionPhase,
    SessionResources, SessionSpec, SessionStatus, WORKLOAD_SPEC_VERSION, WorkloadBundle,
    WorkloadSpec,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use tokio::sync::Mutex;

use crate::jobs::{Acceptance, JobManager, ReservedJob};
use crate::{
    ComputeProvider, ProviderAuthorizer, ProviderError, ProviderErrorKind, ProviderRequest,
    artifact_error, error_code, transport_error,
};

/// The readiness check every environment runs before it is `ready`. Its
/// execution is the session's durable job and its receipt is the evidence
/// that the environment ran under admission.
pub const SESSION_READINESS_SCRIPT: &str = "printf 'compute-session-ready\\n'\n";

/// Sessions live for an hour unless the caller asks otherwise.
pub const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(60 * 60);

/// How often expiry and interrupted transitions are reconciled.
pub const DEFAULT_SESSION_SWEEP: Duration = Duration::from_secs(1);
/// Placement feature advertised only when session processes can execute a
/// path in the target's verified host runtime store.
pub const RUNTIME_STORE_SESSION_FEATURE: &str = "runtime_store_visible";

/// What a caller sends to create a session: the environment's execution
/// contract as an ordinary provider request, and the session's own terms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCreateRequest {
    pub request: ProviderRequest,
    #[serde(default)]
    pub spec: SessionSpec,
}

/// The environment a session asks for, before it is placed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionEnvironmentSpec {
    pub resources: SessionResources,
    pub network: NetworkPolicy,
    pub isolation: IsolationProfile,
    /// Required architecture (`x86_64`, `arm64`), when it matters.
    pub architecture: Option<String>,
}

impl SessionCreateRequest {
    /// A request for an environment with these resources, network, and
    /// isolation. The environment's contract is a canonical shell workload
    /// whose entrypoint is the readiness check, so placement, policy, and
    /// admission evaluate a session exactly as they evaluate any workload.
    pub fn new(
        environment: &SessionEnvironmentSpec,
        spec: SessionSpec,
    ) -> Result<Self, ProviderError> {
        let bundle = environment_bundle(environment)?;
        let mut request = ProviderRequest::bundle(bundle.to_bytes().map_err(artifact_error)?);
        request.expected.workload_id = Some(bundle.workload_id().map_err(artifact_error)?);
        request.expected.bundle_id = Some(bundle.bundle_id().map_err(artifact_error)?);
        request.execution.isolation = Some(environment.isolation);
        Ok(Self { request, spec })
    }

    /// The environment's contract bundle, which placement evaluates.
    pub fn environment(&self) -> Result<WorkloadBundle, ProviderError> {
        self.request.artifact.bundle()
    }
}

/// The canonical contract bundle of a session environment.
pub fn environment_bundle(
    environment: &SessionEnvironmentSpec,
) -> Result<WorkloadBundle, ProviderError> {
    let mut bundle = shell_bundle(
        "session.sh",
        SESSION_READINESS_SCRIPT,
        vec![],
        BTreeMap::new(),
        session_limits(&environment.resources, None),
        environment.network.clone(),
        environment.isolation,
    )?;
    if let Some(architecture) = &environment.architecture {
        bundle.workload.architecture = Some(architecture.clone());
        bundle.validate().map_err(artifact_error)?;
    }
    Ok(bundle)
}

fn session_limits(resources: &SessionResources, timeout: Option<Duration>) -> ResourceLimits {
    ResourceLimits {
        cpu_count: resources.cpu_count,
        memory_required_bytes: resources.memory_bytes,
        disk_bytes: resources.disk_bytes,
        wall_time: timeout,
        ..ResourceLimits::default()
    }
}

fn shell_bundle(
    entrypoint: &str,
    script: &str,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    resources: ResourceLimits,
    network: NetworkPolicy,
    isolation: IsolationProfile,
) -> Result<WorkloadBundle, ProviderError> {
    let bundle = WorkloadBundle {
        version: compute_core::WORKLOAD_BUNDLE_VERSION,
        workload: WorkloadSpec {
            version: WORKLOAD_SPEC_VERSION.into(),
            runtime: RuntimeKind::Shell,
            runtime_version: None,
            architecture: None,
            entrypoint: entrypoint.into(),
            args,
            env,
            inputs: vec![],
            outputs: vec![],
            resources,
            network,
            isolation: IsolationRequirement {
                profile: isolation,
                ..IsolationRequirement::default()
            },
            dependencies: None,
        },
        entrypoint: BundleInput {
            path: entrypoint.into(),
            data: script.as_bytes().to_vec(),
        },
        inputs: vec![],
        dependency_capsule: None,
    };
    bundle.validate().map_err(artifact_error)?;
    Ok(bundle)
}

/// A provider request that runs `command` in `directory` of an environment
/// this node executes directly. Providers whose environments live on this
/// node (a workspace, a local VM with a shared filesystem, a test double)
/// build their executions with it.
pub fn command_in_directory(
    environment: &SessionEnvironment,
    directory: &Path,
    command: &SessionCommand,
) -> Result<ProviderRequest, ProviderError> {
    const SCRIPT: &str = "cd \"$COMPUTE_SESSION_WORKSPACE\" || {\n  printf 'compute: the session workspace is unavailable\\n' >&2\n  exit 125\n}\nexec \"$@\"\n";
    let directory = directory.display().to_string();
    session_request(
        environment,
        command,
        SCRIPT,
        [
            ("HOME".to_owned(), directory.clone()),
            ("COMPUTE_SESSION_WORKSPACE".to_owned(), directory),
        ],
    )
}

/// A provider request that runs `command` on this node as it is. Providers
/// whose environments are entered through a command on the node (a
/// container runtime's `exec`, a VM agent) build their executions with it.
pub fn shell_request(
    environment: &SessionEnvironment,
    command: &SessionCommand,
) -> Result<ProviderRequest, ProviderError> {
    session_request(environment, command, "exec \"$@\"\n", [])
}

fn session_request(
    environment: &SessionEnvironment,
    command: &SessionCommand,
    script: &str,
    extra: impl IntoIterator<Item = (String, String)>,
) -> Result<ProviderRequest, ProviderError> {
    command.validate().map_err(|error| {
        ProviderError::new(ProviderErrorKind::ArtifactInvalid, error.to_string())
    })?;
    let mut env = BTreeMap::from([
        ("PATH".to_owned(), "/usr/local/bin:/usr/bin:/bin".to_owned()),
        (
            "COMPUTE_SESSION_ID".to_owned(),
            environment.session_id.to_string(),
        ),
    ]);
    env.extend(extra);
    for (key, value) in &command.env {
        if key.starts_with("COMPUTE_SESSION_") {
            return Err(ProviderError::new(
                ProviderErrorKind::ArtifactInvalid,
                format!("{key} is set by Compute"),
            ));
        }
        env.insert(key.clone(), value.clone());
    }
    let bundle = shell_bundle(
        "session-exec.sh",
        script,
        command.command.clone(),
        env,
        session_limits(&environment.resources, command.timeout),
        environment.network.clone(),
        environment.isolation,
    )?;
    let mut request = ProviderRequest::bundle(bundle.to_bytes().map_err(artifact_error)?);
    request.expected.workload_id = Some(bundle.workload_id().map_err(artifact_error)?);
    request.expected.bundle_id = Some(bundle.bundle_id().map_err(artifact_error)?);
    request.execution.isolation = Some(environment.isolation);
    request.execution.process_runtime = command.runtime.clone();
    Ok(request)
}

/// What a provider is asked to provision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionRequest {
    /// Compute's session identity. Providers use it to make provisioning
    /// idempotent: provisioning the same session twice yields one
    /// environment.
    pub session_id: SessionId,
    pub resources: SessionResources,
    pub network: NetworkPolicy,
    pub isolation: IsolationProfile,
    pub endpoints: Vec<SessionEndpointRequest>,
    pub expires_at: Option<chrono::DateTime<Utc>>,
}

/// A provisioned environment, as the provider reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionedSession {
    pub provider_session_id: String,
    pub connection: SessionConnection,
    pub endpoints: Vec<SessionEndpoint>,
    /// What this environment actually supports.
    pub capabilities: SessionCapabilities,
}

/// What a provider says about an environment it provisioned. Advisory: the
/// session record decides the lifecycle, and a report never revives a
/// session that has ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentState {
    Provisioning,
    Ready,
    Stopped,
    /// The provider no longer has the environment.
    Missing,
}

/// A handle the manager passes to a provider for an existing environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEnvironment {
    pub session_id: SessionId,
    pub provider_session_id: String,
    pub resources: SessionResources,
    pub network: NetworkPolicy,
    pub isolation: IsolationProfile,
}

/// Connection material a provider issues for one connection.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProviderConnection {
    pub connection: Option<SessionConnection>,
    pub command: Vec<String>,
    pub credentials: BTreeMap<String, String>,
    pub expires_at: Option<chrono::DateTime<Utc>>,
}

/// The session contract an environment provider implements.
///
/// Apple Container, a cloud VM, a local workspace, or a WASM runtime can each
/// implement it; none of them appears in the Compute API. Operations a
/// provider does not offer keep their default, which fails with
/// `operation_unsupported`, and its [`SessionProvider::capabilities`] must
/// say so. The manager checks capabilities before calling, so the defaults
/// are a second line of defence.
#[async_trait]
pub trait SessionProvider: Send + Sync {
    /// Implementation name, recorded for operators. Never used for
    /// decisions: capabilities are.
    fn kind(&self) -> String;

    /// What this provider's environments support.
    fn capabilities(&self) -> SessionCapabilities;

    /// Create (or find, for a session already provisioned) the environment.
    async fn provision(
        &self,
        request: &ProvisionRequest,
    ) -> Result<ProvisionedSession, ProviderError>;

    async fn inspect(&self, provider_session_id: &str) -> Result<EnvironmentState, ProviderError>;

    /// Prepare a command to run inside the environment. The manager runs the
    /// returned request as a durable job; the provider decides only how the
    /// command reaches its environment.
    async fn exec(
        &self,
        environment: &SessionEnvironment,
        command: &SessionCommand,
    ) -> Result<ProviderRequest, ProviderError>;

    /// Tear the environment down. Destroying an environment that is already
    /// gone succeeds.
    async fn destroy(&self, provider_session_id: &str) -> Result<(), ProviderError>;

    async fn connect(
        &self,
        environment: &SessionEnvironment,
    ) -> Result<ProviderConnection, ProviderError> {
        let _ = environment;
        Err(unsupported(&self.kind(), "connect"))
    }

    /// The environment's own log, beyond the executions Compute records.
    async fn logs(&self, provider_session_id: &str) -> Result<Option<String>, ProviderError> {
        let _ = provider_session_id;
        Ok(None)
    }

    async fn stop(&self, provider_session_id: &str) -> Result<(), ProviderError> {
        let _ = provider_session_id;
        Err(unsupported(&self.kind(), "stop"))
    }

    /// Resume the same environment. A provider that cannot must fail; it
    /// must never substitute a new environment.
    async fn resume(&self, provider_session_id: &str) -> Result<(), ProviderError> {
        let _ = provider_session_id;
        Err(unsupported(&self.kind(), "resume"))
    }

    async fn claim(&self, provider_session_id: &str) -> Result<(), ProviderError> {
        let _ = provider_session_id;
        Err(unsupported(&self.kind(), "claim"))
    }
}

/// The explicit error for an operation a provider does not offer.
pub fn unsupported(provider: &str, operation: &str) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::OperationUnsupported,
        format!("session provider {provider} does not support {operation}"),
    )
}

/// How long a cancelled execution may take to be recorded terminal before
/// stopping or destroying its session fails with `termination_failed`.
const EXECUTION_TERMINATION_DEADLINE: Duration = Duration::from_secs(20);

/// Environments on this node: a private workspace directory per session,
/// with commands run as durable jobs by the node's own runtimes. It is the
/// provider `compute serve` offers; it is not a security boundary beyond the
/// isolation profile the session asks for.
pub struct WorkspaceSessionProvider {
    root: PathBuf,
}

impl WorkspaceSessionProvider {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn directory(&self, provider_session_id: &str) -> Result<PathBuf, ProviderError> {
        let valid = provider_session_id
            .strip_prefix("wks_")
            .is_some_and(|digest| {
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            });
        if !valid {
            return Err(ProviderError::new(
                ProviderErrorKind::UnknownSession,
                "malformed workspace identity",
            ));
        }
        Ok(self.root.join(provider_session_id))
    }
}

#[async_trait]
impl SessionProvider for WorkspaceSessionProvider {
    fn kind(&self) -> String {
        "workspace".into()
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
            process_tree_termination: crate::processes::ownership_scan_available(),
        }
    }

    async fn provision(
        &self,
        request: &ProvisionRequest,
    ) -> Result<ProvisionedSession, ProviderError> {
        if !request.endpoints.is_empty() {
            return Err(unsupported(&self.kind(), "endpoints"));
        }
        let provider_session_id = format!(
            "wks_{:x}",
            Sha256::digest(format!("workspace:{}", request.session_id).as_bytes())
        );
        let directory = self.directory(&provider_session_id)?;
        fs::create_dir_all(&directory).map_err(transport_error)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
                .map_err(transport_error)?;
        }
        let mut capabilities = self.capabilities();
        capabilities.network = request.network != NetworkPolicy::None;
        Ok(ProvisionedSession {
            provider_session_id,
            connection: SessionConnection {
                mode: SessionConnectionMode::Exec,
                address: None,
                port: None,
                details: BTreeMap::new(),
            },
            endpoints: vec![],
            capabilities,
        })
    }

    async fn inspect(&self, provider_session_id: &str) -> Result<EnvironmentState, ProviderError> {
        Ok(if self.directory(provider_session_id)?.is_dir() {
            EnvironmentState::Ready
        } else {
            EnvironmentState::Missing
        })
    }

    async fn exec(
        &self,
        environment: &SessionEnvironment,
        command: &SessionCommand,
    ) -> Result<ProviderRequest, ProviderError> {
        let directory = self.directory(&environment.provider_session_id)?;
        command_in_directory(environment, &directory, command)
    }

    async fn connect(
        &self,
        environment: &SessionEnvironment,
    ) -> Result<ProviderConnection, ProviderError> {
        self.directory(&environment.provider_session_id)?;
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
        // Executions are cancelled by the manager; the workspace is kept.
        // What the environment left running (a service started detached, a
        // descendant that outlived its job) is terminated and confirmed
        // gone: a stopped environment has no processes, only state.
        let directory = self.directory(provider_session_id)?;
        crate::processes::terminate_owned(&directory).await?;
        Ok(())
    }

    async fn resume(&self, provider_session_id: &str) -> Result<(), ProviderError> {
        if self.directory(provider_session_id)?.is_dir() {
            Ok(())
        } else {
            Err(ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                "the workspace no longer exists; it is not recreated",
            ))
        }
    }

    async fn destroy(&self, provider_session_id: &str) -> Result<(), ProviderError> {
        // Terminate first, and remove nothing until every owned process is
        // confirmed gone: a workspace is never deleted out from under
        // running workloads, and a survivor is reported, not hidden.
        let directory = self.directory(provider_session_id)?;
        crate::processes::terminate_owned(&directory).await?;
        match fs::remove_dir_all(directory) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(transport_error(error)),
        }
    }

    async fn claim(&self, provider_session_id: &str) -> Result<(), ProviderError> {
        self.directory(provider_session_id)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredSessionRequest {
    owner: String,
    request: ProviderRequest,
    spec: SessionSpec,
}

/// Durable sessions on one node.
pub struct SessionManager {
    root: PathBuf,
    jobs: Arc<JobManager>,
    provider: Arc<dyn SessionProvider>,
    compute: Arc<dyn ComputeProvider>,
    authorizer: Arc<dyn ProviderAuthorizer>,
    mutation: Mutex<()>,
    /// Working set: sessions that are not terminal. Rebuilt from the store
    /// at start; never an authority.
    live: std::sync::Mutex<BTreeSet<SessionId>>,
    /// Sessions a task is currently advancing.
    driving: std::sync::Mutex<BTreeSet<SessionId>>,
    /// Session executions a task is watching.
    watching: std::sync::Mutex<BTreeSet<JobId>>,
    sweep: Duration,
}

impl SessionManager {
    pub(crate) fn start(
        root: PathBuf,
        jobs: Arc<JobManager>,
        provider: Arc<dyn SessionProvider>,
        compute: Arc<dyn ComputeProvider>,
        authorizer: Arc<dyn ProviderAuthorizer>,
        sweep: Duration,
    ) -> Result<Arc<Self>, ProviderError> {
        fs::create_dir_all(&root).map_err(transport_error)?;
        let manager = Arc::new(Self {
            root,
            jobs,
            provider,
            compute,
            authorizer,
            mutation: Mutex::new(()),
            live: std::sync::Mutex::new(BTreeSet::new()),
            driving: std::sync::Mutex::new(BTreeSet::new()),
            watching: std::sync::Mutex::new(BTreeSet::new()),
            sweep,
        });
        // Recovery: every session that was not terminal is reconciled.
        for entry in fs::read_dir(&manager.root).map_err(transport_error)? {
            let entry = entry.map_err(transport_error)?;
            let Ok(session_id) = SessionId::parse(entry.file_name().to_string_lossy().into_owned())
            else {
                continue;
            };
            if let Ok(session) = manager.read_session(&session_id)
                && !session.status.is_terminal()
            {
                manager.live().insert(session_id.clone());
                let recovering = manager.clone();
                tokio::spawn(async move { recovering.recover(session_id).await });
            }
        }
        let sweeper = Arc::downgrade(&manager);
        tokio::spawn(async move { sweep_sessions(sweeper).await });
        Ok(manager)
    }

    /// What this node's sessions support.
    pub fn capabilities(&self) -> SessionCapabilities {
        self.provider.capabilities()
    }

    pub fn provider_kind(&self) -> String {
        self.provider.kind()
    }

    pub async fn create(
        self: &Arc<Self>,
        create: SessionCreateRequest,
        owner: String,
    ) -> Result<ComputeSession, ProviderError> {
        let SessionCreateRequest { request, spec } = create;
        request.validate()?;
        let bundle = request.artifact.bundle()?;
        bundle
            .require_ids(
                request.expected.workload_id.as_deref(),
                request.expected.bundle_id.as_deref(),
            )
            .map_err(artifact_error)?;
        SessionCapabilities::validate_names(&spec.required_capabilities).map_err(|error| {
            ProviderError::new(ProviderErrorKind::CapabilityMismatch, error.to_string())
        })?;
        let offered = self.provider.capabilities();
        let mut required = spec.required_capabilities.clone();
        if bundle.workload.network != NetworkPolicy::None {
            required.push("network".into());
        }
        if spec.endpoints.iter().any(|endpoint| endpoint.public) {
            required.push("public_endpoint".into());
        }
        required.sort();
        required.dedup();
        let missing = offered.missing(&required);
        if !missing.is_empty() {
            return Err(ProviderError::new(
                ProviderErrorKind::OperationUnsupported,
                format!(
                    "session provider {} does not offer {}",
                    self.provider.kind(),
                    missing.join(", ")
                ),
            ));
        }
        if spec.persistent {
            if spec.ttl_seconds.is_some() {
                return Err(ProviderError::new(
                    ProviderErrorKind::PolicyRejected,
                    "a persistent session has no TTL",
                ));
            }
            if !offered.claim {
                return Err(unsupported(&self.provider.kind(), "persistent sessions"));
            }
        }
        if let Some(reference) = &spec.reference {
            if !compute_core::valid_session_reference(reference) {
                return Err(ProviderError::new(
                    ProviderErrorKind::PolicyRejected,
                    "a session reference is 1-128 letters, digits, and .:_-",
                ));
            }
            // The same reference: the same session, never a second one.
            if let Some(existing) = self.find_reference(&owner, reference)? {
                return Ok(existing);
            }
        }
        let ttl_seconds = match spec.ttl_seconds {
            Some(0) => {
                return Err(ProviderError::new(
                    ProviderErrorKind::PolicyRejected,
                    "a session TTL must be greater than zero",
                ));
            }
            Some(ttl) => ttl,
            None => DEFAULT_SESSION_TTL.as_secs(),
        };
        // Policy decides whether this environment may exist here, before
        // anything is provisioned. Admission fails closed.
        let admission = self.compute.admit(request.clone()).await?;
        if !admission.decision.admitted {
            return Err(ProviderError::denied(admission.decision));
        }
        let now = Utc::now();
        let session_id = SessionId::generate();
        let identity = self.compute.identity();
        let node_id = request
            .execution
            .placement
            .as_ref()
            .map(|placement| placement.provider_id.clone())
            .unwrap_or_else(|| match &identity {
                compute_core::ProviderIdentity::Local { id }
                | compute_core::ProviderIdentity::Remote { id, .. } => id.clone(),
            });
        let resources = &bundle.workload.resources;
        let session = ComputeSession {
            version: SESSION_VERSION.into(),
            session_id: session_id.clone(),
            status: SessionStatus::Requested,
            node_id,
            provider: identity,
            provider_kind: self.provider.kind(),
            provider_session_id: None,
            job_id: JobId::generate(),
            execution_id: compute_core::new_execution_id(),
            owner: owner.clone(),
            ownership: if spec.persistent {
                SessionOwnership::Claimed
            } else {
                SessionOwnership::Ephemeral
            },
            resources: SessionResources {
                cpu_count: resources.cpu_count,
                memory_bytes: resources.memory_required_bytes.or(resources.memory_bytes),
                disk_bytes: resources.disk_bytes,
            },
            network: bundle.workload.network.clone(),
            capabilities: offered,
            required_capabilities: spec.required_capabilities.clone(),
            reference: spec.reference.clone(),
            connection: None,
            endpoints: vec![],
            requested_endpoints: spec.endpoints.clone(),
            placement_id: request
                .execution
                .placement
                .as_ref()
                .map(|placement| placement.placement_id.clone()),
            placement: request.execution.placement.clone(),
            admission: Some(admission.summary()),
            ttl_seconds: (!spec.persistent).then_some(ttl_seconds),
            created_at: now,
            updated_at: now,
            expires_at: (!spec.persistent).then(|| {
                now + chrono::Duration::seconds(i64::try_from(ttl_seconds).unwrap_or(i64::MAX))
            }),
            ready_at: None,
            ended_at: None,
            generation: 1,
            failure: None,
            executions: vec![],
        };
        // The session is durable before any provider is asked for anything.
        {
            let _guard = self.mutation.lock().await;
            // A concurrent creation with the same reference won.
            if let Some(reference) = &session.reference
                && let Some(existing) = self.find_reference(&session.owner, reference)?
            {
                return Ok(existing);
            }
            let staging = tempfile::Builder::new()
                .prefix(".compute-session-")
                .tempdir_in(&self.root)
                .map_err(transport_error)?;
            write_json(
                &staging.path().join("request.json"),
                &StoredSessionRequest {
                    owner,
                    request,
                    spec,
                },
            )?;
            write_json(&staging.path().join("session.json"), &session)?;
            write_json(
                &staging.path().join("events.json"),
                &vec![event(&session, 1, "requested", None)],
            )?;
            fs::rename(staging.keep(), self.directory(&session_id)).map_err(transport_error)?;
        }
        self.live().insert(session_id.clone());
        self.drive(session_id);
        Ok(session)
    }

    /// The owner's live session with this reference.
    fn find_reference(
        &self,
        owner: &str,
        reference: &str,
    ) -> Result<Option<ComputeSession>, ProviderError> {
        for entry in fs::read_dir(&self.root).map_err(transport_error)? {
            let entry = entry.map_err(transport_error)?;
            let Ok(session_id) = SessionId::parse(entry.file_name().to_string_lossy().into_owned())
            else {
                continue;
            };
            if let Ok(session) = self.read_session(&session_id)
                && session.owner == owner
                && session.reference.as_deref() == Some(reference)
                && !session.status.is_terminal()
            {
                return Ok(Some(session));
            }
        }
        Ok(None)
    }

    pub async fn list(&self, owner: &str) -> Result<Vec<ComputeSession>, ProviderError> {
        let mut sessions = vec![];
        for entry in fs::read_dir(&self.root).map_err(transport_error)? {
            let entry = entry.map_err(transport_error)?;
            let Ok(session_id) = SessionId::parse(entry.file_name().to_string_lossy().into_owned())
            else {
                continue;
            };
            if let Ok(session) = self.read_session(&session_id)
                && session.owner == owner
            {
                sessions.push(session);
            }
        }
        sessions.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| left.session_id.cmp(&right.session_id))
        });
        Ok(sessions)
    }

    /// The authoritative session record. A session that should have a
    /// machine is checked against its provider first: one the provider no
    /// longer has is recorded as lost (failed, `environment_lost`), never
    /// reported as ready and never recreated.
    pub async fn inspect(
        &self,
        session_id: &SessionId,
        owner: &str,
    ) -> Result<ComputeSession, ProviderError> {
        let session = self.owned(session_id, owner)?;
        if matches!(
            session.status,
            SessionStatus::Ready | SessionStatus::Running | SessionStatus::Stopped
        ) && let Some(provider_session_id) = &session.provider_session_id
            && let Ok(EnvironmentState::Missing) = self.provider.inspect(provider_session_id).await
        {
            let failure = SessionFailure {
                phase: SessionPhase::Reconciliation,
                provider: Some(session.provider_kind.clone()),
                code: "environment_lost".into(),
                message: "the provider no longer has this environment".into(),
                retryable: false,
                at: Utc::now(),
            };
            // Another request may have settled it first; either way the
            // record now says what is true.
            let _ = self.fail(&session, failure).await;
            return self.owned(session_id, owner);
        }
        Ok(session)
    }

    pub async fn events(
        &self,
        session_id: &SessionId,
        owner: &str,
    ) -> Result<Vec<SessionEvent>, ProviderError> {
        self.owned(session_id, owner)?;
        read_json(&self.directory(session_id).join("events.json"))
    }

    pub async fn connect(
        &self,
        session_id: &SessionId,
        owner: &str,
    ) -> Result<SessionConnectionGrant, ProviderError> {
        let session = self.usable(session_id, owner, "connect")?;
        let environment = self.environment(&session)?;
        let issued = self
            .provider
            .connect(&environment)
            .await
            .map_err(|error| self.provider_error(error))?;
        let connection = issued
            .connection
            .or(session.connection.clone())
            .ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::ProviderUnavailable,
                    "the provider reported no connection for this session",
                )
            })?;
        // The event names the mode only: connection material is never
        // written down.
        self.record_event(
            session_id,
            "connected",
            Some(connection.mode.as_str().into()),
        )
        .await?;
        Ok(SessionConnectionGrant {
            session_id: session_id.clone(),
            connection,
            command: issued.command,
            credentials: issued.credentials,
            expires_at: issued.expires_at,
        })
    }

    /// Run a command in the session as a durable job.
    pub async fn exec(
        self: &Arc<Self>,
        session_id: &SessionId,
        owner: &str,
        command: SessionCommand,
    ) -> Result<SessionExecSubmission, ProviderError> {
        command.validate().map_err(|error| {
            ProviderError::new(ProviderErrorKind::ArtifactInvalid, error.to_string())
        })?;
        let session = self.usable(session_id, owner, "exec")?;
        if !session.capabilities.exec {
            return Err(unsupported(&session.provider_kind, "exec"));
        }
        let environment = self.environment(&session)?;
        let request = self
            .provider
            .exec(&environment, &command)
            .await
            .map_err(|error| self.provider_error(error))?;
        let execution = SessionExecution {
            job_id: JobId::generate(),
            execution_id: compute_core::new_execution_id(),
            purpose: "exec".into(),
            command: command.command.clone(),
            submitted_at: Utc::now(),
            status: JobStatus::Queued,
        };
        // Record the intent first: a restart between here and acceptance
        // leaves an execution the session can account for.
        self.mutate(session_id, None, "exec_requested", |session| {
            if !session.status.is_usable() {
                return Err(conflict(format!(
                    "session is {}; it cannot run commands",
                    session.status
                )));
            }
            session.executions.push(execution.clone());
            session.status = SessionStatus::Running;
            Ok(())
        })
        .await?;
        let submission = self.accept(&session, request, &execution).await;
        match submission {
            Ok(status) => {
                self.watch(session_id.clone(), execution.job_id.clone());
                Ok(SessionExecSubmission {
                    session_id: session_id.clone(),
                    job_id: execution.job_id,
                    execution_id: execution.execution_id,
                    status,
                })
            }
            Err(error) => {
                let _ = self
                    .settle_execution(session_id, &execution.job_id, JobStatus::Rejected)
                    .await;
                Err(error)
            }
        }
    }

    pub async fn logs(
        &self,
        session_id: &SessionId,
        owner: &str,
    ) -> Result<SessionLogs, ProviderError> {
        let session = self.owned(session_id, owner)?;
        let mut executions = vec![];
        for execution in &session.executions {
            let (logs, status) = match self.jobs.logs(&execution.job_id, owner).await {
                Ok(logs) => {
                    let status = self
                        .jobs
                        .status(&execution.job_id, owner)
                        .await
                        .map(|job| job.status)
                        .unwrap_or(execution.status);
                    (logs, status)
                }
                // Evidence past retention, or an execution that was never
                // accepted: the session still says it existed.
                Err(_) => (
                    compute_core::JobLogs {
                        job_id: execution.job_id.clone(),
                        stdout: String::new(),
                        stderr: String::new(),
                        complete: execution.status.is_terminal(),
                    },
                    execution.status,
                ),
            };
            executions.push(SessionExecutionLogs {
                job_id: execution.job_id.clone(),
                execution_id: execution.execution_id.clone(),
                purpose: execution.purpose.clone(),
                command: execution.command.clone(),
                status,
                stdout: logs.stdout,
                stderr: logs.stderr,
                complete: logs.complete,
            });
        }
        let environment = match (&session.provider_session_id, session.status.is_terminal()) {
            (Some(provider_session_id), false) => self
                .provider
                .logs(provider_session_id)
                .await
                .map_err(|error| self.provider_error(error))?,
            _ => None,
        };
        Ok(SessionLogs {
            session_id: session_id.clone(),
            executions,
            environment,
        })
    }

    /// Stop active executions, keeping the environment and the record.
    pub async fn stop(
        self: &Arc<Self>,
        session_id: &SessionId,
        owner: &str,
    ) -> Result<ComputeSession, ProviderError> {
        let session = self.owned(session_id, owner)?;
        match session.status {
            SessionStatus::Stopped | SessionStatus::Stopping => {}
            status if status.is_usable() => {
                self.mutate(session_id, None, "stopping", |session| {
                    if !session.status.is_usable() {
                        return Err(conflict(format!(
                            "a {} session cannot be stopped",
                            session.status
                        )));
                    }
                    session.status = SessionStatus::Stopping;
                    Ok(())
                })
                .await?;
            }
            status => {
                return Err(conflict(format!("a {status} session cannot be stopped")));
            }
        }
        self.advance_owned(session_id).await
    }

    /// Resume the same environment. Never recreates one.
    pub async fn resume(
        self: &Arc<Self>,
        session_id: &SessionId,
        owner: &str,
    ) -> Result<ComputeSession, ProviderError> {
        let session = self.owned(session_id, owner)?;
        if !session.capabilities.resume {
            return Err(unsupported(&session.provider_kind, "resume"));
        }
        match session.status {
            SessionStatus::Resuming => {}
            SessionStatus::Stopped => {
                self.mutate(session_id, None, "resuming", |session| {
                    if session.status != SessionStatus::Stopped {
                        return Err(conflict(format!(
                            "a {} session cannot be resumed",
                            session.status
                        )));
                    }
                    session.status = SessionStatus::Resuming;
                    Ok(())
                })
                .await?;
            }
            status => {
                return Err(conflict(format!("a {status} session cannot be resumed")));
            }
        }
        let resumed = self.advance_owned(session_id).await?;
        if resumed.status == SessionStatus::Stopped {
            // The provider refused; the failure is recorded on the session.
            let failure = resumed.failure.clone();
            return Err(ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                failure
                    .map(|failure| failure.message)
                    .unwrap_or_else(|| "the provider could not resume the session".into()),
            ));
        }
        Ok(resumed)
    }

    /// Tear the environment down. The record remains as evidence.
    pub async fn destroy(
        self: &Arc<Self>,
        session_id: &SessionId,
        owner: &str,
    ) -> Result<ComputeSession, ProviderError> {
        let session = self.owned(session_id, owner)?;
        if session.status.is_terminal() {
            return Ok(session);
        }
        if !matches!(
            session.status,
            SessionStatus::Destroying | SessionStatus::Expiring
        ) {
            self.mutate(session_id, None, "destroying", |session| {
                session.status = SessionStatus::Destroying;
                Ok(())
            })
            .await?;
        }
        self.advance_owned(session_id).await
    }

    /// Move an ephemeral session to persistent ownership. The owner does not
    /// change; the session stops expiring.
    pub async fn claim(
        self: &Arc<Self>,
        session_id: &SessionId,
        owner: &str,
    ) -> Result<ComputeSession, ProviderError> {
        let session = self.owned(session_id, owner)?;
        if !session.capabilities.claim {
            return Err(unsupported(&session.provider_kind, "claim"));
        }
        if session.ownership == SessionOwnership::Claimed {
            return Ok(session);
        }
        if !(session.status.is_usable() || session.status == SessionStatus::Stopped) {
            return Err(conflict(format!(
                "a {} session cannot be claimed",
                session.status
            )));
        }
        if session.is_expired_at(Utc::now()) {
            return Err(conflict("the session has expired".into()));
        }
        let provider_session_id = session
            .provider_session_id
            .clone()
            .ok_or_else(|| conflict("the session has no environment to claim".into()))?;
        if let Err(error) = self.provider.claim(&provider_session_id).await {
            let failure = self.failure(SessionPhase::Claim, &error);
            let _ = self
                .mutate(session_id, None, "claim_failed", |session| {
                    session.failure = Some(failure);
                    Ok(())
                })
                .await;
            return Err(self.provider_error(error));
        }
        self.mutate(session_id, None, "claimed", |session| {
            if session.is_expired_at(Utc::now()) {
                return Err(conflict("the session has expired".into()));
            }
            session.ownership = SessionOwnership::Claimed;
            session.expires_at = None;
            Ok(())
        })
        .await
    }

    // Lifecycle driving.

    fn drive(self: &Arc<Self>, session_id: SessionId) {
        if !self.driving().insert(session_id.clone()) {
            return;
        }
        let manager = self.clone();
        tokio::spawn(async move {
            let _ = manager.advance_owned(&session_id).await;
            manager.driving().remove(&session_id);
        });
    }

    /// Advance the session through in-flight states until it rests. Each
    /// step is persisted before the provider is asked, so a restart repeats
    /// at most one idempotent provider call.
    async fn advance_owned(
        self: &Arc<Self>,
        session_id: &SessionId,
    ) -> Result<ComputeSession, ProviderError> {
        loop {
            match self.step(session_id).await {
                Ok(Some(session)) => return Ok(session),
                Ok(None) => {}
                // Another task advanced the session first (a caller and the
                // sweep can both drive an in-flight state): read it again.
                Err(error) if is_stale(&error) => {}
                Err(error) => return Err(error),
            }
        }
    }

    /// One step: `Some` when the session rests, `None` to continue.
    async fn step(
        self: &Arc<Self>,
        session_id: &SessionId,
    ) -> Result<Option<ComputeSession>, ProviderError> {
        {
            let session = self.read_session(session_id)?;
            match session.status {
                SessionStatus::Requested => {
                    self.mutate(
                        session_id,
                        Some(session.generation),
                        "provisioning",
                        |session| {
                            session.status = SessionStatus::Provisioning;
                            Ok(())
                        },
                    )
                    .await?;
                }
                SessionStatus::Provisioning => self.provision(session).await?,
                SessionStatus::Stopping => {
                    if let Err(error) = self.terminate_executions(&session).await {
                        let failure = self.failure(SessionPhase::Stopping, &error);
                        self.record_failure(&session, "stop_failed", failure)
                            .await?;
                        return Err(self.provider_error(error));
                    }
                    if session.capabilities.suspend
                        && let Some(provider_session_id) = &session.provider_session_id
                        && let Err(error) = self.provider.stop(provider_session_id).await
                    {
                        let failure = self.failure(SessionPhase::Stopping, &error);
                        self.record_failure(&session, "stop_failed", failure)
                            .await?;
                        return Err(self.provider_error(error));
                    }
                    self.mutate(session_id, Some(session.generation), "stopped", |session| {
                        session.status = SessionStatus::Stopped;
                        Ok(())
                    })
                    .await?;
                }
                SessionStatus::Resuming => {
                    let provider_session_id =
                        session.provider_session_id.clone().ok_or_else(|| {
                            ProviderError::new(
                                ProviderErrorKind::EvidenceInvalid,
                                "a stopped session has no environment to resume",
                            )
                        })?;
                    match self.provider.resume(&provider_session_id).await {
                        Ok(()) => {
                            self.mutate(
                                session_id,
                                Some(session.generation),
                                "resumed",
                                |session| {
                                    session.status = SessionStatus::Ready;
                                    session.failure = None;
                                    Ok(())
                                },
                            )
                            .await?;
                        }
                        Err(error) => {
                            let failure = self.failure(SessionPhase::Resuming, &error);
                            return self
                                .mutate(
                                    session_id,
                                    Some(session.generation),
                                    "resume_failed",
                                    |session| {
                                        session.status = SessionStatus::Stopped;
                                        session.failure = Some(failure);
                                        Ok(())
                                    },
                                )
                                .await
                                .map(Some);
                        }
                    }
                }
                SessionStatus::Destroying | SessionStatus::Expiring => {
                    self.teardown(session).await?;
                }
                SessionStatus::Running => {
                    for execution in session.active_executions() {
                        self.watch(session_id.clone(), execution.job_id.clone());
                    }
                    return Ok(Some(session));
                }
                SessionStatus::Ready
                | SessionStatus::Stopped
                | SessionStatus::Destroyed
                | SessionStatus::Expired
                | SessionStatus::Failed => return Ok(Some(session)),
            }
        }
        Ok(None)
    }

    async fn provision(self: &Arc<Self>, session: ComputeSession) -> Result<(), ProviderError> {
        let session_id = session.session_id.clone();
        if session.provider_session_id.is_none() {
            let stored = self.read_request(&session_id)?;
            let bundle = stored.request.artifact.bundle()?;
            let request = ProvisionRequest {
                session_id: session_id.clone(),
                resources: session.resources.clone(),
                network: session.network.clone(),
                isolation: stored
                    .request
                    .execution
                    .isolation
                    .unwrap_or(bundle.workload.isolation.profile),
                endpoints: session.requested_endpoints.clone(),
                expires_at: session.expires_at,
            };
            let provisioned = match self.provider.provision(&request).await {
                Ok(provisioned) => provisioned,
                Err(error) => {
                    let failure = self.failure(SessionPhase::Provisioning, &error);
                    self.fail(&session, failure).await?;
                    return Ok(());
                }
            };
            let applied = self
                .mutate(
                    &session_id,
                    Some(session.generation),
                    "provisioned",
                    |session| {
                        session.provider_session_id = Some(provisioned.provider_session_id.clone());
                        session.connection = Some(provisioned.connection.clone());
                        session.endpoints = provisioned.endpoints.clone();
                        session.capabilities = provisioned.capabilities;
                        Ok(())
                    },
                )
                .await;
            if applied.is_err() {
                // The session moved on (it was destroyed, or expired) while
                // the provider worked. The environment it built belongs to
                // nothing: tear it down, never adopt it.
                let _ = self
                    .provider
                    .destroy(&provisioned.provider_session_id)
                    .await;
                let _ = self
                    .record_event(
                        &session_id,
                        "orphan_destroyed",
                        Some("a late provisioning response was discarded".into()),
                    )
                    .await;
            }
            return Ok(());
        }
        // Readiness: the session's own durable job, under reserved IDs.
        let owner = session.owner.clone();
        let readiness = SessionExecution {
            job_id: session.job_id.clone(),
            execution_id: session.execution_id.clone(),
            purpose: "provision".into(),
            command: vec![],
            submitted_at: Utc::now(),
            status: JobStatus::Queued,
        };
        if self.jobs.status(&session.job_id, &owner).await.is_err() {
            let environment = self.environment(&session)?;
            let request = match self
                .provider
                .exec(
                    &environment,
                    &SessionCommand::new(vec![
                        "sh".into(),
                        "-c".into(),
                        SESSION_READINESS_SCRIPT.into(),
                    ]),
                )
                .await
            {
                Ok(request) => request,
                Err(error) => {
                    let failure = self.failure(SessionPhase::Provisioning, &error);
                    self.fail(&session, failure).await?;
                    return Ok(());
                }
            };
            if !session
                .executions
                .iter()
                .any(|execution| execution.job_id == session.job_id)
            {
                self.mutate(
                    &session_id,
                    Some(session.generation),
                    "readiness_requested",
                    |session| {
                        session.executions.push(readiness.clone());
                        Ok(())
                    },
                )
                .await?;
            }
            if let Err(error) = self.accept(&session, request, &readiness).await {
                let failure = self.failure(SessionPhase::Provisioning, &error);
                self.fail(&session, failure).await?;
                return Ok(());
            }
        }
        let job = self.jobs.wait_terminal(&session.job_id, &owner).await?;
        let current = self.read_session(&session_id)?;
        self.settle_execution(&session_id, &job.job_id, job.status)
            .await?;
        if current.status != SessionStatus::Provisioning {
            return Ok(());
        }
        if job.status == JobStatus::Succeeded {
            self.mutate(&session_id, Some(current.generation), "ready", |session| {
                session.status = SessionStatus::Ready;
                session.ready_at = Some(Utc::now());
                Ok(())
            })
            .await?;
        } else {
            let (code, message) = job_failure(&job);
            let failure = SessionFailure {
                phase: if code == "admission_denied" {
                    SessionPhase::Admission
                } else {
                    SessionPhase::Provisioning
                },
                provider: None,
                retryable: code != "admission_denied",
                code,
                message,
                at: Utc::now(),
            };
            let current = self.read_session(&session_id)?;
            self.fail(&current, failure).await?;
        }
        Ok(())
    }

    /// Record a failure and tear down whatever the provider built.
    async fn fail(
        &self,
        session: &ComputeSession,
        failure: SessionFailure,
    ) -> Result<(), ProviderError> {
        let failed = self
            .mutate(
                &session.session_id,
                Some(session.generation),
                "failed",
                |session| {
                    session.status = SessionStatus::Failed;
                    session.failure = Some(failure);
                    session.ended_at = Some(Utc::now());
                    Ok(())
                },
            )
            .await?;
        self.cancel_executions(&failed).await;
        if let Some(provider_session_id) = &failed.provider_session_id {
            let outcome = self.provider.destroy(provider_session_id).await;
            let _ = self
                .record_event(
                    &failed.session_id,
                    "teardown",
                    Some(match outcome {
                        Ok(()) => "environment destroyed".into(),
                        Err(error) => format!("teardown failed: {error}"),
                    }),
                )
                .await;
        }
        Ok(())
    }

    async fn teardown(&self, session: ComputeSession) -> Result<(), ProviderError> {
        let expiring = session.status == SessionStatus::Expiring;
        let session_id = session.session_id.clone();
        if expiring && let Err(error) = self.authorizer.authorize_expiry(&session.owner).await {
            // Fail closed: nothing is torn down without authority. The
            // sweep retries.
            let failure = SessionFailure {
                phase: SessionPhase::Authorization,
                provider: None,
                code: error_code(error.kind),
                message: error.message.clone(),
                retryable: true,
                at: Utc::now(),
            };
            self.record_failure(&session, "expiry_denied", failure)
                .await?;
            return Err(error);
        }
        let phase = if expiring {
            SessionPhase::Expiration
        } else {
            SessionPhase::Teardown
        };
        // Executions are confirmed ended before the environment is touched:
        // a destroy is never reported over a command still running.
        if let Err(error) = self.terminate_executions(&session).await {
            let failure = self.failure(phase, &error);
            self.record_failure(&session, "teardown_failed", failure)
                .await?;
            return Err(self.provider_error(error));
        }
        if let Some(provider_session_id) = &session.provider_session_id
            && let Err(error) = self.provider.destroy(provider_session_id).await
        {
            let failure = self.failure(
                if expiring {
                    SessionPhase::Expiration
                } else {
                    SessionPhase::Teardown
                },
                &error,
            );
            self.record_failure(&session, "teardown_failed", failure)
                .await?;
            return Err(self.provider_error(error));
        }
        self.mutate(
            &session_id,
            Some(session.generation),
            if expiring { "expired" } else { "destroyed" },
            |session| {
                session.status = if expiring {
                    SessionStatus::Expired
                } else {
                    SessionStatus::Destroyed
                };
                session.failure = None;
                session.ended_at = Some(Utc::now());
                Ok(())
            },
        )
        .await?;
        Ok(())
    }

    /// Cancel every active execution and wait until each is recorded
    /// terminal. `Ok` means no command of this session is still running; a
    /// command that does not end is a `termination_failed`, not a success.
    async fn terminate_executions(&self, session: &ComputeSession) -> Result<(), ProviderError> {
        self.cancel_executions(session).await;
        for execution in session.active_executions() {
            let ended = tokio::time::timeout(
                EXECUTION_TERMINATION_DEADLINE,
                self.jobs.wait_terminal(&execution.job_id, &session.owner),
            )
            .await;
            match ended {
                Ok(Ok(_)) => {}
                // A job whose evidence is gone is not running.
                Ok(Err(error))
                    if matches!(
                        error.kind,
                        ProviderErrorKind::UnknownJob | ProviderErrorKind::JobExpired
                    ) => {}
                Ok(Err(error)) => return Err(error),
                Err(_) => {
                    return Err(ProviderError::new(
                        ProviderErrorKind::TerminationFailed,
                        format!(
                            "execution {} did not end within {}s of being cancelled",
                            execution.job_id,
                            EXECUTION_TERMINATION_DEADLINE.as_secs()
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    async fn cancel_executions(&self, session: &ComputeSession) {
        for execution in session.active_executions() {
            let _ = self.jobs.cancel(&execution.job_id, &session.owner).await;
        }
    }

    async fn accept(
        &self,
        session: &ComputeSession,
        mut request: ProviderRequest,
        execution: &SessionExecution,
    ) -> Result<JobStatus, ProviderError> {
        // Every execution in the session carries the session's placement, so
        // its receipt names the node the session was placed on.
        request.execution.placement = session.placement.clone();
        let submission = self
            .jobs
            .accept(Acceptance {
                request,
                owner: session.owner.clone(),
                idempotency_key: None,
                reserved: Some(ReservedJob {
                    job_id: execution.job_id.clone(),
                    execution_id: execution.execution_id.clone(),
                    session_id: Some(session.session_id.clone()),
                }),
            })
            .await?;
        Ok(submission.status)
    }

    fn watch(self: &Arc<Self>, session_id: SessionId, job_id: JobId) {
        if !self.watching().insert(job_id.clone()) {
            return;
        }
        let manager = self.clone();
        tokio::spawn(async move {
            let owner = manager
                .read_session(&session_id)
                .map(|session| session.owner);
            if let Ok(owner) = owner {
                let status = match manager.jobs.wait_terminal(&job_id, &owner).await {
                    Ok(job) => job.status,
                    Err(_) => JobStatus::Failed,
                };
                let _ = manager.settle_execution(&session_id, &job_id, status).await;
            }
            manager.watching().remove(&job_id);
        });
    }

    /// Record an execution's terminal status; a running session with no
    /// other active execution is ready again.
    async fn settle_execution(
        &self,
        session_id: &SessionId,
        job_id: &JobId,
        status: JobStatus,
    ) -> Result<ComputeSession, ProviderError> {
        let _guard = self.mutation.lock().await;
        let mut session = self.read_session(session_id)?;
        let Some(execution) = session
            .executions
            .iter_mut()
            .find(|execution| &execution.job_id == job_id)
        else {
            return Ok(session);
        };
        if execution.status == status {
            return Ok(session);
        }
        execution.status = status;
        let idle = session.active_executions().next().is_none();
        let mut event_type = "execution_settled";
        if session.status == SessionStatus::Running && idle {
            session.status = SessionStatus::Ready;
            session.generation += 1;
            event_type = "ready";
        }
        session.updated_at = Utc::now();
        self.write_session(&session)?;
        self.append_event(&session, event_type, Some(format!("{job_id}: {status:?}")))?;
        Ok(session)
    }

    async fn recover(self: Arc<Self>, session_id: SessionId) {
        let Ok(session) = self.read_session(&session_id) else {
            return;
        };
        // Executions recorded but never accepted cannot run now.
        for execution in session.active_executions() {
            if execution.job_id != session.job_id
                && self
                    .jobs
                    .status(&execution.job_id, &session.owner)
                    .await
                    .is_err()
            {
                let _ = self
                    .settle_execution(&session_id, &execution.job_id, JobStatus::Rejected)
                    .await;
            }
        }
        let Ok(session) = self.read_session(&session_id) else {
            return;
        };
        // An environment the provider lost is recorded, never recreated.
        if matches!(
            session.status,
            SessionStatus::Ready | SessionStatus::Running | SessionStatus::Stopped
        ) && let Some(provider_session_id) = &session.provider_session_id
            && let Ok(EnvironmentState::Missing) = self.provider.inspect(provider_session_id).await
        {
            let failure = SessionFailure {
                phase: SessionPhase::Reconciliation,
                provider: Some(session.provider_kind.clone()),
                code: "environment_lost".into(),
                message: "the provider no longer has this environment".into(),
                retryable: false,
                at: Utc::now(),
            };
            let _ = self.fail(&session, failure).await;
            return;
        }
        self.drive(session_id);
    }

    // Guards and helpers.

    fn owned(&self, session_id: &SessionId, owner: &str) -> Result<ComputeSession, ProviderError> {
        let session = self.read_session(session_id)?;
        if session.owner != owner {
            // Indistinguishable from an unknown session, so a principal
            // cannot probe for sessions it does not own, and a refusal
            // (`unauthorized`) always means the credential itself.
            return Err(ProviderError::new(
                ProviderErrorKind::UnknownSession,
                "unknown session",
            ));
        }
        Ok(session)
    }

    fn usable(
        &self,
        session_id: &SessionId,
        owner: &str,
        operation: &str,
    ) -> Result<ComputeSession, ProviderError> {
        let session = self.owned(session_id, owner)?;
        if session.is_expired_at(Utc::now()) {
            return Err(conflict(format!(
                "the session has expired; cannot {operation}"
            )));
        }
        if !session.status.is_usable() {
            let hint = match session.status {
                SessionStatus::Stopped => "; resume it first",
                SessionStatus::Requested | SessionStatus::Provisioning => "; it is not ready yet",
                _ => "",
            };
            return Err(conflict(format!(
                "session is {}{hint}; cannot {operation}",
                session.status
            )));
        }
        Ok(session)
    }

    fn environment(&self, session: &ComputeSession) -> Result<SessionEnvironment, ProviderError> {
        let stored = self.read_request(&session.session_id)?;
        let bundle = stored.request.artifact.bundle()?;
        Ok(SessionEnvironment {
            session_id: session.session_id.clone(),
            provider_session_id: session
                .provider_session_id
                .clone()
                .ok_or_else(|| conflict("the session has no environment yet".into()))?,
            resources: session.resources.clone(),
            network: session.network.clone(),
            isolation: stored
                .request
                .execution
                .isolation
                .unwrap_or(bundle.workload.isolation.profile),
        })
    }

    fn failure(&self, phase: SessionPhase, error: &ProviderError) -> SessionFailure {
        SessionFailure {
            phase,
            provider: Some(self.provider.kind()),
            code: error_code(error.kind),
            message: error.message.clone(),
            retryable: matches!(
                error.kind,
                ProviderErrorKind::TerminationFailed
                    | ProviderErrorKind::ProviderUnavailable
                    | ProviderErrorKind::TransportFailure
                    | ProviderErrorKind::ProviderInterrupted
                    | ProviderErrorKind::RuntimeUnavailable
            ),
            at: Utc::now(),
        }
    }

    /// A provider's error, labelled as the provider's.
    fn provider_error(&self, error: ProviderError) -> ProviderError {
        ProviderError {
            message: format!(
                "session provider {}: {}",
                self.provider.kind(),
                error.message
            ),
            ..error
        }
    }

    /// Apply `change` to the session. With `fence`, it applies only if the
    /// session is still at that generation. A terminal session never
    /// changes status again.
    async fn mutate(
        &self,
        session_id: &SessionId,
        fence: Option<u64>,
        event_type: &str,
        change: impl FnOnce(&mut ComputeSession) -> Result<(), ProviderError>,
    ) -> Result<ComputeSession, ProviderError> {
        let _guard = self.mutation.lock().await;
        let mut session = self.read_session(session_id)?;
        if fence.is_some_and(|generation| generation != session.generation) {
            return Err(conflict(format!("{STALE} (now {})", session.status)));
        }
        if session.status.is_terminal() {
            return Err(conflict(format!(
                "session is {}; it does not change again",
                session.status
            )));
        }
        change(&mut session)?;
        session.generation += 1;
        session.updated_at = Utc::now();
        self.write_session(&session)?;
        self.append_event(&session, event_type, None)?;
        if session.status.is_terminal() {
            self.live().remove(session_id);
        }
        Ok(session)
    }

    /// Record a failure of an operation that will be retried. A repeat of
    /// the failure already recorded changes nothing, so retries do not grow
    /// the record.
    async fn record_failure(
        &self,
        session: &ComputeSession,
        event_type: &str,
        failure: SessionFailure,
    ) -> Result<(), ProviderError> {
        if session.failure.as_ref().is_some_and(|recorded| {
            recorded.phase == failure.phase
                && recorded.code == failure.code
                && recorded.message == failure.message
        }) {
            return Ok(());
        }
        self.mutate(
            &session.session_id,
            Some(session.generation),
            event_type,
            |session| {
                session.failure = Some(failure);
                Ok(())
            },
        )
        .await
        .map(|_| ())
    }

    async fn record_event(
        &self,
        session_id: &SessionId,
        event_type: &str,
        detail: Option<String>,
    ) -> Result<(), ProviderError> {
        let _guard = self.mutation.lock().await;
        let session = self.read_session(session_id)?;
        self.append_event(&session, event_type, detail)
    }

    fn append_event(
        &self,
        session: &ComputeSession,
        event_type: &str,
        detail: Option<String>,
    ) -> Result<(), ProviderError> {
        let path = self.directory(&session.session_id).join("events.json");
        let mut events: Vec<SessionEvent> = read_json(&path).unwrap_or_default();
        events.push(event(session, events.len() as u64 + 1, event_type, detail));
        write_json(&path, &events)
    }

    fn directory(&self, session_id: &SessionId) -> PathBuf {
        self.root.join(&session_id.0)
    }

    fn read_session(&self, session_id: &SessionId) -> Result<ComputeSession, ProviderError> {
        read_json(&self.directory(session_id).join("session.json"))
    }

    fn write_session(&self, session: &ComputeSession) -> Result<(), ProviderError> {
        write_json(
            &self.directory(&session.session_id).join("session.json"),
            session,
        )
    }

    fn read_request(&self, session_id: &SessionId) -> Result<StoredSessionRequest, ProviderError> {
        read_json(&self.directory(session_id).join("request.json"))
    }

    fn live(&self) -> std::sync::MutexGuard<'_, BTreeSet<SessionId>> {
        self.live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn driving(&self) -> std::sync::MutexGuard<'_, BTreeSet<SessionId>> {
        self.driving
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn watching(&self) -> std::sync::MutexGuard<'_, BTreeSet<JobId>> {
        self.watching
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// One pass of expiry and reconciliation over the working set.
    async fn reconcile(self: &Arc<Self>) {
        let now = Utc::now();
        let live = self.live().iter().cloned().collect::<Vec<_>>();
        for session_id in live {
            let Ok(session) = self.read_session(&session_id) else {
                continue;
            };
            if session.status.is_terminal() {
                self.live().remove(&session_id);
                continue;
            }
            if self.driving().contains(&session_id) {
                continue;
            }
            if session.is_expired_at(now)
                && !matches!(
                    session.status,
                    SessionStatus::Expiring | SessionStatus::Destroying
                )
            {
                // Expiry is durable before anything is torn down.
                if self
                    .mutate(
                        &session_id,
                        Some(session.generation),
                        "expiring",
                        |session| {
                            session.status = SessionStatus::Expiring;
                            Ok(())
                        },
                    )
                    .await
                    .is_err()
                {
                    continue;
                }
                self.drive(session_id);
            } else if matches!(
                session.status,
                SessionStatus::Requested
                    | SessionStatus::Provisioning
                    | SessionStatus::Stopping
                    | SessionStatus::Resuming
                    | SessionStatus::Expiring
                    | SessionStatus::Destroying
            ) {
                self.drive(session_id);
            }
        }
    }
}

async fn sweep_sessions(manager: Weak<SessionManager>) {
    loop {
        let interval = match manager.upgrade() {
            Some(manager) => {
                manager.reconcile().await;
                manager.sweep
            }
            None => return,
        };
        tokio::time::sleep(interval).await;
    }
}

fn job_failure(job: &ExecutionJob) -> (String, String) {
    match &job.failure {
        Some(failure) => {
            let code = failure
                .split_once(':')
                .map(|(code, _)| code.trim())
                .filter(|code| {
                    !code.is_empty()
                        && code
                            .bytes()
                            .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
                })
                .unwrap_or("readiness_failed");
            (code.to_owned(), failure.clone())
        }
        None => (
            "readiness_failed".into(),
            format!("the readiness execution ended {:?}", job.status),
        ),
    }
}

fn conflict(message: String) -> ProviderError {
    ProviderError::new(ProviderErrorKind::SessionConflict, message)
}

const STALE: &str = "session changed while this operation ran";

/// A fenced write that lost to a newer transition.
fn is_stale(error: &ProviderError) -> bool {
    error.kind == ProviderErrorKind::SessionConflict
        && (error.message.starts_with(STALE) || error.message.contains("does not change again"))
}

fn event(
    session: &ComputeSession,
    sequence: u64,
    event_type: &str,
    detail: Option<String>,
) -> SessionEvent {
    SessionEvent {
        session_id: session.session_id.clone(),
        sequence,
        event_type: event_type.into(),
        status: session.status,
        generation: session.generation,
        detail,
        timestamp: Utc::now(),
    }
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, ProviderError> {
    let bytes = fs::read(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ProviderError::new(ProviderErrorKind::UnknownSession, "unknown session")
        } else {
            transport_error(error)
        }
    })?;
    serde_json::from_slice(&bytes)
        .map_err(|error| ProviderError::new(ProviderErrorKind::EvidenceInvalid, error.to_string()))
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<(), ProviderError> {
    let parent = path
        .parent()
        .ok_or_else(|| transport_error("session path has no parent"))?;
    fs::create_dir_all(parent).map_err(transport_error)?;
    let mut temporary = NamedTempFile::new_in(parent).map_err(transport_error)?;
    temporary
        .write_all(&serde_json::to_vec_pretty(value).map_err(transport_error)?)
        .map_err(transport_error)?;
    temporary.as_file().sync_all().map_err(transport_error)?;
    temporary
        .persist(path)
        .map_err(|error| transport_error(error.error))?;
    Ok(())
}
