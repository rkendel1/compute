//! A target (`compute serve` hosting workspace sessions, in a runtime of its
//! own) and a daemon whose pool names it: the harness the architecture
//! suites share.
#![allow(dead_code)]

use std::net::TcpListener as StdTcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use compute_environment::*;
use compute_placement::{PoolConfig, ProviderConfig, ProviderKind};
use compute_provider::{RemoteProvider, ServerConfig, WorkspaceSessionProvider};
use compute_state::{ControlState, StateStore};
use tokio::runtime::Runtime;

use super::common;

// ---- A target: `compute serve` hosting sessions, in a runtime of its own --

pub struct Target {
    runtime: Option<Runtime>,
    pub endpoint: String,
    address: std::net::SocketAddr,
    trust: PathBuf,
    pub token_file: PathBuf,
    stores: tempfile::TempDir,
    workspaces: tempfile::TempDir,
}

impl Target {
    /// A target that trusts one control plane: the token in `token_file`.
    pub fn start() -> Self {
        let stores = tempfile::tempdir().unwrap();
        let mut credentials = compute_provider::TargetCredentials::default();
        let (_, token) = credentials.issue("test-control-plane").unwrap();
        let trust = stores.path().join("credentials.json");
        credentials.save(&trust).unwrap();
        let token_file = stores.path().join("control-plane.token");
        compute_provider::credentials::write_token_file(&token_file, &token).unwrap();
        let socket = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        let mut target = Self {
            runtime: None,
            endpoint: format!("http://{address}"),
            address,
            trust,
            token_file,
            stores,
            workspaces: tempfile::tempdir().unwrap(),
        };
        target.serve(socket);
        target
    }

    fn serve(&mut self, socket: StdTcpListener) {
        socket.set_nonblocking(true).unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let endpoint = self.endpoint.clone();
        let mut config = ServerConfig::local(endpoint.clone());
        config.authorizer = Arc::new(compute_provider::TargetAuthorizer::from_file(
            self.trust.clone(),
        ));
        config.provider = Arc::new(
            compute_provider::LocalProvider::with_identity(
                compute_core::ProviderIdentity::Remote {
                    id: endpoint.clone(),
                    endpoint,
                },
            )
            .with_runtime_catalog(common::catalog()),
        );
        config.job_store = self.stores.path().join("jobs");
        config.session_store = self.stores.path().join("sessions");
        config.session_provider = Some(Arc::new(WorkspaceSessionProvider::new(
            self.workspaces.path(),
        )));
        config.execution.sessions = true;
        config.session_sweep = Duration::from_millis(100);
        runtime.spawn(async move {
            let listener = tokio::net::TcpListener::from_std(socket).unwrap();
            let _ = compute_provider::serve_listener(listener, config).await;
        });
        self.runtime = Some(runtime);
    }

    /// Stop answering, as a machine that went away does.
    pub fn stop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }

    /// Answer again on the same address, with the same stores.
    pub fn restart(&mut self) {
        self.stop();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let socket = loop {
            match StdTcpListener::bind(self.address) {
                Ok(socket) => break socket,
                Err(error) => {
                    assert!(std::time::Instant::now() < deadline, "rebind: {error}");
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        };
        self.serve(socket);
    }

    /// Lose a machine: its processes end and its workspace is gone, as a
    /// machine that vanished.
    pub fn lose_machine(&self, resource: &str) {
        let workspace = self.workspaces.path().join(resource);
        kill_processes(&workspace);
        std::fs::remove_dir_all(&workspace).unwrap();
    }

    pub fn client(&self) -> RemoteProvider {
        RemoteProvider::new(self.endpoint.clone()).with_bearer_token(
            compute_provider::credentials::read_token_file(&self.token_file).unwrap(),
        )
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        self.stop();
        // Processes the computers started outlive the target's runtime.
        if let Ok(entries) = std::fs::read_dir(self.workspaces.path()) {
            for entry in entries.flatten() {
                kill_processes(&entry.path());
            }
        }
    }
}

pub fn kill_processes(workspace: &Path) {
    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg(
            "for f in \"$1\"/.compute/processes/*.pid; do [ -e \"$f\" ] && kill -KILL -\"$(cat \"$f\")\" 2>/dev/null; done; true",
        )
        .arg("kill")
        .arg(workspace)
        .status();
}

pub fn member(target: &Target, token_file: &Path) -> ProviderConfig {
    ProviderConfig {
        kind: ProviderKind::Remote,
        endpoint: Some(target.endpoint.clone()),
        application_endpoint: None,
        priority: 0,
        token_env: None,
        token_file: Some(token_file.to_path_buf()),
    }
}

pub fn pool(target: &Target) -> PoolConfig {
    PoolConfig {
        pool: Default::default(),
        providers: [("target-a".to_owned(), member(target, &target.token_file))].into(),
    }
}

pub async fn start_daemon(
    store: Arc<dyn StateStore>,
    pool: Option<PoolConfig>,
) -> (Arc<Daemon>, tempfile::TempDir) {
    let node = tempfile::tempdir().unwrap();
    let artifacts = Arc::new(compute_state::StateArtifacts::new(ControlState::new(
        store.clone(),
    )));
    let mut config = DaemonConfig::new(node.path(), store, artifacts);
    config.provider = Arc::new(common::provider());
    config.pool = pool;
    config.reconcile_interval = Duration::from_millis(200);
    config.computer_probe = Duration::from_millis(400);
    config.computer_liveness = Duration::from_millis(300);
    config.computer_liveness_timeout = Duration::from_secs(3);
    // Endpoints this test's applications listen on: a window of its own.
    let base = {
        let probe = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        port.clamp(20_000, 60_000)
    };
    config.port_range = (base, base + 9);
    config.instance_port_range = (base + 10, base + 19);
    (Daemon::start(config).await.unwrap(), node)
}
