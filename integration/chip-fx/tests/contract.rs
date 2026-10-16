//! The Compute -> Chip -> FX contract, with fixtures only. No live provider, no credentials, no
//! production side effect: the only "effect" is a counter inside the fixture host.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use compute_chip_fx_harness::*;
use fx_core::{Part, Role};
use fx_single_attempt::{Observed, PortFailure, PortFailureKind};

fn failure(kind: PortFailureKind, observed: Observed, status: Option<u16>) -> PortFailure {
    PortFailure {
        kind,
        message: "scripted".into(),
        observed,
        status,
        retry_after: None,
    }
}

fn reply(text: &str) -> Attempt {
    Attempt::Reply(text.into())
}

fn reason(evidence: &Evidence) -> String {
    match evidence.outcome() {
        chip_core::WorkOutcome::Failed { reason }
        | chip_core::WorkOutcome::Blocked { reason }
        | chip_core::WorkOutcome::Escalated { reason } => reason.clone(),
        other => panic!("expected a non-success outcome with a reason, got {other:?}"),
    }
}

fn texts(request: &fx_core::Request) -> String {
    request
        .messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            Part::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 1-4 and 7: a request enters through the environment boundary; Chip makes one FX attempt; the
/// model's tool proposal is authorized by the host before it executes; its result reaches the
/// next FX attempt; the work completes; every kind of fact is on its own line of evidence.
#[tokio::test]
async fn a_proposal_is_authorized_by_the_host_executed_once_and_fed_to_the_next_inference() {
    let host = HostBoundary::new();
    let provider = ScriptedProvider::new(vec![reply(REQUEST_SELFTEST), reply(COMPLETE)]);
    let requests = provider.requests.clone();
    let e = run_work(host.clone(), provider, None).await;

    assert!(
        matches!(e.outcome(), chip_core::WorkOutcome::Completed { .. }),
        "{:?}",
        e.outcome()
    );
    assert_eq!(e.fx_attempts, 2, "one FX attempt per model call");
    assert_eq!(
        e.provider_invocations, 2,
        "and one provider call per FX attempt"
    );
    assert_eq!(
        e.proposed,
        vec![SELFTEST.to_string()],
        "what the model proposed"
    );
    // The host's ledger: authorized strictly before executed, the same execution id, once.
    assert_eq!(e.host.len(), 2, "{:?}", e.host);
    let (
        Host::Authorized {
            execution_id: a, ..
        },
        Host::Executed {
            execution_id: x, ..
        },
    ) = (&e.host[0], &e.host[1])
    else {
        panic!("authorization must precede execution: {:?}", e.host);
    };
    assert_eq!(a, x);
    assert_eq!(host.effects(), 1);
    assert_eq!(host.duplicate_effects(), 0);
    // The tool result is what the second inference was given.
    let second = texts(&requests.lock().unwrap()[1]);
    assert!(
        second.contains("self test passed"),
        "the observation reached the next request: {second}"
    );
    // The environment was acquired once and released once.
    assert_eq!((e.environments_acquired, e.environments_released), (1, 1));
}

/// FX never sees a tool: the inference request is text only, and the tool is a decision Chip parsed.
#[tokio::test]
async fn the_inference_request_carries_no_tool_definitions_and_the_roles_are_chips_not_the_hosts() {
    let host = HostBoundary::new();
    let provider = ScriptedProvider::new(vec![reply(COMPLETE)]);
    let requests = provider.requests.clone();
    let _ = run_work(host, provider, None).await;
    let request = requests.lock().unwrap()[0].clone();
    assert!(
        request
            .messages
            .iter()
            .all(|m| m.parts.iter().all(|p| matches!(p, Part::Text(_))))
    );
    assert!(request.messages.iter().any(|m| m.role == Role::User));
}

/// 5 and 6: authorization denial is explicit, nothing executes, and nothing is retried or repeated.
#[tokio::test]
async fn a_capability_the_host_revoked_after_declaring_it_is_denied_and_never_executes() {
    let host = HostBoundary::with(&[SELFTEST], &[]); // declared to Chip, not authorized by policy
    let provider = ScriptedProvider::new(vec![reply(REQUEST_SELFTEST), reply(COMPLETE)]);
    let e = run_work(host.clone(), provider, None).await;
    assert_eq!(host.effects(), 0, "a denied request has no effect");
    assert!(e.executed().is_empty(), "{:?}", e.host);
    assert!(
        e.host.iter().any(|h| matches!(h, Host::Denied { .. })),
        "the denial is on the ledger, distinct from an execution: {:?}",
        e.host
    );
    assert_eq!(
        e.proposed,
        vec![SELFTEST.to_string()],
        "it WAS proposed; proposal is not authorization"
    );
    assert!(
        !matches!(e.outcome(), chip_core::WorkOutcome::Completed { .. }),
        "{:?}",
        e.outcome()
    );
    assert!(e.provider_invocations <= 2);
}

#[tokio::test]
async fn a_capability_that_was_never_offered_is_rejected_by_chip_and_never_reaches_the_host() {
    for proposal in [
        r#"{"decision":"request_capability","capability":"deploy.production"}"#,
        r#"{"decision":"request_capability","capability":"rm -rf /"}"#,
        r#"{"decision":"request_capability","capability":"compute.selftest","command":"rm -rf /"}"#,
    ] {
        let host = HostBoundary::new();
        let e = run_work(
            host.clone(),
            ScriptedProvider::new(vec![reply(proposal)]),
            None,
        )
        .await;
        assert!(
            !matches!(e.outcome(), chip_core::WorkOutcome::Completed { .. }),
            "{proposal}"
        );
        assert!(
            e.host.is_empty(),
            "{proposal}: the host was never asked: {:?}",
            e.host
        );
        assert_eq!(host.effects(), 0, "{proposal}");
        assert_eq!(
            e.provider_invocations, 1,
            "{proposal}: no second inference to repair it"
        );
    }
}

/// 5 and 6: a provider failure is a terminal, explicit outcome that keeps FX's delivery certainty.
/// It is neither retried nor routed to another executor.
#[tokio::test]
async fn a_provider_failure_ends_the_work_explicitly_without_retry_or_fallback() {
    let host = HostBoundary::new();
    let provider = ScriptedProvider::new(vec![
        Attempt::Fail(failure(
            PortFailureKind::ProviderError,
            Observed::Invoked,
            Some(429),
        )),
        reply(COMPLETE), // would complete the work if anything retried
    ]);
    let e = run_work(host.clone(), provider, None).await;
    let text = reason(&e);
    assert!(matches!(e.outcome(), chip_core::WorkOutcome::Failed { .. }));
    assert!(
        text.contains("possibly_sent"),
        "an error never lowers delivery certainty: {text}"
    );
    assert!(text.contains("not retried"), "{text}");
    assert_eq!(e.provider_invocations, 1);
    assert_eq!(e.fx_attempts, 1);
    assert_eq!(host.effects(), 0);
    assert!(e.host.is_empty());
}

/// 6: a provider failure AFTER a tool already ran does not repeat the tool's effect.
#[tokio::test]
async fn a_failure_after_an_executed_effect_does_not_repeat_the_effect() {
    let host = HostBoundary::new();
    let provider = ScriptedProvider::new(vec![
        reply(REQUEST_SELFTEST),
        Attempt::Fail(failure(
            PortFailureKind::ProviderError,
            Observed::Invoked,
            Some(500),
        )),
        reply(REQUEST_SELFTEST), // what a replaying loop would ask for
    ]);
    let e = run_work(host.clone(), provider, None).await;
    assert!(
        matches!(e.outcome(), chip_core::WorkOutcome::Failed { .. }),
        "{:?}",
        e.outcome()
    );
    assert_eq!(host.effects(), 1, "the effect happened once and only once");
    assert_eq!(host.duplicate_effects(), 0);
    assert_eq!(e.provider_invocations, 2, "no third attempt");
    assert_eq!(e.executed().len(), 1);
    // The committed result is a failure; the executed effect is still on the ledger. They differ.
    assert!(
        e.executed().len() == 1 && matches!(e.outcome(), chip_core::WorkOutcome::Failed { .. })
    );
}

/// 5: cancellation before the provider call: nothing is sent, and the outcome says so.
#[tokio::test]
async fn cancellation_before_invocation_sends_nothing_and_is_not_a_completion() {
    let host = HostBoundary::new();
    let flag = Arc::new(AtomicBool::new(true));
    let provider = ScriptedProvider::new(vec![reply(COMPLETE)]);
    let e = run_work(host.clone(), provider, Some(flag)).await;
    let text = reason(&e);
    assert!(
        text.contains("cancelled") && text.contains("definitely_unsent"),
        "{text}"
    );
    assert_eq!(e.provider_invocations, 0);
    assert_eq!(host.effects(), 0);
}

/// 5: cancellation after the provider was invoked is never reported as unsent, and nothing is
/// repeated.
#[tokio::test]
async fn cancellation_after_invocation_is_not_reported_as_unsent_and_is_not_repeated() {
    let host = HostBoundary::new();
    let flag = Arc::new(AtomicBool::new(false));
    let provider = ScriptedProvider::new(vec![
        Attempt::CancelWhileStreaming(COMPLETE.into()),
        reply(COMPLETE),
    ])
    .with_cancel_flag(flag.clone());
    let e = run_work(host.clone(), provider, Some(flag)).await;
    let text = reason(&e);
    assert!(
        matches!(e.outcome(), chip_core::WorkOutcome::Failed { .. }),
        "{:?}",
        e.outcome()
    );
    assert!(text.contains("cancelled"), "{text}");
    assert!(
        !text.contains("definitely_unsent"),
        "the provider had been invoked: {text}"
    );
    assert_eq!(e.provider_invocations, 1, "no second call after a cancel");
    assert_eq!(host.effects(), 0);
}

/// 7: the four kinds of evidence are not interchangeable. A run can have a proposal with no
/// authorization, an authorization with a failed outcome, an execution with a failed outcome.
#[tokio::test]
async fn the_evidence_keeps_proposed_authorized_executed_and_the_outcome_apart() {
    // proposed, never authorized
    let denied = run_work(
        HostBoundary::with(&[SELFTEST], &[]),
        ScriptedProvider::new(vec![reply(REQUEST_SELFTEST), reply(COMPLETE)]),
        None,
    )
    .await;
    assert!(!denied.proposed.is_empty());
    assert!(
        !denied
            .host
            .iter()
            .any(|h| matches!(h, Host::Authorized { .. }))
    );

    // authorized and executed, yet the work is not completed: the effect failed, and the model's
    // claim of completion is refused because no authoritative observation satisfies the goal.
    let failed_effect = {
        let mut host = Arc::try_unwrap(HostBoundary::new())
            .ok()
            .expect("sole owner");
        host.fail_effect = true;
        let host = Arc::new(host);
        run_work(
            host,
            ScriptedProvider::new(vec![
                reply(REQUEST_SELFTEST),
                reply(COMPLETE),
                reply(COMPLETE),
                reply(COMPLETE),
            ]),
            None,
        )
        .await
    };
    assert!(
        failed_effect
            .host
            .iter()
            .any(|h| matches!(h, Host::Authorized { .. }))
    );
    assert!(
        failed_effect
            .host
            .iter()
            .any(|h| matches!(h, Host::Executed { .. }))
    );
    assert!(
        !matches!(
            failed_effect.outcome(),
            chip_core::WorkOutcome::Completed { .. }
        ),
        "an executed effect that failed is not a completed work: {:?}",
        failed_effect.outcome()
    );
}

/// Contract requirement, shown by its absence. If the integration supplies no host-owned definition of
/// done, Chip accepts the model's own claim of completion even after the only effect FAILED. The
/// integration must therefore always supply required observations (`SelftestPassed` here); this test
/// pins that this is a property of the loop, not a bug in the harness.
#[tokio::test]
async fn without_a_host_owned_definition_of_done_a_model_claim_completes_the_work() {
    let mut host = Arc::try_unwrap(HostBoundary::new())
        .ok()
        .expect("sole owner");
    host.fail_effect = true;
    let host = Arc::new(host);
    let e = run_work_with(
        host,
        ScriptedProvider::new(vec![reply(REQUEST_SELFTEST), reply(COMPLETE)]),
        None,
        false,
    )
    .await;
    assert!(
        matches!(e.outcome(), chip_core::WorkOutcome::Completed { .. }),
        "{:?}",
        e.outcome()
    );
}
