# Audit: Celesto execution-fabric capabilities against Compute

**Date:** 2026-09-30 · **Scope:** `rkendel1/compute` at `c50218a` (v0.1.9)

The purpose is not to make Compute like Celesto. It is to identify the
execution-fabric capabilities Celesto demonstrates, find which Compute already
has, and add only what is genuinely missing, in Compute's own vocabulary
(recipe → requirements → target admission → environment → session/execution →
receipt).

> **Evidence note.** The Celesto source is not in this repository, and this
> session's GitHub scope is `rkendel1/compute` only, so Celesto itself was
> **not read**. The capability list below is the one in the task brief (including
> its description of Celesto's runner flow and its hard-coded
> `/tmp/celesto-actions-runner`). Every claim about **Compute** below was checked
> in source; `file:line` references are at `c50218a`. Documentation was used only
> as a pointer, never as evidence that something exists. (`docs/compute-capabilities.md`
> and `docs/github-runner-protocol.md` are headed "Nothing here is implemented"
> and were treated accordingly.)

Status vocabulary. Section 2 describes Compute **as found at `c50218a`**, before
any change, using EXISTS / PARTIAL / MISSING (and CONFLICTS where a capability
contradicts the architecture). The final table under **Result** gives the status
after this work in the vocabulary EXISTS / PARTIAL / IMPLEMENTED / INTENTIONALLY
DEFERRED / NOT APPLICABLE. A correction pass (2026-10-01) re-verified the landed
work; see **Correction pass**.

## 1. What was inspected

Workspace members (`Cargo.toml:2-19`): `compute-core` (types, receipts,
staging, host isolation), `compute-runtime` / `-process` / `-wasm` /
`-conformance` (runtime abstraction), `compute-provider` (jobs, sessions,
remote), `compute-policy`, `compute-placement`, `compute-state` / `-memory` /
`-file` / `-feltdb` (durable state), `compute-network` (domains, DNS, ingress,
certificates), `compute-environment` (daemon: environments, computers,
recipes, checkpoints, deployments), `compute-project`, `compute-cli`.
Searched: the CLI surface (`crates/compute-cli/src/main.rs:50-155` and
`environment_cmd.rs`, `session_cmd.rs`, `computer_cmd.rs`), the error types, the
recipe format, receipts, the session/computer state machines, the runtimes' timeout
and cancellation paths, the staging code, host isolation, the starters, and the
test layout (`crates/*/tests`).

## 2. Capability map

