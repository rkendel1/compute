# Session architecture

> **Providers are implementations of the Compute session contract. They are
> not alternative authority systems.** Compute decides who may create,
> connect to, run commands in, expose, and destroy a session; a provider only
> builds, reaches, and tears down environments.

User-facing behaviour is in [sessions.md](sessions.md). This document explains
how a session is built from the pieces Compute already had.

```text
                   COMPUTE
                      │
          ┌───────────┴───────────┐
          │                       │
      one-shot                session
     execution               execution
          │                       │
      remote run            durable computer
          │                       │
          └───────────┬───────────┘
                      │
              durable jobs (one lifecycle)
                      │
                   Provider
          ┌───────────┼───────────┐
          │           │           │
      workspace   container    cloud VM  …
```

## The layers

```text
Compute Session        durable record: identities, owner, status, TTL, evidence
      ↓
Placement              which provider can prove it satisfies the session
      ↓
Node                   the selected pool member (`compute serve`)
      ↓
Provider               a SessionProvider: provisions and reaches environments
      ↓
Execution              every command: a durable job, a receipt
```

### Compute Session

`compute_core::ComputeSession` is the authority for everything about a
session. It is written by the node's `SessionManager` to the session store
(`<session-store>/sessions/<session-id>/session.json`, with `request.json`
and `events.json` beside it), atomically, before any provider is called.

It is the only registry of sessions. Nothing about a session lives only in
memory: the manager's in-memory sets (sessions not yet terminal, sessions
being advanced, executions being watched) are a working set rebuilt from the
store at start and never consulted as truth.

### Placement

`compute session create` builds a `SessionCreateRequest`: the environment's
contract as an ordinary `ProviderRequest` — a canonical shell workload with
the session's resources, network, and isolation, whose entrypoint is the
readiness check — plus the session's terms (TTL, required capabilities,
endpoints).

Placement is unchanged apart from one submission mode.
`PlacementRequirements::for_session` derives requirements from that bundle
exactly as for any workload, with `submission: session` and the required
session capabilities. `match_provider` adds two checks: the provider offers
`sessions` in its execution modes, and its advertised
`SessionCapabilities` include every required capability. Admission, health,
capacity, priority, and the deterministic ordering are the existing ones.
`dispatch::create_session` binds the placement and the caller's policy into
the request through the same `prepare` step as `dispatch::submit`, prepares
the runtime the same way, and sends it once.

### Node

The node is the pool member placement selected: a `compute serve` process
whose `compute.remote@1` service (`RemoteService`) owns a `JobManager` and,
when it offers `sessions`, a `SessionManager` over the same job manager.
`ProviderCapabilities` advertise `execution.sessions` and the
`sessions` capabilities, which is what placement reads.

The `session_id` addresses the session on its node. Clients that do not know
the node find it by asking the pool's providers; only the node that holds the
session for that principal answers.

### Provider

```rust
#[async_trait]
pub trait SessionProvider: Send + Sync {
    fn kind(&self) -> String;                       // descriptive only
    fn capabilities(&self) -> SessionCapabilities;  // what its environments support
    async fn provision(&self, request: &ProvisionRequest) -> Result<ProvisionedSession, _>;
    async fn inspect(&self, provider_session_id: &str) -> Result<EnvironmentState, _>;
    async fn exec(&self, env: &SessionEnvironment, command: &SessionCommand) -> Result<ProviderRequest, _>;
    async fn destroy(&self, provider_session_id: &str) -> Result<(), _>;
    // Optional; the defaults fail with `operation_unsupported`.
    async fn connect(&self, env: &SessionEnvironment) -> Result<ProviderConnection, _>;
    async fn logs(&self, provider_session_id: &str) -> Result<Option<String>, _>;
    async fn stop(&self, provider_session_id: &str) -> Result<(), _>;
    async fn resume(&self, provider_session_id: &str) -> Result<(), _>;
    async fn claim(&self, provider_session_id: &str) -> Result<(), _>;
}
```

### Execution

`exec` does not run anything. It returns the `ProviderRequest` that runs the
command *inside that environment*; the manager hands it to the node's
`JobManager` through `JobManager::accept` — the one acceptance path that
`POST /compute/jobs` (`compute remote submit`, and so `compute remote run`)
also uses. Persistence, admission, capacity reservation, execution, logs,
cancellation, results, receipts, retention, and restart recovery are
therefore identical for a one-shot remote run and a command in a session;
sessions add no second execution lifecycle.

