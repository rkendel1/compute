//! Host files enter an execution by **copy** into its private workspace
//! (`compute_core::stage_workload`). A guest therefore cannot write to the
//! host: host → guest is read-only by construction, and the only way results
//! leave is a declared output. A writable host mount is not offered.

mod support;

use compute_core::{ExecutionErrorKind, ExecutionResult, ExecutionStatus};
use compute_runtime::Compute;
use support::{shell, with_mount, with_output};

/// A mount that cannot be staged is refused before anything runs: the
/// execution fails in preparation, with a receipt, and `started` false.
fn refusal(result: &ExecutionResult) -> String {
    assert_eq!(result.status, ExecutionStatus::Failed);
    let error = result.error.as_ref().expect("an error");
    assert_eq!(error.kind, ExecutionErrorKind::Preparation);
    assert!(!error.started, "the workload must not have started");
    assert!(result.receipt.is_some(), "a refusal is still evidenced");
    error.message.clone()
}

fn host_tree(root: &std::path::Path) -> std::path::PathBuf {
    let host = root.join("host-data");
    std::fs::create_dir_all(host.join("nested")).unwrap();
    std::fs::write(host.join("file.txt"), "original\n").unwrap();
    std::fs::write(host.join("nested/inner.txt"), "inner\n").unwrap();
    host
}

#[tokio::test]
async fn a_mount_is_readable_and_the_guest_cannot_change_the_host() {
    let root = tempfile::tempdir().unwrap();
    let host = host_tree(root.path());
    let request = with_output(
        with_mount(
            shell(
                root.path(),
                "cat \"$COMPUTE_WORK_DIR/data/file.txt\" \"$COMPUTE_WORK_DIR/data/nested/inner.txt\" > \"$COMPUTE_OUTPUT_DIR/seen.txt\"\n\
                 echo tampered >> \"$COMPUTE_WORK_DIR/data/file.txt\"\n\
                 rm \"$COMPUTE_WORK_DIR/data/nested/inner.txt\"\n\
                 echo new > \"$COMPUTE_WORK_DIR/data/created.txt\"\n",
            ),
            &host,
            "work/data",
        ),
        "seen.txt",
    );
    let result = Compute::new().run(request).await.unwrap();
    assert_eq!(result.status, ExecutionStatus::Completed);
    assert_eq!(result.outputs[0].data, b"original\ninner\n");

    assert_eq!(
        std::fs::read_to_string(host.join("file.txt")).unwrap(),
        "original\n"
    );
    assert!(host.join("nested/inner.txt").exists());
    assert!(!host.join("created.txt").exists());
}

#[tokio::test]
async fn a_mount_is_removed_with_the_workspace() {
    let root = tempfile::tempdir().unwrap();
    let host = host_tree(root.path());
    let probe = root.path().join("where");
    let request = with_mount(
        shell(
            root.path(),
            &format!("echo \"$COMPUTE_WORK_DIR\" > {}\n", probe.display()),
        ),
        &host,
        "work/data",
    );
    Compute::new().run(request).await.unwrap();
    let work = std::fs::read_to_string(&probe).unwrap();
    assert!(!std::path::Path::new(work.trim()).exists());
    assert!(host.join("file.txt").exists(), "the host copy is untouched");
}

#[tokio::test]
async fn concurrent_executions_do_not_share_a_mounted_copy() {
    let root = tempfile::tempdir().unwrap();
    let host = host_tree(root.path());
    let mut handles = vec![];
    for index in 0..4 {
        let host = host.clone();
        let directory = root.path().join(format!("run-{index}"));
        std::fs::create_dir_all(&directory).unwrap();
        handles.push(tokio::spawn(async move {
            // Each guest rewrites its copy, waits for the others to do the
            // same, then reads its own back.
            let request = with_output(
                with_mount(
                    shell(
                        &directory,
                        &format!(
                            "echo {index} > \"$COMPUTE_WORK_DIR/data/file.txt\"\nsleep 0.3\ncat \"$COMPUTE_WORK_DIR/data/file.txt\" > \"$COMPUTE_OUTPUT_DIR/seen.txt\"\n"
                        ),
                    ),
                    &host,
                    "work/data",
                ),
                "seen.txt",
            );
            Compute::new().run(request).await.unwrap()
        }));
    }
    for (index, handle) in handles.into_iter().enumerate() {
        let result = handle.await.unwrap();
        assert_eq!(result.outputs[0].data, format!("{index}\n").into_bytes());
    }
    assert_eq!(
        std::fs::read_to_string(host.join("file.txt")).unwrap(),
        "original\n"
    );
}

#[tokio::test]
async fn a_guest_path_that_escapes_the_workspace_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let host = host_tree(root.path());
    let ran = root.path().join("ran");
    for guest in ["../escape", "work/../../escape", "work/./x/../../.."] {
        let request = with_mount(
            shell(root.path(), &format!("touch {}\n", ran.display())),
            &host,
            guest,
        );
        let message = refusal(&Compute::new().run(request).await.unwrap());
        assert!(message.contains("invalid mount path"), "{guest}: {message}");
        assert!(!ran.exists(), "{guest}: the workload ran anyway");
    }
}

#[tokio::test]
async fn a_symlink_cannot_be_mounted() {
    let root = tempfile::tempdir().unwrap();
    let host = host_tree(root.path());
    std::os::unix::fs::symlink("/etc/passwd", host.join("link")).unwrap();
    let request = with_mount(shell(root.path(), "true"), &host, "work/data");
    let message = refusal(&Compute::new().run(request).await.unwrap());
    assert!(message.contains("symlink"), "{message}");

    let link = root.path().join("top-level-link");
    std::os::unix::fs::symlink(&host, &link).unwrap();
    let request = with_mount(shell(root.path(), "true"), &link, "work/data");
    let message = refusal(&Compute::new().run(request).await.unwrap());
    assert!(message.contains("symlink"), "{message}");
}

#[tokio::test]
async fn a_missing_host_path_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let request = with_mount(
        shell(root.path(), "true"),
        &root.path().join("nope"),
        "work/data",
    );
    let message = refusal(&Compute::new().run(request).await.unwrap());
    assert!(message.to_lowercase().contains("no such file"), "{message}");
}
