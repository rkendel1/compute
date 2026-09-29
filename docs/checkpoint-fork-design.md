# Checkpoint, restore, and fork

**Status:** design, partly built. Capture is implemented as
[checkpoint.md](checkpoint.md), on a different foundation from the one below:
the canonical artifact is built in Rust from the validated workspace archive
([workspace.md](workspace.md)) and stored in the existing artifact store, with
no archiver job and no new provider method. Restore, retention, quiesce and
provider capabilities remain design. [fork.md](fork.md) is implemented over
export/seed/verify, not over checkpoints. Read
[opencomputer-evaluation.md](opencomputer-evaluation.md) for the context and
[persistent-environments.md](persistent-environments.md) for what survives
what today.

```text
Environment ──▶ Checkpoint ──▶ Fork ──▶ Environment'
                    │
                    └──▶ Restore (seeded replacement of the same environment's computer)
```

## The problem, in Compute's terms

A persistent environment keeps its *declared contents* forever (FeltDB) and its
*workspace* for as long as its machine lives. When the machine is lost or
replaced, contents reconverge (repositories re-cloned, packages re-installed,
processes restarted) and **everything else in the workspace is gone**
(`replace_computer`, `compute-environment/src/daemon/computers.rs:1856`). There
is also no way to derive a second environment from a first, to try several
things from one known state, or to return to one.

## Decision: what the abstraction is

Three candidates were evaluated:

| Candidate | Captures | Portable across providers? | Verdict |
| --- | --- | --- | --- |
| **Machine snapshot** (VM disk + memory) | everything, opaquely | **No.** Bound to the hypervisor, architecture, and kernel; a memory image cannot move between a workspace, a container, and a VM | **Rejected** as the portable abstraction. May exist later as a *provider-scoped* capability with its own name |
| **Execution checkpoint** (process state) | one process | No; process-bound, not environment-bound | Rejected: Compute recovers processes from *declaration* (`ProcessSpec`, restart policy), not memory |
| **Environment checkpoint** — the environment's filesystem plus its declared contents, plus provenance | what Compute can prove and reproduce | **Yes:** any environment with the `filesystem` capability whose files a Compute component can read | **Adopted** |

**Name.** *Checkpoint.* Not "snapshot": `ControlState::snapshot`
(`compute-state/src/control.rs:155`) and `CapacitySnapshot`
(`compute-core/src/lib.rs:518`) already use that word for unrelated things.

**Owner layer.** The **Environment** (daemon; FeltDB is authority). A raw
`compute session` is the substrate an environment's machine is made of and does
not get checkpoints: the durable unit is the environment.

**Provider dependence.** The portable *contract* is Compute's. A provider
supplies only access: it must let a Compute component read (and later write)
the environment's workspace. Providers that cannot advertise no `checkpoint`
capability and get an explicit `operation_unsupported`.

## What is captured

| State | Captured? | How / why |
| --- | --- | --- |
| Filesystem (workspace tree) | **Yes** | The archive's entries; the primary payload |
| Installed dependencies | **Yes, as files** | They live in the workspace (`repos/*/node_modules`, package installs); the manifest also records the fingerprints in `observed.packages` so a verifier can tell *which install* the tree corresponds to |
| Runtime | **Referenced, not embedded** | The runtimes named by `ProcessSpec.runtime` and their identities (`RuntimeDistribution`, the runtime lock) are recorded in provenance. A fork's target resolves and verifies its own runtime; a runtime binary is never copied between machines |
| Environment configuration | **Names only** | `EnvironmentRecord.config` (`compute-state/src/model.rs:259`) is "configuration visible to every workload" and may hold secret *values*. A checkpoint records key names, never values |
| Process state | **No** | Re-derived from declared processes after restore/fork; memory is not portable |
| Network configuration | **Declared only** | Ports/endpoints are declared in contents and re-established by placement; nothing about a machine's live network is captured |
| Application version | **Yes** | The active `Version`/rollout ids and each repository's checked-out commit (already in `observed.repositories`) |
| Source state | **Yes** | Repository checkouts are in the workspace; commits recorded |
| Compute metadata | **Yes** | Environment id/name, `EnvironmentContents.generation`, `ComputerRecord.generation`/`spec_generation`, source platform, Compute distribution identity |
| Receipt/evidence lineage | **Yes** | The capture job's `job_id`, `execution_id`, and receipt hash; the parent checkpoint (if any) |

**Not captured, on purpose:** credentials, tokens, connection material, target
credentials, control-plane identity, environment *values*, pid/log/exit files
under `.compute/` (machine-local runtime state), and anything matching the
recorded exclusion list.

## The checkpoint object

```text
Checkpoint  (FeltDB record; needs compute.flow + MODEL_GENERATION — see AGENTS.md)
├── checkpoint_id          ckp_…                 record identity
├── digest                 sha256:…              identity of the archive (content address)
├── environment_id / name  the environment it was taken from
├── name                   caller's label
├── status                 pending → ready → deleted     (failed: capture did not verify)
├── contents_generation    EnvironmentContents.generation at capture
├── computer_generation    ComputerRecord.generation / spec_generation
├── source_platform        os-architecture of the machine it came from
├── capture                job_id, execution_id, receipt hash  (the evidence)
├── consistency            crash | quiesced      (see below)
├── parent_checkpoint      the checkpoint this environment was itself seeded from, if any
├── size, entry_count, created_at, created_by
└── archive                stored in the daemon's ArtifactStore (Artifact/ArtifactChunk collections)
```

