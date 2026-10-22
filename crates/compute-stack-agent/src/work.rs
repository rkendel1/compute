//! The Compute entry for running a stack-agent work in process: a work cannot start without a
//! host-owned definition of done.
//!
//! stack-agent's bare work loop (`Agent::run_work`) accepts a model's claim of completion when the
//! work carries no requirement, even after the only effect failed. The shipped service paths avoid
//! that with a typed completion contract per kind (`change`, `verify`, `inspect`: PAX observations
//! and grounded answers, chosen by the host). Any other Compute path that submits work goes through
//! [`HostWork`], which refuses a work whose `required_observations` are empty. The model never
//! defines its own success: the predicates are the host's, and the work loop only asks them.
//! See compute#93 (G6).

use chip_core::{Agent, LocalWorkPolicy, WorkDecisionBoundary, WorkReport, WorkSpec};

/// A work rejected before it ran: it names no host-owned requirement, so a model's own claim
/// would be its only definition of done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingCompletionContract {
    pub work: String,
}

impl std::fmt::Display for MissingCompletionContract {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "work {} has no required observations: the host must say what establishes that it is done; nothing was run",
            self.work
        )
    }
}

impl std::error::Error for MissingCompletionContract {}

/// A work that carries a host-owned completion contract. The only way to build one is
/// [`HostWork::new`], which validates it.
#[derive(Debug, Clone)]
pub struct HostWork {
    spec: WorkSpec,
}

impl HostWork {
    pub fn new(spec: WorkSpec) -> Result<Self, MissingCompletionContract> {
        if spec.required_observations.is_empty() {
            return Err(MissingCompletionContract {
                work: spec.id.to_string(),
            });
        }
        Ok(Self { spec })
    }

    pub fn spec(&self) -> &WorkSpec {
        &self.spec
    }

    /// Runs the work to a terminal outcome. Completion still requires every required observation
    /// to be satisfied by what was actually observed.
    pub async fn run(
        &self,
        agent: &Agent,
        policy: &dyn LocalWorkPolicy,
        boundary: &dyn WorkDecisionBoundary,
    ) -> WorkReport {
        agent.run_work(&self.spec, policy, boundary).await
    }
}
