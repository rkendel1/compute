//! Fakes-only harness for the Compute -> Rust Chip -> FX contract.
//!
//! Nothing here is a runtime. It wires the REAL Chip work loop (`chip-core`), the REAL model-port
//! adapter (`chip-fx`) and the REAL FX executor (`fx-core` + `fx-single-attempt`) to doubles that
//! stand where the host stands:
//!
//! * [`ScriptedProvider`] sits under FX (`ProviderPort`): it is the provider.
//! * [`HostBoundary`] sits behind Chip's capability boundary (`CapabilityBackend`): it is the
//!   host that declares, authorizes and executes capabilities, and records the evidence.
//! * [`FakeEnvironments`] is an `EnvironmentProvider`, the generic contract Compute serves with a
//!   Compute session today (`crates/compute-rust-chip`).
//!
//! What this does NOT prove: that Compute's own session, admission, policy or persistence code works
//! (that is covered by `crates/compute-rust-chip/tests` at the product pin), or anything about a
//! live provider. See docs/chip-fx-integration.md.

use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use chip_core::{
    Agent, CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId,
    CapabilityProvider, CapabilitySet, EnvironmentDescription, EnvironmentError, EnvironmentId,
    EnvironmentProvider, Environments, ExecutionError, ExecutionRequest, ExecutionResult,
    ExecutionStatus, Executor, ModelDecisionBoundary, NoLocalPolicy, Observation,
    ObservationInvariant, ObservationKind, ObservationPredicate, WorkEnvironment, WorkEvent,
    WorkGoal, WorkId, WorkOutcome, WorkReport, WorkSpec,
};
use chip_fx::{CancelSignal, FxModelProvider, OptionsPolicy, RequestIds};
use fx_core::{AttemptBudget, Request};
use fx_single_attempt::testing::{done, poll};
use fx_single_attempt::{
    AbortReport, Observed, PortFailure, PortItem, PortPoll, PortSupport, ProviderPort, Terminal,
};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---- the provider under FX ------------------------------------------------------------------------

/// What one FX attempt looks like to the port.
#[derive(Clone, Debug)]
pub enum Attempt {
    /// A complete reply followed by a provider-backed finish.
    Reply(String),
    /// The call fails before any output.
    Fail(PortFailure),
    /// A reply whose stream flips the shared flag, as a caller cancelling mid-call would.
    CancelWhileStreaming(String),
}

/// A `ProviderPort` serving one scripted attempt per `invoke`. It counts invocations: a retry or a
/// fallback would show up as more invocations than the script has attempts.
pub struct ScriptedProvider {
    attempts: VecDeque<Attempt>,
    pub invokes: Arc<AtomicUsize>,
    pub requests: Arc<Mutex<Vec<Request>>>,
    cancel_flag: Option<Arc<AtomicBool>>,
}

pub struct Call {
    polls: VecDeque<Result<PortPoll, PortFailure>>,
    flip: Option<Arc<AtomicBool>>,
}

impl ScriptedProvider {
    pub fn new(attempts: Vec<Attempt>) -> Self {
        Self {
            attempts: attempts.into(),
            invokes: Arc::new(AtomicUsize::new(0)),
            requests: Arc::new(Mutex::new(Vec::new())),
            cancel_flag: None,
        }
    }

    pub fn with_cancel_flag(mut self, flag: Arc<AtomicBool>) -> Self {
        self.cancel_flag = Some(flag);
        self
    }
}

impl ProviderPort for ScriptedProvider {
    type Call = Call;

    fn support(&self) -> PortSupport {
        PortSupport::default()
    }

