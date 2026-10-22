//! G6 (compute#93): the Compute entry refuses a work without a host-owned definition of done, and
//! a model's own claim of success never completes a work that has one. No target, no PAX, no live
//! model: these run everywhere CI runs.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chip_core::{
    Agent, ExecutionStatus, ModelDecisionBoundary, NoLocalPolicy, Observation, ObservationKind,
    ObservationPredicate, WorkGoal, WorkId, WorkLimits, WorkOutcome, WorkSpec,
};
use chip_inference::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};
use compute_stack_agent::work::HostWork;

/// A model that claims success at every turn and asks for nothing.
struct Claimer(AtomicUsize);

#[async_trait::async_trait]
impl ModelProvider for Claimer {
    async fn complete(&self, _: ModelRequest) -> Result<ModelResponse, FxError> {
        let n = self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ModelResponse::new(
            format!("claim-{n}"),
            r#"{"decision":"complete","summary":"all done, trust me"}"#,
            Usage::new(3, 2),
        ))
    }
}

/// The host's definition of done: an execution the host completed and receipted.
#[derive(Debug)]
struct ExecutedWithReceipt;

impl ObservationPredicate for ExecutedWithReceipt {
    fn describe(&self) -> String {
        "an execution completed successfully with a receipt".into()
    }

    fn satisfied_by(&self, observation: &Observation) -> bool {
        observation.kind == ObservationKind::ExecutionCompleted
            && observation.status == ExecutionStatus::Success
            && observation.receipt_id.is_some()
    }
}

fn spec() -> WorkSpec {
    let mut spec = WorkSpec::new(WorkId::new("g6"), WorkGoal::new("Run the self test"));
    spec.limits = WorkLimits {
        max_turns: 3,
        max_executions: 2,
    };
    spec
}

#[test]
fn a_work_without_required_observations_is_refused_before_it_runs() {
    let error = HostWork::new(spec()).unwrap_err();
    assert_eq!(error.work, "g6");
    assert!(error.to_string().contains("nothing was run"));
    // Required outputs alone are not the contract the entry requires.
    assert!(HostWork::new(spec().with_required_output("ok")).is_err());
    assert!(HostWork::new(spec().with_required_observation(Arc::new(ExecutedWithReceipt))).is_ok());
}

#[tokio::test]
async fn a_models_claim_never_completes_a_host_work() {
    let model = Arc::new(Claimer(AtomicUsize::new(0)));
    let agent = Agent::new(model.clone());
    let work =
        HostWork::new(spec().with_required_observation(Arc::new(ExecutedWithReceipt))).unwrap();
    let report = work
        .run(&agent, &NoLocalPolicy, &ModelDecisionBoundary)
        .await;
    assert!(
        !matches!(report.outcome, WorkOutcome::Completed { .. }),
        "a model's claim completed the work: {:?}",
        report.outcome
    );
    assert!(model.0.load(Ordering::SeqCst) >= 1, "the model was asked");
}
