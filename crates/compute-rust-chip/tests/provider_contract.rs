//! The generic Rust Chip environment contract, held against the real Compute-backed provider.
//! (The contract's own tests live in `chip-rs`; they use fakes. These use a real target.)

mod support;

use std::sync::Arc;
use std::time::Duration;

use chip_core::{
    CapabilityId, CapabilityProvider, EnvironmentError, EnvironmentId, Environments, ExecutionId,
    ExecutionRequest, Executor, InputValue, WorkId,
};
use compute_core::SessionStatus;
use support::*;

fn read(path: &str) -> ExecutionRequest {
    ExecutionRequest::new(ExecutionId::new("x-read"), "project.read")
        .with_inputs([("path".to_string(), InputValue::Text(path.into()))].into())
}

async fn session_status(
    provider: &compute_rust_chip::ComputeSessionEnvironments,
    session: &str,
) -> SessionStatus {
    for _ in 0..200 {
        let status = provider.remote().session(session).await.unwrap().status;
        if status.is_terminal() {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    provider.remote().session(session).await.unwrap().status
}

#[tokio::test(flavor = "multi_thread")]
async fn acquire_gives_a_real_session_with_the_project_and_release_destroys_it() {
    if !pax_installed() {
        eprintln!("SKIPPED: PAX is not installed");
        return;
    }
    let target = Target::start();
    let source = project("contract", true);
    let provider = provider(&target, &source, 2);
    let envs = Environments::new(provider.clone());

    let owned = envs.acquire(WorkId::new("A")).await.unwrap();
    let id = owned.id().clone();
    let session = provider.session_of(&id).expect("a live session");
    // The identity is opaque: no session id, no path, no host.
    assert!(id.as_str().starts_with("env_") && !id.as_str().contains(&session));
    assert!(!id.as_str().contains('/'));

    // Capabilities execute in the session: Rust Chip's own executor reads the project there.
    let set = owned.environment().capabilities();
    let declared: Vec<String> = set
        .capabilities()
        .await
        .unwrap()
        .iter()
        .map(|d| d.id.to_string())
        .collect();
    for capability in [
        "project.read",
        "project.write",
        "project.list",
        "project.search",
        "project.git.status",
        "project.git.diff",
        "project.git.diff_stat",
        "project.git.log",
        "pax.test",
    ] {
        assert!(
            declared.iter().any(|d| d == capability),
            "{capability} in {declared:?}"
        );
    }
    let readme = set.execute(read("README.md")).await.unwrap();
    assert!(readme.output.contains("baseline"), "{}", readme.output);
    // The same environment answers again: one stable environment for the trajectory.
    assert!(set.execute(read("README.md")).await.is_ok());
    assert_eq!(provider.session_of(&id).as_deref(), Some(session.as_str()));
    assert!(
        provider.stats().commands_run >= 4,
        "every operation was a Compute command"
    );

    // The session is Compute's, ready, and the source project is untouched by reading.
    assert!(!matches!(
        session_status(&provider, &session).await,
        SessionStatus::Destroyed
    ));

    drop(owned);
    assert_eq!(
        session_status(&provider, &session).await,
        SessionStatus::Destroyed
    );
    assert_eq!(provider.stats().released, 1);
    assert!(provider.session_of(&id).is_none());
    assert_eq!(
        std::fs::read_to_string(source.join("README.md")).unwrap(),
        "baseline\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_released_environment_can_no_longer_be_used() {
    if !pax_installed() {
        return;
    }
    let target = Target::start();
    let source = project("released", true);
    let provider = provider(&target, &source, 1);
    let envs = Environments::new(provider.clone());
    let owned = envs.acquire(WorkId::new("A")).await.unwrap();
    let id = owned.id().clone();
    let session = provider.session_of(&id).unwrap();
    let set = owned.environment().capabilities();
    drop(owned);
    assert_eq!(
        session_status(&provider, &session).await,
        SessionStatus::Destroyed
    );
    // The old handle reaches a destroyed session: the operation fails, it is not answered.
    let err = set.execute(read("README.md")).await.unwrap_err();
    assert!(
        matches!(err, chip_core::ExecutionError::ExecutorUnavailable(_)),
        "{err}"
    );
    // And the environment broker lets a new work acquire again (a new session, a new identity).
    let again = envs.acquire(WorkId::new("B")).await.unwrap();
    assert_ne!(again.id(), &id);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_works_get_distinct_isolated_sessions() {
    if !pax_installed() {
        return;
    }
    let target = Target::start();
    let source = project("distinct", true);
    let provider = provider(&target, &source, 2);
    let envs = Environments::new(provider.clone());
    let (a, b) = tokio::join!(
        envs.acquire(WorkId::new("A")),
        envs.acquire(WorkId::new("B"))
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_ne!(a.id(), b.id());
    let (sa, sb) = (
        provider.session_of(a.id()).unwrap(),
        provider.session_of(b.id()).unwrap(),
    );
    assert_ne!(sa, sb);
    // A third is refused by the declared isolation capacity: no sharing, no fallback.
    assert_eq!(
        envs.acquire(WorkId::new("C")).await.err().unwrap(),
        EnvironmentError::AtCapacity { capacity: 2 }
    );
    assert_eq!(provider.stats().acquired, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn acquisition_failure_fails_closed_and_creates_nothing() {
    let target = Target::start();
    let source = project("failclosed", true);
    // 1. A target that does not answer.
    let mut dead = config(&target, &source, 1);
    dead.endpoint = "http://127.0.0.1:1".into();
    let envs = Environments::new(Arc::new(
        compute_rust_chip::ComputeSessionEnvironments::new(dead),
    ));
    let err = envs.acquire(WorkId::new("A")).await.err().unwrap();
    assert!(matches!(err, EnvironmentError::Unavailable(_)), "{err}");
    // 2. A project that cannot be loaded: the session that was made is destroyed again.
    let missing = std::env::temp_dir().join("compute-rust-chip-no-such-project");
    let provider = provider(&target, &missing, 1);
    let envs = Environments::new(provider.clone());
    let err = envs.acquire(WorkId::new("A")).await.err().unwrap();
    assert!(err.to_string().contains("could not be loaded"), "{err}");
    let stats = provider.stats();
    assert_eq!((stats.acquired, stats.acquire_failed), (0, 1));
    assert_eq!(stats.released, 1, "the half-made session was destroyed");
    let sessions = provider.remote().sessions().await.unwrap();
    assert!(
        sessions.iter().all(|s| s.status.is_terminal()),
        "{sessions:?}"
    );
    // The wrong token is refused by the target, and that is also a closed failure.
    let mut wrong = config(&target, &source, 1);
    wrong.token = Some("not-the-token".into());
    let envs = Environments::new(Arc::new(
        compute_rust_chip::ComputeSessionEnvironments::new(wrong),
    ));
    assert!(envs.acquire(WorkId::new("A")).await.is_err());
    let _ = (CapabilityId::new("pax.test"), EnvironmentId::new("x"));
}
