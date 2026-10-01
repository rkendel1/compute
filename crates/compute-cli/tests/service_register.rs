//! `compute service register --endpoint` records the service's endpoint. It
//! once also redirected the CLI to that endpoint, because the flag shared a
//! clap id with the global daemon location, so a service with an endpoint could
//! not be registered at all.

#[path = "support/runtimes.rs"]
mod runtimes;

use std::process::{Command, Output};

#[test]
fn a_services_endpoint_is_the_services_not_the_daemons() {
    let temporary = tempfile::tempdir().unwrap();
    let listen = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("127.0.0.1:{}", probe.local_addr().unwrap().port())
    };
    let daemon = format!("http://{listen}");
    let mut start = Command::new(env!("CARGO_BIN_EXE_compute"));
    runtimes::with_fixture_runtimes(&mut start);
    let started = start
        .args(["start", "--detach", "--listen", &listen, "--state-dir"])
        .arg(temporary.path().join("state"))
        .current_dir(temporary.path())
        .output()
        .unwrap();
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    struct Stop(String);
    impl Drop for Stop {
        fn drop(&mut self) {
            let _ = Command::new(env!("CARGO_BIN_EXE_compute"))
                .args(["stop", "--daemon", &self.0])
                .output();
        }
    }
    let _stop = Stop(daemon.clone());

    let run = |args: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_compute"))
            .args(args)
            .args(["--daemon", &daemon, "--json"])
            .output()
            .unwrap()
    };
    let registered = run(&[
        "service",
        "register",
        "elsewhere",
        "--capability",
        "AppPort/ui/1",
        "--endpoint",
        "http://127.0.0.1:1",
    ]);
    assert!(
        registered.status.success(),
        "{}",
        String::from_utf8_lossy(&registered.stderr)
    );
    let listed: serde_json::Value =
        serde_json::from_slice(&run(&["service", "list"]).stdout).unwrap();
    assert_eq!(listed[0]["name"], "elsewhere");
    assert_eq!(listed[0]["endpoint"], "http://127.0.0.1:1");
    assert_eq!(listed[0]["capabilities"][0], "AppPort/ui/1");
}
