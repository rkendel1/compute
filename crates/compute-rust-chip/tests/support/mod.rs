//! A real Compute target (`compute.remote@1` with sessions, bearer-token authorization, durable
//! jobs, workspace sessions) served in this process, plus a real git project to load into it. The
//! only substitution is the shell runtime: the target uses Compute's own host-backed fixture
//! catalog (`compute_provider::testing`) instead of downloading the pinned runtime, because the
//! build machine cannot reach the runtime CDN. Nothing about sessions, jobs or isolation is faked.
#![allow(dead_code)]

use std::net::TcpListener as StdTcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use compute_rust_chip::{ComputeSessionConfig, ComputeSessionEnvironments};

pub struct Target {
    runtime: Option<tokio::runtime::Runtime>,
    pub endpoint: String,
    pub token: String,
    address: std::net::SocketAddr,
    stores: tempfile::TempDir,
    authorizer: Arc<compute_provider::TargetAuthorizer>,
    catalog: PathBuf,
}

impl Target {
    pub fn start() -> Self {
        let socket = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        let (authorizer, token) =
            compute_provider::TargetAuthorizer::issue_for("test-control-plane").unwrap();
        let catalog_dir = tempfile::tempdir().unwrap().keep();
        let catalog = compute_provider::testing::host_fixture_catalog(&catalog_dir)
            .unwrap()
            .path;
        let mut target = Self {
            runtime: None,
            endpoint: format!("http://{address}"),
            token,
            address,
            stores: tempfile::tempdir().unwrap(),
            authorizer: Arc::new(authorizer),
            catalog,
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
        let mut config = compute_provider::ServerConfig::local(endpoint.clone());
        config.authorizer = self.authorizer.clone();
        config.provider = Arc::new(
            compute_provider::LocalProvider::with_identity(
                compute_core::ProviderIdentity::Remote {
                    id: endpoint.clone(),
                    endpoint,
                },
            )
            .with_runtime_catalog(
                compute_provider::RuntimeCatalog::from_path(&self.catalog)
                    .expect("fixture catalog"),
            ),
        );
        config.job_store = self.stores.path().join("jobs");
        config.session_store = self.stores.path().join("sessions");
        config.execution.sessions = true;
        config.max_concurrent_jobs = 8;
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

    /// Answer again on the same address with the same stores.
    pub fn restart(&mut self) {
        self.stop();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let socket = loop {
            match StdTcpListener::bind(self.address) {
                Ok(socket) => break socket,
                Err(e) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "cannot listen again: {e}"
                    );
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        };
        self.serve(socket);
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        self.stop();
    }
}

const TESTS: &str =
    "use fpfixture::double;\n\n#[test]\nfn doubles() {\n    assert_eq!(double(2), 4);\n}\n";

/// A git repository: a Rust crate whose tests pass, and a README. `tests_pass = false` makes the
/// crate's test fail.
pub fn project(tag: &str, tests_pass: bool) -> PathBuf {
    let root = std::env::temp_dir().join(format!("compute-rust-chip-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fpfixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nname = \"fpfixture\"\n",
    )
    .unwrap();
    let body = if tests_pass { "x * 2" } else { "x + 3" };
    std::fs::write(
        root.join("src/lib.rs"),
        format!("pub fn double(x: i32) -> i32 {{\n    {body}\n}}\n"),
    )
    .unwrap();
    std::fs::write(root.join("tests/double.rs"), TESTS).unwrap();
    std::fs::write(root.join("README.md"), "baseline\n").unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["add", "-A"]);
    git(
        &root,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "baseline",
        ],
    );
    root
}

fn git(root: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

pub fn worker() -> String {
    env!("CARGO_BIN_EXE_compute-rust-chip").to_string()
}

pub fn pax_installed() -> bool {
    std::process::Command::new("pax")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// The configuration a Compute-configured launch would supply: the target, the project, Rust
/// Chip's executable, and the tool locations the target's commands need.
pub fn config(target: &Target, source: &Path, max_environments: usize) -> ComputeSessionConfig {
    let mut config = ComputeSessionConfig::new(
        target.endpoint.clone(),
        source.display().to_string(),
        worker(),
    );
    config.token = Some(target.token.clone());
    config.max_environments = max_environments;
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let path = format!("{home}/.cargo/bin:/usr/local/bin:/usr/bin:/bin");
    config.command_environment.insert("PATH".into(), path);
    config
        .command_environment
        .insert("CARGO_HOME".into(), format!("{home}/.cargo"));
    config
        .command_environment
        .insert("RUSTUP_HOME".into(), format!("{home}/.rustup"));
    config
}

pub fn provider(
    target: &Target,
    source: &Path,
    max_environments: usize,
) -> Arc<ComputeSessionEnvironments> {
    Arc::new(ComputeSessionEnvironments::new(config(
        target,
        source,
        max_environments,
    )))
}