    fn invoke(
        &mut self,
        request: &Request,
        _budget: Option<AttemptBudget>,
    ) -> Result<Call, PortFailure> {
        self.invokes.fetch_add(1, Ordering::SeqCst);
        lock(&self.requests).push(request.clone());
        let attempt = self
            .attempts
            .pop_front()
            .expect("the provider was invoked more times than the script has attempts (a retry or fallback)");
        let finish = || done(Some("stop"), Terminal::ProviderMarker);
        match attempt {
            Attempt::Fail(failure) => Err(failure),
            Attempt::Reply(text) => Ok(Call {
                polls: VecDeque::from([Ok(poll(
                    vec![PortItem::Text(text), finish()],
                    Observed::OutputObserved,
                    true,
                ))]),
                flip: None,
            }),
            Attempt::CancelWhileStreaming(text) => Ok(Call {
                polls: VecDeque::from([
                    Ok(poll(
                        vec![PortItem::Text(text)],
                        Observed::OutputObserved,
                        false,
                    )),
                    Ok(poll(Vec::new(), Observed::OutputObserved, false)),
                    Ok(poll(vec![finish()], Observed::OutputObserved, true)),
                ]),
                flip: self.cancel_flag.clone(),
            }),
        }
    }

    fn poll(&mut self, call: &mut Call, _max_items: usize) -> Result<PortPoll, PortFailure> {
        let next = call
            .polls
            .pop_front()
            .unwrap_or_else(|| Ok(poll(Vec::new(), Observed::Invoked, true)));
        if let Some(flag) = &call.flip
            && call.polls.len() < 2
        {
            flag.store(true, Ordering::SeqCst);
        }
        next
    }

    fn abort(&mut self, _call: &mut Call) -> AbortReport {
        AbortReport {
            transport_closed: false,
        }
    }

    fn release(&mut self, _call: Call) {}
}

/// The caller's cancellation, polled by FX.
pub struct Flag(pub Arc<AtomicBool>);

impl CancelSignal for Flag {
    fn cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

// ---- the host capability boundary ------------------------------------------------------------------

/// The capability the harness's host offers. One effect, so that "the effect happened" is countable.
pub const SELFTEST: &str = "compute.selftest";

/// One fact in the host's ledger. The kinds are deliberately separate: a proposal is not an
/// authorization, an authorization is not an execution, and none of them is a committed outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Host {
    /// The host's policy allowed this execution request at the moment it was made.
    Authorized {
        execution_id: String,
        capability: String,
    },
    /// The host's policy refused it. Nothing ran.
    Denied {
        execution_id: String,
        capability: String,
        reason: String,
    },
    /// The effect happened (exactly once per execution id, or [`HostBoundary::duplicate_effects`] says so).
    Executed {
        execution_id: String,
        capability: String,
    },
}

/// The host: declares what it offers, authorizes at execution time, performs the effect, records all
/// of it. It re-checks its policy when the request arrives, so a capability that was declared and
/// later revoked is refused: a proposal (and even a declaration) is never the authorization.
pub struct HostBoundary {
    offered: BTreeSet<String>,
    policy: Mutex<BTreeSet<String>>,
    ledger: Mutex<Vec<Host>>,
    effects: AtomicUsize,
    seen_executions: Mutex<BTreeSet<String>>,
    duplicate: AtomicUsize,
    pub fail_effect: bool,
}

impl HostBoundary {
    /// Offers and authorizes `SELFTEST`.
    pub fn new() -> Arc<Self> {
        Self::with(&[SELFTEST], &[SELFTEST])
    }

    /// `offered` is what is declared to Chip; `authorized` is what the policy allows at execution time.
    pub fn with(offered: &[&str], authorized: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            offered: offered.iter().map(|s| s.to_string()).collect(),
            policy: Mutex::new(authorized.iter().map(|s| s.to_string()).collect()),
            ledger: Mutex::new(Vec::new()),
            effects: AtomicUsize::new(0),
            seen_executions: Mutex::new(BTreeSet::new()),
            duplicate: AtomicUsize::new(0),
            fail_effect: false,
        })
    }

    pub fn revoke(&self, capability: &str) {
        lock(&self.policy).remove(capability);
    }

    pub fn ledger(&self) -> Vec<Host> {
        lock(&self.ledger).clone()
    }

    /// How many real effects happened.
    pub fn effects(&self) -> usize {
        self.effects.load(Ordering::SeqCst)
    }

    /// How many times the same execution id was executed more than once. Must stay 0.
    pub fn duplicate_effects(&self) -> usize {
        self.duplicate.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl CapabilityProvider for HostBoundary {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        self.offered
            .iter()
            .map(|id| {
                Ok(CapabilityDescriptor::new(
                    CapabilityId::new(id.clone())?,
                    id.clone(),
                    "a fixture capability",
                ))
            })
            .collect()
    }

    async fn availability(&self, id: &CapabilityId) -> CapabilityAvailability {
        if self.offered.contains(id.as_str()) {
            CapabilityAvailability::Available
        } else {
            CapabilityAvailability::Unavailable(format!("{id} is not offered by this host"))
        }
    }
}

