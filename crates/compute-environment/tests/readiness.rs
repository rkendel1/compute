//! Configured environment readiness, against a real target.
//!
//! The contract (`docs/readiness.md`): `ready` means Compute has verified,
//! against the environment's own target and now, that the machine is
//! confirmed, the requirements hold, and the declared contents are held. A
//! computer that exists is not a ready environment; a stopped one is
//! unavailable; one whose target no longer satisfies its requirements is
//! unavailable with placement's reason; and no workload is admitted to an
//! environment that is not ready.
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
    RecipeRef, RecipeSpec, SessionCommand,
};
use compute_environment::*;
use compute_state::StateStore;
use compute_state_file::FileState;
use compute_state_memory::MemoryState;
use target::*;

fn requirements() -> ComputerRequirements {
    ComputerRequirements {
        cpu_count: Some(1),
        memory_bytes: Some(64 << 20),
        network: NetworkPolicy::Network,
        // What Compute verifies on the target, not what a recipe declares.
        capabilities: vec!["process_tree_termination".into()],
        ..Default::default()
    }
}

fn definition(name: &str, requirements: ComputerRequirements) -> ComputerEnvironmentDefinition {
    ComputerEnvironmentDefinition {
        name: name.into(),
        desired_state: DesiredState::Running,
        env: BTreeMap::new(),
        policy: None,
        computer: ComputerRequest {
            lifecycle: ComputerLifecycle::Persistent,
            requirements,
            target: None,
            ttl_seconds: None,
        },
        contents: EnvironmentContents::default(),
        recipe: None,
    }
}

fn on_disk(path: &Path) -> Arc<dyn StateStore> {
    Arc::new(FileState::open(path).unwrap())
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

async fn readiness_is(daemon: &Arc<Daemon>, name: &str, wanted: ReadinessState) -> ComputerView {
    wait_until(&format!("{name} to be {}", wanted.as_str()), async || {
        let view = daemon.computer(name).await.ok()?;
        (view.readiness.state == wanted).then_some(view)
    })
    .await
}

async fn sh(daemon: &Arc<Daemon>, name: &str, script: &str) -> Result<String, EnvironmentError> {
    let exec = daemon
        .computer_exec(
            name,
            "alice",
            SessionCommand::new(vec!["sh".into(), "-c".into(), script.into()]),
        )
        .await?;
    let job = wait_until("the command", async || {
        daemon
            .computer_job(name, "alice", &exec.job_id)
            .await
            .ok()
            .filter(|job| job.result.is_some())
    })
    .await;
    Ok(job.result.unwrap().result.stdout.text)
}

fn condition<'a>(view: &'a ComputerView, name: &str) -> &'a ReadinessCondition {
    view.readiness
        .conditions
        .iter()
        .find(|condition| condition.name == name)
        .unwrap_or_else(|| panic!("no {name} condition: {:#?}", view.readiness))
}

/// Ready is verified, never inferred from the computer existing: at every
/// moment between creating an environment and its readiness, a `ready`
/// answer is backed by a confirmed, converged, requirement-satisfying
/// machine, and every condition is shown.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ready_is_verified_and_a_computer_that_exists_is_not_ready() {
    let target = Target::start();
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;
    daemon
        .create_computer_environment(definition("dev", requirements()), "alice")
        .await
        .unwrap();
    let mut saw_not_ready = false;
    let ready = wait_until("dev to be ready", async || {
        let view = daemon.computer("dev").await.ok()?;
        if view.readiness.state == ReadinessState::Ready {
            return Some(view);
        }
        // Not ready while there is no confirmed running machine.
        saw_not_ready = true;
        None
    })
    .await;
    assert!(
        saw_not_ready,
        "it was reported ready before a machine existed"
    );
    assert_eq!(ready.status, ComputerStatus::Running);
    assert!(ready.converged);
    assert!(ready.readiness.unsatisfied.is_empty());
    for name in ["machine", "requirements", "contents", "processes"] {
        assert!(condition(&ready, name).satisfied, "{name}");
    }
    // Requirements are the existing model, beside what was verified.
    assert_eq!(ready.requirements, requirements());
    // The environment inspection carries the very same readiness.
    let environment = daemon.environment("dev").await.unwrap();
    assert_eq!(
        environment.computer.unwrap().readiness.state,
        ReadinessState::Ready
    );
    assert_eq!(sh(&daemon, "dev", "echo ok").await.unwrap(), "ok\n");
    daemon.shutdown().await;
}

