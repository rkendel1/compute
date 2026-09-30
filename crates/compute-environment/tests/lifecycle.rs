//! Lifecycle: what stop, destroy, replace, and fork do to the processes an
//! environment owns, against a real target and real process trees.
//!
//! The contract under test (`docs/lifecycle.md`): destroy and stop return
//! success only once every process the machine owns, however deep in its
//! tree, is confirmed gone; a survivor is `termination_failed`, a machine
//! that cannot be removed is `destruction_failed`, and neither is ever
//! reported as `destroyed` or `stopped`. Stop keeps persistent state and no
//! process. Replace and fork give the new environment its own lifecycle.
#![cfg(target_os = "linux")]

mod common;
#[path = "common/target.rs"]
mod target;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use compute_core::{
    ComputerLifecycle, ComputerRequirements, ComputerStatus, EnvironmentContents, NetworkPolicy,
    ProcessDesired, ProcessKind, ProcessSpec, SessionCommand,
};
use compute_environment::*;
use compute_provider::processes::alive;
use compute_state::StateStore;
use compute_state_file::FileState;
use compute_state_memory::MemoryState;
use target::*;

/// A process that starts a tree: a child, a grandchild, and a descendant in
/// its own session. Every pid it starts is recorded, under the name of the
/// workspace it runs in, in `$PIDS`.
const TREE: &str = r#"
P="$PIDS/$(basename "$COMPUTE_SESSION_WORKSPACE")"
sleep 300 & echo $! >> "$P"
sh -c 'echo $$ >> "$0"; sleep 301 & echo $! >> "$0"; sleep 302' "$P" &
setsid sleep 304 >/dev/null 2>&1 & echo $! >> "$P"
wait
"#;

fn tree(pids: &Path) -> ProcessSpec {
    ProcessSpec {
        name: "tree".into(),
        kind: ProcessKind::Service,
        runtime: None,
        command: vec!["sh".into(), "-c".into(), TREE.into()],
        repository: None,
        env: BTreeMap::from([("PIDS".into(), pids.display().to_string())]),
        desired: ProcessDesired::Running,
        port: None,
        restart: 0,
        readiness: None,
        restart_policy: Default::default(),
        max_restarts: compute_core::DEFAULT_MAX_RESTARTS,
    }
}

fn requirements() -> ComputerRequirements {
    ComputerRequirements {
        cpu_count: Some(1),
        memory_bytes: Some(64 << 20),
        network: NetworkPolicy::Network,
        ..Default::default()
    }
}

async fn create(daemon: &Arc<Daemon>, name: &str, pids: &Path) {
    daemon
        .create_computer_environment(
            ComputerEnvironmentDefinition {
                name: name.into(),
                desired_state: DesiredState::Running,
                env: BTreeMap::new(),
                policy: None,
                computer: ComputerRequest {
                    lifecycle: ComputerLifecycle::Persistent,
                    requirements: requirements(),
                    target: None,
                    ttl_seconds: None,
                },
                contents: EnvironmentContents {
                    processes: vec![tree(pids)],
                    ..Default::default()
                },
                recipe: None,
            },
            "alice",
        )
        .await
        .unwrap();
}

