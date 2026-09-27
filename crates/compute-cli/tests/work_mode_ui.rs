//! The control plane's Manage and Work modes, in a browser. Runs where
//! Playwright and Chromium are installed (`PLAYWRIGHT_MODULE`, or the global
//! `playwright` package; `COMPUTE_UI_CHROMIUM`, or Playwright's Chromium in
//! `$PLAYWRIGHT_BROWSERS_PATH`) and says it was skipped otherwise.

#[path = "support/runtimes.rs"]
mod runtimes;
#[path = "support/targets.rs"]
mod targets;

use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_for_port(address: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while std::net::TcpStream::connect(address).is_err() {
        assert!(Instant::now() < deadline, "{address} never listened");
        std::thread::sleep(Duration::from_millis(20));
    }
}

struct Kill(Child);

impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn playwright() -> Option<PathBuf> {
    if let Ok(module) = std::env::var("PLAYWRIGHT_MODULE") {
        return Some(PathBuf::from(module));
    }
    let root = std::process::Command::new("npm")
        .args(["root", "-g"])
        .output()
        .ok()?;
    let module = PathBuf::from(String::from_utf8(root.stdout).ok()?.trim())
        .join("playwright")
        .join("index.mjs");
    module.exists().then_some(module)
}

fn chromium() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("COMPUTE_UI_CHROMIUM") {
        return Some(PathBuf::from(path));
    }
    let browsers = PathBuf::from(std::env::var("PLAYWRIGHT_BROWSERS_PATH").ok()?);
    let path = browsers.join("chromium");
    path.exists().then_some(path)
}

#[test]
fn manage_and_work_are_modes_of_one_environment() {
    let (Some(module), Some(browser)) = (playwright(), chromium()) else {
        eprintln!("skipped: no Playwright or Chromium here");
        return;
    };
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().to_path_buf();
    let source = root.join("app");
    std::fs::create_dir_all(&source).unwrap();
    let git = |arguments: &[&str]| {
        let output = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(arguments)
            .current_dir(&source)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    };
    git(&["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(source.join("serve.sh"), "exec sleep 600\n").unwrap();
    for version in ["v1", "v2"] {
        std::fs::write(source.join("VERSION"), version).unwrap();
        git(&["add", "."]);
        git(&["commit", "--quiet", "-m", version]);
        git(&["tag", version]);
    }

    let target = format!("127.0.0.1:{}", free_port());
    let credential = targets::issue(&root, "target-a", "control-plane");
    let mut serve = std::process::Command::new(env!("CARGO_BIN_EXE_compute"));
    runtimes::with_fixture_runtimes(&mut serve);
    let _target = Kill(
        serve
            .args(["serve", "--listen", &target, "--public-url"])
            .arg(format!("http://{target}"))
            .arg("--job-store")
            .arg(root.join("jobs"))
            .arg("--session-store")
            .arg(root.join("sessions"))
            .arg("--credentials")
            .arg(&credential.credentials)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for_port(&target);
    std::fs::write(
        root.join("pool.toml"),
        format!(
            "[providers.target-a]\nkind = \"remote\"\nendpoint = \"http://{target}\"\n{}",
            credential.pool_line()
        ),
    )
    .unwrap();
    let listen = format!("127.0.0.1:{}", free_port());
    let compute = |arguments: &[&str]| {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_compute"));
        runtimes::with_fixture_runtimes(&mut command);
        let output = command
            .current_dir(&root)
            .args(arguments)
            .env("COMPUTE_DAEMON", format!("http://{listen}"))
            .env("COMPUTE_DAEMON_TOKEN", "secret")
            .output()
            .unwrap();
        assert!(output.status.success(), "compute {arguments:?}: {output:?}");
    };
    compute(&[
        "start",
        "--detach",
        "--listen",
        &listen,
        "--require-token-env",
        "COMPUTE_DAEMON_TOKEN",
        "--reconcile-interval-ms",
        "200",
        "--state-dir",
        &root.join("state").display().to_string(),
        "--pool-config",
        &root.join("pool.toml").display().to_string(),
    ]);
    struct Stop<F: Fn()>(F);
    impl<F: Fn()> Drop for Stop<F> {
        fn drop(&mut self) {
            (self.0)();
        }
    }
    let _stop = Stop(|| {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_compute"));
        let _ = command
            .arg("stop")
            .env("COMPUTE_DAEMON", format!("http://{listen}"))
            .env("COMPUTE_DAEMON_TOKEN", "secret")
            .output();
    });
    wait_for_port(&listen);
    let url = source.display().to_string();
    compute(&[
        "environment",
        "create",
        "myapp",
        "--cpu",
        "1",
        "--memory",
        "64Mi",
        "--persistent",
    ]);
    compute(&[
        "environment",
        "repo",
        "add",
        "myapp",
        "app",
        "--url",
        &url,
        "--revision",
        "v1",
    ]);
    compute(&[
        "environment",
        "project",
        "add",
        "myapp",
        "app",
        "--repository",
        "app",
        "--build",
        "cat VERSION > BUILT",
    ]);
    compute(&[
        "environment",
        "service",
        "add",
        "myapp",
        "api",
        "--repository",
        "app",
        "--",
        "sh",
        "serve.sh",
    ]);

    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/support/work_mode_ui.mjs");
    let output = std::process::Command::new("node")
        .arg(script)
        .arg(&module)
        .arg(&browser)
        .arg(format!("http://{listen}"))
        .arg("secret")
        .arg("myapp")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
