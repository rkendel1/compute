//! `compute` (the launcher) sets up a control plane that its computer host
//! trusts, and nobody else: the host refuses anonymous callers, the pool
//! presents the control plane's credential without showing it, the
//! control plane keeps its identity across launches, and control state
//! says plainly that a file is local development.

#[path = "support/runtimes.rs"]
mod runtimes;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use compute_provider::{ComputeProvider, ProviderErrorKind, RemoteProvider};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Launched {
    home: PathBuf,
    listen: String,
    target: String,
}

impl Launched {
    fn command(&self, arguments: &[&str]) -> std::process::Command {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_compute"));
        runtimes::with_fixture_runtimes(&mut command);
        command
            .args(arguments)
            .env("COMPUTE_HOME", &self.home)
            .env("COMPUTE_LISTEN", &self.listen)
            .env("COMPUTE_TARGET_LISTEN", &self.target)
            .env("COMPUTE_DAEMON", format!("http://{}", self.listen))
            .env("COMPUTE_NO_BROWSER", "1")
            .env_remove("COMPUTE_CONFIG")
            .env_remove("COMPUTE_DAEMON_TOKEN");
        command
    }

    fn json(&self, arguments: &[&str]) -> serde_json::Value {
        let output = self.command(arguments).output().unwrap();
        assert!(output.status.success(), "{arguments:?}: {output:?}");
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.listen)
    }

    /// Wait until the controller answers on its own address.
    fn wait_until_answering(&self) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while std::net::TcpStream::connect(&self.listen).is_err() {
            assert!(
                Instant::now() < deadline,
                "the control plane never answered on {}",
                self.listen
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// What the controller says about itself, over HTTP.
    ///
    /// Every headless assertion reads this rather than the command's stdout:
    /// the point is that the daemon agrees, not that the CLI agrees with
    /// itself.
    fn http(&self, path: &str) -> (u16, String) {
        self.wait_until_answering();
        let output = std::process::Command::new("curl")
            .args([
                "-s",
                "-o",
                "/dev/stdout",
                "-w",
                "\n%{http_code}",
                &self.url(path),
            ])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        let (body, status) = text.rsplit_once('\n').expect("curl status line");
        (
            status.trim().parse().expect("status code"),
            body.trim().to_owned(),
        )
    }

    fn ready(&self) -> serde_json::Value {
        let (status, body) = self.http("/ready");
        assert_eq!(status, 200, "/ready: {body}");
        serde_json::from_str(&body).expect("readiness is JSON")
    }

    /// Identifies the running controller, so reuse can be told from a restart.
    fn instance_id(&self) -> String {
        self.ready()["instance_id"]
            .as_str()
            .expect("readiness identifies the controller")
            .to_owned()
    }

    /// Whether `/ui/` is served. A headless controller answers 404.
    fn ui_status(&self) -> u16 {
        self.http("/ui/").0
    }
}

/// A command's combined output, for failure messages.
fn said(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

impl Drop for Launched {
    fn drop(&mut self) {
        let _ = self.command(&["down"]).output();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_launcher_makes_a_host_that_trusts_only_its_control_plane() {
    let temporary = tempfile::tempdir().unwrap();
    let launched = Launched {
        home: temporary.path().join("home"),
        listen: format!("127.0.0.1:{}", free_port()),
        target: format!("127.0.0.1:{}", free_port()),
    };
    let output = launched.command(&["up"]).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let said = String::from_utf8_lossy(&output.stdout);
    assert!(
        said.contains("authenticates to it with a target credential"),
        "{said}"
    );
    assert!(said.contains("local development"), "{said}");
    let deadline = Instant::now() + Duration::from_secs(30);
    while std::net::TcpStream::connect(&launched.listen).is_err() {
        assert!(
            Instant::now() < deadline,
            "the control plane never answered"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // The host trusts one credential, kept as a verifier; the control plane
    // holds the token, and the pool names the file, never the token.
    let identity = std::fs::read_to_string(launched.home.join("identity")).unwrap();
    let credentials = compute_provider::TargetCredentials::load(
        &launched.home.join("computers/credentials.json"),
    )
    .unwrap();
    assert_eq!(credentials.credentials.len(), 1);
    assert_eq!(credentials.credentials[0].control_plane, identity.trim());
    let token_file = launched
        .home
        .join("control-plane/targets/this-machine.token");
    let token = compute_provider::credentials::read_token_file(&token_file).unwrap();
    let pool = std::fs::read_to_string(launched.home.join("pool.toml")).unwrap();
    assert!(pool.contains("token_file"), "{pool}");
    assert!(!pool.contains(&token), "the pool never holds the token");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [
            &token_file,
            &launched.home.join("computers/credentials.json"),
        ] {
            let mode = std::fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "{} is private", path.display());
        }
    }

    // Anyone else who reaches the host is refused; the control plane is not.
    let endpoint = format!("http://{}", launched.target);
    let anonymous = RemoteProvider::new(endpoint.clone());
    assert_eq!(
        anonymous.sessions().await.unwrap_err().kind,
        ProviderErrorKind::Unauthorized
    );
    let last = if token.ends_with('0') { '1' } else { '0' };
    let forged = RemoteProvider::new(endpoint.clone())
        .with_bearer_token(format!("{}{last}", &token[..token.len() - 1]));
    assert_eq!(
        forged.health().await.unwrap_err().kind,
        ProviderErrorKind::Unauthorized
    );
    let trusted = RemoteProvider::new(endpoint).with_bearer_token(token.clone());
    assert_eq!(
        trusted
            .capabilities()
            .await
            .unwrap()
            .authentication
            .as_deref(),
        Some("credential")
    );

    // The control plane says how it reaches its target, and what its
    // control state is.
    let targets = launched.json(&["target", "list", "--json"]);
    let host = targets
        .as_array()
        .unwrap()
        .iter()
        .find(|target| target["target_id"] == "this-machine")
        .unwrap_or_else(|| panic!("{targets}"));
    assert_eq!(host["authentication"], "credential");
    assert_eq!(host["credential"], true);
    assert!(!targets.to_string().contains(&token));
    let info = launched.json(&["node", "info", "--json"]);
    assert_eq!(info["control_plane"]["durability"], "local-development");
    assert_eq!(info["control_plane"]["state"]["kind"], "file");

    // Launching again keeps the identity and the credential.
    let again = launched.command(&["up"]).output().unwrap();
    assert!(again.status.success(), "{again:?}");
    assert_eq!(
        std::fs::read_to_string(launched.home.join("identity")).unwrap(),
        identity
    );
    assert_eq!(
        compute_provider::credentials::read_token_file(&token_file).unwrap(),
        token
    );
    assert_eq!(
        compute_provider::TargetCredentials::load(
            &launched.home.join("computers/credentials.json")
        )
        .unwrap()
        .credentials
        .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_up_starts_a_headless_daemon_and_reports_its_actual_state() {
    // Case 1: nothing is listening, so `up --headless` starts the controller
    // and passes the flag to it.
    let temporary = tempfile::tempdir().unwrap();
    let launched = Launched {
        home: temporary.path().join("home"),
        listen: format!("127.0.0.1:{}", free_port()),
        target: format!("127.0.0.1:{}", free_port()),
    };
    let output = launched.command(&["up", "--headless"]).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("UI:        disabled (headless)"),
        "{output:?}"
    );

    // The claim is checked against the daemon, not against stdout: the
    // controller itself must report no UI and serve none.
    launched.wait_until_answering();
    let ready = launched.ready();
    assert_eq!(ready["ui"], false, "{ready}");
    assert_eq!(ready["status"], "ready", "{ready}");
    assert_eq!(launched.ui_status(), 404, "/ui must not be served");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_up_reuses_a_running_headless_daemon_without_restarting_it() {
    // Case 2: the requested mode already matches, so reuse is correct and
    // must not restart a controller that is doing useful work.
    let temporary = tempfile::tempdir().unwrap();
    let launched = Launched {
        home: temporary.path().join("home"),
        listen: format!("127.0.0.1:{}", free_port()),
        target: format!("127.0.0.1:{}", free_port()),
    };
    let first = launched.command(&["up", "--headless"]).output().unwrap();
    assert!(first.status.success(), "{first:?}");
    launched.wait_until_answering();
    let before = launched.instance_id();

    let again = launched.command(&["up", "--headless"]).output().unwrap();
    assert!(again.status.success(), "{again:?}");
    assert!(said(&again).contains("disabled (headless)"), "{again:?}");
    // Same daemon: reuse, not a restart.
    assert_eq!(launched.instance_id(), before, "the daemon was restarted");
    assert_eq!(launched.ready()["ui"], false);
    assert_eq!(launched.ui_status(), 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_up_refuses_to_claim_a_ui_daemon_is_headless() {
    // Case 3, and the bug this fixes: a UI-enabled controller is already
    // serving, so `up --headless` used to print "UI: .../ui/" while the caller
    // believed it had asked for headless.
    let temporary = tempfile::tempdir().unwrap();
    let launched = Launched {
        home: temporary.path().join("home"),
        listen: format!("127.0.0.1:{}", free_port()),
        target: format!("127.0.0.1:{}", free_port()),
    };
    let started = launched.command(&["up"]).output().unwrap();
    assert!(started.status.success(), "{started:?}");
    launched.wait_until_answering();
    assert_eq!(launched.ready()["ui"], true, "precondition: a UI daemon");

    let conflicted = launched.command(&["up", "--headless"]).output().unwrap();
    let said = said(&conflicted);
    assert!(
        !conflicted.status.success(),
        "--headless must not succeed against a UI-enabled controller: {said}"
    );
    // It must not report headless, and it must say why.
    assert!(
        !said.contains("disabled (headless)"),
        "the command claimed headless while the daemon serves a UI: {said}"
    );
    assert!(
        said.contains("UI-enabled") && said.contains("--headless"),
        "the conflict must be explained: {said}"
    );

    // And the controller it refused to disturb is untouched: still UI-enabled,
    // still serving the UI. It is never stopped behind the caller's back.
    assert_eq!(launched.ready()["ui"], true, "the UI daemon was disturbed");
    assert_eq!(launched.ui_status(), 200, "the UI daemon lost its UI");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn up_without_headless_reuses_a_ui_daemon_and_reports_the_ui() {
    // The ordinary case must keep working: asking for nothing in particular
    // against a UI daemon reuses it and says so.
    let temporary = tempfile::tempdir().unwrap();
    let launched = Launched {
        home: temporary.path().join("home"),
        listen: format!("127.0.0.1:{}", free_port()),
        target: format!("127.0.0.1:{}", free_port()),
    };
    let started = launched.command(&["up"]).output().unwrap();
    assert!(started.status.success(), "{started:?}");
    launched.wait_until_answering();

    let again = launched.command(&["up"]).output().unwrap();
    assert!(again.status.success(), "{again:?}");
    let said = said(&again);
    assert!(said.contains("/ui/"), "{said}");
    assert!(!said.contains("disabled (headless)"), "{said}");
    assert_eq!(launched.ready()["ui"], true);
    assert_eq!(launched.ui_status(), 200);
}

#[test]
fn a_development_start_is_one_operation_and_can_open_a_temporary_session() {
    let temporary = tempfile::tempdir().unwrap();
    let port = free_port();
    let launched = Launched {
        home: temporary.path().join("home"),
        listen: format!("127.0.0.1:{port}"),
        target: format!("127.0.0.1:{}", free_port()),
    };
    let state = temporary.path().join("state");
    let output = launched
        .command(&["start", "--detach", "--listen", &launched.listen])
        .args(["--state-dir"])
        .arg(&state)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");

    let targets = launched.json(&["target", "list", "--json"]);
    let local = targets
        .as_array()
        .unwrap()
        .iter()
        .find(|target| target["target_id"] == "this-machine")
        .unwrap_or_else(|| panic!("{targets}"));
    assert_eq!(local["hosts_computers"], true);

    let session = launched.json(&[
        "session", "open", "--cpu", "1", "--memory", "64Mi", "--json",
    ]);
    assert_eq!(session["kind"], "ephemeral");
    assert_eq!(session["status"], "open");
}
