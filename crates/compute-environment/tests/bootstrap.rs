//! Configured environment bootstrap, against a real target.
//!
//! Bootstrap is the existing contents reconciliation (`docs/bootstrap.md`):
//! a declared package is a durable job on the computer's target, applied
//! once, retried only on request (`reconcile`), and never reported complete
//! while it has failed. Readiness verifies the result independently, and
//! admits a workload only afterwards.
#![cfg(target_os = "linux")]

mod common;
#[path = "common/target.rs"]
mod target;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use compute_core::{
    ComputerLifecycle, ComputerRequirements, ComputerStatus, EnvironmentContents, NetworkPolicy,
    PackageSpec, ProcessDesired, ProcessKind, ProcessSpec, RecipeSpec, SessionCommand,
};
use compute_environment::*;
use compute_provider::processes::alive;
use compute_state::StateStore;
use compute_state_file::FileState;
use compute_state_memory::MemoryState;
use target::*;

fn requirements() -> ComputerRequirements {
    ComputerRequirements {
        cpu_count: Some(1),
        memory_bytes: Some(64 << 20),
        network: NetworkPolicy::Network,
        capabilities: vec!["process_tree_termination".into()],
        ..Default::default()
    }
}

/// A package that records every attempt, and succeeds only once `gate`
/// exists: an install that can be made to fail, then to work.
fn gated_package(counter: &Path, gate: &Path) -> PackageSpec {
    PackageSpec {
        name: "deps".into(),
        install: vec![
            "sh".into(),
            "-c".into(),
            "echo run >> \"$0\"; test -f \"$1\"".into(),
            counter.display().to_string(),
            gate.display().to_string(),
        ],
        repository: None,
    }
}

/// A package that is still running, with its whole process tree recorded.
fn slow_package(pids: &Path) -> PackageSpec {
    PackageSpec {
        name: "slow".into(),
        install: vec![
            "sh".into(),
            "-c".into(),
            "echo $$ > \"$0\"; sleep 300 & echo $! >> \"$0\"; wait".into(),
            pids.display().to_string(),
        ],
        repository: None,
    }
}

fn definition(name: &str, packages: Vec<PackageSpec>) -> ComputerEnvironmentDefinition {
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
            packages,
            ..Default::default()
        },
        recipe: None,
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

