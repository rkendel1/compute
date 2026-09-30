//! Environment state and lifecycle provenance, against a real target.
//!
//! The contract (`docs/lifecycle.md`, "Persistent, ephemeral, and
//! provenance"): an environment is one durable object across stop, start,
//! replace, fork, and controller restarts. Its recipe evidence is carried to
//! a derived environment only while that recipe version still resolves to
//! the configuration actually in force, and released when it does not.
#![cfg(target_os = "linux")]

mod common;
#[path = "common/target.rs"]
mod target;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use compute_core::{
    ComputerLifecycle, ComputerRequirements, ComputerStatus, NetworkPolicy, RecipeRef, RecipeSpec,
};
use compute_environment::*;
use compute_state::StateStore;
use compute_state_file::FileState;
use compute_state_memory::MemoryState;
use target::*;

fn requirements(cpus: u32) -> ComputerRequirements {
    ComputerRequirements {
        cpu_count: Some(cpus),
        memory_bytes: Some(64 << 20),
        network: NetworkPolicy::Network,
        ..Default::default()
    }
}

fn developer() -> RecipeSpec {
    RecipeSpec {
        lifecycle: ComputerLifecycle::Persistent,
        requirements: requirements(1),
        ..Default::default()
    }
}

fn on_disk(path: &Path) -> Arc<dyn StateStore> {
    Arc::new(FileState::open(path).unwrap())
}

async fn wait_until<T>(what: &str, mut check: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(value) = check().await {
            return value;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn running(daemon: &Arc<Daemon>, name: &str) -> ComputerView {
    wait_until(&format!("{name} to run"), async || {
        let view = daemon.computer(name).await.ok()?;
        (view.status == ComputerStatus::Running).then_some(view)
    })
    .await
}

async fn recipe_of(daemon: &Arc<Daemon>, name: &str) -> Option<RecipeRef> {
    daemon.environment(name).await.unwrap().recipe
}

/// `developer` version 1 written, and an environment created from it the way
/// `environment create --recipe` does.
async fn create_from_recipe(daemon: &Arc<Daemon>, name: &str) -> RecipeRef {
    if daemon.recipe("developer", None).await.is_err() {
        daemon
            .write_recipe(
                "alice",
                RecipeDefinition {
                    name: "developer".into(),
                    spec: developer(),
                    expected_version: None,
                },
            )
            .await
            .unwrap();
    }
    let resolution = daemon
        .resolve_recipe("developer", None, None)
        .await
        .unwrap();
    let resolved = resolution.resolved.clone().expect("it resolves");
    let reference = resolution.recipe.clone().expect("it names a version");
    daemon
        .create_computer_environment(
            ComputerEnvironmentDefinition {
                name: name.into(),
                desired_state: DesiredState::Running,
                env: Default::default(),
                policy: resolved.policy,
                computer: resolved.computer,
                contents: Default::default(),
                recipe: Some(reference.clone()),
            },
            "alice",
        )
        .await
        .unwrap();
    running(daemon, name).await;
    reference
}

fn fork_request(name: &str) -> ForkRequest {
    ForkRequest {
        name: name.into(),
        target: None,
        copy_config: false,
    }
}

async fn replace_events(daemon: &Arc<Daemon>) -> Vec<serde_json::Value> {
    daemon
        .events(EventFilter::default())
        .await
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "computer.replaced")
        .map(|event| event.data)
        .collect()
}

/// Stop then start: the same environment, the same durable identity, the same
/// recipe evidence, and the durable record is unchanged by either.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_then_start_is_the_same_environment_with_the_same_provenance() {
    let target = Target::start();
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;
    let reference = create_from_recipe(&daemon, "dev").await;
    let before = running(&daemon, "dev").await;

    daemon
        .set_environment_state("dev", DesiredState::Stopped, false)
        .await
        .unwrap();
    wait_until("dev to stop", async || {
        (daemon.computer("dev").await.ok()?.status == ComputerStatus::Stopped).then_some(())
    })
    .await;
    // Stopped is not destroyed: the environment and its evidence are here.
    assert_eq!(recipe_of(&daemon, "dev").await, Some(reference.clone()));

    daemon
        .set_environment_state("dev", DesiredState::Running, false)
        .await
        .unwrap();
    let after = running(&daemon, "dev").await;
    assert_eq!(after.environment_id, before.environment_id);
    assert_eq!(after.requirements, before.requirements);
    assert_eq!(after.lifecycle, before.lifecycle);
    assert_eq!(recipe_of(&daemon, "dev").await, Some(reference));
    daemon.shutdown().await;
}