| Celesto capability | Compute status | Existing implementation (source) | Gap | Action |
| --- | --- | --- | --- | --- |
| **Persistent environment lifecycle** | **EXISTS** | `ComputerLifecycle::{Persistent,Ephemeral}` (`compute-core/src/computers.rs:21`); `ComputerStatus` pending/provisioning/running/stopping/stopped/resuming/failed/destroying/destroyed/expired/unreachable/lost (`:51`); `SessionStatus` incl. distinct `Ready` vs `Running` (`compute-core/src/sessions.rs:67-81`); `compute environment create/apply/start/stop/restart/destroy` (`environment_cmd.rs:914-982`); stop/destroy confirmation, `termination_failed`, process-tree termination (`compute-provider/src/processes.rs`, `docs/lifecycle.md`); tests `compute-environment/tests/{lifecycle,readiness,computers}.rs` | none found that justify new code; "complete" is `JobStatus`, not an environment state | No change |
| **Doctor / host preflight** | **PARTIAL** | `compute doctor [--json] [--runtimes-only]` (`main.rs:419-428`, handler `:1393`); per-runtime `RuntimeReport` (`compute-runtime/src/lib.rs:257`) with availability and `remediation`; controller diagnosis with reachability/auth findings (`node_cmd.rs:516`); host isolation report (`compute-core/src/host.rs:501`); distribution provenance (`distribution.rs:408`); test `cli.rs:676` | no `--strict` (exit status is 0 whatever the findings); JSON has no `ok`/`exit_code`/`error` fields and no flat `checks` list an agent can iterate | Add `--strict` and an **additive** envelope + `data.checks`; keep `runtimes`/`controller` keys byte-compatible |
| **Stable machine-readable CLI output** | **PARTIAL** | `--json` is a per-command flag with per-command shapes (e.g. `main.rs:410`, `session_cmd.rs`, `pool.rs::print_json`); typed error kinds exist server-side: `EnvironmentError::kind()` (`compute-environment/src/lib.rs:106`) | success shapes are heterogeneous (existing consumers depend on them); **failures are not machine-readable**: `async_main` prints `{error}` to stderr (`main.rs:710-714`) and `environment_cmd::error` flattens `EnvironmentError` into `ComputeError::Runtime(String)` (`environment_cmd.rs:52`), losing the kind | Do not rewrite success shapes. Add a uniform **failure** envelope for any command run with `--json`, derived from a stable code |
| **Actionable recovery in errors** | **PARTIAL** | Human remediation text exists: `"start the controller with \`compute start\`"` (`node_cmd.rs:536`), client message (`compute-environment/src/client.rs:145`), `RuntimeReport.availability.remediation`, destroy failure phases (`docs/lifecycle.md`) | recovery is prose only | Add a structured `recovery` object only where the action is deterministic. (Landed with two cases; the correction pass cut it to one, `controller_unavailable` on the default local endpoint → `compute start`) |
| **Host mounts; read-only vs writable** | **MISSING** as a mount; **EXISTS** as copy-in | Compute has no host mount. `Mount { host_path, execution_path }` (`compute-core/src/lib.rs:240`, CLI `--mount`) is **copy-in / input materialization**: `stage_workload` copies the host path into the execution's private workspace (`:2401-2404`, `copy_path`), refuses symlinks (`:2605`) and `..`/prefix components (`sanitize_execution_path`, `ComputeError::InvalidMountPath`). The workload sees only the copy; nothing is copied back. Results leave only as declared `outputs`, which the receipt digests | the name `mount` is historical and misleading; before this work no test covered copy-in. Copy-in is **not** a sandbox: under `process` isolation the workload can write anywhere its user can (the receipt says `filesystem: unavailable`) | Tests and accurate terminology. **Writable host mounts: not implemented**: a write-through bind would change the host outside the declared-output/receipt model |
| **Network policy** | **PARTIAL** | `NetworkPolicy::{None,Localhost,Network}` default `None` (`compute-core/src/lib.rs:245-252`); per-runtime capability matrix (`:1726-1794`); OS-level enforcement or refusal `plan()` in `compute-core/src/host.rs:300-360` (netns for none/localhost; `network_isolation_unavailable` refusal otherwise, fail-closed; tests `host.rs:558-619`); recipes carry `requirements.network` (`recipes/starters/*.json`); admission: `compute-placement/src/requirements.rs:178` | no `restricted`/allowlist mode, and **no runtime can enforce a domain/CIDR allowlist** (`compute-network` is *ingress* — domains, DNS, certificates — not egress) | **Deferred.** Adding a `restricted` variant that nothing enforces would be a lie on the wire; fail-closed refusal would be its only behaviour. Stated in `docs/execution-capabilities.md`; fail-closed behaviour now tested |
| **Snapshots / restore** | **EXISTS** | `compute environment checkpoint`, `restore`, `fork` (`compute-environment/src/daemon/{checkpoint,restore,fork}.rs`); content-addressed artifact; tree-digest verification; lineage; `docs/checkpoint.md`, `restore.md`, `fork.md`; tests `crates/compute-environment/tests` | filesystem checkpoints only; no memory/VM snapshot (documented as deliberately not offered, `docs/compute-capabilities.md`) | No change |
| **Browser-specific environments** | **MISSING** | Chromium appears only in UI tests (`compute-cli/tests/work_mode_ui.rs:54`, `product_journey.rs:41`) and `compute up` opening the control UI (`launch_cmd.rs:223`). No CDP, no browser capability: `SessionCapabilities` has ten fixed names (`sessions.rs:142`) and `TARGET_FEATURES` six (`computers.rs:129`) | no capability, runtime, or recipe | **Deferred** (see Result) |
| **Desktop / computer-use** | **MISSING** | no display, keyboard, mouse, clipboard anywhere in `crates/` (searched) | same | **Deferred** |
| **Agent presets** | **PARTIAL** | `recipes/starters/agent-task.json` (ephemeral, TTL 1800, sandboxed, network none), embedded by `compute-environment/src/starters.rs:14`; `ProcessKind::Agent` (`computers.rs:275`) and `compute environment computer agent` (`computer_cmd.rs:385`) | recipes hold *requirements only* (`RecipeSpec`, `recipes.rs:27-60`); a `agent-codex`/`-claude`/`-opencode` recipe would be requirement-identical to `agent-task` and carry no reproducibility. Software install is bootstrap/contents, not a recipe | **Deferred**; `agent-task` stays. Guard test added: starters and recipes contain no credential-shaped keys |
| **Interactive vs detached execution** | **EXISTS** | `compute run` is never interactive and does not read the TTY (nothing in `compute-cli/src` consults a TTY; the only `is_terminal` is `JobStatus::is_terminal`); `compute session exec --detach` (`session_cmd.rs:450`), `compute environment computer exec --detach` (`computer_cmd.rs:522,1547`), `compute remote submit/status/wait` (`main.rs:245-255`); `ExecutionRequest.stdin` is explicit and "adapters must never inherit the host terminal" (`compute-core/src/lib.rs:532-535`); terminal capability exists but is `false` everywhere (`docs/compute-capabilities.md`) | `compute run` has no `--detach` (the remote async path is `remote submit`) | No change; guard test that the CLI never infers lifecycle from a TTY |
| **Strict ephemeral cleanup** | **PARTIAL** | One-shot: process runtime stages in a `TempDir` dropped after the run; timeout and cancellation kill the whole process group (`compute-runtime-process/src/lib.rs:860-913`, `terminate` `:990`), statuses `timed_out`/`cancelled`; timeout conformance (`compute-runtime-conformance/src/lib.rs:513`). Persistent-ish: ephemeral computers with TTL (`computers.rs:21`, `recipes.rs:79`), TTL sweeper (`compute-provider/src/sessions.rs:1982`), orphan sweep (`daemon/computers.rs:4788`), work sessions that destroy their own environment (`daemon/work.rs:210-360`) | cleanup on **cancellation/failure** of a one-shot run is not asserted by any test (conformance tests timeout only); no component runs *provision→execute→collect→teardown* as one guaranteed unit for an external worker | Add tests for success/failure/timeout/cancel cleanup at the runtime layer; the worker adapter owns its own guaranteed teardown |
| **Ephemeral GitHub Actions runner** | **MISSING** | `docs/github-runner-protocol.md` is a design for *serving* the runner protocol ("Nothing here is implemented"); no source mentions `actions/runner`, `--ephemeral`, or a registration token | whole feature | **Implement** as an external worker crate, not a runtime (see §4) |

