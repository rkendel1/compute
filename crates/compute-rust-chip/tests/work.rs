//! Rust Chip's work loop over real Compute sessions: equivalence with the local environment,
//! Git isolation, and failure isolation. Real Compute target, real Git, real PAX, real Cargo; the
//! model is a scripted `ModelProvider` (there is no live model on the build machine).

mod support;

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chip_cli::local_environment::LocalEnvironment;
use chip_cli::software_work::{CompleteWhenVerified, SoftwareWork, run_software_work_with_budget};
use chip_core::{
    EnvironmentDescription, EnvironmentId, Environments, ExecutionId, ExecutionRequest, Executor,
    InputValue, WorkEnvironment, WorkId, WorkLimits, WorkOutcome,
};
use compute_core::SessionStatus;
use compute_rust_chip::ComputeSessionEnvironments;
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};
use serde_json::json;
use support::*;

enum Behavior {
    Replies(VecDeque<String>),
    Error,
    Panic,
}

/// A model that answers per work (by the marker in the request) from a script.
struct Model(
    Mutex<HashMap<&'static str, Behavior>>,
    Mutex<HashMap<&'static str, usize>>,
);

impl Model {
    fn new(entries: Vec<(&'static str, Behavior)>) -> Arc<Self> {
        Arc::new(Self(
            Mutex::new(entries.into_iter().collect()),
            Mutex::default(),
        ))
    }
}

#[async_trait::async_trait]
impl ModelProvider for Model {
    async fn complete(&self, r: ModelRequest) -> Result<ModelResponse, FxError> {
        let text: String = r.messages.iter().map(|m| m.content.as_str()).collect();
        let (reply, id) = {
            let mut map = self.0.lock().unwrap();
            let (marker, behavior) = map
                .iter_mut()
                .find(|(marker, _)| text.contains(**marker))
                .expect("a request that names no work");
            match behavior {
                Behavior::Panic => panic!("the model for this work panicked"),
                Behavior::Error => {
                    return Err(FxError::Provider("this work's model is down".into()));
                }
                Behavior::Replies(q) => {
                    let mut counts = self.1.lock().unwrap();
                    let n = counts.entry(*marker).or_default();
                    *n += 1;
                    // A distinct response id per work and call: Chip derives execution ids from it.
                    (q.pop_front(), format!("{}-{n}", marker.to_lowercase()))
                }
            }
        };
        Ok(ModelResponse::new(
            id,
            reply.unwrap_or_else(|| "{}".into()),
            Usage::new(3, 2),
        ))
    }
}

fn replies(list: Vec<String>) -> Behavior {
    Behavior::Replies(list.into())
}

fn write(path: &str, content: &str) -> String {
    json!({"decision": "request_capability", "capability": "project.write",
           "inputs": {"path": path, "content": content}})
    .to_string()
}

fn capability(name: &str) -> String {
    json!({"decision": "request_capability", "capability": name}).to_string()
}

const LIMITS: WorkLimits = WorkLimits {
    max_turns: 12,
    max_executions: 8,
};

async fn work(env: &dyn WorkEnvironment, model: Arc<Model>, goal: &str) -> SoftwareWork {
    run_software_work_with_budget(
        WorkId::new(goal.split(':').next().unwrap().to_string()),
        model,
        "scripted".into(),
        env,
        goal,
        LIMITS,
        &CompleteWhenVerified,
        None,
    )
    .await
}

async fn run_in(env: &dyn WorkEnvironment, req: ExecutionRequest) -> String {
    env.capabilities()
        .execute(req)
        .await
        .map(|r| r.output)
        .unwrap_or_else(|e| e.to_string())
}

fn request(id: &str, capability: &str, inputs: &[(&str, &str)]) -> ExecutionRequest {
    ExecutionRequest::new(ExecutionId::new(id), capability).with_inputs(
        inputs
            .iter()
            .map(|(k, v)| (k.to_string(), InputValue::Text(v.to_string())))
            .collect(),
    )
}

async fn destroyed(provider: &ComputeSessionEnvironments, session: &str) -> bool {
    for _ in 0..200 {
        if provider.remote().session(session).await.unwrap().status == SessionStatus::Destroyed {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

fn shape(w: &SoftwareWork) -> Vec<String> {
    w.report
        .events
        .iter()
        .map(|e| {
            format!("{e:?}")
                .split([' ', '{', '('])
                .next()
                .unwrap()
                .to_string()
        })
        .collect()
}

fn local_copy(source: &Path, tag: &str) -> LocalEnvironment {
    let root = std::env::temp_dir().join(format!(
        "compute-rust-chip-local-{}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    assert!(
        std::process::Command::new("git")
            .args(["clone", "-q"])
            .arg(source)
            .arg(&root)
            .status()
            .unwrap()
            .success()
    );
    LocalEnvironment::new(
        EnvironmentId::new("env_local"),
        &root,
        chip_pax::PaxExecutor::new(&root),
        EnvironmentDescription::default(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_work_is_the_same_standalone_and_in_compute() {
    if !pax_installed() {
        return;
    }
    let target = Target::start();
    let source = project("equivalence", true);
    let script = || vec![write("README.md", "SAME\n"), capability("pax.test")];

    let local_work = work(
        &local_copy(&source, "eq"),
        Model::new(vec![("EQ", replies(script()))]),
        "EQ: edit the README and verify.",
    )
    .await;

    let provider = provider(&target, &source, 1);
    let envs = Environments::new(provider.clone());
    let owned = envs.acquire(WorkId::new("EQ")).await.unwrap();
    let compute_work = work(
        owned.environment(),
        Model::new(vec![("EQ", replies(script()))]),
        "EQ: edit the README and verify.",
    )
    .await;

    for w in [&local_work, &compute_work] {
        assert!(
            matches!(w.report.outcome, WorkOutcome::Completed { .. }),
            "{:?}",
            w.report.outcome
        );
        assert!(
            w.verified,
            "verified from PAX's own observation, not from execution success"
        );
        w.audit.assert_clean();
        assert_eq!(w.trajectory_violations, 0);
    }
    // The agent's behaviour is identical; only the environment under it differs.
    assert_eq!(shape(&local_work), shape(&compute_work));
    assert_eq!(
        local_work.report.summary.executions,
        compute_work.report.summary.executions
    );
    assert_eq!(local_work.paths_written, compute_work.paths_written);
    // The edit is in the Compute session, not in the source project.
    assert_eq!(
        std::fs::read_to_string(source.join("README.md")).unwrap(),
        "baseline\n"
    );
    let session = provider.session_of(owned.id()).unwrap();
    drop(owned);
    assert!(destroyed(&provider, &session).await);
    // Compute recorded real commands for it, and that is not the goal's verification.
    assert!(provider.stats().commands_run > 5);
}

#[tokio::test(flavor = "multi_thread")]
async fn git_state_is_each_environments_own() {
    if !pax_installed() {
        return;
    }
    let target = Target::start();
    let source = project("git", true);
    let provider = provider(&target, &source, 2);
    let envs = Arc::new(Environments::new(provider.clone()));
    let (a, b) = tokio::join!(
        envs.acquire(WorkId::new("A")),
        envs.acquire(WorkId::new("B"))
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    let model = Model::new(vec![
        (
            "ALPHA",
            replies(vec![write("README.md", "ALPHA\n"), capability("pax.test")]),
        ),
        (
            "BRAVO",
            replies(vec![write("NOTES.md", "BRAVO\n"), capability("pax.test")]),
        ),
    ]);
    let (wa, wb) = tokio::join!(
        work(a.environment(), model.clone(), "ALPHA: edit README.md."),
        work(b.environment(), model.clone(), "BRAVO: add NOTES.md.")
    );
    assert!(wa.verified && wb.verified);

    let (sa, sb) = (
        run_in(a.environment(), request("g", "project.git.status", &[])).await,
        run_in(b.environment(), request("g", "project.git.status", &[])).await,
    );
    assert!(sa.contains("README.md") && !sa.contains("NOTES.md"), "{sa}");
    assert!(sb.contains("NOTES.md") && !sb.contains("README.md"), "{sb}");
    let (da, db) = (
        run_in(a.environment(), request("d", "project.git.diff", &[])).await,
        run_in(b.environment(), request("d", "project.git.diff", &[])).await,
    );
    assert!(da.contains("ALPHA") && !da.contains("BRAVO"), "{da}");
    assert!(!db.contains("ALPHA"), "{db}");
    // Execution ids and evidence are each work's own.
    let ids = |w: &SoftwareWork| -> Vec<String> {
        w.report
            .observations
            .iter()
            .map(|o| o.execution_id.to_string())
            .collect()
    };
    assert!(
        ids(&wa).iter().all(|id| !ids(&wb).contains(id)),
        "execution ids crossed"
    );
    assert_eq!(wa.report.summary.observations, 2);
    assert_eq!(wb.report.summary.observations, 2);
    // The source repository is untouched.
    let out = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&source)
        .output()
        .unwrap();
    assert!(out.stdout.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn one_works_failures_do_not_touch_another_and_every_environment_is_released() {
    if !pax_installed() {
        return;
    }
    let target = Target::start();
    let source = project("failures", true);
    let provider = provider(&target, &source, 4);
    let envs = Arc::new(Environments::new(provider.clone()));
    let broken = "pub fn double(x: i32) -> i32 { x +\n";
    let model = Model::new(vec![
        // PAX fails for real in A (the build is broken by A's own edit).
        (
            "ALPHA",
            replies(vec![write("src/lib.rs", broken), capability("pax.test")]),
        ),
        // The model fails in C; the model panics in D.
        ("CHARLIE", Behavior::Error),
        ("DELTA", Behavior::Panic),
        // B is unaffected.
        (
            "BRAVO",
            replies(vec![write("README.md", "BRAVO\n"), capability("pax.test")]),
        ),
    ]);
    let mut sessions = Vec::new();
    let mut tasks = Vec::new();
    for goal in [
        "ALPHA: break the build.",
        "BRAVO: edit the README.",
        "CHARLIE: model that errors.",
        "DELTA: model that panics.",
    ] {
        let owned = envs
            .acquire(WorkId::new(goal.split(':').next().unwrap()))
            .await
            .unwrap();
        sessions.push(provider.session_of(owned.id()).unwrap());
        let model = model.clone();
        tasks.push((
            goal,
            tokio::spawn(async move {
                // The environment moves into the task: it is released when the task ends, however it ends.
                let w = work(owned.environment(), model, goal).await;
                drop(owned);
                w
            }),
        ));
    }
    let mut results = Vec::new();
    for (goal, task) in tasks {
        results.push((goal, task.await));
    }
    let outcome = |name: &str| &results.iter().find(|(g, _)| g.starts_with(name)).unwrap().1;
    // A: real execution that failed the goal; it is not success and not verified.
    let a = outcome("ALPHA").as_ref().unwrap();
    assert!(!a.verified && !matches!(a.report.outcome, WorkOutcome::Completed { .. }));
    assert_eq!(a.audit.false_completions, 0);
    // B: independently verified, unaffected by A, C and D.
    let b = outcome("BRAVO").as_ref().unwrap();
    assert!(b.verified && matches!(b.report.outcome, WorkOutcome::Completed { .. }));
    b.audit.assert_clean();
    // C: the model failed; it ended without executing anything.
    let c = outcome("CHARLIE").as_ref().unwrap();
    assert!(!c.verified);
    assert_eq!(c.report.summary.executions, 0);
    // D: a panic ended that task only.
    assert!(
        outcome("DELTA")
            .as_ref()
            .err()
            .expect("D panicked")
            .is_panic()
    );
    // Every session was destroyed, including the one whose work panicked.
    for session in &sessions {
        assert!(
            destroyed(&provider, session).await,
            "{session} was not destroyed"
        );
    }
    let stats = provider.stats();
    assert_eq!(
        (stats.acquired, stats.released, stats.cleanup_failed),
        (4, 4, 0)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cleanup_failure_keeps_the_work_result_and_never_wedges_the_next_work() {
    if !pax_installed() {
        return;
    }
    let mut target = Target::start();
    let source = project("cleanup", true);
    let provider = provider(&target, &source, 1);
    let envs = Environments::new(provider.clone());

    let owned = envs.acquire(WorkId::new("ALPHA")).await.unwrap();
    let first = work(
        owned.environment(),
        Model::new(vec![(
            "ALPHA",
            replies(vec![write("README.md", "ALPHA\n"), capability("pax.test")]),
        )]),
        "ALPHA: edit the README.",
    )
    .await;
    assert!(first.verified && matches!(first.report.outcome, WorkOutcome::Completed { .. }));
    // The machine goes away before the environment is released: destroying the session fails.
    target.stop();
    drop(owned);
    for _ in 0..200 {
        if provider.stats().cleanup_failed == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        provider.stats().cleanup_failed,
        1,
        "recorded as an operational failure"
    );
    // The work's result stands; cleanup did not rewrite it.
    assert!(first.verified);
    // And the slot is free: with the target back, the next work acquires and runs.
    target.restart();
    let owned = envs
        .acquire(WorkId::new("BRAVO"))
        .await
        .expect("a failed cleanup must not wedge the next acquire");
    let second = work(
        owned.environment(),
        Model::new(vec![(
            "BRAVO",
            replies(vec![write("README.md", "BRAVO\n"), capability("pax.test")]),
        )]),
        "BRAVO: edit the README.",
    )
    .await;
    assert!(second.verified);
}
