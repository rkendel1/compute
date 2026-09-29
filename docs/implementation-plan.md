# Implementation plan: checkpoints, lineage, and the capabilities around them

**Status:** plan. Nothing here is implemented. Read
[opencomputer-evaluation.md](opencomputer-evaluation.md) first; the designs
behind each phase are in [checkpoint-fork-design.md](checkpoint-fork-design.md),
[compute-capabilities.md](compute-capabilities.md), and
[persistent-environments.md](persistent-environments.md).

Ground rules (from `AGENTS.md` and the existing architecture) that apply to
every phase:

- **FeltDB is the authority for durable state.** A new durable record needs a
  model change: `compute.flow` (`crates/compute-state-feltdb/model/compute.flow`),
  the regenerated manifest (`npm run generate` in `packages/compute-state-model`),
  and a `MODEL_GENERATION` bump (`crates/compute-state/src/model.rs:64`,
  currently `9`). Reads are by identity or by an indexed equality with the
  order and limit done by FeltDB; never a scan
  (`crates/compute-environment/tests/feltdb_consumer.rs` fails otherwise).
- **No second store, no local fallback, no hidden state.** A `compute serve`
  node's session store is its own; the daemon's state is FeltDB.
- **Every phase is independently shippable** and leaves every existing test
  green. Wire additions are additive (`#[serde(default)]`), and readers ship
  before writers.
- **Every state-changing operation has evidence** by the existing chain (job →
  receipt → `OperationEvidence` → `reality`); no new receipt system.

## Sequence

| Phase | Goal | Depends on |
| --- | --- | --- |
| **1** | `compute.checkpoint@1`: deterministic archive of a directory (library only) | — |
| 2 | Capture as a receipted job; durable `Checkpoint` record; `checkpoint create/list/inspect/verify/delete` | 1 |
| 3 | Restore as seeded replacement | 2 |
| 4 | Fork with durable lineage; `lineage` on receipts | 3 |
| 5 | Verified resume, idle policy, wake-on-use | — (parallel to 2–4) |
| 6 | `resize` capability | 3 (for the state-preserving fallback) |
| 7 | Terminals through `connect` | — |
| 8 | Endpoint state model | — |
| 9 | `max_lifetime` capability | 3 (for the accommodation) |

Ordering rationale: checkpoint → restore → fork is the chain that unlocks the
most (state-preserving replacement, parallel experiments, reproducible
failures) and each link is the smallest step that makes the next possible.

<a name="phase-1"></a>
## Phase 1 — `compute.checkpoint@1` (one small PR)

**Not** "persistent environments". This phase adds a *file format and its
verification*, nothing else — the same first step `DependencyCapsule` was for
dependencies. It changes no state, no wire protocol, no provider, no CLI, no
FeltDB model, no receipt, and no placement.

### What it adds

A pure library in `compute-core`: capture a directory tree into a
deterministic, content-addressed archive; verify and read it; extract it
safely; and *observe* a live tree's digest so a later phase can prove a seeded
tree matches its checkpoint.

### Exact changes

| File | Change |
| --- | --- |
| **new** `crates/compute-core/src/checkpoint.rs` | The whole feature (below) |
| `crates/compute-core/src/lib.rs` | `mod checkpoint; pub use checkpoint::*;` beside `mod dependencies; pub use dependencies::*;` (lines 22–23); add `ComputeError::InvalidCheckpoint(String)` (`#[error("invalid checkpoint: {0}")]`) beside `InvalidDependencyCapsule` (line ~2777) |
| `crates/compute-core/src/dependencies.rs` | `fn validate_dependency_path` (line 524) becomes `pub(crate)` and is reused for path safety (no behaviour change); the case-collision and symlink checks in `DependencyCapsule::create` (line 137) are the model to follow, not code to edit |
| **new** `docs/checkpoint-fork-design.md` §"Archive format" | Already written with this plan; the PR only updates it if the format differs |

No new crate dependencies: `compute-core` already depends on `tar`, `sha2`,
`walkdir`, `tempfile` (`crates/compute-core/Cargo.toml`). `sha256_identity`
(`compute-core/src/receipt.rs`) is reused for identities.