### Remaining areas audited (no Celesto gap, listed for completeness)

| Area | Status | Source |
| --- | --- | --- |
| Runtime abstraction | EXISTS | `RuntimeAdapter`, `RuntimeKind` (`compute-core/src/lib.rs:58`); 11 runtimes (`cli.rs:683`) |
| Target/provider abstraction | EXISTS | `compute-provider` (`ComputeProvider`, `SessionProvider` `sessions.rs:307`), `compute-placement` |
| Requirements / capability admission | EXISTS | `SessionCapabilities::missing/validate_names`, `ComputerRequirements` (`computers.rs:155`), `PlacementRequirements`, `compute-policy` admission; unknown capability names are errors |
| Isolation | EXISTS | `IsolationProfile` process/sandboxed/strict (`lib.rs:258`), `HostProfile` Landlock/netns (`host.rs`), `compute isolation` |
| Persistence | EXISTS | `compute-state` over FeltDB; `AGENTS.md` contract; `docs/feltdb.md` |
| Remote execution | EXISTS | `RemoteProvider`, `compute remote …` (`main.rs:245`) |
| Distribution / certification | EXISTS | `distribution.rs`, `certification.rs`, `compute certify` |
| Receipts | EXISTS | `ExecutionReceipt` (`compute-core/src/receipt.rs:185`): workload, runtime, provider, placement, scope, project, outputs, status. **Env values are never recorded — only names** (`receipt.rs:909-910`); stdin only as size + sha256 (`:901-902`) |
| Applications, deployments, promotion | EXISTS | `daemon/applications.rs:177`, `daemon/deploy.rs:191,565`, deployment receipts `views.rs:738` |
| Concurrency | EXISTS | provider capacity & reservations (`compute-provider/src/jobs.rs`), `compute-placement` capacity, per-session mutation lock |
| Garbage collection | EXISTS | see ephemeral row |
| Examples | EXISTS | `examples/compute-demo`, `examples/recipes` (pointer to starters) |

