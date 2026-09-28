//! The acceptance journey: on a clean machine, `compute` alone launches the
//! control plane, and everything after that happens in its UI — run a
//! project, add another to the same computer, change it, build and test it,
//! publish a version, deploy it to test, promote it to production, operate
//! it, roll it back, stop and resume the computer, open a terminal, and try
//! other software on a temporary computer.
//!
//! Runs where Playwright and Chromium are installed (see `work_mode_ui.rs`)
//! and says it was skipped otherwise. With `COMPUTE_JOURNEY_SCREENSHOTS`
//! set, the screenshot of every surface is kept there
//! (`docs/product-surface/` is made this way).

#[path = "support/runtimes.rs"]
mod runtimes;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
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
    let path = PathBuf::from(std::env::var("PLAYWRIGHT_BROWSERS_PATH").ok()?).join("chromium");
    path.exists().then_some(path)
}

fn git(directory: &Path, arguments: &[&str]) {
    let output = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(arguments)
        .current_dir(directory)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

/// A repository with a Makefile and a Procfile, as a project might have.
fn repository(root: &Path, name: &str, files: &[(&str, &str)]) -> PathBuf {
    let directory = root.join(name);
    std::fs::create_dir_all(&directory).unwrap();
    git(&directory, &["init", "--quiet", "--initial-branch=main"]);
    for (file, content) in files {
        std::fs::write(directory.join(file), content).unwrap();
    }
    git(&directory, &["add", "."]);
    git(&directory, &["commit", "--quiet", "-m", "first"]);
    directory
}

struct Launched {
    root: PathBuf,
    listen: String,
    target: String,
}

impl Launched {
    fn command(&self, arguments: &[&str]) -> std::process::Command {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_compute"));
        runtimes::with_fixture_runtimes(&mut command);
        command
            .args(arguments)
            .env("COMPUTE_HOME", self.root.join("home"))
            .env("COMPUTE_LISTEN", &self.listen)
            .env("COMPUTE_TARGET_LISTEN", &self.target)
            .env("COMPUTE_NO_BROWSER", "1")
            .env_remove("COMPUTE_DAEMON_TOKEN");
        command
    }
}

impl Drop for Launched {
    fn drop(&mut self) {
        let _ = self.command(&["down"]).output();
    }
}

#[test]
fn the_whole_product_from_one_command() {
    let (Some(module), Some(browser)) = (playwright(), chromium()) else {
        // CI certifies the UI in a browser: there, a missing browser fails.
        assert!(
            std::env::var_os("COMPUTE_REQUIRE_BROWSER").is_none(),
            "COMPUTE_REQUIRE_BROWSER is set, but there is no Playwright or Chromium"
        );
        eprintln!("skipped: no Playwright or Chromium here");
        return;
    };
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().to_path_buf();
    let web = repository(
        &root,
        "web-app",
        &[
            ("VERSION", "v1"),
            (
                "Makefile",
                "build:\n\tmkdir -p public && cp VERSION public/version.txt\n\
                 test:\n\ttest -f public/version.txt\nlint:\n\ttrue\n",
            ),
            (
                "Procfile",
                "web: python3 -m http.server $PORT --directory public\n",
            ),
        ],
    );
    let api = repository(
        &root,
        "api",
        &[
            ("Makefile", "test:\n\ttrue\n"),
            ("Procfile", "web: python3 -m http.server $PORT\n"),
            ("index.html", "api\n"),
        ],
    );
    let launched = Launched {
        root: root.clone(),
        listen: format!("127.0.0.1:{}", free_port()),
        target: format!("127.0.0.1:{}", free_port()),
    };
    // One command.
    let output = launched.command(&[]).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let said = String::from_utf8_lossy(&output.stdout);
    assert!(said.contains("Compute is running"), "{said}");
    let deadline = Instant::now() + Duration::from_secs(30);
    while std::net::TcpStream::connect(&launched.listen).is_err() {
        assert!(
            Instant::now() < deadline,
            "the control plane never answered"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // Running it again reuses what runs.
    assert!(launched.command(&[]).output().unwrap().status.success());

    let shots = std::env::var("COMPUTE_JOURNEY_SCREENSHOTS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| root.join("screenshots"));
    let config = serde_json::json!({
        "module": module,
        "chromium": browser,
        "base": format!("http://{}", launched.listen),
        "shots": shots,
        "repos": { "web": web, "api": api },
        "ports": { "web": free_port(), "api": free_port(), "trial": free_port() },
        "commit": format!(
            "cd '{}' && printf v2 > VERSION && git -c user.name=t -c user.email=t@example.invalid commit --quiet -am v2",
            web.display()
        ),
    });
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/support/product_journey.mjs");
    let output = std::process::Command::new("node")
        .arg(script)
        .arg(config.to_string())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
