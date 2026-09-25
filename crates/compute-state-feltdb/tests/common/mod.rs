//! A real `feltdb-server` for integration tests: `FELTDB_SERVER_BIN` names
//! the certified build, `FELTDB_PREVIOUS_SERVER_BIN` the previous release.
#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

pub const MASTER_KEY: &str = "compute-state-certification";

pub struct Server {
    pub child: Child,
    pub url: String,
    pub port: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn binary() -> PathBuf {
    PathBuf::from(
        std::env::var("FELTDB_SERVER_BIN")
            .expect("FELTDB_SERVER_BIN must name a feltdb-server binary"),
    )
}

pub fn create_key(data: &Path) -> String {
    let output = Command::new(binary())
        .args(["keys", "create", "--keys"])
        .arg(data.join("keys.json"))
        .args(["--name", "compute", "--namespace", "compute", "--scope"])
        .arg(
            "state:read,state:write,events:read,application:read,application:write,\
             application:revision:read,application:revision:create,application:revision:promote,\
             application:environment:read,application:environment:write",
        )
        .env("FELTDB_MASTER_KEY", MASTER_KEY)
        .output()
        .expect("feltdb-server runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .find(|word| word.starts_with("fdb_live_"))
        .expect("a key")
        .to_string()
}

pub fn start(data: &Path) -> Server {
    start_with(&binary(), data, 0)
}

/// The previous FeltDB release, when one was built.
pub fn previous_binary() -> Option<PathBuf> {
    std::env::var_os("FELTDB_PREVIOUS_SERVER_BIN").map(PathBuf::from)
}

/// Start `binary` over `data`, on `port` (0 for any): a restart on the
/// same port is the same authority at the same URL.
pub fn start_with(binary: &Path, data: &Path, port: u16) -> Server {
    let mut child = Command::new(binary)
        .args(["--host", "127.0.0.1", "--port"])
        .arg(port.to_string())
        .args(["--namespace", "compute", "--auth"])
        .arg("--data")
        .arg(data.join("state.log"))
        .arg("--keys")
        .arg(data.join("keys.json"))
        .arg("--audit")
        .arg(data.join("audit.log"))
        .env("FELTDB_MASTER_KEY", MASTER_KEY)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("feltdb-server starts");
    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();
    let url = loop {
        let line = lines
            .next()
            .expect("feltdb-server reports readiness")
            .unwrap();
        if let Some(index) = line.find("http://") {
            break line[index..].split_whitespace().next().unwrap().to_string();
        }
    };
    // Keep draining output so the server never blocks on a full pipe.
    std::thread::spawn(move || for _ in lines {});
    let port = url
        .trim_end_matches('/')
        .rsplit(':')
        .next()
        .and_then(|port| port.parse().ok())
        .unwrap_or_default();
    Server { child, url, port }
}

/// Create an API key with exactly `scopes`.
pub fn create_scoped_key(data: &Path, name: &str, scopes: &str) -> String {
    let output = Command::new(binary())
        .args(["keys", "create", "--keys"])
        .arg(data.join("keys.json"))
        .args(["--name", name, "--namespace", "compute", "--scope", scopes])
        .env("FELTDB_MASTER_KEY", MASTER_KEY)
        .output()
        .expect("feltdb-server runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .find(|word| word.starts_with("fdb_live_"))
        .expect("a key")
        .to_string()
}

pub fn tempfile_dir() -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "compute-state-feltdb-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    directory
}