## 3. Architecture constraints that shaped the decisions

1. **A recipe states requirements only** (`RecipeSpec`: lifecycle, ttl,
   `ComputerRequirements`, policy). It has no software list and no inputs, so it
   *cannot* hold credentials or repository identity by construction.
2. **A receipt records env names and digests, never values** (`receipt.rs:901-910`),
   so a secret passed to a workload through its environment does not enter the
   receipt.
3. **The session-exec path has no secret channel.** `SessionCommand.env` is a plain
   `BTreeMap<String,String>` carried into the durable job request, and
   `SessionExecution.command` is persisted in the session record
   (`compute-core/src/sessions.rs:372-382,500-514`). A registration token must not
   travel that way; the workload-execution path (`Compute::run`) takes
   env/stdin directly from the caller and persists neither.
4. **`compute run` execution is the one-shot lifecycle** (stage → execute →
   collect → drop), already cancellation- and timeout-safe at the runtime layer.
5. **Compute owns execution, FeltDB owns durable state** (`AGENTS.md`): no new state
   store is introduced by this work.

## 4. Decisions

- **Doctor / JSON / recovery (A, B):** extend the existing surfaces additively.
- **Copy-in (E):** Compute copies declared inputs in and declared outputs out;
  it has no host mount. Add the missing tests and correct the terminology; do not
  add writable mounts.
- **Network (F):** no change to the enum; document what is enforced.
- **Snapshots (G):** exists; untouched.
- **Browser / desktop (H, I):** deferred. A capability name cannot be added to
  `SessionCapabilities` without a readers-first wire rollout
  (`docs/compute-capabilities.md`, "Extending SessionCapabilities safely"), and no
  Compute target can honestly advertise a display or CDP endpoint today.
- **Agent presets (J):** deferred (no reproducibility value without contents).
- **Interactive/detached (K):** exists; guarded by a test.
- **Ephemeral (D):** tests for the existing one-shot guarantees; the worker adapter
  adds its own guaranteed teardown.
- **GitHub runner (L):** new crate `compute-worker-github` (external adapter) +
  optional recipe + a thin `compute worker github-actions run` command. Core crates
  do not mention it; a guard test enforces that.
- **`ExternalWorker` trait (M):** *not* introduced. Exactly one integration exists,
  so a trait would be speculative generality; the adapter's seams are two small
  private-to-the-crate traits used for testing.

## 5. Findings made while implementing

These came from running code, not reading it, and are recorded because they
change what can be promised.