The session reserves each execution's `job_id` and `execution_id` and
records them before the job exists. `JobManager::accept` accepts a reserved
identity (idempotently: the same reservation, owner, and request return the
existing job), stores `session_id` on the job, and passes the execution
identity to the runtime through `ExecutionControl::with_execution_id`, so the
result and the receipt carry it. A result under any other identity is
rejected as `evidence_invalid`.

## The provider contract

A provider implements the environment; Compute implements everything else.

| Compute (the manager) | The provider |
| --- | --- |
| Identities, owner, status, TTL, events, failures | Its own handle (`provider_session_id`) |
| Authorization of every operation, expiry authority | — |
| Admission under policy before provisioning | — |
| When to provision, stop, resume, claim, destroy | How |
| Every execution: job, lifecycle, receipt | How a command reaches its environment (`exec`) |
| Reconciliation after a restart | Idempotent `provision` and `destroy` |

Rules a provider must follow:

- **Capabilities are the environment's.** `capabilities()` and the
  capabilities in `ProvisionedSession` describe what the environment does,
  not what the provider's name suggests. The manager refuses unsupported
  operations before calling; the trait defaults return
  `operation_unsupported` as a second line of defence.
- **Provisioning is idempotent per `session_id`.** A restart can repeat
  `provision` for a session whose environment already exists; it must return
  that environment, not a second one.
- **Destroying is idempotent.** Destroying an environment that is gone
  succeeds.
- **Resume never substitutes.** A provider that cannot resume the original
  environment fails. Compute records the failure and does not create a new
  machine.
- **Identifiers are handles, not identities.** `provider_session_id` names
  the environment to the provider. It is never shown as, compared with, or
  used as a Compute identity or credential.
- **Credentials are per connection.** `connect` may issue short-lived
  material; Compute returns it to the authorized caller and never stores it.

Compute ships two implementations: `WorkspaceSessionProvider` (a private
directory per session on the node; what `compute serve` offers) and, in the
test suite, a provider-neutral fake. `command_in_directory` builds the
execution for any provider whose environments are reachable as a directory
on the node. A container or VM provider returns a request that its own node
agent executes inside the environment; the manager, the lifecycle, and the
API are unchanged.

## Consistency

- **Durable first.** Each lifecycle step is written before the provider is
  asked to perform it, so a restart repeats at most one idempotent provider
  call.
- **Generations fence provider answers.** Every transition increments the
  session's `generation`. A provider response is applied only to the
  generation it answers; a response for a session that has moved on (for
  example, `provision` finishing after `destroy`) is discarded and the
  environment it produced is destroyed as an orphan. User operations re-check
  their precondition atomically instead of fencing, so a command finishing
  never makes a concurrent `stop` fail.
- **Terminal is final.** `destroyed`, `expired`, and `failed` sessions never
  change status again; `destroy` of a terminal session returns it unchanged.
- **Recovery.** At start the manager reads the store once, and for every
  session that is not terminal: executions that were recorded but never
  accepted are marked `rejected`; a `ready`, `running`, or `stopped` session
  whose provider no longer has the environment becomes `failed` /
  `environment_lost`; everything in flight is advanced. A periodic sweep
  expires sessions past their TTL and retries in-flight transitions.

## Authority

The `ProviderAuthorizer` that authorizes jobs authorizes sessions: each route
maps to a `ProviderOperation` (`SessionCreate`, `SessionInspect`,
`SessionExec`, `SessionConnect`, `SessionDestroy`, …) and every request is
authorized before it reaches the manager. `SessionExpose` is authorized once
per requested endpoint. The owner is `ProviderAuthorizer::owner(credential)`;
the manager compares it with the session's recorded owner on every operation
and answers a mismatch as `unauthorized`. The jobs a session runs belong to
the same owner, so the job routes enforce the same boundary. Expiry teardown
is authorized by `ProviderAuthorizer::authorize_expiry(owner)`; a refusal
leaves the session `expiring` and nothing is torn down.

## Where state lives

Session records live in the `compute serve` session store, beside its job
store, as remote jobs always have. They are Compute's durable records for
that server; there is no second copy, cache, or fallback. The Compute daemon
(`compute start`), whose durable state is FeltDB ([feltdb.md](feltdb.md)),
does not host sessions: hosting them there means modelling them in
`compute.flow` first, not writing them to a file.
