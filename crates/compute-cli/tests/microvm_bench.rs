//! Lifecycle latency of microVM sessions, measured through the real
//! `compute serve --session-provider microvm` and CLI on a node with KVM.
//!
//! Each run creates a session, runs commands, suspends and resumes it, forks
//! it, and destroys everything. Two views of every operation are kept
//! apart: the CLI's wall time (control plane: the CLI process, HTTP,
//! authorization, the durable job, admission) and the provider's own phase
//! timings (`lifecycle.jsonl` beside each VM: VMM start, snapshot, load,
//! guest readiness). The first run is reported separately as cold.
//!
//! A benchmark, not a test: ignored unless asked for, and it needs what
//! `tests/microvm.rs` needs (COMPUTE_VM_KERNEL, COMPUTE_VM_ROOTFS, /dev/kvm,
//! COMPUTE_FIRECRACKER). COMPUTE_BENCH_RUNS (default 10) sets the runs;
//! COMPUTE_BENCH_OUT where the JSON goes.
//!
//! ```sh
//! cargo test --release -p compute-cli --test microvm_bench -- --ignored --nocapture
//! ```

#[path = "support/targets.rs"]
mod targets;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

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

fn ok(root: &Path, arguments: &[&str]) -> String {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_compute"))
        .current_dir(root)
        .env_remove("COMPUTE_POOL_CONFIG")
        .env_remove("COMPUTE_CAPABILITY_CACHE")
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "compute {arguments:?}:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// Wall time of one CLI invocation, and its output.
fn timed(root: &Path, arguments: &[&str]) -> (f64, String) {
    let started = Instant::now();
    let output = ok(root, arguments);
    (started.elapsed().as_secs_f64() * 1000.0, output)
}

/// Order statistics of a sample. The p95 is reported only with at least 20
/// samples; below that it would only be the maximum by another name.
fn summary(samples: &[f64]) -> Value {
    if samples.is_empty() {
        return json!({ "n": 0 });
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |quantile: f64| {
        let rank = (quantile * (sorted.len() - 1) as f64).round() as usize;
        sorted[rank]
    };
    json!({
        "n": sorted.len(),
        "min_ms": sorted[0],
        "median_ms": at(0.5),
        "mean_ms": sorted.iter().sum::<f64>() / sorted.len() as f64,
        "p95_ms": (sorted.len() >= 20).then(|| at(0.95)),
        "max_ms": sorted[sorted.len() - 1],
    })
}

/// The provider's phase timings for one VM, by operation.
fn phases(vm: &Path) -> Vec<Value> {
    std::fs::read_to_string(vm.join("lifecycle.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn command_line(program: &str, arguments: &[&str]) -> String {
    std::process::Command::new(program)
        .args(arguments)
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_default()
}

#[test]
#[ignore = "benchmark: needs KVM; run with --ignored"]
fn microvm_lifecycle_latency() {
    let (Ok(kernel), Ok(rootfs)) = (
        std::env::var("COMPUTE_VM_KERNEL"),
        std::env::var("COMPUTE_VM_ROOTFS"),
    ) else {
        panic!("COMPUTE_VM_KERNEL and COMPUTE_VM_ROOTFS are required");
    };
    assert!(Path::new("/dev/kvm").exists(), "/dev/kvm is required");
    let firecracker = std::env::var("COMPUTE_FIRECRACKER").unwrap_or_else(|_| "firecracker".into());
    let runs: usize = std::env::var("COMPUTE_BENCH_RUNS")
        .ok()
        .and_then(|runs| runs.parse().ok())
        .unwrap_or(10)
        .max(2);
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().to_path_buf();
    let listen = format!("127.0.0.1:{}", free_port());
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
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while std::net::TcpStream::connect(&listen).is_err() {
        assert!(Instant::now() < deadline, "the server never answered");
        std::thread::sleep(Duration::from_millis(50));
    }
    let vms = root.join("sessions/microvms");

    // CLI wall time, by operation, cold (first run) and warm (the rest).
    let mut cli: BTreeMap<&str, (Vec<f64>, Vec<f64>)> = BTreeMap::new();
    let mut add = |operation: &'static str, run: usize, ms: f64| {
        let entry = cli.entry(operation).or_default();
        if run == 0 { &mut entry.0 } else { &mut entry.1 }.push(ms);
    };
    // Provider phases, by "operation.phase", warm runs only.
    let mut backend: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut guest_kernel = String::new();
    for run in 0..runs {
        let (create_ms, created) = timed(
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
        );
        add("create_to_ready", run, create_ms);
        let created: Value = serde_json::from_str(&created).unwrap();
        let id = created["session"]["session_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let exec = |session: &str| timed(&root, &["session", "exec", session, "--", "true"]).0;
        add("first_exec", run, exec(&id));
        for _ in 0..5 {
            add("exec", run, exec(&id));
        }
        if guest_kernel.is_empty() {
            guest_kernel = ok(&root, &["session", "exec", &id, "--", "uname", "-r"])
                .trim()
                .to_owned();
        }
        add("suspend", run, timed(&root, &["session", "stop", &id]).0);
        add(
            "resume_to_ready",
            run,
            timed(&root, &["session", "resume", &id]).0,
        );
        add("exec_after_resume", run, exec(&id));
        let (fork_ms, forked) = timed(&root, &["session", "fork", &id, "--count", "1", "--json"]);
        add("fork_one_child_to_ready", run, fork_ms);
        let forked: Value = serde_json::from_str(&forked).unwrap();
        let child = forked["children"][0]["session_id"]
            .as_str()
            .unwrap()
            .to_owned();
        add("exec_in_child", run, exec(&child));
        // The provider's own phases, before the VMs (and their logs) go.
        if run > 0 {
            for session in [&id, &child] {
                let info: Value =
                    serde_json::from_str(&ok(&root, &["session", "info", session, "--json"]))
                        .unwrap();
                let vm = vms.join(info["provider_session_id"].as_str().unwrap());
                for entry in phases(&vm) {
                    let operation = entry["operation"].as_str().unwrap_or("?").to_owned();
                    for (key, value) in entry.as_object().unwrap() {
                        if let (Some(phase), Some(ms)) = (key.strip_suffix("_ms"), value.as_f64()) {
                            backend
                                .entry(format!("{operation}.{phase}"))
                                .or_default()
                                .push(ms);
                        }
                    }
                }
            }
        }
        add(
            "destroy",
            run,
            timed(&root, &["session", "destroy", &child]).0,
        );
        add("destroy", run, timed(&root, &["session", "destroy", &id]).0);
    }

    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .unwrap_or_default()
        .lines()
        .find_map(|line| line.strip_prefix("model name"))
        .map(|name| name.trim_start_matches([' ', '\t', ':']).to_owned())
        .unwrap_or_default();
    let report = json!({
        "format": "compute.microvm-bench@1",
        "measured_at": chrono::Utc::now().to_rfc3339(),
        "conditions": {
            "runs": runs,
            "cold_run": "the first run (the VMM binary, kernel, and rootfs not yet in the page cache)",
            "build": if cfg!(debug_assertions) { "debug" } else { "release" },
            "cpu": cpu,
            "host_cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
            "host_kernel": command_line("uname", &["-r"]),
            "vmm": command_line(&firecracker, &["--version"]).lines().next().unwrap_or_default(),
            "guest_kernel": guest_kernel,
            "rootfs_bytes": std::fs::metadata(&rootfs).map(|meta| meta.len()).unwrap_or(0),
            "vm": { "vcpus": 1, "memory_mib": 256, "network": "network" },
            "note": "CLI times include the CLI process, HTTP, authorization, the durable job, and admission; backend phases are the provider's own, from lifecycle.jsonl",
        },
        "cli": cli.iter().map(|(operation, (cold, warm))| {
            (operation.to_string(), json!({ "cold_ms": cold.first(), "warm": summary(warm) }))
        }).collect::<serde_json::Map<_, _>>(),
        "backend_warm": backend.iter().map(|(phase, samples)| {
            (phase.clone(), summary(samples))
        }).collect::<serde_json::Map<_, _>>(),
    });
    let out = std::env::var("COMPUTE_BENCH_OUT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| root.join("microvm-bench.json"));
    std::fs::write(&out, serde_json::to_vec_pretty(&report).unwrap()).unwrap();

    println!("\nmicroVM lifecycle latency ({runs} runs; warm = runs 2..{runs}; ms)");
    println!(
        "{}",
        serde_json::to_string_pretty(&report["conditions"]).unwrap()
    );
    println!(
        "\n{:<26} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "CLI (end to end)", "cold", "median", "p95", "max", "n"
    );
    for (operation, (cold, warm)) in &cli {
        let stats = summary(warm);
        let cell = |key: &str| {
            stats[key]
                .as_f64()
                .map_or("-".into(), |ms| format!("{ms:.0}"))
        };
        println!(
            "{operation:<26} {:>9} {:>9} {:>9} {:>9} {:>9}",
            cold.first().map_or("-".into(), |ms| format!("{ms:.0}")),
            cell("median_ms"),
            cell("p95_ms"),
            cell("max_ms"),
            stats["n"],
        );
    }
    println!(
        "\n{:<26} {:>9} {:>9} {:>9} {:>9}",
        "backend phase (warm)", "median", "p95", "max", "n"
    );
    for (phase, samples) in &backend {
        let stats = summary(samples);
        let cell = |key: &str| {
            stats[key]
                .as_f64()
                .map_or("-".into(), |ms| format!("{ms:.1}"))
        };
        println!(
            "{phase:<26} {:>9} {:>9} {:>9} {:>9}",
            cell("median_ms"),
            cell("p95_ms"),
            cell("max_ms"),
            stats["n"],
        );
    }
    println!("\nwritten to {}", out.display());
}