1. **Environment errors lost their kind.** `EnvironmentError::kind()` existed, but
   `environment_cmd::error` flattened every controller error into
   `ComputeError::Runtime(String)`. The failure envelope needed the kind, so
   `ComputeError::Coded { code, message }` carries it; its text is byte-for-byte
   what `Runtime` printed.
2. **A failing command is `completed` with a non-zero exit code.** Execution
   status describes how the execution *ended*; the command's own failure is
   `exit_code`. The worker and CLI therefore treat `Completed` + non-zero exit as
   failure, as `compute run` already does.
3. **A refused copy-in is a failed execution with a receipt, not an `Err`.** Staging
   errors (`..`, symlink, missing host path) surface as `status: failed`,
   `error.kind: preparation`, `started: false`, and the receipt records them.
4. **Fail-closed network is observable.** The `shell` runtime under the `process`
   isolation profile refuses `network: none` and `localhost` with
   `network_policy_unavailable` instead of running unrestricted
   (`crates/compute-runtime/tests/network.rs`).
5. **The process runtime does not reap descendants after a normal exit, and it
   waits for the output pipes.** On timeout and cancellation the whole process
   group is killed (tested). When the command exits on its own, a descendant that
   still holds stdout/stderr keeps the execution open until it ends or the wall
   time passes, and one that detached its stdio survives. Session jobs rely on
   this (background work started by a session command is deliberately allowed to
   continue, `docs/lifecycle.md`, Process ownership), so the runtime was **not**
   changed. The correction pass confirmed the GitHub runner *does* create
   descendants (listener, worker, job steps) and so the worker script ends its
   own process group when it exits, captures the runner's output to a file
   rather than a pipe, and is covered by tests for success, failure, timeout and
   cancellation (`a_runner_that_exits_leaves_no_descendant_behind`, the CLI
   timeout and signal tests). A descendant that ignores `SIGTERM` is not
   force-killed on the normal-exit path; the wall-time limit bounds it.

## 6. What was implemented

| # | Change | Where | Tests |
| --- | --- | --- | --- |
| A | `compute doctor --strict`; additive envelope and `data.checks` with `pass`/`warn`/`fail` | `compute-cli/src/main.rs` | `compute-cli/tests/contract.rs` |
| B | `--json` failure envelope; `ComputeError::code()`; `Coded` variant; recovery computed in the CLI | `compute-core/src/lib.rs`, `compute-cli/src/contract.rs`, `environment_cmd.rs`, `application.rs` | `contract.rs`, unit tests in `compute-core` and `contract.rs`, `architecture.rs` |
| D | Cleanup tests for success, command failure, timeout, cancellation; `Compute::run_controlled` | `compute-runtime/src/lib.rs` | `compute-runtime/tests/ephemeral.rs` |
| E | Copy-in tests (original untouched, escape, symlink, missing path, cleanup, concurrency; and that `process` isolation is not a boundary) | tests only | `compute-runtime/tests/copy_in.rs` |
| F | Fail-closed network test | tests only | `compute-runtime/tests/network.rs` |
| L | `compute-worker-github` crate, `compute worker github-actions run`, recipe | `crates/compute-worker-github`, `compute-cli/src/worker_cmd.rs`, `examples/recipes/github-actions-runner.json` | `compute-worker-github/tests/{runner,github_api}.rs`, `compute-cli/tests/worker.rs` |
| Guards | Architecture invariants | `compute-cli/tests/architecture.rs`; `docs/audit.json` entry for the new command (extends `audit.rs`) | same |
| Docs | `cli-contract.md`, `execution-capabilities.md`, `github-actions-runner.md`, `architecture.md` note, `examples/recipes/README.md` | `docs/` | — |

Nothing was added to `compute-state`, FeltDB, the wire protocol, or
`SessionCapabilities`.

## 7. Architecture guard (Phase 6)