async fn wait_until<T>(what: &str, mut check: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(value) = check().await {
            return value;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn computer_where(
    daemon: &Arc<Daemon>,
    name: &str,
    what: &str,
    wanted: impl Fn(&ComputerView) -> bool,
) -> ComputerView {
    wait_until(what, async || {
        daemon.computer(name).await.ok().filter(&wanted)
    })
    .await
}

/// The pids a machine's tree recorded, once the tree is all running.
async fn recorded(pids: &Path, machine: &ComputerView) -> Vec<u32> {
    let file = pids.join(machine.machine.as_ref().unwrap().resource.clone().unwrap());
    wait_until("the process tree to start", async || {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let found = text
            .lines()
            .filter_map(|line| line.trim().parse::<u32>().ok())
            .collect::<Vec<_>>();
        (found.len() >= 4 && found.iter().all(|pid| alive(*pid))).then_some(found)
    })
    .await
}

fn all_gone(pids: &[u32]) -> bool {
    pids.iter().all(|pid| !alive(*pid))
}

async fn running(daemon: &Arc<Daemon>, name: &str) -> ComputerView {
    computer_where(daemon, name, "the computer to run", |view| {
        view.status == ComputerStatus::Running
            && view
                .observed
                .processes
                .get("tree")
                .is_some_and(|process| process.pid.is_some())
    })
    .await
}

async fn sh(daemon: &Arc<Daemon>, name: &str, script: &str) -> String {
    common::wait_admitting(daemon, name).await;
    let exec = daemon
        .computer_exec(
            name,
            "alice",
            SessionCommand::new(vec!["sh".into(), "-c".into(), script.into()]),
        )
        .await
        .unwrap();
    let job = wait_until("the command", async || {
        daemon
            .computer_job(name, "alice", &exec.job_id)
            .await
            .ok()
            .filter(|job| job.result.is_some())
    })
    .await;
    job.result.unwrap().result.stdout.text
}

fn on_disk(path: &Path) -> Arc<dyn StateStore> {
    Arc::new(FileState::open(path).unwrap())
}

// ---- destroy -----------------------------------------------------------------

/// `destroy` is `destroyed` only after the whole tree, including a
/// descendant that left its parent's session, is confirmed gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn destroy_terminates_the_whole_process_tree_before_reporting_destroyed() {
    let target = Target::start();
    let pids = tempfile::tempdir().unwrap();
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;
    create(&daemon, "dev", pids.path()).await;
    let machine = running(&daemon, "dev").await;
    let tree = recorded(pids.path(), &machine).await;

    daemon.destroy_computer("dev", "alice").await.unwrap();
    let destroyed = computer_where(&daemon, "dev", "the computer to be destroyed", |view| {
        view.status == ComputerStatus::Destroyed
    })
    .await;
    assert!(destroyed.failure.is_none());
    // The moment it says destroyed, nothing it owned is running.
    assert!(
        all_gone(&tree),
        "a process outlived its destroyed environment: {tree:?}"
    );
    assert!(
        destroyed
            .observed
            .processes
            .values()
            .all(|process| process.pid.is_none())
    );
    daemon.shutdown().await;
}

/// A destroy the target cannot confirm is not a destroy. What is true is on
/// the record, survives a controller restart, and the destroy completes when
/// the target can confirm it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_destroy_that_cannot_be_confirmed_is_never_reported_destroyed() {
    let target = Target::start();
    let pids = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let path = state.path().join("control-state.json");
    let (daemon, _node) = start_daemon(on_disk(&path), Some(pool(&target))).await;
    create(&daemon, "dev", pids.path()).await;
    let machine = running(&daemon, "dev").await;
    let tree = recorded(pids.path(), &machine).await;

    // A survivor the target cannot prove gone.
    target.faults.termination.store(true, Ordering::SeqCst);
    daemon.destroy_computer("dev", "alice").await.unwrap();
    let held = computer_where(
        &daemon,
        "dev",
        "termination_failed to be recorded",
        |view| {
            view.failure
                .as_ref()
                .is_some_and(|failure| failure.code == "termination_failed")
        },
    )
    .await;
    assert_eq!(
        held.status,
        ComputerStatus::Destroying,
        "reported early: {held:#?}"
    );
    assert_eq!(held.failure.as_ref().unwrap().phase, "teardown");

    // The machine cannot be removed even once its processes are: another
    // answer, not the same one.
    target.faults.termination.store(false, Ordering::SeqCst);
    target.faults.destruction.store(true, Ordering::SeqCst);
    let held = computer_where(
        &daemon,
        "dev",
        "destruction_failed to be recorded",
        |view| {
            view.failure
                .as_ref()
                .is_some_and(|failure| failure.code == "destruction_failed")
        },
    )
    .await;
    assert_eq!(held.status, ComputerStatus::Destroying);

    // The controller restarts mid-destroy: nothing claims it is gone.
    daemon.shutdown().await;
    let (daemon, _node) = start_daemon(on_disk(&path), Some(pool(&target))).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let after = daemon.computer("dev").await.unwrap();
    assert_eq!(after.status, ComputerStatus::Destroying, "{after:#?}");

    // The target recovers. Only now is it destroyed, with nothing left.
    target.faults.destruction.store(false, Ordering::SeqCst);
    let done = computer_where(&daemon, "dev", "the destroy to complete", |view| {
        view.status == ComputerStatus::Destroyed
    })
    .await;
    assert!(done.failure.is_none());
    assert!(all_gone(&tree));
    daemon.shutdown().await;
}