/// Unsatisfied requirements are not a failure to establish: a target that
/// cannot satisfy them refuses placement with placement's reason and
/// records nothing; a target that accepts and cannot provision is `failed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsatisfied_requirements_and_a_failed_provision_are_different() {
    let target = Target::start();
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;

    target
        .faults
        .without_termination_guarantee
        .store(true, Ordering::SeqCst);
    let refused = daemon
        .create_computer_environment(definition("strict", requirements()), "alice")
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
    target
        .faults
        .without_termination_guarantee
        .store(false, Ordering::SeqCst);

    target.faults.provisioning.store(true, Ordering::SeqCst);
    daemon
        .create_computer_environment(definition("broken", requirements()), "alice")
        .await
        .unwrap();
    let failed = readiness_is(&daemon, "broken", ReadinessState::Failed).await;
    assert!(failed.readiness.unsatisfied.is_empty(), "not a requirement");
    assert!(
        failed.readiness.explanation.contains("injected"),
        "{}",
        failed.readiness.explanation
    );
    let rejected = sh(&daemon, "broken", "echo no").await.unwrap_err();
    assert!(rejected.to_string().contains("failed"), "{rejected}");
    daemon.shutdown().await;
}

/// ready → stop → start → ready, and a start onto a target that no longer
/// satisfies the requirements is unavailable, with the reason, and admits
/// nothing: no fallback runs the workload somewhere else.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_start_reestablishes_readiness_and_never_claims_it_falsely() {
    let target = Target::start();
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;
    daemon
        .create_computer_environment(definition("dev", requirements()), "alice")
        .await
        .unwrap();
    let before = readiness_is(&daemon, "dev", ReadinessState::Ready).await;

    // Stop: unavailable, not ready, and no workload is admitted.
    daemon
        .set_environment_state("dev", DesiredState::Stopped, false)
        .await
        .unwrap();
    let stopped = wait_until("dev to stop", async || {
        let view = daemon.computer("dev").await.ok()?;
        (view.status == ComputerStatus::Stopped).then_some(view)
    })
    .await;
    assert_eq!(stopped.readiness.state, ReadinessState::Unavailable);
    assert!(!condition(&stopped, "machine").satisfied);
    assert!(stopped.readiness.explanation.contains("stopped"));
    let refused = sh(&daemon, "dev", "echo no").await.unwrap_err();
    assert!(refused.to_string().contains("unavailable"), "{refused}");
    assert!(stopped.observed.processes.values().all(|p| p.pid.is_none()));

    // The target stops offering what the environment requires while it is
    // stopped. Start brings the machine back, and it is not ready.
    target
        .faults
        .without_termination_guarantee
        .store(true, Ordering::SeqCst);
    daemon
        .set_environment_state("dev", DesiredState::Running, false)
        .await
        .unwrap();
    let broken = wait_until("the honest verdict after start", async || {
        let view = daemon.computer("dev").await.ok()?;
        (view.status == ComputerStatus::Running
            && view.readiness.state == ReadinessState::Unavailable)
            .then_some(view)
    })
    .await;
    assert_eq!(broken.environment_id, before.environment_id);
    assert!(!condition(&broken, "requirements").satisfied);
    assert!(
        broken
            .readiness
            .unsatisfied
            .iter()
            .any(|reason| reason.code.as_str() == "session_capability_unsupported"),
        "{:#?}",
        broken.readiness
    );
    let jobs_before = broken.observed.processes.len();
    let refused = sh(&daemon, "dev", "echo no").await.unwrap_err();
    let message = refused.to_string();
    assert!(
        message.contains("session_capability_unsupported"),
        "the rejection names the unsatisfied condition: {message}"
    );
    assert_eq!(
        daemon
            .computer("dev")
            .await
            .unwrap()
            .observed
            .processes
            .len(),
        jobs_before
    );

    // The target offers it again: readiness is re-established, and only
    // then is a workload admitted.
    target
        .faults
        .without_termination_guarantee
        .store(false, Ordering::SeqCst);
    let again = readiness_is(&daemon, "dev", ReadinessState::Ready).await;
    assert_eq!(again.environment_id, before.environment_id);
    assert_eq!(sh(&daemon, "dev", "echo back").await.unwrap(), "back\n");
    daemon.shutdown().await;
}

