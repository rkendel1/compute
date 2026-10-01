//! Ephemeral execution: a run is *execute → collect → clean up*, and the
//! cleanup happens on every ending. After each, the staged workspace is gone
//! and nothing the workload started is still running.

mod support;

use std::time::Duration;

use compute_core::{ExecutionControl, ExecutionStatus};
use compute_runtime::Compute;
use support::{alive, read_pid, shell, with_output, with_timeout};

/// A script that records its workspace, then does `ending`.
fn script(probe: &std::path::Path, ending: &str) -> String {
    format!(
        "echo \"$COMPUTE_WORK_DIR\" > {work}\n{ending}\n",
        work = probe.join("work").display(),
    )
}

/// The same, with a long-lived descendant that shares the command's output.
/// Interrupting the execution (timeout, cancellation) must end it too.
fn script_with_descendant(probe: &std::path::Path, ending: &str) -> String {
    script(
        probe,
        &format!(
            "sleep 300 &\necho $! > {pid}\n{ending}",
            pid = probe.join("pid").display()
        ),
    )
}

struct Probe {
    directory: tempfile::TempDir,
}

impl Probe {
    fn new() -> Self {
        Self {
            directory: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self) -> &std::path::Path {
        self.directory.path()
    }

    /// The execution is over: its workspace is gone and its descendant dead.
    fn assert_clean(&self, what: &str) {
        let work = std::fs::read_to_string(self.path().join("work")).expect("the script ran");
        assert!(
            !std::path::Path::new(work.trim()).exists(),
            "{what}: the workspace {work} outlived the execution"
        );
        if let Some(pid) = read_pid(&self.path().join("pid")) {
            assert!(
                !alive(pid),
                "{what}: descendant {pid} outlived the execution"
            );
        }
    }
}

#[tokio::test]
async fn success_collects_outputs_then_cleans_up() {
    let probe = Probe::new();
    let request = with_output(
        shell(
            probe.path(),
            &script(
                probe.path(),
                "echo done > \"$COMPUTE_OUTPUT_DIR/result.txt\"",
            ),
        ),
        "result.txt",
    );
    let result = Compute::new().run(request).await.unwrap();
    assert_eq!(result.status, ExecutionStatus::Completed);
    assert_eq!(
        result.outputs.len(),
        1,
        "the output was collected before cleanup"
    );
    assert!(result.receipt.is_some());
    probe.assert_clean("success");
}

#[tokio::test]
async fn a_failing_command_is_cleaned_up() {
    let probe = Probe::new();
    let request = shell(probe.path(), &script(probe.path(), "exit 5"));
    let result = Compute::new().run(request).await.unwrap();
    // The command ended by itself: its failure is its exit status, and it
    // still has a receipt.
    assert_eq!(result.exit_code, Some(5));
    assert!(result.receipt.is_some());
    probe.assert_clean("command failure");
}

#[tokio::test]
async fn a_timeout_is_cleaned_up() {
    let probe = Probe::new();
    let request = with_timeout(
        shell(probe.path(), &script_with_descendant(probe.path(), "wait")),
        Duration::from_secs(2),
    );
    let result = Compute::new().run(request).await.unwrap();
    assert_eq!(result.status, ExecutionStatus::TimedOut);
    assert!(
        read_pid(&probe.path().join("pid")).is_some(),
        "the descendant started"
    );
    probe.assert_clean("timeout");
}

#[tokio::test]
async fn cancellation_is_cleaned_up() {
    let probe = Probe::new();
    let request = shell(probe.path(), &script_with_descendant(probe.path(), "wait"));
    let control = ExecutionControl::new();
    let canceller = control.clone();
    let watched = probe.path().join("pid");
    tokio::spawn(async move {
        for _ in 0..200 {
            if watched.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        canceller.cancel();
    });
    let result = Compute::new()
        .run_controlled(request, &control)
        .await
        .unwrap();
    assert_eq!(result.status, ExecutionStatus::Cancelled);
    assert!(
        read_pid(&probe.path().join("pid")).is_some(),
        "the descendant started"
    );
    probe.assert_clean("cancellation");
}

#[tokio::test]
async fn a_workload_that_cannot_start_is_an_error_and_runs_nothing() {
    let probe = Probe::new();
    let mut request = shell(probe.path(), "true");
    request.entrypoint = probe.path().join("does-not-exist.sh");
    // Reported either way, never a hang or a silent success.
    match Compute::new().run(request).await {
        Err(_) => {}
        Ok(result) => {
            assert!(result.error.is_some(), "{:?}", result.status);
            assert_ne!(result.exit_code, Some(0));
        }
    }
}
