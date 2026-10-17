# Compute + Rust Chip + FX: integration contract (preparation)

Status: **preparation only.** Nothing here changes a shipped binary or an execution path. This page
records what the three repositories actually expose today (measured, with commit ids), the contract
the real integration must meet, the gaps found, and the checks that now keep the boundaries honest.
Statements are marked **verified** (observed in this change) or **unverified** (read from source or
docs, not exercised). It is not a production-readiness claim.

```text
Compute host ──► Chip agent runtime ──► FX inference boundary ──► configured provider
 (this repo)       (chip-rs)             (fx: rust/fx-core, fx-single-attempt)
```

FX never depends on Chip; Chip never depends on Compute. Compute depends on Chip only through Chip's
generic environment contract, and reaches effects only through the session `exec` primitive. See
[agents.md](agents.md) and [rust-chip.md](rust-chip.md) for the existing, shipped half of this.

## 1. Audit

### What exists today

| Piece | Where | Fact |
| --- | --- | --- |
| Compute's Rust Chip adapter | `crates/compute-rust-chip` | `ComputeSessionEnvironments` implements Chip's `EnvironmentProvider`; `compute-rust-chip serve` runs Chip's runtime service over Compute sessions; `capability-exec` is the worker Chip runs inside a session. Shipped; its 11 tests (plus the 3 added here) pass at the product pin (**verified**). |
| Agent-neutral host | `crates/compute-agent` | `AgentHost::acquire` → `AgentSession::{exec, launch, release}`. Depends on no agent (enforced by `crates/compute-agent/tests/dependencies.rs`). |
| Chip's contract | chip-rs `chip-core` | `EnvironmentProvider` / `WorkEnvironment::capabilities() -> Arc<CapabilitySet>`; the set is both `CapabilityProvider` and `Executor`; `Agent::run_work(spec, local_policy, decision_boundary) -> WorkReport`. |
| Chip over FX | chip-rs `chip-fx` | `FxModelProvider<P: ProviderPort>`: one `complete` = one FX attempt; takes a `CancelSignal` and a per-attempt budget. |
| Native provider for the binary | chip-rs `chip-fx-native`, `chip-cli::provider_selection` | The default executor of `chip` is FX (chip-rs #31, #33); `--executor legacy` is still present until the legacy crate is deleted. |
| FX | fx `rust/fx-core`, `rust/fx-single-attempt`, `rust/fx-native-provider`, `rust/fx-wasm-abi` | Single attempt, delivery certainty never lowered, no retry/fallback by contract. |

### Pins and targets (measured)

| | Compute product workspace today | This change's harness workspace |
| --- | --- | --- |
| chip-rs | `f5b6639d…` (2026-10-07). **Not an ancestor of chip-rs `main`.** Was a `branch =` pin to `claude/vigilant-noether-j1ddif`, a branch that no longer appears in the remote's refs; the commit is reachable by SHA only. Pre-dates all FX work: it still carries chip-rs's retired toy `fx-core` / `fx-provider-http`. | `2ea3ade3…` (chip-rs `main`, merge of #34) |
| fx | none directly; a *toy* `fx-core` comes from chip-rs | `72ef7a41…` (the revision chip-rs itself builds against) |
| Targets | Linux x86_64 certified, macOS ARM64 preview (README) | native; `wasm32-unknown-unknown` checked for the Wasm-bound crates |

**Change made here:** the product pin is now `rev = "f5b6639d…"` (the same commit, no `branch`), with
`Cargo.lock` updated to match (`cargo metadata --locked` passes). The build graph is identical; only the
mutable name is gone. `crates/compute-rust-chip/tests/integration_boundary.rs` fails if a branch/tag
pin returns.

### Entry points (exact, none invented)

| Need | Entry point | Where |
| --- | --- | --- |
| Start a Chip turn / work | `POST /v1/work {"goal","kind"}` on the service `compute-rust-chip serve` runs (`chip_cli::service::serve_in(args, Some(environments))`); in-process: `Agent::run_work` | chip-rs `service.rs`, `chip-core` |
| Supply authorized capabilities | `EnvironmentProvider::acquire(&WorkId) -> Arc<dyn WorkEnvironment>`; `WorkEnvironment::capabilities() -> Arc<CapabilitySet>` | `chip-core::environment` |
| Invoke FX | `FxModelProvider::complete` (library); `provider_selection::build_model` (binary) | `chip-fx`, `chip-cli` |
| Deliver cancellation | to Chip: `POST /v1/work/{id}/cancel`; to FX: `CancelSignal::cancelled()` polled by FX before the provider call and between polls | `service.rs`, `chip-fx::native` |
| Collect terminal outcomes | `WorkReport { outcome: WorkOutcome, events: Vec<WorkEvent> }`; `GET /v1/work/{id}` and `/events` | `chip-core::work` |
| Compute side of an effect | `AgentSession::exec(argv, env)`: a durable job with captured output and a receipt | `compute-agent` |

## 2. Contract

**Roles.** Compute: execution infrastructure, sessions, host capabilities, resource authorization.
Chip: the loop, tool/capability validation, turn/work lifecycle, recovery behavior. FX: one provider
attempt, request validation, stream normalization, cancellation, delivery certainty, terminal inference
outcome. The host application supplies configuration, credentials and the capabilities it authorizes.
No duplicate provider implementation, loop, persistence authority or control plane.

**Model output is a proposal.** A tool call the model emits is data Chip parses against the capabilities
declared by the environment. It becomes an effect only if the capability is declared *and* the executing
backend accepts the request. The harness proves the proposal / authorization / execution / outcome
separation (§3).

**Initialization and shutdown.** `acquire` builds an environment (a Compute session, loaded project,
worker connection); `Environments` enforces one owner per environment; the environment is released when
the work ends, including by panic. A session whose destroy fails is left to its TTL (default 30 min).
No fallback to another session or to the host. Shutdown of the service does not cancel in-flight FX
attempts beyond the cooperative signal below.

**Provider and model configuration.** Chip's own `CHIP_PROVIDER` / `CHIP_MODEL` / `CHIP_ENDPOINT`,
passed through untouched by `compute-rust-chip serve`; Compute is not the model provider. Supported:
`openai-compatible`, `anthropic`. Ollama's own API is not supported (use its OpenAI-compatible
endpoint). HTTP proxy and custom CA are **not supported** by the native provider (chip-rs documents
this; implementing it is out of scope here).

**Credentials.** Provider keys come from the process environment of the program running Chip
(`CHIP_API_KEY`, `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`). FX holds them in a type that does not print.
Compute job receipts never record environment values, arguments or full command lines
([receipts.md](receipts.md)), so a credential placed in a session's `exec` environment would not appear
in a receipt. **Unverified:** that the launcher never forwards a provider key into a session, and that
no Compute log or record echoes one. A test for this belongs to the integration PR (gap G8). No new
credential store is introduced.

**Capability discovery, authorization, invocation, results.** Discovery: the environment's
`CapabilitySet` declares ids and typed inputs. Authorization: Chip rejects an undeclared id or malformed
inputs before anything runs; the backend that owns a declared id is the last gate and must re-check at
execution time (the harness's `HostBoundary` does, and a revoked capability is refused with no effect).
Invocation in production: Chip asks the session to run its own executor
(`compute-rust-chip capability-exec`) via Compute `exec`. Results: an `ExecutionResult` becomes an
observation the next inference sees. Compute does not know what a capability means
([agents.md](agents.md)); if Compute must authorize individual capabilities, **no interface for that
exists** and none is invented here (gap G10).

**Completion belongs to the host, not the model.** With no host-owned definition of done, the bare work
loop accepts a model's claim of completion even after the only effect failed (shown by
`without_a_host_owned_definition_of_done_a_model_claim_completes_the_work`). The shipped `chip`
paths avoid this with kind-specific verification (`CompleteWhenVerified`, PAX observations). Any new
integration path must supply required observations.

**Cancellation and deadlines.** FX cancellation is cooperative: checked before the provider is invoked
and between polls; it cannot interrupt a call in progress. Before invocation → `definitely_unsent`;
after → never reported as unsent. Per-attempt budget via `FxModelProvider::with_budget`; Compute bounds
each command by `command_timeout` (default 600 s). **Gap G3:** `compute-agent` exposes no cancellation
of a running `exec`; the provider layer has `cancel` for jobs, but nothing connects a Compute-side cancel
to Chip's `POST /v1/work/{id}/cancel` or to FX's signal.

**Persistence, interrupted turns, recovery.** Compute's durable state is FeltDB through
`compute-state`; Chip has its own Zig-compatible session store (`chip-session`, `chip-turns`). The shipped
`chip serve` / `compute-rust-chip serve` do **not** persist turns. A crash mid-work loses the work;
re-running the prompt re-executes capabilities; nothing replays automatically and there is no
idempotence guard. This is the open design question recorded as the unapproved crash-recovery
differences in the fx parity evidence. Do not promise transparent recovery or exactly-once effects.
**Gap G4:** which side is the authority for a turn record is undecided.

**Errors, delivery certainty, terminal outcomes.** FX failures reach Chip as `FxError::Inference` with
delivery (`definitely_unsent` / `possibly_sent` / `confirmed_response`), never lowered, never retried.
Chip ends the work as `Failed` with that text. Terminal states: `Completed`, `Escalated`, `Blocked`,
`LimitReached`, `Failed`. **Gap G5:** delivery certainty reaches the outcome as text in `reason`, not a
structured field.

**Logging, audit, observability.** Chip's `WorkEvent` stream (ids and counts, no prompt text);
Compute job receipts (Compute ran a command, never "the goal was met"). The harness keeps four kinds
apart: *proposed* (model), *authorized/denied* (host policy), *executed* (effect happened), *outcome*
(Chip's terminal state; nothing persists it in the harness, so it is a result, not a durable commit).

**Native and WebAssembly.** Native: Linux x86_64 certified. Wasm: `chip-core`, `chip-fx`, `fx-core`,
`fx-single-attempt` `cargo check` for `wasm32-unknown-unknown` (**verified**), and a test fails if
`reqwest`, `hyper`, `rustls`, `tokio`, `fx-native-provider`, `chip-fx-native` or `chip-legacy-http`
enter the graph of the Wasm-bound crates. **Gap G7:** Compute's Wasm engine is wasmtime/WASI p1 for
workloads (`runtime-wasm`); running chip-rs's `chip-wasm-host` under it is untested, and a Wasm-hosted
Chip has no native provider, so it needs a host-supplied provider as the Node bridge in fx does.

**Versions and pinning.** Immutable revisions only. Update procedure: (1) merge the chip-rs change to
`main`; (2) set the chip-rs `rev` and, if chip-rs moved its fx `rev`, the fx `rev`, together; (3) run
`integration/chip-fx` (`cargo test --locked`, the Wasm check) and the product `compute-rust-chip` tests;
(4) record the pair here. `tests/pins.rs` fails on a branch/tag/short pin and on two FX revisions in one
graph. Compatibility expectation: Chip's `EnvironmentProvider` / `CapabilitySet` / `Executor` and FX's
`ProviderPort` are the only seams Compute relies on; a change to them is a breaking change for this repo.

## 3. The harness

`integration/chip-fx` (own workspace, pinned as above) runs the real Chip work loop, the real `chip-fx`
adapter and the real FX executor against fixtures: a scripted provider under FX, a host capability
boundary behind Chip, and an `EnvironmentProvider` standing where `ComputeSessionEnvironments` stands.
10 contract tests, 4 boundary tests, 2 pin tests; they ran 5 times with identical results (**verified**,
local). It demonstrates: entry through the environment boundary; one FX attempt per model call; a
proposal authorized by the host before it executes, its result reaching the next inference; provider
failure, authorization denial, cancellation before and after invocation as explicit terminal outcomes;
no retry, no fallback, no repeated effect (a failure *after* an executed effect leaves exactly one
effect); proposed / authorized / executed / outcome kept apart.

**What it does not prove:** that Compute's session, admission, policy or persistence code works with
the new Chip (the existing `compute-rust-chip` tests cover those at the product pin); anything about a
live provider; recovery; Wasm execution. Passing it does not make the integration production-ready.

## 4. Measured drift between the product pin and chip-rs `main`

Done in a scratch copy, not committed. Against `2ea3ade3…`, `compute-rust-chip` (lib and bin) compiles
unchanged. Needed: (a) remove the `fx-core` dev-dependency (the toy crate is gone; tests use
`chip_inference` instead: one import line in `tests/work.rs`); (b) `serde_json` locked at 1.0.154 (FX
requires `=1.0.154`; Compute locks 1.0.151); (c) the two `launcher.rs` tests fail: the model stand-in
there is non-streaming JSON while the default executor is FX (the same fix chip-rs applied to its own
stand-ins), and one of them exits 3 where 2 is expected for a cause not yet diagnosed. After (a)–(b) the
other compute-rust-chip tests passed against `main` (agent_contract 1, work 4, provider_contract 4).

## 5. Gaps

| # | Gap | Smallest compatible change | Owner |
| --- | --- | --- | --- |
| G1 | Product pin is off-`main`, pre-FX, toy FX | bump to a chip-rs `main` rev with the §4 edits (a)–(c) | compute |
| G2 | `launcher.rs` stand-in non-streaming; one undiagnosed exit code | stream SSE like chip-rs's; diagnose the `3` vs `2` | compute |
| G3 | No Compute→Chip/FX cancellation path | expose job cancel on `AgentSession` and call Chip's cancel endpoint | compute + chip-rs |
| G4 | Turn-record authority undecided; no persistence in shipped path; no idempotence guard | decision record first; then wire `chip-turns` or explicitly refuse side-effecting capabilities in persisted turns | compute + chip-rs (decision: owner) |
| G5 | Delivery certainty is text, not structure | a structured field on `WorkOutcome::Failed` | chip-rs |
| G6 | Completion definition must be host-owned for any new path | require `required_observations` in the integration entry | compute |
| G7 | `chip-wasm-host` under `runtime-wasm` untested | one conformance test or a documented "not supported" | compute |
| G8 | No test that provider keys stay out of sessions and records | add to the integration PR | compute |
| G9 | Native provider has no proxy / custom-CA support | separate follow-up inside the FX provider boundary | fx / chip-rs |
| G10 | Compute cannot authorize individual capabilities (by design) | decide whether it should; do not invent an interface | owner |

Follow-up issues are filed as compute#88 (G1) to #97 (G10), each carrying its recorded decision; #97 (G10, by design)
is closed as not planned.

**Status after the product integration PR.** The product pin (`crates/compute-rust-chip`) is now the immutable chip-rs `main`
commit `b3f9a1c8…` (merge of chip-rs#38; fx `72ef7a41…` through it), with the §4 edits (a)–(b) applied. G2: the launcher stand-in
serves SSE when FX asks for a stream. The exit code `3` was diagnosed: the service validates the model provider before it checks
capacity, and the FX executor requires an endpoint ending in `/chat/completions`; the test passed `http://127.0.0.1:1`, so the
launch ended with 3 before the capacity check ran. The test now passes a well-formed endpoint (expected code `2` unchanged) and a
second test pins the `3` for a malformed endpoint. G8 and G6 have launcher-path tests (key sentinel; a model's own claim of
success does not complete a work). G1/G2/G6/G8 remain open until this PR merges and its CI is observed; the harness in
`integration/chip-fx` is not yet folded into `crates/compute-rust-chip/tests`.

**Status after the gap PR (compute#88–#96).** The product pin is now chip-rs `c14522a…` (merge of chip-rs#42; fx `0babf6a…`, the merge of fx#31).

| # | Status | Where |
| --- | --- | --- |
| G1 | Pin `b3f9a1c8…` with (a)–(b). The `compute-rust-chip` tests that need PAX used to print `SKIPPED` and pass in CI, where PAX was not installed, so `work`, `provider_contract` and the launcher tests had never run there. CI now installs PAX at a pinned commit and sets `COMPUTE_REQUIRE_PAX`, which turns a missing PAX into a failure. | `.github/workflows/test.yml`, `tests/support/mod.rs` |
| G2 | SSE stand-in and the `3`/`2` diagnosis (above). Concurrent launcher tests also shared one token file (named from the token's fixed prefix) and overwrote each other's credential; each launch now has its own. | `tests/launcher.rs` |
| G3 | Compute side done: `AgentSession::cancel` (Compute's confirmed, idempotent job cancellation; an interrupted command is never reported as not having run), commands whose caller stops waiting are cancelled rather than left running, `ComputeSessionEnvironments::cancel_work`, and `compute_rust_chip::cancel::request` for Chip's `POST /v1/work/{id}/cancel`. Chip's half landed in chip-rs#42: a run's cancellation is scoped to its model call (`chip_fx::with_call_cancel`), so a model call in flight through FX is abandoned at its next poll with its delivery certainty. | `compute-agent`, `src/cancel.rs`, `tests/cancellation.rs`, `tests/launcher.rs`; chip-rs `chip-fx`, `chip-fx-native` |
| G4 | Decision recorded in [rust-chip.md](rust-chip.md#turn-records-and-recovery-decision); the no-persistence warning is printed when `serve` starts and checked by a test. | `src/main.rs` |
| G5 | Done in chip-rs#42: `WorkOutcome::Failed { reason, delivery }` and `WorkEvent::WorkFailed` carry FX's delivery certainty as a field (`result.delivery` in the service). | chip-rs `chip-core` |
| G6 | `compute_rust_chip::work::HostWork` refuses a work without `required_observations`; a model's claim never completes a host work (tested without PAX). The service's kinds are the typed contracts of its entry. | `src/work.rs`, `tests/host_work.rs` |
| G7 | Documented unsupported at the Wasm runtime and in [rust-chip.md](rust-chip.md#limits). | `compute-runtime-wasm`, docs |
| G8 | On the real launcher path: the key reaches the model; it is in no work, event, metric, service log, model input, stored Compute record or the session worker's recorded environment. A mutation that leaks it into the worker's environment fails the test. | `tests/launcher.rs` |
| G9 | Done: an explicit HTTP `CONNECT` proxy in `fx-native-provider` with proxy and private-CA TLS integration tests (fx#31), wired in Chip as `CHIP_PROXY*` (chip-rs#42; chip-rs#35 closed). | fx / chip-rs |

**New finding (chip-rs, not fixed here).** `FxModelProvider::complete` holds its capability lock for the whole attempt, and
`chip serve` shares one provider across works, so two concurrent works' model calls are serialized: a second work's inference waits for
the first's to finish. The old launcher test assumed both could be in flight and waited on a barrier, which then outlasted the 30 s
attempt budget. The test now holds each work's first reply for a moment and checks that both works are running together instead.

## 6. Readiness gates for the integration PR

- ☐ Repository and dependency audit completed. *(done here for the three repos at the commits above; re-check at bump time)*
- ☐ Integration contract matches actual public interfaces. *(entry points above read from source; the §4 drift probe compiled them)*
- ☐ Native build and required Wasm targets pass. *(harness native + wasm32 check pass locally; CI not yet observed)*
- ☐ Harness passes deterministically. *(5/5 locally; CI not yet observed)*
- ☐ Authorization and no-fallback tests pass. *(locally; CI not yet observed)*
- ☐ Cancellation and provider-failure behavior is verified. *(at the FX/Chip layer only; G3 open for Compute-side cancel)*
- ☐ Session persistence and crash-recovery limitations are documented. *(§2; G4 open)*
- ☐ Audit evidence is retained and inspectable. *(in-memory `Evidence`; no durable audit sink exists in the harness)*
- ☐ CI passes on the relevant branches.
- ☐ Remaining gaps have owners and follow-up issues. *(owners proposed; issues not filed)*

## 7. Smallest follow-up to wire it for real

One PR: G1 (bump the pin with the three §4 edits), G2 (streaming launcher stand-ins), move
`integration/chip-fx/tests/contract.rs` into `crates/compute-rust-chip/tests` against
`ComputeSessionEnvironments`, and G8. Cancellation (G3) and persistence (G4) each need a prior decision
and are separate.