/// Evaluating readiness reads. It creates no computer and no process,
/// records no event, and changes no record, however often it is asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn evaluating_readiness_changes_nothing() {
    let target = Target::start();
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;
    daemon
        .write_recipe(
            "alice",
            RecipeDefinition {
                name: "developer".into(),
                spec: RecipeSpec {
                    lifecycle: ComputerLifecycle::Persistent,
                    requirements: ComputerRequirements {
                        cpu_count: Some(1),
                        memory_bytes: Some(64 << 20),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                expected_version: None,
            },
        )
        .await
        .unwrap();
    daemon
        .create_computer_environment(definition("dev", requirements()), "alice")
        .await
        .unwrap();
    let ready = readiness_is(&daemon, "dev", ReadinessState::Ready).await;
    let events = daemon.events(EventFilter::default()).await.unwrap().len();
    let recipe = daemon.recipe("developer", None).await.unwrap();
    let environments = daemon.environments().await.unwrap().len();

    for _ in 0..20 {
        let view = daemon.computer("dev").await.unwrap();
        assert_eq!(view.readiness.state, ReadinessState::Ready);
        daemon.environment("dev").await.unwrap();
    }
    let after = daemon.computer("dev").await.unwrap();
    assert_eq!(after.generation, ready.generation);
    assert_eq!(after.session_id, ready.session_id);
    assert_eq!(after.observed.processes, ready.observed.processes);
    assert_eq!(
        daemon.events(EventFilter::default()).await.unwrap().len(),
        events
    );
    assert_eq!(daemon.recipe("developer", None).await.unwrap(), recipe);
    assert_eq!(daemon.environments().await.unwrap().len(), environments);
    daemon.shutdown().await;
}

/// A controller restart keeps identity, requirements and recipe evidence and
/// re-verifies readiness against the target as it is now: a stopped
/// environment stays stopped with no process, and a ready claim is never
/// read back from disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn readiness_is_reverified_after_a_restart_and_never_persisted() {
    let target = Target::start();
    let state = tempfile::tempdir().unwrap();
    let path = state.path().join("control-state.json");
    let (id, reference) = {
        let (daemon, _node) = start_daemon(on_disk(&path), Some(pool(&target))).await;
        daemon
            .write_recipe(
                "alice",
                RecipeDefinition {
                    name: "developer".into(),
                    spec: RecipeSpec {
                        lifecycle: ComputerLifecycle::Persistent,
                        requirements: requirements(),
                        ..Default::default()
                    },
                    expected_version: None,
                },
            )
            .await
            .unwrap();
        let resolution = daemon
            .resolve_recipe("developer", None, None)
            .await
            .unwrap();
        let resolved = resolution.resolved.clone().unwrap();
        let reference: RecipeRef = resolution.recipe.clone().unwrap();
        let mut request = definition("dev", resolved.computer.requirements.clone());
        request.computer = resolved.computer;
        request.policy = resolved.policy;
        request.recipe = Some(reference.clone());
        daemon
            .create_computer_environment(request, "alice")
            .await
            .unwrap();
        daemon
            .create_computer_environment(definition("sleeper", requirements()), "alice")
            .await
            .unwrap();
        let ready = readiness_is(&daemon, "dev", ReadinessState::Ready).await;
        readiness_is(&daemon, "sleeper", ReadinessState::Ready).await;
        daemon
            .set_environment_state("sleeper", DesiredState::Stopped, false)
            .await
            .unwrap();
        readiness_is(&daemon, "sleeper", ReadinessState::Unavailable).await;
        daemon.shutdown().await;
        (ready.environment_id, reference)
    };

    // While the controller is down the target stops satisfying what the
    // environment requires.
    target
        .faults
        .without_termination_guarantee
        .store(true, Ordering::SeqCst);
    let (daemon, _node) = start_daemon(on_disk(&path), Some(pool(&target))).await;
    let dev = wait_until("dev to be reconciled", async || {
        let view = daemon.computer("dev").await.ok()?;
        (view.status == ComputerStatus::Running).then_some(view)
    })
    .await;
    // Identity, requirements and provenance survive; readiness was not
    // trusted from before.
    assert_eq!(dev.environment_id, id);
    assert_eq!(dev.requirements, requirements());
    assert_eq!(
        daemon.environment("dev").await.unwrap().recipe,
        Some(reference)
    );
    let dev = readiness_is(&daemon, "dev", ReadinessState::Unavailable).await;
    assert!(!condition(&dev, "requirements").satisfied);
    assert!(sh(&daemon, "dev", "echo no").await.is_err());

    // The stopped environment was not resurrected.
    let sleeper = daemon.computer("sleeper").await.unwrap();
    assert_eq!(sleeper.status, ComputerStatus::Stopped);
    assert_eq!(sleeper.readiness.state, ReadinessState::Unavailable);
    assert!(
        sleeper
            .observed
            .processes
            .values()
            .all(|process| process.pid.is_none())
    );

    // When the target satisfies it again, readiness returns by itself.
    target
        .faults
        .without_termination_guarantee
        .store(false, Ordering::SeqCst);
    readiness_is(&daemon, "dev", ReadinessState::Ready).await;
    daemon.shutdown().await;
}