// ---- stop and start ------------------------------------------------------------

/// Stop keeps the persistent state and no process. Start brings the same
/// environment back: the same workspace, new processes, new pids.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_preserves_state_not_processes_and_a_stop_is_confirmed() {
    let target = Target::start();
    let pids = tempfile::tempdir().unwrap();
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;
    create(&daemon, "dev", pids.path()).await;
    let machine = running(&daemon, "dev").await;
    let first_tree = recorded(pids.path(), &machine).await;
    let first_pid = machine.observed.processes["tree"].pid;
    sh(
        &daemon,
        "dev",
        "echo persistent > state.txt; echo transient > /dev/null",
    )
    .await;

    // A stop the target cannot confirm leaves the computer stopping, not
    // stopped, and the record says why.
    target.faults.termination.store(true, Ordering::SeqCst);
    daemon
        .set_environment_state("dev", DesiredState::Stopped, false)
        .await
        .unwrap();
    let held = computer_where(
        &daemon,
        "dev",
        "termination_failed to be recorded",
        |view| {
            view.failure
                .as_ref()
                .is_some_and(|failure| failure.code == "termination_failed")
        },
    )
    .await;
    assert_eq!(held.status, ComputerStatus::Stopping, "{held:#?}");
    target.faults.termination.store(false, Ordering::SeqCst);

    let stopped = computer_where(&daemon, "dev", "the computer to stop", |view| {
        view.status == ComputerStatus::Stopped
    })
    .await;
    // Stopped: no process of the machine is running, and none is recorded.
    assert!(all_gone(&first_tree), "a stopped computer has processes");
    assert!(
        stopped
            .observed
            .processes
            .values()
            .all(|process| process.pid.is_none())
    );

    // Start: the same environment. The workspace is the one it was. (The
    // tree records its pids by workspace: start a fresh record.)
    let _ = std::fs::remove_file(
        pids.path()
            .join(machine.machine.as_ref().unwrap().resource.clone().unwrap()),
    );
    // Start: the same environment. The workspace is the one it was.
    daemon
        .set_environment_state("dev", DesiredState::Running, false)
        .await
        .unwrap();
    let restarted = wait_until("the tree to run again", async || {
        let view = daemon.computer("dev").await.ok()?;
        let pid = view.observed.processes.get("tree")?.pid?;
        (view.status == ComputerStatus::Running && Some(pid) != first_pid).then_some(view)
    })
    .await;
    assert_eq!(restarted.environment_id, machine.environment_id);
    assert_eq!(
        restarted.machine.as_ref().unwrap().resource,
        machine.machine.as_ref().unwrap().resource
    );
    assert_eq!(sh(&daemon, "dev", "cat state.txt").await, "persistent\n");
    // Processes did not survive as processes: the new tree is new pids.
    let second_tree = recorded(pids.path(), &restarted).await;
    assert!(second_tree.iter().all(|pid| !first_tree.contains(pid)));

    daemon.destroy_computer("dev", "alice").await.unwrap();
    computer_where(&daemon, "dev", "destroyed", |view| {
        view.status == ComputerStatus::Destroyed
    })
    .await;
    assert!(all_gone(&second_tree));
    daemon.shutdown().await;
}

// ---- replace -------------------------------------------------------------------