| Invariant | Verified by |
| --- | --- |
| Recipes contain no secrets or repository identity | `architecture::recipes_describe_environments_and_never_hold_credentials` (every starter and example recipe; keys and credential-shaped values); `RecipeSpec` is `deny_unknown_fields` |
| GitHub Actions is not in core execution | `architecture::github_actions_is_an_adapter_not_a_core_special_case` (no core crate mentions it; the CLI reaches it only from `worker_cmd.rs`) |
| Provider-specific behaviour stays in adapters | same; the worker depends on `compute-core` and `compute-runtime` only |
| Lifecycle and receipts stay authoritative; no second state system | `architecture::the_worker_adds_no_second_state_system_and_no_fixed_paths`; the worker writes no state and no receipt of its own, it returns Compute's |
| No hidden global mutable state | same (`static mut`, `lazy_static`, `OnceLock`, `thread_local!` forbidden in the worker) |
| No background process escapes | worker tests (timeout, cancellation, exit with a lingering descendant); script check forbids `&`, `nohup`, `disown` |
| Secrets cannot be printed or serialized | `architecture::a_secret_cannot_be_printed_or_serialized`; receipts record env names only (`architecture::receipts_record_environment_names_never_values`) |
| Machine-readable contracts are stable | `architecture::error_codes_are_stable_identifiers`; `contract::success_output_of_other_commands_is_unchanged`; the original `doctor --json` test still passes unchanged |
| No lifecycle inferred from a TTY | `architecture::lifecycle_is_never_inferred_from_a_terminal` |
| Execution sites stay classified | existing `execution_paths::every_execution_site_is_classified` passes without an allow-list change: the worker spawns nothing |
| Existing runtimes and targets keep working | the full workspace suite (below) |

## Result

Final status after this work and the correction pass. "Source" points at what
the claim rests on.

| Capability | Status | Source and note |
| --- | --- | --- |
| Persistent environment lifecycle | **EXISTS** | `compute-core/src/computers.rs:21,51`, `sessions.rs:67-81`; tests `compute-environment/tests/{lifecycle,readiness,computers}.rs`. No change |
| Doctor / host preflight | **PARTIAL → IMPLEMENTED** | Doctor existed (`main.rs`, `Commands::Doctor`). Added `--strict`, statuses `pass`/`warn`/`fail`, a stable `data.checks`; original keys unchanged. Reports adapter and controller self-description only: no requirement or admission logic (`contract.rs::doctor_checks`) |
| Stable machine-readable output | **PARTIAL → IMPLEMENTED** (failure side) | Success shapes untouched. Failure envelope on the last stderr line; controller error kinds survive via `ComputeError::Coded` (`environment_cmd.rs::error`, `EnvironmentError::kind`) |
| Actionable recovery | **PARTIAL → IMPLEMENTED** (one case) | Only `controller_unavailable` on the default local endpoint → `compute start` (`contract.rs::recovery`). The earlier `runtime_unavailable`/`unknown_runtime` → `compute doctor` was removed in the correction pass: several fixes exist, so no single one is deterministic |
| Host mounts (read-only / writable) | **EXISTS** (copy-in); **INTENTIONALLY DEFERRED** (writable mount) | Compute has copy-in, not host mounts (`stage_workload`, `lib.rs:2401`). Writable mounts would bypass declared outputs and receipt digests |
| Network policy | **EXISTS** (`none`/`localhost`/`network`, fail closed); **INTENTIONALLY DEFERRED** (`restricted`) | `host.rs:300-360`, `compute-runtime/tests/network.rs`. No runtime enforces an allow-list; `compute-network` is ingress |
| Snapshots / restore | **EXISTS** | `compute-environment/src/daemon/{checkpoint,restore,fork}.rs`. Filesystem checkpoints only |
| Browser runtime | **INTENTIONALLY DEFERRED** | No capability or target; `SessionCapabilities::NAMES` is a closed, wire-versioned list |
| Desktop runtime | **INTENTIONALLY DEFERRED** | Same |
| Agent presets | **PARTIAL**; per-agent recipes **INTENTIONALLY DEFERRED** | `recipes/starters/agent-task.json` exists; recipes hold requirements only |
| Interactive vs detached | **EXISTS** | `session exec --detach`, `remote submit`; a guard test enforces that no TTY is consulted |
| Strict ephemeral cleanup | **PARTIAL → IMPLEMENTED** (one-shot path) | Tests for success, failure, timeout, cancellation (`compute-runtime/tests/ephemeral.rs`); `Compute::run_controlled` |
| Ephemeral GitHub Actions runner | **IMPLEMENTED** (verified with a fake runner; **not** against live GitHub) | `crates/compute-worker-github`; see **Verification status** in `docs/github-actions-runner.md` |
| `ExternalWorker` trait | **NOT APPLICABLE** | One integration exists |