/// Fork and replace keep the recipe only while it is still true of the
/// configuration; a replacement with different requirements is a different
/// configuration and releases it, on the record and in the event.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provenance_survives_only_while_it_describes_the_configuration() {
    let target = Target::start();
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;
    let reference = create_from_recipe(&daemon, "dev").await;

    // A fork inherits the configuration, so it inherits the evidence, and is
    // a new environment with an identity, lifecycle and machine of its own.
    daemon
        .fork_environment("dev", "alice", fork_request("dev-fork"))
        .await
        .unwrap();
    let fork = running(&daemon, "dev-fork").await;
    let source = running(&daemon, "dev").await;
    assert_ne!(fork.environment_id, source.environment_id);
    assert_eq!(
        recipe_of(&daemon, "dev-fork").await,
        Some(reference.clone())
    );

    // Replacing with the requirements the recipe resolves to keeps it.
    daemon
        .replace_computer("dev", "alice", requirements(1))
        .await
        .unwrap();
    running(&daemon, "dev").await;
    assert_eq!(recipe_of(&daemon, "dev").await, Some(reference.clone()));
    assert_eq!(
        replace_events(&daemon).await[0]["recipe_released"],
        serde_json::Value::Null
    );

    // Replacing with different requirements is not what the recipe asked
    // for: the evidence is released, and the event says which was released.
    daemon
        .replace_computer("dev", "alice", requirements(2))
        .await
        .unwrap();
    let replaced = running(&daemon, "dev").await;
    assert_eq!(replaced.requirements, requirements(2));
    assert_eq!(recipe_of(&daemon, "dev").await, None);
    let events = replace_events(&daemon).await;
    assert_eq!(events.len(), 2);
    assert_eq!(events[1]["recipe_released"]["digest"], reference.digest);
    // The creation history is still there.
    let created = daemon
        .events(EventFilter::default())
        .await
        .unwrap()
        .into_iter()
        .find(|event| event.kind == "environment.created" && event.data["recipe"].is_object())
        .expect("the creation event names the recipe");
    assert_eq!(created.data["recipe"]["digest"], reference.digest);

    // A fork of the now-different environment does not claim the recipe;
    // the earlier fork's evidence is its own and unchanged.
    daemon
        .fork_environment("dev", "alice", fork_request("dev-two"))
        .await
        .unwrap();
    running(&daemon, "dev-two").await;
    assert_eq!(recipe_of(&daemon, "dev-two").await, None);
    assert_eq!(recipe_of(&daemon, "dev-fork").await, Some(reference));
    daemon.shutdown().await;
}

/// Destroying a fork removes the fork's computer and leaves the source, its
/// machine and its evidence untouched, and the reverse.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn destroying_a_fork_never_touches_the_source() {
    let target = Target::start();
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;
    let reference = create_from_recipe(&daemon, "dev").await;
    daemon
        .fork_environment("dev", "alice", fork_request("dev-fork"))
        .await
        .unwrap();
    running(&daemon, "dev-fork").await;
    let source = running(&daemon, "dev").await;

    daemon.destroy_computer("dev-fork", "alice").await.unwrap();
    wait_until("the fork to be destroyed", async || {
        (daemon.computer("dev-fork").await.ok()?.status == ComputerStatus::Destroyed).then_some(())
    })
    .await;
    let still = running(&daemon, "dev").await;
    assert_eq!(still.environment_id, source.environment_id);
    assert_eq!(still.machine, source.machine);
    assert_eq!(recipe_of(&daemon, "dev").await, Some(reference));
    daemon.shutdown().await;
}