#[async_trait]
impl Executor for HostBoundary {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        let capability = request.intent.clone();
        let execution_id = request.id.0.clone();
        let allowed = lock(&self.policy).contains(&capability);
        if !allowed {
            lock(&self.ledger).push(Host::Denied {
                execution_id,
                capability,
                reason: "not authorized by host policy".into(),
            });
            return Err(ExecutionError::InvalidRequest(
                "the host did not authorize this capability".into(),
            ));
        }
        lock(&self.ledger).push(Host::Authorized {
            execution_id: execution_id.clone(),
            capability: capability.clone(),
        });
        if !lock(&self.seen_executions).insert(execution_id.clone()) {
            self.duplicate.fetch_add(1, Ordering::SeqCst);
        }
        self.effects.fetch_add(1, Ordering::SeqCst);
        lock(&self.ledger).push(Host::Executed {
            execution_id: execution_id.clone(),
            capability,
        });
        if self.fail_effect {
            return Ok(ExecutionResult::failure(request.id, "the effect failed"));
        }
        Ok(ExecutionResult::success(request.id, "self test passed")
            .with_receipt_id("fixture-receipt"))
    }
}

// ---- the environment boundary Compute serves --------------------------------------------------------

struct FakeEnvironment {
    id: EnvironmentId,
    set: Arc<CapabilitySet>,
}

impl WorkEnvironment for FakeEnvironment {
    fn id(&self) -> &EnvironmentId {
        &self.id
    }
    fn capabilities(&self) -> Arc<CapabilitySet> {
        self.set.clone()
    }
    fn observation_invariants(&self) -> Vec<Arc<dyn ObservationInvariant>> {
        Vec::new()
    }
    fn description(&self) -> EnvironmentDescription {
        EnvironmentDescription {
            verifier_version: None,
            diagnostic: None,
        }
    }
}

/// An `EnvironmentProvider` that hands out one environment, backed by the host boundary. This is the
/// seam `ComputeSessionEnvironments` (compute-rust-chip) implements with a Compute session.
pub struct FakeEnvironments {
    host: Arc<HostBoundary>,
    pub acquired: AtomicUsize,
    pub released: AtomicUsize,
}