Also decided: **no `ExternalWorker` trait** (M). One integration exists; the
adapter's only seam is the `GitHubApi` trait it needs for testing. A trait would
be written against one example.

Deliberately **not** done: running the runner inside a persistent Computer (no
secret-input channel on the session path), runner deregistration through GitHub's
removal API, a session-based worker host, and any change to the process
runtime's post-exit behaviour.

## Correction pass (2026-10-01)

Re-verified against source; nothing from the first report was taken on trust.

| Found | Fix |
| --- | --- |
| Runner job name/result were parsed from `_diag` logs; the real runner prints them to the terminal (`JobDispatcher.cs`) | read from the runner's captured output |
| `file://` and unchecked URLs were accepted for the archive | https only (incl. redirects, TLS 1.2+); `--archive-file` for local archives; both verified by SHA-256 |
| A job the runner reported as `Failed` still exited 0 | `job_failed`, exit 1 |
| `doctor` reported an uninstalled runtime as `fail`, and a **healthy real controller as `fail`** (`authentication` is an info object there; mocks hid it) | `warn`/`fail` split; string-only auth problem; real-controller test |
| Recovery `runtime_unavailable`/`unknown_runtime` → `compute doctor`, and `compute start` for any controller | only `controller_unavailable` on the default endpoint; recovery moved out of `compute-core` into the CLI |
| Usage errors had no envelope | `invalid_arguments` (exit 2) on stderr in `--json` mode |
| "Mounts" described as read-only host mounts and "guest cannot change the host" | copy-in terminology; `process` isolation is not a boundary (tested) |
| `recipe` in the report implied the host satisfied it | renamed `declared_recipe`; documented as unevaluated |
| Receipt lacked repository/runner identity | sealed `runner-metadata.json` now carries them |

Verified against the **real** v2.337.0 runner archive (not mocked): token read
from `ACTIONS_RUNNER_INPUT_TOKEN`; flags accepted; through the worker as an
unprivileged user the runner reached GitHub, which returned 404 for a fake
token; reported stage `configure`, cleanup complete. The recipe was accepted by
a real controller (`recipe validate`: `satisfiable`, `recipe create`). **Not
verified:** registration with a real token and a real job (the sandbox cannot
reach the registration API with a valid credential).

## Test report

Full `cargo test --workspace --no-fail-fast` on the hardened tree:
**655 passed, 1 failed, 19 ignored** (baseline `c50218a`: 581 / 0 / 19).

The one failure, `compute-cli/tests/recovery.rs::an_execution_that_ends_while_the_controller_is_down_is_recorded_after_recovery`
(`left: 2, right: 1`), **fails identically on the untouched baseline `c50218a`**
built in a clean worktree in this environment, and in isolation on the hardened
tree, so it is not caused by this work. It passed earlier the same day, so it
is environment-sensitive; it was not investigated further. `cargo fmt --check`
is clean; `cargo clippy --workspace --all-targets` reports no warning in any
file added or changed here (existing warnings elsewhere are untouched). The
ignored tests need a FeltDB server, real packages or Playwright and were not run.
