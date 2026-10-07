//! The agent boundary, held against a real Compute target: sessions, jobs, bearer-token
//! authorization and workspace isolation are Compute's own. The agents here are plain scripts;
//! the point is that Compute needs to know nothing about them.

mod support;

use std::collections::BTreeMap;

use compute_agent::{AgentHost, AgentSpec};
use compute_core::SessionStatus;
use support::*;

fn scripts() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// Writes `word` over the README in the session's project, then prints what it sees.
const EDIT: &str = r#"cd project && printf '%s\n' "$WORD" > README.md && cat README.md"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_generic_agent_is_launched_through_the_agent_boundary() {
    let target = Target::start();
    let source = project("generic");
    let dir = scripts();
    let program = agent_script(dir.path(), "any-agent", EDIT);
    let host = AgentHost::new(host_config(&target, &source));

    let session = host.acquire().await.unwrap();
    let spec = AgentSpec::new("anything", program).env("WORD", "ALPHA");
    let outcome = session.launch(&spec).await.unwrap();
    assert_eq!(outcome.exit_code, Some(0));
    assert_eq!(outcome.stdout, "ALPHA\n");
    assert!(outcome.job_id.starts_with("job_") || !outcome.job_id.is_empty());
    session.release().await;
    assert_eq!(host.stats().released, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn two_concurrent_agents_modify_the_same_logical_project_independently() {
    let target = Target::start();
    let source = project("collision");
    let dir = scripts();
    let program = agent_script(dir.path(), "editor", EDIT);
    let host = AgentHost::new(host_config(&target, &source));

    let (a, b) = tokio::join!(host.acquire(), host.acquire());
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_ne!(a.session_id(), b.session_id());
    assert_ne!(a.opaque_id(), b.opaque_id());

    let spec_a = AgentSpec::new("a", program.clone()).env("WORD", "ALPHA");
    let spec_b = AgentSpec::new("b", program.clone()).env("WORD", "BRAVO");
    let (alpha, bravo) = tokio::join!(a.launch(&spec_a), b.launch(&spec_b));
    assert_eq!(alpha.unwrap().stdout, "ALPHA\n");
    assert_eq!(bravo.unwrap().stdout, "BRAVO\n");

    // Each session still holds only its own agent's work.
    let cat = || vec!["cat".to_string(), "project/README.md".into()];
    assert_eq!(
        a.exec(cat(), BTreeMap::new()).await.unwrap().stdout,
        "ALPHA\n"
    );
    assert_eq!(
        b.exec(cat(), BTreeMap::new()).await.unwrap().stdout,
        "BRAVO\n"
    );
    // The source repository is untouched.
    assert_eq!(
        std::fs::read_to_string(source.join("README.md")).unwrap(),
        "baseline\n"
    );

    a.release().await;
    b.release().await;
    let stats = host.stats();
    assert_eq!(
        (stats.acquired, stats.released, stats.cleanup_failed),
        (2, 2, 0)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_receives_nothing_of_another_agent_or_of_compute() {
    let target = Target::start();
    let source = project("isolation");
    let dir = scripts();
    // Dumps its whole environment and argv.
    let program = agent_script(dir.path(), "snoop", r#"env; echo "ARGV:$0 $*""#);
    let host = AgentHost::new(host_config(&target, &source));
    let (a, b) = (host.acquire().await.unwrap(), host.acquire().await.unwrap());

    let seen = a
        .launch(&AgentSpec::new("a", program).arg("x").env("MINE", "a"))
        .await
        .unwrap()
        .stdout;
    // Nothing of another agent, and nothing of the operator's side of Compute.
    for forbidden in [
        b.session_id(),
        &a.opaque_id(),
        &b.opaque_id(),
        &source.display().to_string(),
        &target.token,
        &target.endpoint,
    ] {
        assert!(!seen.contains(forbidden), "{forbidden} leaked to the agent");
    }
    // Compute's session execution contract gives a command its *own* session's identity and
    // workspace (COMPUTE_SESSION_ID / COMPUTE_SESSION_WORKSPACE). That is the agent's own
    // computer, never another's; an agent that must not see it (Rust Chip's model never does)
    // simply does not forward it.
    assert!(seen.contains(&format!("COMPUTE_SESSION_ID={}", a.session_id())));
    assert!(seen.contains("MINE=a"));

    // The opaque id is the only identity an agent contract may carry.
    assert!(a.opaque_id().starts_with("env_") && !a.opaque_id().contains(a.session_id()));
    a.release().await;
    b.release().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn one_failed_agent_does_not_affect_another_and_compute_does_not_judge_it() {
    let target = Target::start();
    let source = project("failure");
    let dir = scripts();
    let fails = agent_script(dir.path(), "fails", "echo broke >&2; exit 7");
    let works = agent_script(dir.path(), "works", EDIT);
    let host = AgentHost::new(host_config(&target, &source));
    let (a, b) = (host.acquire().await.unwrap(), host.acquire().await.unwrap());

    let (spec_a, spec_b) = (
        AgentSpec::new("a", fails),
        AgentSpec::new("b", works).env("WORD", "BRAVO"),
    );
    let (failed, fine) = tokio::join!(a.launch(&spec_a), b.launch(&spec_b));
    // A non-zero exit is reported as what happened, not turned into a Compute error.
    let failed = failed.unwrap();
    assert_eq!(failed.exit_code, Some(7));
    assert_eq!(failed.stderr, "broke\n");
    assert_eq!(fine.unwrap().stdout, "BRAVO\n");

    // A's failure neither ended B's session nor stopped A's own from being used and released.
    let status = |id: &str| {
        let host = host.clone();
        let id = id.to_string();
        async move { host.remote().session(&id).await.unwrap().status }
    };
    assert!(!status(b.session_id()).await.is_terminal());
    a.release().await;
    assert!(!status(b.session_id()).await.is_terminal());
    let again = b
        .exec(
            vec!["cat".into(), "project/README.md".into()],
            BTreeMap::new(),
        )
        .await;
    assert_eq!(again.unwrap().stdout, "BRAVO\n");
    b.release().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn acquisition_fails_closed_and_no_agent_runs() {
    let target = Target::start();
    let missing = std::env::temp_dir().join("compute-agent-no-such-project");
    let dir = scripts();
    let marker = dir.path().join("ran");
    let program = agent_script(dir.path(), "marker", &format!("touch {}", marker.display()));
    let host = AgentHost::new(host_config(&target, &missing));

    // The caller launches only after a successful acquire; there is no session to launch in.
    match host.acquire().await {
        Ok(session) => session
            .launch(&AgentSpec::new("a", program))
            .await
            .map(|_| ())
            .unwrap(),
        Err(why) => assert!(why.0.contains("project could not be loaded"), "{why}"),
    }
    assert!(!marker.exists(), "an agent ran after acquisition failed");

    // The partially created session was destroyed, not left behind.
    let stats = host.stats();
    assert_eq!(
        (stats.acquired, stats.acquire_failed, stats.released),
        (0, 1, 1)
    );
    for session in host.remote().sessions().await.unwrap() {
        assert_eq!(session.status, SessionStatus::Destroyed);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_cleanup_is_recorded_and_changes_no_result() {
    let mut target = Target::start();
    let source = project("cleanup");
    let dir = scripts();
    let program = agent_script(dir.path(), "editor", EDIT);
    let host = AgentHost::new(host_config(&target, &source));
    let session = host.acquire().await.unwrap();
    let outcome = session
        .launch(&AgentSpec::new("a", program).env("WORD", "ALPHA"))
        .await
        .unwrap();

    target.stop(); // the machine goes away before release
    session.release().await;
    assert_eq!(host.stats().cleanup_failed, 1);
    assert_eq!(host.stats().released, 0);
    assert_eq!(
        (outcome.exit_code, outcome.stdout.as_str()),
        (Some(0), "ALPHA\n")
    );
}