impl FakeEnvironments {
    pub fn new(host: Arc<HostBoundary>) -> Arc<Self> {
        Arc::new(Self {
            host,
            acquired: AtomicUsize::new(0),
            released: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl EnvironmentProvider for FakeEnvironments {
    fn isolation_capacity(&self) -> usize {
        1
    }

    async fn acquire(&self, work: &WorkId) -> Result<Arc<dyn WorkEnvironment>, EnvironmentError> {
        self.acquired.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(FakeEnvironment {
            id: EnvironmentId::new(format!("fixture-env-{}", work.as_str())),
            set: Arc::new(CapabilitySet::new().with(self.host.clone())),
        }))
    }

    fn release(&self, _work: &WorkId, _environment: &EnvironmentId) {
        self.released.fetch_add(1, Ordering::SeqCst);
    }
}

// ---- running one work through the boundary ----------------------------------------------------------

/// Everything observable about one work, kept apart by kind.
#[derive(Debug)]
pub struct Evidence {
    pub report: WorkReport,
    /// Chip's event stream reduced to what the MODEL proposed: capability ids, in order.
    pub proposed: Vec<String>,
    /// The host's ledger: authorized, denied, executed.
    pub host: Vec<Host>,
    /// FX attempts started by Chip (one per model call).
    pub fx_attempts: u64,
    /// Calls that reached the provider under FX.
    pub provider_invocations: usize,
    pub environments_acquired: usize,
    pub environments_released: usize,
}

impl Evidence {
    /// The terminal outcome Chip reported. In this harness nothing persists it, so it is a result,
    /// not a durable commit; persistence is outside the harness (docs/chip-fx-integration.md).
    pub fn outcome(&self) -> &WorkOutcome {
        &self.report.outcome
    }

    pub fn executed(&self) -> Vec<&Host> {
        self.host
            .iter()
            .filter(|h| matches!(h, Host::Executed { .. }))
            .collect()
    }
}

/// What "done" means for the fixture goal, owned by the host and not by the model: the host's own
/// execution of the self test succeeded and produced a receipt. Chip refuses a model's claim of
/// completion until an authoritative observation satisfies every required predicate.
#[derive(Debug)]
pub struct SelftestPassed;

impl ObservationPredicate for SelftestPassed {
    fn describe(&self) -> String {
        "the host executed compute.selftest successfully and returned a receipt".into()
    }

    fn satisfied_by(&self, observation: &Observation) -> bool {
        observation.kind == ObservationKind::ExecutionCompleted
            && observation.status == ExecutionStatus::Success
            && observation.receipt_id.is_some()
    }
}

/// A request enters Chip through the environment boundary: acquire an environment, run the work with
/// the environment's capability set as both provider and executor, release the environment.
pub async fn run_work(
    host: Arc<HostBoundary>,
    provider: ScriptedProvider,
    cancel: Option<Arc<AtomicBool>>,
) -> Evidence {
    run_work_with(host, provider, cancel, true).await
}

/// As [`run_work`]. With `require_observation == false` the work carries no host-owned definition of
/// done, which is NOT a supported configuration: it exists so a test can show why the integration must
/// always supply one.
pub async fn run_work_with(
    host: Arc<HostBoundary>,
    provider: ScriptedProvider,
    cancel: Option<Arc<AtomicBool>>,
    require_observation: bool,
) -> Evidence {
    let invokes = provider.invokes.clone();
    let mut model = FxModelProvider::new(
        provider,
        RequestIds::new(*b"harness\0"),
        OptionsPolicy::DropUnsupported,
    );
    if let Some(flag) = cancel {
        model = model.with_cancel(Arc::new(Flag(flag)));
    }
    let model = Arc::new(model);

    let provider_of_environments = FakeEnvironments::new(host.clone());
    let environments = Environments::new(provider_of_environments.clone());
    let work = WorkId::new("harness-work");
    let owned = environments
        .acquire(work.clone())
        .await
        .expect("the fixture environment is always available");

    let set = owned.environment().capabilities();
    let agent = Agent::new(model.clone())
        .with_capabilities(set.clone())
        .with_executor(set)
        .with_observer(Arc::new(chip_core::ExecutionObserver));
    let mut spec = WorkSpec::new(
        work,
        WorkGoal::new("Run the self test and report the result"),
    );
    if require_observation {
        spec = spec.with_required_observation(Arc::new(SelftestPassed));
    }
    let report = agent
        .run_work(&spec, &NoLocalPolicy, &ModelDecisionBoundary)
        .await;
    drop(owned);

    let proposed = report
        .events
        .iter()
        .filter_map(|event| match event {
            WorkEvent::CapabilityRequested { capability, .. } => Some(capability.to_string()),
            _ => None,
        })
        .collect();
    Evidence {
        proposed,
        host: host.ledger(),
        fx_attempts: model.stats().attempts,
        provider_invocations: invokes.load(Ordering::SeqCst),
        environments_acquired: provider_of_environments.acquired.load(Ordering::SeqCst),
        environments_released: provider_of_environments.released.load(Ordering::SeqCst),
        report,
    }
}

pub const REQUEST_SELFTEST: &str =
    r#"{"decision":"request_capability","capability":"compute.selftest"}"#;
pub const COMPLETE: &str = r#"{"decision":"complete","summary":"the self test passed"}"#;
