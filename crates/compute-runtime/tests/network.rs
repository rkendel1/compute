//! Network is a requirement of the workload, and an unenforceable one fails
//! closed: the runtime refuses to run rather than run with more network than
//! was asked for. (Whether the *host* profile can enforce `none`/`localhost`
//! with namespaces is covered in `compute-core/src/host.rs`.)

mod support;

use compute_core::{ComputeError, ExecutionStatus, NetworkPolicy};
use compute_runtime::Compute;
use support::shell;

#[tokio::test]
async fn a_network_requirement_the_runtime_cannot_enforce_is_refused_not_ignored() {
    let root = tempfile::tempdir().unwrap();
    let ran = root.path().join("ran");
    for policy in [NetworkPolicy::None, NetworkPolicy::Localhost] {
        let mut request = shell(root.path(), &format!("touch {}\n", ran.display()));
        request.network = policy.clone();
        match Compute::new().run(request).await {
            Err(ComputeError::IsolationUnavailable { code, .. }) => {
                assert_eq!(code, "network_policy_unavailable", "{policy:?}");
            }
            other => panic!("{policy:?}: expected a refusal, got {other:?}"),
        }
        assert!(!ran.exists(), "{policy:?}: the workload ran anyway");
    }
}

#[tokio::test]
async fn network_on_is_what_an_unconfined_runtime_offers() {
    let root = tempfile::tempdir().unwrap();
    let request = shell(root.path(), "true\n");
    assert_eq!(request.network, NetworkPolicy::Network);
    let result = Compute::new().run(request).await.unwrap();
    assert_eq!(result.status, ExecutionStatus::Completed);
    assert_eq!(result.network, NetworkPolicy::Network);
}

#[test]
fn the_refusal_has_a_stable_machine_readable_code() {
    let error = ComputeError::IsolationUnavailable {
        runtime: compute_core::RuntimeKind::Shell,
        profile: compute_core::IsolationProfile::Process,
        code: "network_policy_unavailable".into(),
    };
    assert_eq!(error.code(), "isolation_unavailable");
}