/// The durable object survives the controller: identity, recipe evidence on
/// the original and on a fork, and the fact that a destroyed environment
/// stays destroyed, all read back from disk by a new daemon.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn environment_identity_and_provenance_survive_a_controller_restart() {
    let target = Target::start();
    let state = tempfile::tempdir().unwrap();
    let path = state.path().join("control-state.json");
    let (reference, dev_id, fork_id) = {
        let (daemon, _node) = start_daemon(on_disk(&path), Some(pool(&target))).await;
        let reference = create_from_recipe(&daemon, "dev").await;
        daemon
            .fork_environment("dev", "alice", fork_request("dev-fork"))
            .await
            .unwrap();
        daemon
            .fork_environment("dev", "alice", fork_request("dev-gone"))
            .await
            .unwrap();
        running(&daemon, "dev-gone").await;
        daemon.destroy_computer("dev-gone", "alice").await.unwrap();
        wait_until("dev-gone to be destroyed", async || {
            (daemon.computer("dev-gone").await.ok()?.status == ComputerStatus::Destroyed)
                .then_some(())
        })
        .await;
        let ids = (
            running(&daemon, "dev").await.environment_id,
            running(&daemon, "dev-fork").await.environment_id,
        );
        daemon.shutdown().await;
        (reference, ids.0, ids.1)
    };

    let (daemon, _node) = start_daemon(on_disk(&path), Some(pool(&target))).await;
    assert_eq!(recipe_of(&daemon, "dev").await, Some(reference.clone()));
    assert_eq!(recipe_of(&daemon, "dev-fork").await, Some(reference));
    assert_eq!(
        daemon.environment("dev").await.unwrap().environment_id,
        dev_id
    );
    assert_eq!(
        daemon.environment("dev-fork").await.unwrap().environment_id,
        fork_id
    );
    // Nothing resurrects what was destroyed.
    assert_eq!(
        daemon.computer("dev-gone").await.unwrap().status,
        ComputerStatus::Destroyed
    );
    daemon.shutdown().await;
}

/// A controller restart reconstructs the durable environment, never its
/// processes: a stopped environment is still stopped afterwards, with no
/// process recorded or running, and stays so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_does_not_resurrect_processes() {
    use compute_core::{EnvironmentContents, ProcessDesired, ProcessKind, ProcessSpec};
    let target = Target::start();
    let state = tempfile::tempdir().unwrap();
    let path = state.path().join("control-state.json");
    let (daemon, _node) = start_daemon(on_disk(&path), Some(pool(&target))).await;
    daemon
        .create_computer_environment(
            ComputerEnvironmentDefinition {
                name: "dev".into(),
                desired_state: DesiredState::Running,
                env: Default::default(),
                policy: None,
                computer: ComputerRequest {
                    lifecycle: ComputerLifecycle::Persistent,
                    requirements: requirements(1),
                    target: None,
                    ttl_seconds: None,
                },
                contents: EnvironmentContents {
                    processes: vec![ProcessSpec {
                        name: "svc".into(),
                        kind: ProcessKind::Service,
                        runtime: None,
                        command: vec!["sleep".into(), "300".into()],
                        repository: None,
                        env: Default::default(),
                        desired: ProcessDesired::Running,
                        port: None,
                        restart: 0,
                        readiness: None,
                        restart_policy: Default::default(),
                        max_restarts: compute_core::DEFAULT_MAX_RESTARTS,
                    }],
                    ..Default::default()
                },
                recipe: None,
            },
            "alice",
        )
        .await
        .unwrap();
    wait_until("svc to run", async || {
        daemon
            .computer("dev")
            .await
            .ok()?
            .observed
            .processes
            .get("svc")?
            .pid
    })
    .await;
    daemon
        .set_environment_state("dev", DesiredState::Stopped, false)
        .await
        .unwrap();
    wait_until("dev to stop", async || {
        (daemon.computer("dev").await.ok()?.status == ComputerStatus::Stopped).then_some(())
    })
    .await;
    daemon.shutdown().await;

    let (daemon, _node) = start_daemon(on_disk(&path), Some(pool(&target))).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let view = daemon.computer("dev").await.unwrap();
    assert_eq!(view.status, ComputerStatus::Stopped, "{view:#?}");
    assert!(view.observed.processes.values().all(|p| p.pid.is_none()));
    // The declared contents are durable; running them is a decision.
    assert_eq!(view.desired.processes.len(), 1);
    daemon.shutdown().await;
}