/// Replace: state carries over, the old machine's whole tree is ended, and
/// the new machine runs its own tree and is managed on its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replace_ends_the_old_tree_and_the_new_machine_is_managed_on_its_own() {
    let target = Target::start();
    let pids = tempfile::tempdir().unwrap();
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;
    create(&daemon, "dev", pids.path()).await;
    let old = running(&daemon, "dev").await;
    let old_tree = recorded(pids.path(), &old).await;
    sh(&daemon, "dev", "echo carried > state.txt").await;

    let replaced = daemon
        .replace_computer(
            "dev",
            "alice",
            ComputerRequirements {
                cpu_count: Some(2),
                ..requirements()
            },
        )
        .await
        .unwrap();
    assert_ne!(
        replaced.machine.as_ref().unwrap().resource,
        old.machine.as_ref().unwrap().resource
    );
    // The old machine's processes did not transfer and were not orphaned.
    wait_until("the old tree to end", async || {
        all_gone(&old_tree).then_some(())
    })
    .await;
    assert_eq!(sh(&daemon, "dev", "cat state.txt").await, "carried\n");

    // The new machine has its own tree, its own lifecycle.
    let new = running(&daemon, "dev").await;
    let new_tree = recorded(pids.path(), &new).await;
    assert!(new_tree.iter().all(|pid| !old_tree.contains(pid)));
    daemon.destroy_computer("dev", "alice").await.unwrap();
    computer_where(&daemon, "dev", "destroyed", |view| {
        view.status == ComputerStatus::Destroyed
    })
    .await;
    assert!(all_gone(&new_tree));
    daemon.shutdown().await;
}

// ---- fork ------------------------------------------------------------------------

/// A fork is independent: its own machine, tree, and lifecycle. Destroying
/// either leaves the other's processes and state alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fork_has_a_lifecycle_of_its_own() {
    let target = Target::start();
    let pids = tempfile::tempdir().unwrap();
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;
    create(&daemon, "alpha", pids.path()).await;
    let alpha = running(&daemon, "alpha").await;
    let alpha_tree = recorded(pids.path(), &alpha).await;
    sh(&daemon, "alpha", "echo forked > state.txt").await;

    let fork = daemon
        .fork_environment(
            "alpha",
            "alice",
            ForkRequest {
                name: "beta".into(),
                target: None,
                copy_config: false,
            },
        )
        .await
        .unwrap();
    assert!(fork.workspace_verified);
    let beta = running(&daemon, "beta").await;
    let beta_tree = recorded(pids.path(), &beta).await;
    assert_eq!(sh(&daemon, "beta", "cat state.txt").await, "forked\n");
    // Nothing of alpha is attached to beta.
    assert!(beta_tree.iter().all(|pid| !alpha_tree.contains(pid)));
    assert_ne!(
        alpha.machine.as_ref().unwrap().resource,
        beta.machine.as_ref().unwrap().resource
    );

    // Destroying beta leaves alpha's whole tree running.
    daemon.destroy_computer("beta", "alice").await.unwrap();
    computer_where(&daemon, "beta", "beta destroyed", |view| {
        view.status == ComputerStatus::Destroyed
    })
    .await;
    assert!(all_gone(&beta_tree));
    assert!(
        alpha_tree.iter().all(|pid| alive(*pid)),
        "destroying the fork touched the original"
    );
    assert_eq!(sh(&daemon, "alpha", "cat state.txt").await, "forked\n");

    // And the other way round.
    let again = daemon
        .fork_environment(
            "alpha",
            "alice",
            ForkRequest {
                name: "gamma".into(),
                target: None,
                copy_config: false,
            },
        )
        .await
        .unwrap();
    assert!(again.workspace_verified);
    let gamma = running(&daemon, "gamma").await;
    let gamma_tree = recorded(pids.path(), &gamma).await;
    daemon.destroy_computer("alpha", "alice").await.unwrap();
    computer_where(&daemon, "alpha", "alpha destroyed", |view| {
        view.status == ComputerStatus::Destroyed
    })
    .await;
    assert!(all_gone(&alpha_tree));
    assert!(
        gamma_tree.iter().all(|pid| alive(*pid)),
        "destroying the original touched the fork"
    );
    assert_eq!(sh(&daemon, "gamma", "cat state.txt").await, "forked\n");
    daemon.destroy_computer("gamma", "alice").await.unwrap();
    computer_where(&daemon, "gamma", "gamma destroyed", |view| {
        view.status == ComputerStatus::Destroyed
    })
    .await;
    assert!(all_gone(&gamma_tree));
    daemon.shutdown().await;
}