### Symbols (proposed, mirroring `DependencyCapsule`)

```rust
pub const CHECKPOINT_FORMAT: &str = "compute.checkpoint@1";

pub struct CaptureOptions {
    pub excludes: Vec<String>,   // recorded in the manifest
    pub max_bytes: u64,          // refuse larger trees explicitly
}
impl CaptureOptions { pub fn default_excludes() -> Vec<String>; }   // ".compute", ".env*", "*.pem", "id_*"

pub struct CheckpointEntry { pub path: PathBuf, pub kind: EntryKind, pub size: u64,
                             pub sha256: String, pub executable: bool }
pub struct CheckpointManifest { pub format: String, pub version: u32, pub checkpoint_id: String,
                                pub source_platform: PlatformIdentity, pub excludes: Vec<String>,
                                pub tree_digest: String, pub entries: Vec<CheckpointEntry> }
pub struct CheckpointArchive { /* manifest + file bytes */ }

impl CheckpointArchive {
    pub fn capture(root: &Path, options: &CaptureOptions) -> Result<Self>;
    pub fn checkpoint_id(&self) -> Result<String>;   // sha256 over the canonical manifest
    pub fn write(&self, path: &Path) -> Result<u64>; // canonical tar
    pub fn read(path: &Path) -> Result<Self>;        // verifies identity, ordering, paths
    pub fn extract(&self, destination: &Path) -> Result<()>;   // empty destination only
}
/// Observe a live tree: the digest a checkpoint's `tree_digest` is compared with.
pub fn observe_tree_digest(root: &Path, excludes: &[String]) -> Result<String>;
```