async fn view_where(
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

async fn ready(daemon: &Arc<Daemon>, name: &str) -> ComputerView {
    view_where(daemon, name, &format!("{name} to be ready"), |view| {
        view.readiness.state == ReadinessState::Ready
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

fn attempts(counter: &Path) -> usize {
    std::fs::read_to_string(counter)
        .unwrap_or_default()
        .lines()
        .count()
}

fn workspace(dir: &tempfile::TempDir, name: &str) -> PathBuf {
    dir.path().join(name)
}

/// create → bootstrap → readiness → workload. Ready is never reported until
/// bootstrap has succeeded, and the workload is admitted only after.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_bootstraps_then_readiness_verifies_then_a_workload_runs() {
    let target = Target::start();
    let scratch = tempfile::tempdir().unwrap();
    let (counter, gate) = (workspace(&scratch, "counter"), workspace(&scratch, "gate"));
    std::fs::write(&gate, "").unwrap();
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;
    daemon
        .create_computer_environment(
            definition("dev", vec![gated_package(&counter, &gate)]),
            "alice",
        )
        .await
        .unwrap();

    // At every sample, `ready` implies a succeeded bootstrap.
    let done = wait_until("dev to be ready", async || {
        let view = daemon.computer("dev").await.ok()?;
        if view.readiness.state == ReadinessState::Ready {
            assert_eq!(view.bootstrap.state, BootstrapState::Succeeded, "{view:#?}");
            return Some(view);
        }
        assert_ne!(view.bootstrap.state, BootstrapState::Failed, "{view:#?}");
        None
    })
    .await;
    let bootstrap = &done.bootstrap;
    assert_eq!(
        bootstrap.converged_generation,
        bootstrap.contents_generation
    );
    assert!(bootstrap.completed_at.is_some());
    assert!(bootstrap.failure.is_none());
    let step = &bootstrap.steps[0];
    assert_eq!(
        (
            step.kind.as_str(),
            step.name.as_str(),
            step.outcome.as_str()
        ),
        ("package", "deps", "succeeded")
    );
    assert!(step.job_id.is_some() && step.execution_id.is_some());
    assert_eq!(attempts(&counter), 1);
    // Verification is its own answer, against the real target.
    let verified = done.readiness.configuration.as_ref().expect("verified");
    assert_eq!(verified.target, "target-a");
    assert!(verified.platform.is_some());
    assert!(
        done.readiness
            .conditions
            .iter()
            .any(|condition| condition.name == "bootstrap" && condition.satisfied)
    );
    // The environment inspection carries the same bootstrap and readiness.
    let environment = daemon.environment("dev").await.unwrap().computer.unwrap();
    assert_eq!(environment.bootstrap.state, BootstrapState::Succeeded);
    assert_eq!(environment.readiness.state, ReadinessState::Ready);
    assert_eq!(sh(&daemon, "dev", "echo ok").await.unwrap(), "ok\n");
    daemon.shutdown().await;
}

/// Bootstrapping again changes nothing: the package is not run again, the
/// machine and environment are the same, and the evidence is the original.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bootstrap_is_idempotent() {
    let target = Target::start();
    let scratch = tempfile::tempdir().unwrap();
    let (counter, gate) = (workspace(&scratch, "counter"), workspace(&scratch, "gate"));
    std::fs::write(&gate, "").unwrap();
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;
    daemon
        .create_computer_environment(
            definition("dev", vec![gated_package(&counter, &gate)]),
            "alice",
        )
        .await
        .unwrap();
    let first = ready(&daemon, "dev").await;
    for _ in 0..3 {
        let again = daemon.reconcile_computer("dev", "alice").await.unwrap();
        assert_eq!(again.environment_id, first.environment_id);
    }
    let after = ready(&daemon, "dev").await;
    assert_eq!(attempts(&counter), 1, "the package ran again");
    assert_eq!(
        after.session_id, first.session_id,
        "the computer was replaced"
    );
    assert_eq!(after.environment_id, first.environment_id);
    assert_eq!(after.bootstrap.steps, first.bootstrap.steps);
    assert_eq!(after.bootstrap.completed_at, first.bootstrap.completed_at);
    assert_eq!(daemon.environments().await.unwrap().len(), 1);
    daemon.shutdown().await;
}

/// A failed package is a failed bootstrap: converged is not claimed, readiness
/// is `failed` with a class and the operation that failed, no workload is
/// admitted, and the environment stays inspectable. Retry succeeds without
/// deleting anything, and re-running it does not repeat what worked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_bootstrap_is_truthful_inspectable_and_retryable() {
    let target = Target::start();
    let scratch = tempfile::tempdir().unwrap();
    let (counter, gate) = (workspace(&scratch, "counter"), workspace(&scratch, "gate"));
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;
    daemon
        .create_computer_environment(
            definition("dev", vec![gated_package(&counter, &gate)]),
            "alice",
        )
        .await
        .unwrap();
    let failed = view_where(&daemon, "dev", "the bootstrap to fail", |view| {
        view.bootstrap.state == BootstrapState::Failed
    })
    .await;
    let failure = failed.bootstrap.failure.as_ref().unwrap();
    assert_eq!(failure.class, FailureClass::ConfigurationFailed);
    assert_eq!(failure.operation, "package install");
    assert!(failure.retryable);
    assert!(failure.job_id.is_some(), "it names the job that failed");
    assert!(!failed.converged, "converged must mean held, not attempted");
    assert_ne!(
        failed.bootstrap.converged_generation,
        failed.bootstrap.contents_generation
    );
    assert_eq!(failed.readiness.state, ReadinessState::Failed);
    assert_eq!(
        failed.readiness.class,
        Some(FailureClass::ConfigurationFailed)
    );
    assert_eq!(failed.status, ComputerStatus::Running);
    let refused = sh(&daemon, "dev", "echo no").await.unwrap_err();
    assert!(
        refused.to_string().contains("bootstrap failed")
            && refused.to_string().contains("package install"),
        "{refused}"
    );
    // It stays failed and inspectable; nothing retries by itself.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(attempts(&counter), 1);
    assert_eq!(
        daemon.computer("dev").await.unwrap().bootstrap.state,
        BootstrapState::Failed
    );

    // The cause is fixed; the same environment is retried in place.
    std::fs::write(&gate, "").unwrap();
    let retried = daemon.reconcile_computer("dev", "alice").await.unwrap();
    assert_eq!(retried.environment_id, failed.environment_id);
    let done = ready(&daemon, "dev").await;
    assert_eq!(done.session_id, failed.session_id, "no replacement needed");
    assert_eq!(done.bootstrap.state, BootstrapState::Succeeded);
    assert_eq!(attempts(&counter), 2, "one failure and one retry");
    assert_eq!(sh(&daemon, "dev", "echo fixed").await.unwrap(), "fixed\n");
    daemon.reconcile_computer("dev", "alice").await.unwrap();
    assert_eq!(
        attempts(&counter),
        2,
        "a converged environment is not rebuilt"
    );
    daemon.shutdown().await;
}

/// ready → stop → start → ready: the configured state is kept, not
/// re-applied, and readiness is re-established before a workload is admitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_start_keeps_the_configuration_and_reestablishes_readiness() {
    let target = Target::start();
    let scratch = tempfile::tempdir().unwrap();
    let (counter, gate) = (workspace(&scratch, "counter"), workspace(&scratch, "gate"));
    std::fs::write(&gate, "").unwrap();
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;
    daemon
        .create_computer_environment(
            definition("dev", vec![gated_package(&counter, &gate)]),
            "alice",
        )
        .await
        .unwrap();
    let first = ready(&daemon, "dev").await;
    daemon
        .set_environment_state("dev", DesiredState::Stopped, false)
        .await
        .unwrap();
    let stopped = view_where(&daemon, "dev", "dev to stop", |view| {
        view.status == ComputerStatus::Stopped
    })
    .await;
    // The configuration persists; readiness does not.
    assert_eq!(stopped.bootstrap.state, BootstrapState::Succeeded);
    assert_eq!(stopped.readiness.state, ReadinessState::Unavailable);
    assert!(sh(&daemon, "dev", "echo no").await.is_err());
    daemon
        .set_environment_state("dev", DesiredState::Running, false)
        .await
        .unwrap();
    let again = ready(&daemon, "dev").await;
    assert_eq!(again.environment_id, first.environment_id);
    assert_eq!(again.session_id, first.session_id);
    assert_eq!(attempts(&counter), 1, "start re-applied the configuration");
    assert_eq!(sh(&daemon, "dev", "echo back").await.unwrap(), "back\n");
    daemon.shutdown().await;
}

