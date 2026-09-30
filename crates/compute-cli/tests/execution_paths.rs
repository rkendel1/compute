//! Every way Compute executes software is accounted for
//! (docs/architecture.md, "Every way Compute executes software").
//!
//! - `compute run` is ephemeral local execution: it runs on the caller's
//!   machine, in the CLI's process tree, and leaves no durable state, no
//!   deployment, no endpoint, and nothing to recover — only the receipt
//!   the caller asks for.
//! - Every place in the source that spawns a process, starts a supervised
//!   workload, serves the provider protocol, or dispatches a workload is on
//!   an allowlist with the reason it exists. A new execution path fails
//!   here until it is classified in the architecture document: no third
//!   execution architecture can appear unnoticed.

#[path = "support/runtimes.rs"]
mod runtimes;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn files(root: &Path) -> Vec<PathBuf> {
    let mut found = vec![];
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(files(&path));
            } else {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

fn run(work: &Path, home: &Path, arguments: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_compute"));
    runtimes::with_fixture_runtimes(&mut command);
    command
        .current_dir(work)
        .env("COMPUTE_HOME", home)
        // No control plane: nothing listens here, and `compute run` must
        // not need one.
        .env("COMPUTE_DAEMON", "http://127.0.0.1:9")
        .env_remove("COMPUTE_POOL_CONFIG")
        .env_remove("COMPUTE_DAEMON_TOKEN")
        .args(arguments)
        .output()
        .unwrap()
}

#[test]
fn compute_run_is_ephemeral_local_execution() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let work = root.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("main.py"), "print('ran here')\n").unwrap();
    let receipt = root.path().join("receipt.json");
    let first = run(
        &work,
        &home,
        &[
            "run",
            "main.py",
            "--network",
            "network",
            "--receipt",
            receipt.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(first.status.success(), "{first:?}");
    let result: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(result["status"], "completed", "{result:#}");
    assert_eq!(result["stdout"]["text"], "ran here\n");

    // No durable state anywhere: no Compute home, no control state, no
    // daemon or target started, nothing in the working directory but its
    // own files. The receipt is the one the caller asked for.
    assert!(!home.exists(), "compute run created {}", home.display());
    let written = files(root.path())
        .into_iter()
        .map(|path| {
            path.strip_prefix(root.path())
                .unwrap()
                .display()
                .to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(written, ["receipt.json", "work/main.py"]);

    // Local, non-deployment evidence: this machine's provider, placed by
    // the caller's own placement, bound to no environment, deployment, or
    // application.
    let evidence: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
    assert_eq!(evidence["provider_protocol"], "compute.local@1");
    assert_eq!(evidence["provider"]["kind"], "local");
    assert!(
        evidence.get("scope").is_none_or(serde_json::Value::is_null),
        "{evidence:#}"
    );
    assert!(
        evidence
            .get("application")
            .is_none_or(serde_json::Value::is_null),
        "{evidence:#}"
    );
    let verified = run(
        &work,
        &home,
        &["receipt", "verify", receipt.to_str().unwrap()],
    );
    assert!(verified.status.success(), "{verified:?}");

    // Nothing is remembered: running it again is a new, unrelated execution.
    let second = run(
        &work,
        &home,
        &["run", "main.py", "--network", "network", "--json"],
    );
    let again: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_ne!(again["execution_id"], result["execution_id"]);
    assert!(!home.exists());
}

/// Where a pattern may appear in `crates/*/src`, how often, and why. The
/// counts are exact: an added site fails until it is classified here and in
/// docs/architecture.md; a removed one fails until the list is updated.
struct Allowed {
    file: &'static str,
    count: usize,
    why: &'static str,
}

/// Whether `line` contains one of `needles` as a whole call: not preceded
/// by an identifier character (`SessionCommand::new(` is not a process).
fn matches(line: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| {
        line.match_indices(needle).any(|(at, _)| {
            !line[..at]
                .chars()
                .next_back()
                .is_some_and(|before| before.is_alphanumeric() || before == '_')
        })
    })
}

fn sites(needles: &[&str]) -> BTreeMap<String, usize> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut found = BTreeMap::new();
    for crate_dir in std::fs::read_dir(root.join("crates")).unwrap().flatten() {
        for file in files(&crate_dir.path().join("src")) {
            if file.extension().is_none_or(|extension| extension != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&file).unwrap();
            let hits = text
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .filter(|line| matches(line, needles))
                .count();
            if hits > 0 {
                let name = file.strip_prefix(&root).unwrap().display().to_string();
                found.insert(name, hits);
            }
        }
    }
    found
}

fn check(what: &str, needles: &[&str], allowed: &[Allowed]) {
    let actual = sites(needles);
    let expected = allowed
        .iter()
        .map(|site| {
            assert!(!site.why.is_empty());
            (site.file.to_owned(), site.count)
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        actual, expected,
        "{what}: the sites changed. Classify the new execution path in docs/architecture.md \
         (\"Every way Compute executes software\") and in this allowlist."
    );
}

#[test]
fn every_execution_site_is_classified() {
    // Processes. Workloads run only through the process runtime (and
    // wasmtime, in process); every other spawn is Compute running itself or
    // a tool, never a workload.
    check(
        "process spawns",
        &["Command::new("],
        &[
            Allowed {
                file: "crates/compute-runtime-process/src/lib.rs",
                count: 4,
                why: "THE workload process engine (probes and the workload itself), behind LocalProvider",
            },
            Allowed {
                file: "crates/compute-provider/src/containers.rs",
                count: 1,
                why: "the container session substrate of a target (docker/podman), inside a Computer",
            },
            Allowed {
                file: "crates/compute-provider/src/runtime.rs",
                count: 3,
                why: "runtime acquisition and verification (curl, archive tools, the runtime's own --version)",
            },
            Allowed {
                file: "crates/compute-core/src/executables.rs",
                count: 2,
                why: "its unit test: running a file still open for writing, then once closed",
            },
            Allowed {
                file: "crates/compute-core/src/host.rs",
                count: 1,
                why: "host capability probe",
            },
            Allowed {
                file: "crates/compute-core/src/application_artifact.rs",
                count: 1,
                why: "fetching an application artifact by URL (curl)",
            },
            Allowed {
                file: "crates/compute-environment/src/dataplane.rs",
                count: 1,
                why: "the daemon-host supervisor process (`compute supervisor`) for node environments: G-ARCH-5",
            },
            Allowed {
                file: "crates/compute-environment/src/upgrade.rs",
                count: 1,
                why: "starting the upgraded controller",
            },
            Allowed {
                file: "crates/compute-cli/src/environment_cmd.rs",
                count: 2,
                why: "starting the daemon detached, and its supervisor",
            },
            Allowed {
                file: "crates/compute-cli/src/launch_cmd.rs",
                count: 4,
                why: "`compute up`: the target (`compute serve`), the daemon, the browser, `compute down`",
            },
            Allowed {
                file: "crates/compute-cli/src/session_cmd.rs",
                count: 1,
                why: "a SessionCommand for a target session (no local process)",
            },
            Allowed {
                file: "crates/compute-cli/src/certification.rs",
                count: 2,
                why: "distribution certification: running the built binary and its fixtures",
            },
            Allowed {
                file: "crates/compute-cli/src/distribution.rs",
                count: 9,
                why: "building and verifying a distribution (curl, npm, scripts, and read-only probes of the built binary)",
            },
            Allowed {
                file: "crates/compute-runtime-conformance/src/lib.rs",
                count: 4,
                why: "compiling conformance fixtures (javac, jar, dotnet, cc)",
            },
            Allowed {
                file: "crates/compute-state-feltdb/src/upgrade.rs",
                count: 1,
                why: "the FeltDB backup verifier",
            },
            Allowed {
                file: "crates/compute-project/src/pax.rs",
                count: 1,
                why: "observing a project with the external, read-only `pax` executable; never a workload",
            },
        ],
    );
    // Supervised workloads on the daemon host: only the node model.
    check(
        "daemon-host supervision",
        &["data_plane().start(", "plane.start(manifest"],
        &[
            Allowed {
                file: "crates/compute-environment/src/daemon/execute.rs",
                count: 1,
                why: "a node-environment service start (G-ARCH-5)",
            },
            Allowed {
                file: "crates/compute-environment/src/dataplane.rs",
                count: 1,
                why: "the supervisor protocol forwarding a start",
            },
        ],
    );
    // Serving the provider protocol (`compute.remote@1`): a target, and the
    // daemon's own node as a caller's provider.
    check(
        "provider services",
        &["RemoteService::new("],
        &[
            Allowed {
                file: "crates/compute-provider/src/lib.rs",
                count: 1,
                why: "`compute serve`: a target (sessions, jobs, runs)",
            },
            Allowed {
                file: "crates/compute-environment/src/daemon/mod.rs",
                count: 1,
                why: "the daemon's /compute/* service: one-shot runs and jobs on its node",
            },
        ],
    );
    // Dispatching one workload to a provider.
    check(
        "workload dispatch",
        &["dispatch::execute(", "execute_controlled("],
        &[
            Allowed {
                file: "crates/compute-core/src/lib.rs",
                count: 1,
                why: "the runtime trait's default",
            },
            Allowed {
                file: "crates/compute-runtime/src/lib.rs",
                count: 1,
                why: "the runtime trait",
            },
            Allowed {
                file: "crates/compute-runtime-process/src/lib.rs",
                count: 3,
                why: "the process runtime",
            },
            Allowed {
                file: "crates/compute-runtime-wasm/src/lib.rs",
                count: 1,
                why: "the WASM runtime",
            },
            Allowed {
                file: "crates/compute-provider/src/lib.rs",
                count: 1,
                why: "LocalProvider",
            },
            Allowed {
                file: "crates/compute-cli/src/pool.rs",
                count: 1,
                why: "`compute run` / `compute pool run`: ephemeral one-shot execution on the placed provider",
            },
            Allowed {
                file: "crates/compute-environment/src/daemon/execute.rs",
                count: 1,
                why: "a node-environment task (G-ARCH-5)",
            },
            Allowed {
                file: "crates/compute-environment/src/dataplane.rs",
                count: 1,
                why: "the supervisor running a node service (G-ARCH-5)",
            },
            Allowed {
                file: "crates/compute-cli/src/placement_certification.rs",
                count: 4,
                why: "placement certification's own harness",
            },
            Allowed {
                file: "crates/compute-cli/src/policy_certification.rs",
                count: 2,
                why: "policy certification's own harness",
            },
        ],
    );
}
