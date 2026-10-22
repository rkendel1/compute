//! MicroVM sessions through the `compute` binary, on a node with KVM:
//!
//! ```text
//! compute egress serve (allows one host)    compute serve --session-provider microvm
//! compute session create --network network  → a Firecracker VM, its own kernel
//! compute session exec …                    → a job that enters it over vsock
//! compute session stop / resume             → a memory snapshot: processes survive
//! ```
//!
//! Needs `/dev/kvm`, Firecracker, a guest kernel, and a rootfs built by
//! `distribution/microvm/build-rootfs.sh` (COMPUTE_VM_KERNEL,
//! COMPUTE_VM_ROOTFS, and COMPUTE_FIRECRACKER when not on PATH). Without
//! them it is skipped, unless COMPUTE_REQUIRE_MICROVM is set (CI), when it
//! fails instead.

#[path = "support/targets.rs"]
mod targets;

use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_for(address: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while std::net::TcpStream::connect(address).is_err() {
        assert!(Instant::now() < deadline, "{address} never answered");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn compute(root: &std::path::Path, arguments: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_compute"))
        .current_dir(root)
        .env_remove("COMPUTE_POOL_CONFIG")
        .env_remove("COMPUTE_CAPABILITY_CACHE")
        .args(arguments)
        .output()
        .unwrap()
}

fn ok(root: &std::path::Path, arguments: &[&str]) -> String {
    let output = compute(root, arguments);
    assert!(
        output.status.success(),
        "compute {arguments:?}:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn a_microvm_session_is_its_own_machine_and_survives_suspension() {
    let (Ok(kernel), Ok(rootfs)) = (
        std::env::var("COMPUTE_VM_KERNEL"),
        std::env::var("COMPUTE_VM_ROOTFS"),
    ) else {
        assert!(
            std::env::var_os("COMPUTE_REQUIRE_MICROVM").is_none(),
            "COMPUTE_VM_KERNEL and COMPUTE_VM_ROOTFS are required"
        );
        eprintln!("skipping: no guest kernel and rootfs (COMPUTE_VM_KERNEL, COMPUTE_VM_ROOTFS)");
        return;
    };
    if !std::path::Path::new("/dev/kvm").exists() {
        assert!(
            std::env::var_os("COMPUTE_REQUIRE_MICROVM").is_none(),
            "/dev/kvm is required"
        );
        eprintln!("skipping: no /dev/kvm");
        return;
    }
    let firecracker = std::env::var("COMPUTE_FIRECRACKER").unwrap_or_else(|_| "firecracker".into());
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().to_path_buf();

    // The only way out: a gateway that allows one host.
    let gateway = format!("127.0.0.1:{}", free_port());
    std::fs::write(
        root.join("egress.json"),
        r#"{"allow":[{"host":"example.com","ports":[80,443]}]}"#,
    )
    .unwrap();
    let _gateway = Running(
        std::process::Command::new(env!("CARGO_BIN_EXE_compute"))
            .args(["egress", "serve", "--rules"])
            .arg(root.join("egress.json"))
            .args(["--listen", &gateway])
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for(&gateway);

    let port = free_port();
    let listen = format!("127.0.0.1:{port}");
    let credential = targets::issue(&root, "node", "cli");
    std::fs::write(
        root.join("compute-pool.toml"),
        format!(
            "[providers.node]\nkind = \"remote\"\nendpoint = \"http://{listen}\"\n{}",
            credential.pool_line()
        ),
    )
    .unwrap();
    let _server = Running(
        std::process::Command::new(env!("CARGO_BIN_EXE_compute"))
            .args(["serve", "--listen", &listen, "--public-url"])
            .arg(format!("http://{listen}"))
            .arg("--job-store")
            .arg(root.join("jobs"))
            .arg("--session-store")
            .arg(root.join("sessions"))
            .arg("--credentials")
            .arg(root.join("node-credentials.json"))
            .args([
                "--session-provider",
                "microvm",
                "--vm-firecracker",
                &firecracker,
            ])
            .args(["--vm-kernel", &kernel, "--vm-rootfs", &rootfs])
            .args(["--vm-egress", &gateway])
            .spawn()
            .unwrap(),
    );
    wait_for(&listen);

    let created: Value = serde_json::from_str(&ok(
        &root,
        &[
            "session",
            "create",
            "--cpu",
            "1",
            "--memory",
            "256Mi",
            "--network",
            "network",
            "--ttl",
            "1h",
            "--refresh",
            "--wait",
            "--json",
        ],
    ))
    .unwrap();
    let session = &created["session"];
    assert_eq!(session["status"], "ready", "{created}");
    assert_eq!(session["provider_kind"], "microvm");
    let id = session["session_id"].as_str().unwrap().to_owned();
    let exec = |script: &str| ok(&root, &["session", "exec", &id, "--", "sh", "-c", script]);

    // Its own kernel and init, its own workspace, no network interface.
    let host_kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap();
    assert_ne!(
        exec("uname -r").trim(),
        host_kernel.trim(),
        "a guest kernel"
    );
    assert_eq!(exec("cat /proc/1/comm").trim(), "compute-guest");
    assert_eq!(exec("pwd").trim(), "/workspace");
    assert_eq!(exec("ls /sys/class/net").trim(), "lo");

    // Egress goes through the gateway, and only where it allows.
    let allowed = exec("wget -q -O - http://example.com/ | head -c 2000");
    assert!(allowed.contains("Example Domain"), "{allowed}");
    let refused = compute(
        &root,
        &[
            "session",
            "exec",
            &id,
            "--",
            "sh",
            "-c",
            "wget -q -O - http://github.com/",
        ],
    );
    assert!(
        !refused.status.success(),
        "a host the gateway does not allow"
    );

    // Standard input and files.
    let mut piped = std::process::Command::new(env!("CARGO_BIN_EXE_compute"))
        .current_dir(&root)
        .args([
            "session",
            "exec",
            &id,
            "--stdin",
            "--",
            "sh",
            "-c",
            "cat > note && cat note",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(piped.stdin.as_mut().unwrap(), b"kept\n").unwrap();
    drop(piped.stdin.take());
    assert_eq!(piped.wait_with_output().unwrap().stdout, b"kept\n");

    // A process running when the session is stopped is still running after
    // it resumes: the VM was suspended, not rebooted.
    exec("sleep 100000 > /dev/null 2>&1 & echo $! > sleeper");
    ok(&root, &["session", "stop", &id]);
    ok(&root, &["session", "resume", &id]);
    assert_eq!(exec("cat note").trim(), "kept");
    assert_eq!(
        exec("kill -0 \"$(cat sleeper)\" && echo alive").trim(),
        "alive",
        "the process survived suspension"
    );

    ok(&root, &["session", "destroy", &id]);
    assert!(
        !root.join("sessions/microvms").exists()
            || std::fs::read_dir(root.join("sessions/microvms"))
                .unwrap()
                .next()
                .is_none(),
        "the VM and its disk are gone"
    );
}