/// Destroy during bootstrap ends the bootstrap work: no process of the
/// in-flight install survives, and nothing is reported complete or ready.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn destroy_during_bootstrap_leaves_no_bootstrap_work_running() {
    let target = Target::start();
    let scratch = tempfile::tempdir().unwrap();
    let pids = workspace(&scratch, "pids");
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;
    daemon
        .create_computer_environment(definition("dev", vec![slow_package(&pids)]), "alice")
        .await
        .unwrap();
    let recorded = wait_until("the install to be running", async || {
        let text = std::fs::read_to_string(&pids).unwrap_or_default();
        let found: Vec<u32> = text.lines().filter_map(|l| l.trim().parse().ok()).collect();
        (found.len() >= 2 && found.iter().all(|pid| alive(*pid))).then_some(found)
    })
    .await;
    let mid = daemon.computer("dev").await.unwrap();
    assert_eq!(mid.bootstrap.state, BootstrapState::Running);
    assert_ne!(mid.readiness.state, ReadinessState::Ready);
    assert!(sh(&daemon, "dev", "echo no").await.is_err());

    daemon.destroy_computer("dev", "alice").await.unwrap();
    let destroyed = view_where(&daemon, "dev", "dev destroyed", |view| {
        view.status == ComputerStatus::Destroyed
    })
    .await;
    assert!(
        recorded.iter().all(|pid| !alive(*pid)),
        "bootstrap work survived destruction: {recorded:?}"
    );
    assert_eq!(destroyed.bootstrap.state, BootstrapState::Failed);
    assert_eq!(
        destroyed.bootstrap.failure.as_ref().unwrap().class,
        FailureClass::BootstrapCancelled
    );
    assert_ne!(destroyed.readiness.state, ReadinessState::Ready);
    daemon.shutdown().await;
}