// ---- ownership ---------------------------------------------------------------------

/// Compute knows which processes belong to which environment: each is
/// recorded on its environment's computer, with the machine that holds it,
/// and the machine is what a process's owner marker names.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_process_belongs_to_one_environment_and_machine() {
    let target = Target::start();
    let pids = tempfile::tempdir().unwrap();
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;
    create(&daemon, "one", pids.path()).await;
    create(&daemon, "two", pids.path()).await;
    let one = running(&daemon, "one").await;
    let two = running(&daemon, "two").await;
    assert_ne!(one.environment_id, two.environment_id);
    let one_tree = recorded(pids.path(), &one).await;
    let two_tree = recorded(pids.path(), &two).await;
    assert!(one_tree.iter().all(|pid| !two_tree.contains(pid)));
    // The record's pid is a process the machine owns: its environment names
    // the machine's workspace.
    for (view, tree) in [(&one, &one_tree), (&two, &two_tree)] {
        let pid = view.observed.processes["tree"].pid.unwrap();
        let marker = format!(
            "COMPUTE_SESSION_WORKSPACE={}",
            target
                .workspaces
                .path()
                .join(view.machine.as_ref().unwrap().resource.as_ref().unwrap())
                .display()
        );
        let environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap();
        assert!(
            environ
                .split(|byte| *byte == 0)
                .any(|entry| entry == marker.as_bytes())
        );
        assert!(tree.iter().all(|pid| alive(*pid)));
    }
    // Destroying one environment ends its tree and only its tree.
    daemon.destroy_computer("one", "alice").await.unwrap();
    computer_where(&daemon, "one", "one destroyed", |view| {
        view.status == ComputerStatus::Destroyed
    })
    .await;
    assert!(all_gone(&one_tree));
    assert!(two_tree.iter().all(|pid| alive(*pid)));
    daemon.destroy_computer("two", "alice").await.unwrap();
    computer_where(&daemon, "two", "two destroyed", |view| {
        view.status == ComputerStatus::Destroyed
    })
    .await;
    assert!(all_gone(&two_tree));
    daemon.shutdown().await;
}

// ---- the capability boundary ---------------------------------------------------------

/// A target that cannot guarantee tree termination says so, and a computer
/// that requires the guarantee is refused before anything is acquired, with
/// the reason. Nothing is silently emulated.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_target_without_the_termination_guarantee_is_refused_before_acquisition() {
    let target = Target::start();
    target
        .faults
        .without_termination_guarantee
        .store(true, Ordering::SeqCst);
    let pids = tempfile::tempdir().unwrap();
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;
    let mut definition = ComputerEnvironmentDefinition {
        name: "strict".into(),
        desired_state: DesiredState::Running,
        env: BTreeMap::new(),
        policy: None,
        computer: ComputerRequest {
            lifecycle: ComputerLifecycle::Persistent,
            requirements: ComputerRequirements {
                capabilities: vec!["process_tree_termination".into()],
                ..requirements()
            },
            target: None,
            ttl_seconds: None,
        },
        contents: EnvironmentContents {
            processes: vec![tree(pids.path())],
            ..Default::default()
        },
        recipe: None,
    };
    let refused = daemon
        .create_computer_environment(definition.clone(), "alice")
        .await
        .unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("session_capability_unsupported"),
        "{refused}"
    );
    assert!(
        daemon.computer("strict").await.is_err(),
        "something was recorded"
    );
    // A computer that does not require it is still placed: the requirement
    // is the caller's, stated up front.
    definition.name = "lenient".into();
    definition.computer.requirements.capabilities.clear();
    daemon
        .create_computer_environment(definition, "alice")
        .await
        .unwrap();
    daemon.destroy_computer("lenient", "alice").await.unwrap();
    daemon.shutdown().await;
}