### Archive format: `compute.checkpoint@1`

Modelled on `DependencyCapsule` (`compute-core/src/dependencies.rs`), because
that format already answers every hard question — determinism, safety,
identity — and reviewers already trust it:

- canonical ordering; zero timestamps and ownership; normalized modes
  (`0644` / `0755`); explicit SHA-256 per entry; no absolute host paths;
- rejects symbolic links, special files, case-colliding paths, absolute or
  `..` paths, unsupported versions;
- `checkpoint_id` = `sha256:` over the canonical manifest, which covers every
  entry digest, so the identity covers the bytes transitively;
- identical trees give byte-identical archives; verification rejects modified
  or non-canonical archives;
- the manifest records the exclusion list that was applied, the source
  platform, and a `tree_digest` (digest over the sorted `(path, mode, sha256)`
  list) — the value a later verification job recomputes over a *seeded* tree.

## Capture

1. **Authorization.** A new `ProviderOperation` (next to `EnvironmentMutate`,
   `compute-provider/src/lib.rs:2012`) and an operator scope; owner-only, as
   every environment operation is.
2. **Precondition.** The computer is `running` (`running()`,
   `daemon/computers.rs:1931`); the provider advertises `checkpoint`.
3. **Consistency.** `crash` (default): archive the tree as it is. `quiesced`:
   stop declared processes with the existing `STOP_PROCESS` jobs, capture,
   restart them — using only mechanisms that exist. A checkpoint never claims
   more than the mode says; the mode is recorded.
4. **Execution.** The archiver runs as an ordinary durable job on the target,
   so it is admitted, reserved, placed, and receipted like any work: a
   `RuntimeKind::Native` execution of Compute's own executable with the
   session's workspace as its working directory (`command_in_directory`,
   `docs/session-architecture.md`, is the precedent). The receipt carries the
   executable's identity and the output digest (`OutputReceipt.sha256`), so
   **the receipt is the evidence** and nothing new is invented.
5. **Verification.** The daemon re-reads the stored archive and recomputes
   `checkpoint_id` before the record becomes `ready`. A mismatch marks it
   `failed`; no receipt records a failed checkpoint as a success.
6. **Storage.** Chunked into the `ArtifactStore`; a record holds the digest.

Open questions, documented rather than guessed:

- **Workspace access contract.** How a provider exposes the workspace to the
  archiver. Workspace and container providers already have a node-side
  directory (`WorkspaceSessionProvider`, container `/workspace` mount); a VM
  provider would implement the same contract through its node agent. The
  contract is a *provider method with an `unsupported` default*, mirroring
  `stop`/`resume`/`claim` in `SessionProvider` (`sessions.rs:350-362`).
- **Transport size.** Provider requests are capped (`DEFAULT_MAX_REQUEST_BYTES`,
  64 MiB, `compute-provider/src/lib.rs:55`) and outputs are bounded; a large
  workspace needs the existing job artifacts route
  (`GET /compute/jobs/{id}/artifacts`, `compute-provider/src/lib.rs:2662`)
  used in chunks. Phase 2 must state a maximum checkpoint size and refuse
  larger ones explicitly.
- **Quiesce cost.** Restarting processes changes `restarts` accounting; the
  policy for that is to record the restart as a normal start.

## Restore: seeded replacement

**Decision:** restore is *replacement seeded from a checkpoint*, not an
in-place rollback.

Compute's rule is that the requirements are the only thing that provisions a
machine and that **replacement stays explicit** (`docs/computers.md`
"Replacement"). The replacement flow already provisions the new machine,
reconciles the same desired contents onto it, and only then retires the old
session, with fenced writes at every step (`replace_computer`,
`begin_replacement`). Restore reuses it:

1. Authorize; choose the checkpoint (owner-checked; environment match, or a
   fork parent).
2. Increment `spec_generation`; place the new machine under the *same*
   requirements (placement decides where, and may refuse with reasons).
3. Provision; **seed** the workspace: a durable job extracts the archive into
   the empty workspace (extraction refuses a non-empty destination and any
   entry that escapes it).
4. **Verify**: a durable job recomputes the seeded tree's digest and compares
   it with the checkpoint's `tree_digest`. Only then is the seed `verified`.
5. Reconcile declared contents (repositories are already present; packages
   whose fingerprints match are not reinstalled), start processes.
6. Retire the old session.

If seeding or verification fails, the old machine stays; the failure names
the phase (`seeding`), and nothing is recreated silently. In-place rollback
may be added later as an optional provider capability; it is not the portable
contract.

## Fork

`fork` creates a **new environment** from a checkpoint:

```text
environment A ──checkpoint C──▶ fork ──▶ environment B (own id, own computer, own generation)
                                    └───▶ environment C, D, … (independent)
```