/// A stop during bootstrap ends the install and is reported as cancelled; it
/// is never ready, and the environment can be started and retried.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_during_bootstrap_is_cancelled_never_ready() {
    let target = Target::start();
    let scratch = tempfile::tempdir().unwrap();
    let pids = workspace(&scratch, "pids");
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;
    daemon
        .create_computer_environment(definition("dev", vec![slow_package(&pids)]), "alice")
        .await
        .unwrap();
    let recorded = wait_until("the install to be running", async || {
        let text = std::fs::read_to_string(&pids).unwrap_or_default();
        let found: Vec<u32> = text.lines().filter_map(|l| l.trim().parse().ok()).collect();
        (found.len() >= 2 && found.iter().all(|pid| alive(*pid))).then_some(found)
    })
    .await;
    daemon
        .set_environment_state("dev", DesiredState::Stopped, false)
        .await
        .unwrap();
    let stopped = view_where(&daemon, "dev", "dev to stop", |view| {
        view.status == ComputerStatus::Stopped
    })
    .await;
    assert!(
        recorded.iter().all(|pid| !alive(*pid)),
        "install survived a stop"
    );
    assert_ne!(stopped.bootstrap.state, BootstrapState::Succeeded);
    assert_ne!(stopped.readiness.state, ReadinessState::Ready);
    assert!(sh(&daemon, "dev", "echo no").await.is_err());
    daemon.shutdown().await;
}

/// A controller restart cannot turn a failure into success, or a success into
/// an unverified claim: the evidence is durable, readiness is evaluated again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_keeps_the_evidence_and_never_reports_success_without_it() {
    let target = Target::start();
    let scratch = tempfile::tempdir().unwrap();
    let (counter, gate) = (workspace(&scratch, "counter"), workspace(&scratch, "gate"));
    let state = tempfile::tempdir().unwrap();
    let path = state.path().join("control-state.json");
    let (good_id, bad_id) = {
        let (daemon, _node) = start_daemon(on_disk(&path), Some(pool(&target))).await;
        daemon
            .create_computer_environment(
                definition(
                    "good",
                    vec![gated_package(&workspace(&scratch, "c2"), &counter)],
                ),
                "alice",
            )
            .await
            .unwrap();
        std::fs::write(&counter, "").unwrap();
        daemon
            .create_computer_environment(
                definition("bad", vec![gated_package(&counter, &gate)]),
                "alice",
            )
            .await
            .unwrap();
        let good = ready(&daemon, "good").await;
        let bad = view_where(&daemon, "bad", "bad to fail", |view| {
            view.bootstrap.state == BootstrapState::Failed
        })
        .await;
        daemon.shutdown().await;
        (good.environment_id, bad.environment_id)
    };

    let (daemon, _node) = start_daemon(on_disk(&path), Some(pool(&target))).await;
    let bad = view_where(&daemon, "bad", "bad to be reloaded", |view| {
        view.status == ComputerStatus::Running
    })
    .await;
    assert_eq!(bad.environment_id, bad_id);
    assert_eq!(bad.bootstrap.state, BootstrapState::Failed, "{bad:#?}");
    assert_ne!(bad.readiness.state, ReadinessState::Ready);
    assert!(sh(&daemon, "bad", "echo no").await.is_err());
    let good = ready(&daemon, "good").await;
    assert_eq!(good.environment_id, good_id);
    assert_eq!(good.bootstrap.state, BootstrapState::Succeeded);
    daemon.shutdown().await;
}

