//! Cancelling an agent's commands (compute#90, G3) against a real Compute target. Cancellation
//! is Compute's own confirmed, idempotent job cancellation: a command is reported cancelled only
//! when Compute recorded it so, and a command interrupted while it ran is never reported as not
//! having run.

mod support;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use compute_agent::{AgentHost, CommandCancellation};
use compute_core::JobStatus;
use support::*;

fn sh(script: &str) -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into(), script.into()]
}

async fn until_running(session: &compute_agent::AgentSession) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(job) = session.running_commands().into_iter().next() {
            return job;
        }
        assert!(Instant::now() < deadline, "the command never started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn with_nothing_running_cancel_does_nothing() {
    let target = Target::start();
    let source = project("cancel-idle");
    let host = AgentHost::new(host_config(&target, &source));
    let session = host.acquire().await.unwrap();
    assert!(session.cancel().await.is_empty());
    // After a command completed, there is still nothing to cancel, and its result stands.
    let done = session
        .exec(sh("echo done"), BTreeMap::new())
        .await
        .unwrap();
    assert_eq!(done.stdout, "done\n");
    assert!(session.cancel().await.is_empty());
    session.release().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_running_command_is_interrupted_and_reported_as_possibly_effective() {
    let target = Target::start();
    let source = project("cancel-running");
    let host = AgentHost::new(host_config(&target, &source));
    let session = host.acquire().await.unwrap();
    let started = Instant::now();
    let running = {
        let session = session.clone();
        tokio::spawn(async move {
            session
                .exec(
                    sh("echo before > project/effect; sleep 30"),
                    BTreeMap::new(),
                )
                .await
        })
    };
    let job = until_running(&session).await;
    // Let the command produce its effect first: cancellation must not claim it did not happen.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let first = session.cancel().await;
    assert_eq!(
        first,
        [CommandCancellation::Interrupted {
            job_id: job.clone()
        }]
    );
    let error = running.await.unwrap().unwrap_err();
    assert!(error.0.contains("may have happened"), "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(25),
        "cancel did not stop the command"
    );

    // Compute's record agrees, and asking again changes nothing.
    let record = host.remote().job_status(&job).await.unwrap();
    assert_eq!(record.status, JobStatus::Cancelled);
    assert!(record.cancellation.effective);
    let again = session.cancel().await;
    assert!(
        again.is_empty()
            || again
                == [CommandCancellation::Interrupted {
                    job_id: job.clone()
                }],
        "{again:?}"
    );
    assert_eq!(
        host.remote().job_status(&job).await.unwrap().status,
        JobStatus::Cancelled
    );
    session.release().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_whose_caller_stops_waiting_is_cancelled_not_left_running() {
    let target = Target::start();
    let source = project("cancel-dropped");
    let host = AgentHost::new(host_config(&target, &source));
    let session = host.acquire().await.unwrap();
    let running = {
        let session = session.clone();
        tokio::spawn(async move { session.exec(sh("sleep 30"), BTreeMap::new()).await })
    };
    let job = until_running(&session).await;
    running.abort();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let status = host.remote().job_status(&job).await.unwrap().status;
        if status == JobStatus::Cancelled {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the abandoned command is still {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(session.running_commands().is_empty());
    session.release().await;
}