Directories are entries (`kind: dir`), unlike capsules, because workspaces
contain meaningful empty directories (a Git checkout's `refs/`); modes are
normalized to `0644`/`0755`.

### Tests (all in `compute-core`, no network, no processes)

- **Determinism:** the same tree captured twice → byte-identical archives and
  equal `checkpoint_id`; changing mtimes, owners, or creation order changes
  nothing.
- **Identity:** changing one byte, one path, one executable bit, or the
  exclusion list changes `checkpoint_id`; `tree_digest` ignores the exclusions
  it records but reflects every included entry.
- **Safety:** capture rejects symbolic links, sockets/FIFOs/devices, and
  case-colliding paths; `read` rejects absolute paths, `..`, backslashes,
  drive prefixes, unsupported versions, unknown fields, reordered or duplicated
  entries, hash mismatches, and truncated archives; `extract` refuses a
  non-empty destination and any entry that would escape it.
- **Exclusions:** `.compute/` and default secret patterns are omitted and
  *recorded*; a caller override is recorded too.
- **No host leakage:** the manifest contains no absolute host path and no
  timestamp.
- **Limits:** a tree over `max_bytes` is refused with a message naming the
  limit.
- **Reality primitive:** capture → extract into a fresh directory →
  `observe_tree_digest` equals `tree_digest`; deleting or altering a file
  makes them differ.
- **Compatibility:** all existing `compute-core` tests pass unchanged; no other
  crate needs an edit (verify with `cargo build --workspace`).

### Acceptance criteria

1. `cargo test -p compute-core` and `cargo test --workspace` pass; `cargo
   clippy` reports nothing new in the added file.
2. The archive is a pure function of the tree and options: no clock, no host
   path, no environment.
3. Nothing outside `compute-core` changes, except documentation.
4. A reviewer can read one file (`checkpoint.rs`) against one precedent
   (`dependencies.rs`).

### Why this first

- It is the **identity and verification substrate** every later phase relies
  on (capture output, seed verification, lineage digests).
- It is pure: zero migration, zero rollout ordering, trivially reversible.
- It fixes the two hardest semantic questions early — *what is byte-for-byte
  identity of a workspace*, and *what is excluded* — before any operational
  machinery depends on the answers.
- `observe_tree_digest` gives Compute a **reality primitive** (observed tree
  vs declared checkpoint), the mechanism by which "verified" will be earned
  later instead of claimed.

### Explicitly out of Phase 1

A CLI (`compute checkpoint …`), any provider change, `SessionCapabilities`
fields, `ProviderOperation` variants, the FeltDB `Checkpoint` collection,
`ArtifactStore` use, jobs, receipts, placement, restore, fork, resize, idle
policy, terminals, endpoints, or lifetime ceilings.

## Phase 2 — Capture as a receipted job; the `Checkpoint` record

**Goal.** `compute environment checkpoint create <env> --name N` produces a
verified, durable, named checkpoint whose evidence is an ordinary receipt.

| Area | Change |
| --- | --- |
| Existing code to modify | `SessionProvider` (`compute-provider/src/sessions.rs:307`): add a workspace-access method with an `unsupported` default beside `stop`/`resume`/`claim` (`:350-362`); implement it in `WorkspaceSessionProvider` (`:380`, `directory()`) and `ContainerSessionProvider` (`compute-provider/src/containers.rs:27`); `SessionManager` (`:538`): a `checkpoint` operation that submits the archiver as a job through `JobManager::accept` (`compute-provider/src/jobs.rs:71`); `ProviderOperation` (`compute-provider/src/lib.rs:2012`): `SessionCheckpoint`, the route table (`:2662`), and a `RemoteProvider` method; `SessionCapabilities` (`compute-core/src/sessions.rs:122`): `checkpoint` (`#[serde(default)]`, `NAMES`, `get`) |
| Daemon | `checkpoint_environment` beside `replace_computer` (`compute-environment/src/daemon/computers.rs:1856`); precondition via `running()` (`:1931`); authorization owner-only |
| New abstractions | `Checkpoint` (record); consistency mode `crash | quiesced` (quiesced reuses `STOP_PROCESS`, `:900`) |
| API | `POST/GET/DELETE /environments/{env}/checkpoints…` (`compute-environment/src/api.rs`, `client.rs`) |
| Persistence | `Collection::Checkpoint` (`compute-state/src/store.rs:16`, `ALL`), `CheckpointRecord` (`compute-state/src/model.rs`, `document!`), `compute.flow` + index on `environment_id`, manifest, `MODEL_GENERATION` 10; archives in the daemon's `ArtifactStore` (`Artifact`/`ArtifactChunk`); memory and file backends and `compute-state/src/conformance.rs` |
| Provider | Workspace and container providers implement the access method; others default to unsupported |
| CLI | `compute environment checkpoint create|list|inspect|verify|delete` (`compute-cli/src/environment_cmd.rs`) |
| Receipt / evidence | The capture job's receipt (executable identity + output digest); `OperationEvidence` on the record; event `checkpoint.created` |
| Tests | Conformance "Checkpoint" and "Unsupported" cases; feltdb consumer test still passes; maximum-size refusal |
| Migration | Additive collection; model generation bump follows the FeltDB upgrade path (`compute-state-feltdb/src/upgrade.rs`, `provision.rs`) |
| Acceptance | A checkpoint is `ready` only after re-verification; a provider without the method returns `operation_unsupported` before any send; a failed capture leaves a `failed` record and no `ready` one |

Open questions to settle in this phase (recorded in
[checkpoint-fork-design.md](checkpoint-fork-design.md#capture)): the workspace
access contract, transport size (the job artifacts route,
`compute-provider/src/lib.rs:2662`, against the 64 MiB request cap at `:55`),
and quiesce accounting.

## Phase 3 — Restore as seeded replacement

**Goal.** `compute environment restore <env> <checkpoint>` replaces the
computer with one seeded and *verified* from the checkpoint. Also the
state-preserving form of `replace`.

| Area | Change |
| --- | --- |
| Existing code | `replace_computer` (`daemon/computers.rs:1856`) and `begin_replacement` (`:3210`): accept `seed: Option<SeedRef>`; provisioning step "seed" before contents converge |
| New abstractions | `SeedRef` (checkpoint id + digest); jobs `SEED_WORKSPACE` and `VERIFY_TREE` beside `INSTALL_PACKAGE` (`:847`) |
| Persistence | `ComputerRecord.seed` (additive, `compute-state/src/model.rs:289`); `MODEL_GENERATION` 11 if the record schema is versioned in `compute.flow` |
| Evidence | Seed job receipt + verification job receipt; `ComputerReality.seed` (`compute-environment/src/status.rs:516`); events `computer.seeding`, `computer.seeded`, `computer.seed_failed` |
| CLI | `compute environment restore` |
| Tests | Conformance "Persistence" and "Checkpoint": create → mutate → checkpoint → mutate → restore → verify |
| Acceptance | Old machine retained until the seed verifies; failure names phase `seeding`; nothing is recreated silently; the seeded tree digest equals the checkpoint's |

## Phase 4 — Fork and lineage

**Goal.** `compute environment fork <env> <new> [--checkpoint C]`.

| Area | Change |
| --- | --- |
| Existing code | `create_computer_environment` (`daemon/computers.rs:973`): a fork variant that copies contents and `policy`, drops `provider`, sets `owner`; placement adds a default architecture requirement from `source_platform` |
| New abstractions | `Lineage` on `EnvironmentRecord` (`compute-state/src/model.rs:259`; additive, immutable) |
| Receipts | Optional `lineage` block on `ExecutionReceipt` (`compute-core/src/receipt.rs:184`), bound like `project`/`stack`/`app_bundle` via `ExecutionOptions` (`compute-provider/src/lib.rs`) and sealed at `receipt.seal()` |
| Security | No config *values* copied unless `--config-from-parent` (separately authorized); no domain; no credentials |
| CLI | `compute environment fork` |
| Tests | Conformance "Fork" and "Receipt lineage"; independence; lineage survives deletion of the parent |
| Acceptance | Fork is independent (mutating A never changes B); its receipts show the lineage; old receipts verify unchanged |

## Phase 5 — Verified lifecycle, idle policy, wake-on-use

**Goal.** Stop trusting the provider's answer for resume; add OpenComputer's
"laptop lid" behaviour with Compute's semantics.

| Area | Change |
| --- | --- |
| Existing code | `resume_step` (`daemon/computers.rs:3988`): after a successful provider `resume`, run a verification job (workspace marker digest + process probe) and set `observed` only from it, else `unverified`; `stop_step` (`:3930`): confirm with `inspect`; `running()` (`:1931`): optional wake for `exec --resume` |
| New abstractions | `idle_timeout` in the environment's lifetime settings (additive); controller rule over execution submission times that are already recorded (`SessionExecution.submitted_at`, `compute-core/src/sessions.rs:363`, and the `environment.exec` events). Connections record no time today, so "idle" counts executions only until connection events exist (Phase 7) |
| Evidence | Verification job receipts; events `computer.idle_stopped`, `computer.woken` with their cause |
| CLI | `compute environment lifetime --idle 30m`; `exec --resume` |
| Tests | Conformance "Persistence"; idle policy under a fake clock; wake failure leaves `stopped` |
| Acceptance | No automatic transition without an event naming its cause; a resume that cannot be verified is displayed `unverified`, never `running` |

## Phase 6 — `resize`

**Goal.** In-place resize where a provider can; explicit replacement elsewhere.

| Area | Change |
| --- | --- |
| Existing code | `SessionCapabilities` (`resize`); `SessionProvider` (`resize` with an `unsupported` default); container provider: `docker update` for CPU/memory; `ComputerRequirements` (`compute-core/src/computers.rs:155`) diff logic beside `replace_computer` |
| Evidence | Verification job reads applied limits; requested vs observed recorded; event `computer.resized` |
| CLI | `compute environment resize` (`operation_unsupported` names `replace`) |
| Tests | Conformance "Unsupported capabilities" and a container-only case (skipped without an engine) |
| Acceptance | Never replaces silently; capacity reservations stay derived (`docs/capacity.md`) |

## Phase 7 — Terminals

**Goal.** Implement `terminal` over the existing `connect` contract.

| Area | Change |
| --- | --- |
| Existing code | `SessionProvider::connect` (`sessions.rs`), `SessionConnectionMode::Terminal` (`compute-core/src/sessions.rs:199`); a node route with upgrade, authorized as `SessionConnect` on every connection; container provider via `docker exec -it` |
| Evidence | connected/closed events; optional transcript artifact with a digest |
| Tests | Conformance "Failure": a killed provider-side process; unsupported providers refuse |
| Acceptance | Credentials are per connection and never stored; a stopped session cannot be reconnected |

## Phase 8 — Endpoint state model

**Goal.** Represent `unavailable | internal | published`, honestly.

| Area | Change |
| --- | --- |
| Existing code | `SessionEndpoint` (`compute-core/src/sessions.rs:258`): add a state; `compute-network` ingress for `published`; authorization `session_expose` stays per endpoint |
| Evidence | Readiness probe from inside the computer; ingress events |
| Acceptance | No provider is asked to publish what it cannot; `unavailable` is recorded with its reason; nothing is public by default |

## Phase 9 — `max_lifetime`

**Goal.** Providers disclose ceilings; placement respects them.

| Area | Change |
| --- | --- |
| Existing code | `ProviderCapabilities` (`compute-provider/src/lib.rs:331`); `match_provider` (`compute-placement/src/matching.rs:185`) with a new `ReasonCode`; `reality` shows the deadline |
| Acceptance | A persistent environment is refused on a ceilinged provider unless it opts into checkpoint-and-replace |

<a name="conformance"></a>
## Conformance suite

**Purpose:** make every capability claim provable across providers, in the
manner of `compute-state/src/conformance.rs` and `compute-runtime-conformance`.

**Where:** a public module in `compute-provider` (`conformance.rs`) exercised by
`crates/compute-provider/tests/session_conformance.rs` against the workspace
provider, the container provider (skipped where no engine is present, with the
skip reported), and the fake provider used by the session tests. Environment-
layer cases (checkpoint, fork, lineage) run against a fake *target* in
`crates/compute-environment/tests/`, the way `computers.rs` does today.

Every case reports one of **verified**, **unsupported (as advertised)**, or
**failed**; a provider that advertises a capability and fails its case is a
failure, and one that does not advertise it must refuse explicitly.

| Case | Steps | Asserts |
| --- | --- | --- |
| **Persistence** | create → write a marker file → `stop` → `resume` → read | Same bytes; same `provider_session_id`; the provider never substituted a machine. A provider without `resume` refuses `resume` with `operation_unsupported` and stays `stopped` |
| **Checkpoint** | create → mutate → checkpoint → mutate → restore → verify | The observed tree digest equals the checkpoint's; mutations after the checkpoint are gone; the archive's `checkpoint_id` is stable across captures of an unchanged tree |
| **Fork** | environment A → fork B → mutate A → verify B unchanged | B is a distinct environment and computer; A's mutation is not visible in B; B's mutation is not visible in A |
| **Receipt lineage** | create → checkpoint → fork → execute in the fork | The execution receipt verifies independently and its `lineage` names the parent environment and checkpoint digest; the capture receipt's output digest equals the checkpoint digest; an execution in an unrelated environment has no `lineage` |
| **Unsupported capabilities** | ask a provider that cannot checkpoint (or resize, terminal, endpoint) for it | An explicit `operation_unsupported`/placement reason; nothing is sent; no record claims success |
| **Failure** | kill the provider-side execution (and, separately, restart the target) mid-operation | The documented semantics hold: the job is `provider_interrupted`, never a fabricated success; an in-flight checkpoint is `failed`, not `ready`; the old computer survives a failed seed; a machine the provider lost is `lost`, never silently recreated |
| **Idempotence** | repeat `provision`/`destroy`/`resume` after a simulated restart | One environment; teardown succeeds when the machine is already gone |
| **Isolation of credentials** | fork an environment that has config values and connection grants | The fork has none of them unless explicitly supplied |

The suite is also the definition of "verified" for these operations: a
capability is advertised only if its case passes for that provider.