/// The consumer sequence and provenance: recipe@N → resolve → create →
/// bootstrap → ready → workload, with the recipe's exact evidence attached
/// and untouched, and no command in the recipe.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recipe_environment_bootstraps_and_keeps_its_provenance() {
    let target = Target::start();
    let scratch = tempfile::tempdir().unwrap();
    let (counter, gate) = (workspace(&scratch, "counter"), workspace(&scratch, "gate"));
    std::fs::write(&gate, "").unwrap();
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;
    let written = daemon
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
    assert!(
        daemon.computer("workspace").await.is_err(),
        "resolve created it"
    );
    let resolved = resolution.resolved.clone().expect("it resolves");
    let mut request = definition("workspace", vec![gated_package(&counter, &gate)]);
    request.computer = resolved.computer;
    request.policy = resolved.policy;
    request.recipe = resolution.recipe.clone();
    daemon
        .create_computer_environment(request, "alice")
        .await
        .unwrap();

    let done = ready(&daemon, "workspace").await;
    assert_eq!(done.bootstrap.state, BootstrapState::Succeeded);
    let environment = daemon.environment("workspace").await.unwrap();
    let recipe = environment.recipe.expect("its provenance");
    assert_eq!(
        (recipe.name.as_str(), recipe.version, recipe.digest.as_str()),
        ("developer", 1, written.digest.as_str())
    );
    assert_eq!(daemon.recipe("developer", Some(1)).await.unwrap(), written);
    assert_eq!(
        sh(&daemon, "workspace", "echo built").await.unwrap(),
        "built\n"
    );
    daemon.shutdown().await;
}

/// Each failure is classified, with no provider internals: requirements the
/// target no longer satisfies, a provider that could not provision, a
/// declared process that will not start (degraded, workloads admitted), and a
/// machine that could not be removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failures_are_classified() {
    let target = Target::start();
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;

    // provider_failed: the target accepted and could not provision.
    target.faults.provisioning.store(true, Ordering::SeqCst);
    daemon
        .create_computer_environment(definition("provider", vec![]), "alice")
        .await
        .unwrap();
    let failed = view_where(&daemon, "provider", "a provider failure", |view| {
        view.status == ComputerStatus::Failed
    })
    .await;
    let failure = failed.bootstrap.failure.as_ref().unwrap();
    assert_eq!(failure.class, FailureClass::ProviderFailed);
    assert_eq!(failed.readiness.class, Some(FailureClass::ProviderFailed));
    target.faults.provisioning.store(false, Ordering::SeqCst);

    // runtime_failed: a declared process exits as it starts. The environment
    // is configured and usable, and says the process is not running.
    let mut request = definition("runtime", vec![]);
    request.contents.processes = vec![ProcessSpec {
        name: "svc".into(),
        kind: ProcessKind::Service,
        runtime: None,
        command: vec!["sh".into(), "-c".into(), "exit 1".into()],
        repository: None,
        env: BTreeMap::new(),
        desired: ProcessDesired::Running,
        port: None,
        restart: 0,
        readiness: None,
        restart_policy: Default::default(),
        max_restarts: compute_core::DEFAULT_MAX_RESTARTS,
    }];
    daemon
        .create_computer_environment(request, "alice")
        .await
        .unwrap();
    let degraded = view_where(&daemon, "runtime", "a runtime failure", |view| {
        view.bootstrap
            .failure
            .as_ref()
            .is_some_and(|failure| failure.class == FailureClass::RuntimeFailed)
    })
    .await;
    assert_eq!(degraded.bootstrap.state, BootstrapState::Failed);
    assert_eq!(degraded.readiness.state, ReadinessState::Degraded);
    assert_eq!(degraded.readiness.class, Some(FailureClass::RuntimeFailed));
    assert!(degraded.readiness.state.admits_workloads());

    // requirements_unsatisfied: the target stops offering what was required.
    daemon
        .create_computer_environment(definition("dev", vec![]), "alice")
        .await
        .unwrap();
    ready(&daemon, "dev").await;
    target
        .faults
        .without_termination_guarantee
        .store(true, Ordering::SeqCst);
    let unsatisfied = view_where(&daemon, "dev", "unsatisfied requirements", |view| {
        view.readiness.class == Some(FailureClass::RequirementsUnsatisfied)
    })
    .await;
    assert_eq!(unsatisfied.readiness.state, ReadinessState::Unavailable);
    assert!(!unsatisfied.readiness.unsatisfied.is_empty());
    target
        .faults
        .without_termination_guarantee
        .store(false, Ordering::SeqCst);

    // destruction_failed: the machine cannot be removed.
    ready(&daemon, "dev").await;
    target.faults.destruction.store(true, Ordering::SeqCst);
    daemon.destroy_computer("dev", "alice").await.unwrap();
    let stuck = view_where(&daemon, "dev", "a destruction failure", |view| {
        view.bootstrap
            .failure
            .as_ref()
            .is_some_and(|failure| failure.class == FailureClass::DestructionFailed)
    })
    .await;
    assert_ne!(stuck.status, ComputerStatus::Destroyed);
    assert_eq!(stuck.readiness.class, Some(FailureClass::DestructionFailed));
    target.faults.destruction.store(false, Ordering::SeqCst);
    daemon.shutdown().await;
}