- **New identity.** New `environment_id`, name, `ComputerRecord`, and record
  ids. The fork's owner is the principal that forks, and that principal must
  be authorized to read the source checkpoint (in this design: the source
  environment's owner).
- **Copied:** declared contents (repositories, packages, processes, projects),
  the execution `policy` (it only restricts), and the checkpoint reference.
- **Not copied:** `config` *values* (names are listed; values must be
  supplied, or copied only by an explicit, separately authorized
  `--config-from-parent`), the `provider` pin, target, session, receipts,
  credentials, and any endpoint/domain assignment (a domain belongs to one
  environment).
- **Placement is fresh.** The fork's computer is placed on whatever target
  satisfies its requirements. Workspaces contain architecture-bound files
  (native `node_modules`, compiled output), so the checkpoint's
  `source_platform` becomes a default *architecture requirement* of the fork;
  changing platform is an explicit, evidenced choice, and contents that are
  architecture-independent reconverge anyway.
- **Lineage** (below) is written in the same fenced change that creates the
  fork.
- Forks do not share state after creation; independence is a conformance test.

### Lineage and identity semantics

```text
Environment.lineage  (set once, immutable)
├── parent_environment_id, parent_name
├── checkpoint_id, checkpoint_digest
├── forked_at, forked_by
└── depth              (informational; lineage is a DAG of records, not a live link)
```

- Lineage is a durable **record**, never a live dependency: deleting a parent
  environment or checkpoint does not delete or alter descendants; the record
  keeps the digest, so provenance survives deletion.
- A checkpoint's identity is its digest: the same tree captured twice has the
  same digest and *different* records (different capture receipts).
- Evidence: executions in a forked or restored environment carry an optional
  additive `lineage` block in the receipt (like `project`/`stack`/`app_bundle`),
  so a receipt shows *this ran in an environment seeded from checkpoint X of
  environment Y*. Old receipts are unchanged.

### Use cases

| Use case | Served by | What it needs |
| --- | --- | --- |
| Agent experimentation | fork × N from a "ready" checkpoint | independence; cheap capture; receipts per fork |
| Parallel builds | fork × N; each builds a variant | same |
| Test matrix | fork per matrix cell, each on a placement-chosen target | `source_platform` handling; per-cell receipts |
| Debugging | checkpoint at the failing state, fork to investigate | the archive is the failure's reproducible state |
| Safe upgrades | checkpoint → upgrade → verify → restore if wrong | verified seed; explicit replacement |
| Rollback | restore = seeded replacement | same |
| Deployment promotion | **not** by promoting a machine: publish source as a `Version` and use the existing `promote` | no new mechanism; what is promoted is source and declared build |
| Reproducing failures | checkpoint digest + receipt hashes name the exact state | lineage on receipts |

## Provider support and unsupported behaviour

- A provider without the workspace-access method reports `checkpoint: false`;
  `checkpoint create`, `restore`, and `fork` against such an environment fail
  with `operation_unsupported`, **before** anything is sent, exactly like an
  unsupported `stop` today.
- A provider that cannot seed a new machine (the counterpart method) makes
  `restore`/`fork` placement-incompatible with a reason
  (`session_capability_unsupported`), so the operation is refused rather than
  landing on a machine that cannot honour it.
- No provider is asked for a memory capture. If a request names one
  (`kind: full` in OpenComputer's vocabulary) it is refused; a checkpoint is
  never given a name that promises more than it holds
  (`docs/sandboxes/checkpoints.mdx` makes the same refusal).

<a name="security"></a>
## Security

- **Contents.** A workspace can contain secrets a workload wrote. Compute
  cannot recognise arbitrary secrets, so the design does not pretend to:
  checkpoint capture is owner-only and separately authorized; the archive is an
  owned artifact; the exclusion list (recorded in the manifest) covers what
  Compute itself puts in a workspace (`.compute/`) and conventional secret files
  by default (`.env*`, `*.pem`, `id_*`), overridable only by an explicit flag
  that is itself recorded.
- **Nothing Compute manages is in a checkpoint.** Target credentials,
  connection material (never persisted, `docs/sessions.md` "Security"), the
  control-plane token, and environment values are not written into workspaces
  by Compute, so they cannot be captured.
- **Forks inherit no authority.** No credentials, no connection grants, no
  config values by default, no domain. A fork starts with what its own
  requirements and its caller's explicit inputs supply.
- **Isolation.** Restore and fork provision under the same isolation profile and
  admission as any environment; a seed never weakens a profile.
- **Extraction safety.** Extraction refuses non-empty destinations, symbolic
  links, path escapes, and hash mismatches, as capsule materialization does.
- **Retention.** Checkpoints have owner-set retention and a per-environment
  limit; deletion removes bytes and keeps the digest in lineage.
- **AuthBoundry** is not involved unless an application chooses it as its
  authority boundary above Compute.

## Implementation note: what exists

`compute environment fork` ([fork.md](fork.md)) composes export, seed and a tree digest over existing primitives without a stored checkpoint. It shows the remaining work for checkpoint/restore/fork is storage, a record and lineage, not new transport or verification mechanism.
