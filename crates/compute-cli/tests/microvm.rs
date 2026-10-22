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
    // Kept when the test fails, so CI can print the VMM's log; removed at
    // the end when it passes.
    let root = tempfile::Builder::new()
        .prefix("compute-microvm-")
        .tempdir()
        .unwrap()
        .keep();

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
    let serve = || {
        Running(
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
        )
    };
    let mut server = serve();
    wait_for(&listen);

    let started = Instant::now();
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
    assert_eq!(session["capabilities"]["memory_suspend"], true, "{created}");
    let created_in = started.elapsed();
    let id = session["session_id"].as_str().unwrap().to_owned();
    let exec_in = |session: &str, script: &str| {
        ok(
            &root,
            &[
                "session",
                "exec",
                session,
                "--timeout",
                "60s",
                "--",
                "sh",
                "-c",
                script,
            ],
        )
    };
    let exec = |script: &str| exec_in(&id, script);
    let info = |session: &str| -> Value {
        serde_json::from_str(&ok(&root, &["session", "info", session, "--json"])).unwrap()
    };

    // Its own kernel and init, its own workspace, no network interface.
    let first = Instant::now();
    exec("true");
    let first_exec_in = first.elapsed();
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
    let allowed = exec("wget -q -T 20 -O - http://example.com/");
    assert!(allowed.contains("Example Domain"), "{allowed}");
    let refused = compute(
        &root,
        &[
            "session",
            "exec",
            &id,
            "--timeout",
            "60s",
            "--",
            "sh",
            "-c",
            "wget -q -T 20 -O - http://github.com/",
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
            "--timeout",
            "60s",
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

    // Suspension keeps memory. The evidence is held in the guest's memory,
    // not in any command's output: a tmpfs file (lost by any reboot), a
    // process with a known PID, and a counter that process keeps in memory
    // and reports only through tmpfs.
    let nonce = format!("{:x}", std::process::id() as u64 * 7919 + port as u64);
    exec(&format!("printf {nonce} > /tmp/nonce"));
    exec("sleep 100000 > /dev/null 2>&1 & echo $! > /tmp/sleeper");
    exec(
        "(i=0; while :; do i=$((i+1)); echo $i > /tmp/count.new; mv /tmp/count.new /tmp/count; \
         sleep 1; done) > /dev/null 2>&1 & echo $! > /tmp/counter",
    );
    std::thread::sleep(Duration::from_millis(2500));
    let sleeper = exec("cat /tmp/sleeper").trim().to_owned();
    let counter = exec("cat /tmp/counter").trim().to_owned();
    let counted: u64 = exec("cat /tmp/count").trim().parse().unwrap();
    assert!(counted > 0);
    let vms = root.join("sessions/microvms");
    let vm_directory = vms.join(info(&id)["provider_session_id"].as_str().unwrap());

    // Stop suspends the machine, and reports it only once the suspension is
    // on stable storage.
    let stopping = Instant::now();
    ok(&root, &["session", "stop", &id]);
    let stopped_in = stopping.elapsed();
    let record: Value =
        serde_json::from_slice(&std::fs::read(vm_directory.join("suspension.json")).unwrap())
            .unwrap();
    assert_eq!(record["format"], "compute.vm-suspension@1", "{record}");
    assert_eq!(
        record["memory_bytes"].as_u64(),
        Some(
            std::fs::metadata(vm_directory.join("memory"))
                .unwrap()
                .len()
        )
    );
    assert!(!vm_directory.join("pid").exists(), "the VMM has ended");
    // Stopping a stopped session is not an error, and changes nothing.
    ok(&root, &["session", "stop", &id]);

    // The controller restarts while the machine is suspended.
    drop(server);
    server = serve();
    wait_for(&listen);

    let resuming = Instant::now();
    ok(&root, &["session", "resume", &id]);
    let resumed_in = resuming.elapsed();
    // Ready means ready: the first command after the resume runs.
    assert_eq!(exec("cat /tmp/nonce").trim(), nonce, "tmpfs survived");
    assert_eq!(exec("cat note").trim(), "kept", "the disk survived");
    assert_eq!(
        exec(&format!("kill -0 {sleeper} && echo alive")).trim(),
        "alive",
        "the process survived suspension, with its PID"
    );
    assert_eq!(exec("cat /tmp/counter").trim(), counter);
    std::thread::sleep(Duration::from_millis(2500));
    let continued: u64 = exec("cat /tmp/count").trim().parse().unwrap();
    assert!(
        continued > counted,
        "the counter continued from memory ({counted} -> {continued})"
    );
    assert!(
        !vm_directory.join("suspension.json").exists(),
        "a resumed VM's snapshot is retired"
    );
    // Egress is reconnected after the controller restart.
    assert!(exec("wget -q -T 20 -O - http://example.com/").contains("Example Domain"));
    // Resuming a running session is a conflict, not a second machine.
    let again = compute(&root, &["session", "resume", &id]);
    assert!(!again.status.success());
    assert!(
        String::from_utf8_lossy(&again.stderr).contains("cannot be resumed"),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    eprintln!(
        "lifecycle (1 vCPU, 256 MiB, one run, debug build): create+boot {created_in:?}, \
         first exec {first_exec_in:?}, stop {stopped_in:?}, resume {resumed_in:?}"
    );

    // Fork: two children continue from one instant of the running machine,
    // each with its own copy of everything mutable; the source keeps running.
    let forking = Instant::now();
    let fork: Value = serde_json::from_str(&ok(
        &root,
        &["session", "fork", &id, "--count", "2", "--json"],
    ))
    .unwrap();
    let forked_in = forking.elapsed();
    let digest = fork["checkpoint"]["evidence"]["snapshot"]
        .as_str()
        .expect("the checkpoint's evidence")
        .to_owned();
    let children = fork["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|child| child["session_id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(children.len(), 2, "{fork}");
    assert!(!children.contains(&id));
    let counted_at_fork: u64 = exec("cat /tmp/count").trim().parse().unwrap();
    for child in &children {
        let record = info(child);
        assert_eq!(record["status"], "ready", "{record}");
        assert_eq!(record["lineage"]["parent_session_id"], id.as_str());
        assert_eq!(record["lineage"]["evidence"]["snapshot"], digest.as_str());
        assert_ne!(
            record["provider_session_id"],
            info(&id)["provider_session_id"]
        );
        // The same instant: memory, the same processes, the disk.
        assert_eq!(exec_in(child, "cat /tmp/nonce").trim(), nonce);
        assert_eq!(
            exec_in(child, &format!("kill -0 {sleeper} && echo alive")).trim(),
            "alive",
            "the source's process runs in the child, with its PID"
        );
        assert_eq!(exec_in(child, "cat /tmp/counter").trim(), counter);
        assert_eq!(exec_in(child, "cat note").trim(), "kept");
    }
    std::thread::sleep(Duration::from_millis(2500));
    for child in &children {
        let count: u64 = exec_in(child, "cat /tmp/count").trim().parse().unwrap();
        assert!(
            count > counted,
            "the counter continues in the child ({count})"
        );
    }
    // Independent: one child's writes and kills reach neither the source nor
    // its sibling.
    let (first, second) = (&children[0], &children[1]);
    exec_in(
        first,
        &format!("echo first > /tmp/branch && echo first > branch-note && kill {sleeper}"),
    );
    for other in [second.as_str(), id.as_str()] {
        assert_eq!(
            exec_in(other, "cat /tmp/branch branch-note 2>/dev/null; echo end").trim(),
            "end",
            "{other} sees nothing of the first child's writes"
        );
        assert_eq!(
            exec_in(other, &format!("kill -0 {sleeper} && echo alive")).trim(),
            "alive"
        );
    }
    assert!(exec("cat /tmp/count").trim().parse::<u64>().unwrap() >= counted_at_fork);
    // Each child's random generator was reseeded: siblings diverge.
    let random = |session: &str| exec_in(session, "head -c 32 /dev/urandom | od -An -tx1");
    assert_ne!(random(first), random(second));
    // Each child's egress is its own forward through the gateway.
    assert!(exec_in(second, "wget -q -T 20 -O - http://example.com/").contains("Example Domain"));
    // A child's receipts name the checkpoint it continues from.
    let child_receipt = root.join("child-receipt.json");
    ok(
        &root,
        &[
            "session",
            "exec",
            second,
            "--receipt",
            child_receipt.to_str().unwrap(),
            "--",
            "true",
        ],
    );
    let receipt: Value = serde_json::from_slice(&std::fs::read(&child_receipt).unwrap()).unwrap();
    assert_eq!(
        receipt["substrate"]["artifacts"]["checkpoint"],
        digest.as_str(),
        "{receipt}"
    );
    ok(
        &root,
        &["receipt", "verify", child_receipt.to_str().unwrap()],
    );
    // The checkpoint is released once every child runs.
    let checkpoints = || {
        std::fs::read_dir(vms.join(".checkpoints"))
            .map(|entries| entries.count())
            .unwrap_or(0)
    };
    assert_eq!(checkpoints(), 0, "the checkpoint is released");
    // Lineage and the children survive a controller restart.
    drop(server);
    server = serve();
    wait_for(&listen);
    assert_eq!(info(first)["lineage"]["parent_session_id"], id.as_str());
    assert_eq!(exec_in(first, "cat /tmp/branch").trim(), "first");
    // A checkpoint that cannot be written fails the fork's child, with its
    // reason, and the source runs on.
    let checkpoint_root = vms.join(".checkpoints");
    std::fs::create_dir_all(&checkpoint_root).unwrap();
    std::fs::set_permissions(
        &checkpoint_root,
        std::os::unix::fs::PermissionsExt::from_mode(0o555),
    )
    .unwrap();
    let refused = compute(&root, &["session", "fork", &id, "--count", "1", "--json"]);
    std::fs::set_permissions(
        &checkpoint_root,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    assert!(!refused.status.success());
    let listed: Value = serde_json::from_str(&ok(&root, &["session", "list", "--json"])).unwrap();
    let failed = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| &entry["session"])
        .filter(|session| session["lineage"].is_object() && session["status"] == "failed")
        .collect::<Vec<_>>();
    assert_eq!(failed.len(), 1, "{listed}");
    assert!(
        failed[0]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("checkpoint")
    );
    assert_eq!(
        exec(&format!("kill -0 {sleeper} && echo alive")).trim(),
        "alive",
        "the source runs on"
    );
    for child in &children {
        ok(&root, &["session", "destroy", child]);
    }
    eprintln!("fork (2 children, 1 vCPU, 256 MiB, one run, debug build): {forked_in:?}");

    // Interactive: commands on a terminal in the VM, driven through the CLI.
    // Standard input here is a pipe, so the CLI streams it as typed input.
    let terminal_receipt = root.join("terminal-receipt.json");
    let mut typed = std::process::Command::new(env!("CARGO_BIN_EXE_compute"))
        .current_dir(&root)
        .args(["session", "exec", &id, "--tty", "--receipt"])
        .arg(&terminal_receipt)
        .args(["--", "sh"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(
        typed.stdin.as_mut().unwrap(),
        b"tty; stty size; echo $((6*7)); exit 7\n",
    )
    .unwrap();
    drop(typed.stdin.take());
    let typed = typed.wait_with_output().unwrap();
    let screen = String::from_utf8_lossy(&typed.stdout);
    assert_eq!(typed.status.code(), Some(7), "{screen}");
    assert!(
        screen.contains("/dev/pts/"),
        "a terminal in the guest: {screen}"
    );
    assert!(screen.contains("24 80"), "{screen}");
    assert!(screen.contains("42"), "{screen}");
    // The same durable job and receipt as any command.
    ok(
        &root,
        &["receipt", "verify", terminal_receipt.to_str().unwrap()],
    );

    // Detached, driven by `session input`, then attached again: the same
    // command throughout, never started twice.
    let detached: Value = serde_json::from_str(&ok(
        &root,
        &["session", "exec", &id, "--tty", "--detach", "--", "sh"],
    ))
    .unwrap();
    let job = detached["job_id"].as_str().unwrap().to_owned();
    ok(
        &root,
        &["session", "input", &id, &job, "--data", "echo one\n"],
    );
    ok(
        &root,
        &["session", "input", &id, &job, "--resize", "30x100"],
    );
    ok(
        &root,
        &["session", "input", &id, &job, "--data", "stty size\n"],
    );
    let mut attached = std::process::Command::new(env!("CARGO_BIN_EXE_compute"))
        .current_dir(&root)
        .args(["session", "attach", &id, &job, "--from-start"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(attached.stdin.as_mut().unwrap(), b"exit 9\n").unwrap();
    drop(attached.stdin.take());
    let attached = attached.wait_with_output().unwrap();
    let screen = String::from_utf8_lossy(&attached.stdout);
    assert_eq!(attached.status.code(), Some(9), "{screen}");
    assert!(screen.contains("one"), "earlier output, replayed: {screen}");
    assert!(screen.contains("30 100"), "the resize reached it: {screen}");

    // A signal reaches the command's process group.
    let sleeping: Value = serde_json::from_str(&ok(
        &root,
        &[
            "session", "exec", &id, "--tty", "--detach", "--", "sleep", "1000",
        ],
    ))
    .unwrap();
    let job = sleeping["job_id"].as_str().unwrap().to_owned();
    ok(&root, &["session", "input", &id, &job, "--signal", "15"]);
    let ended = compute(&root, &["session", "attach", &id, &job, "--from-start"]);
    assert_eq!(ended.status.code(), Some(128 + 15));
    // Input for a command that has ended is refused.
    assert!(
        !compute(&root, &["session", "input", &id, &job, "--data", "x"])
            .status
            .success()
    );

    // A snapshot that cannot be written fails the stop and loses nothing.
    std::fs::create_dir(vm_directory.join("snapshot")).unwrap();
    let refused = compute(&root, &["session", "stop", &id]);
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&refused.stdout),
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(!refused.status.success(), "{said}");
    assert!(said.contains("still running, unchanged"), "{said}");
    assert!(
        !vm_directory.join("suspension.json").exists(),
        "no suspension is recorded for a snapshot that failed"
    );
    std::fs::remove_dir(vm_directory.join("snapshot")).unwrap();
    // The retried stop suspends it, and nothing was lost by the failure.
    ok(&root, &["session", "stop", &id]);
    ok(&root, &["session", "resume", &id]);
    assert_eq!(
        exec(&format!("kill -0 {sleeper} && echo alive")).trim(),
        "alive",
        "a failed suspension lost nothing"
    );
    assert_eq!(exec("cat /tmp/nonce").trim(), nonce);

    // The receipt says the command ran in this VM: the VMM, what it booted,
    // and the limits it was booted with.
    let receipt_path = root.join("vm-receipt.json");
    ok(
        &root,
        &[
            "session",
            "exec",
            &id,
            "--receipt",
            receipt_path.to_str().unwrap(),
            "--",
            "true",
        ],
    );
    let receipt: Value = serde_json::from_slice(&std::fs::read(&receipt_path).unwrap()).unwrap();
    let substrate = &receipt["substrate"];
    assert_eq!(substrate["kind"], "microvm", "{receipt}");
    assert!(
        substrate["backend"]
            .as_str()
            .unwrap()
            .starts_with("Firecracker"),
        "{substrate}"
    );
    assert!(
        substrate["artifacts"]["kernel"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert!(
        substrate["artifacts"]["rootfs"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert_eq!(substrate["limits"]["vcpus"], "1");
    assert_eq!(
        substrate["limits"]["memory_bytes"],
        (256u64 * 1024 * 1024).to_string()
    );
    assert_eq!(substrate["limits"]["network"], "egress-gateway-only");
    ok(
        &root,
        &["receipt", "verify", receipt_path.to_str().unwrap()],
    );

    // A damaged snapshot is refused: the machine is not cold-booted in its
    // place, and the session stays stopped with the reason.
    ok(&root, &["session", "stop", &id]);
    let memory = std::fs::OpenOptions::new()
        .write(true)
        .open(vm_directory.join("memory"))
        .unwrap();
    memory.set_len(4096).unwrap();
    let damaged = compute(&root, &["session", "resume", &id]);
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&damaged.stdout),
        String::from_utf8_lossy(&damaged.stderr)
    );
    assert!(said.contains("not cold-booted"), "{said}");
    let info: Value =
        serde_json::from_str(&ok(&root, &["session", "info", &id, "--json"])).unwrap();
    assert_eq!(info["status"], "stopped", "{info}");
    assert!(
        info["failure"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("not cold-booted")),
        "{info}"
    );
    assert!(!vm_directory.join("pid").exists(), "nothing was booted");

    ok(&root, &["session", "destroy", &id]);
    drop(server);
    let left = std::fs::read_dir(&vms)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("compute-vm-"))
        .collect::<Vec<_>>();
    assert!(left.is_empty(), "every VM and its disk are gone: {left:?}");
    assert_eq!(checkpoints(), 0, "no checkpoint is left");
    let _ = std::fs::remove_dir_all(&root);
}
