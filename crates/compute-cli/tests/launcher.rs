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
