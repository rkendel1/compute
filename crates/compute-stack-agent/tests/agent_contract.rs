//! stack-agent is launched like any other agent: through the `compute-agent` boundary, as an
//! executable with arguments, in a session. Compute reports what happened; it does not interpret it.

mod support;

use compute_agent::{AgentHost, AgentSpec, DEFAULT_AGENT};
use support::*;

#[tokio::test(flavor = "multi_thread")]
async fn stack_agent_launches_through_the_agent_contract() {
    assert_eq!(DEFAULT_AGENT, "stack-agent");
    let target = Target::start();
    let source = project("agent-contract", true);
    let mut host_config = compute_agent::HostConfig::new(target.endpoint.clone());
    host_config.token = Some(target.token.clone());
    host_config.resources.memory_bytes = Some(TEST_SESSION_MEMORY);
    host_config.project_source = Some(source.display().to_string());
    let host = AgentHost::new(host_config);

    let session = host.acquire().await.unwrap();
    // stack-agent's worker, with no subcommand, prints its usage and exits 2. Compute launched the
    // real executable in the session and reports exactly that; "2" is stack-agent's, not a verdict.
    let outcome = session
        .launch(&AgentSpec::new(DEFAULT_AGENT, worker()))
        .await
        .unwrap();
    assert_eq!(outcome.exit_code, Some(2));
    assert!(outcome.stderr.contains("usage: compute-stack-agent"));
    session.release().await;
    assert_eq!(host.stats().released, 1);
}