/// Bootstrap adds nothing that was not declared: an environment with no
/// contents has no steps, and succeeds without installing anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nothing_is_added_that_was_not_declared() {
    let target = Target::start();
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;
    daemon
        .create_computer_environment(definition("bare", vec![]), "alice")
        .await
        .unwrap();
    let done = ready(&daemon, "bare").await;
    assert!(done.bootstrap.steps.is_empty());
    assert_eq!(done.bootstrap.state, BootstrapState::Succeeded);
    assert_eq!(done.requirements, requirements());
    assert!(done.observed.packages.is_empty() && done.observed.repositories.is_empty());
    daemon.shutdown().await;
}

/// A controller that restarts in the middle of bootstrap has no record that
/// the in-flight item finished, so it applies the item again (a declared
/// package is an idempotent install) and reports success only when the
/// evidence exists, never before.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_during_bootstrap_reapplies_and_never_claims_success_early() {
    let target = Target::start();
    let scratch = tempfile::tempdir().unwrap();
    let (started, counter) = (
        workspace(&scratch, "started"),
        workspace(&scratch, "counter"),
    );
    let state = tempfile::tempdir().unwrap();
    let path = state.path().join("control-state.json");
    let package = PackageSpec {
        name: "slowish".into(),
        install: vec![
            "sh".into(),
            "-c".into(),
            "echo start >> \"$0\"; sleep 3; echo done >> \"$1\"".into(),
            started.display().to_string(),
            counter.display().to_string(),
        ],
        repository: None,
    };
    let id = {
        let (daemon, _node) = start_daemon(on_disk(&path), Some(pool(&target))).await;
        daemon
            .create_computer_environment(definition("dev", vec![package]), "alice")
            .await
            .unwrap();
        wait_until("the install to start", async || {
            started.exists().then_some(())
        })
        .await;
        let mid = daemon.computer("dev").await.unwrap();
        assert_eq!(mid.bootstrap.state, BootstrapState::Running);
        assert_ne!(mid.readiness.state, ReadinessState::Ready);
        let id = mid.environment_id;
        daemon.shutdown().await;
        id
    };

    let (daemon, _node) = start_daemon(on_disk(&path), Some(pool(&target))).await;
    let done = wait_until("dev to be ready", async || {
        let view = daemon.computer("dev").await.ok()?;
        if view.readiness.state == ReadinessState::Ready {
            assert_eq!(view.bootstrap.state, BootstrapState::Succeeded, "{view:#?}");
            return Some(view);
        }
        None
    })
    .await;
    assert_eq!(done.environment_id, id);
    assert!(
        done.bootstrap
            .steps
            .iter()
            .all(|step| step.outcome == "succeeded")
    );
    assert!(attempts(&counter) >= 1);
    assert_eq!(sh(&daemon, "dev", "echo ok").await.unwrap(), "ok\n");
    daemon.shutdown().await;
}