/// The consumer sequence, with no knowledge of how readiness is
/// established: recipe → create → wait for readiness → run a workload. The
/// answer to "is it ready" is Compute's; the workload is Compute's too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_consumer_creates_from_a_recipe_waits_for_readiness_and_runs() {
    let target = Target::start();
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;
    daemon
        .write_recipe(
            "alice",
            RecipeDefinition {
                name: "developer".into(),
                spec: RecipeSpec {
                    lifecycle: ComputerLifecycle::Persistent,
                    requirements: requirements(),
                    ..Default::default()
                },
                expected_version: None,
            },
        )
        .await
        .unwrap();
    // Resolution answers "can this policy be placed?" and nothing exists.
    let resolution = daemon
        .resolve_recipe("developer", None, None)
        .await
        .unwrap();
    assert!(daemon.computer("workspace").await.is_err());
    let resolved = resolution.resolved.clone().expect("it resolves");
    let mut request = definition("workspace", resolved.computer.requirements.clone());
    request.computer = resolved.computer;
    request.policy = resolved.policy;
    request.recipe = resolution.recipe.clone();
    daemon
        .create_computer_environment(request, "alice")
        .await
        .unwrap();

    // Readiness answers "is the environment usable now?".
    let ready = readiness_is(&daemon, "workspace", ReadinessState::Ready).await;
    assert_eq!(
        daemon.environment("workspace").await.unwrap().recipe,
        resolution.recipe
    );
    assert_eq!(ready.readiness.state, ReadinessState::Ready);
    assert_eq!(
        sh(&daemon, "workspace", "echo built").await.unwrap(),
        "built\n"
    );
    daemon.shutdown().await;
}
