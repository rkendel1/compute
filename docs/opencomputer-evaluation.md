# OpenComputer: what Compute should borrow, and what it should not

**Status:** design and plan. Nothing in this document is implemented. Every claim
about Compute cites the code it was checked against (paths and symbols, as of
this branch); every claim about OpenComputer cites the documentation source
it came from.

> Compute is authoritative. OpenComputer is source material for evaluating
> primitives, not a reference architecture. Where one of its concepts
> conflicts with Compute's portability, provider contract, receipt/evidence
> chain, or environment model, this document rejects or reshapes the concept
> rather than reshaping Compute.

Companion documents:

| Document | Contents |
| --- | --- |
| [persistent-environments.md](persistent-environments.md) | What survives what today; hibernate/resume, long-running work |
| [checkpoint-fork-design.md](checkpoint-fork-design.md) | Checkpoint, restore, fork: what is captured, identity, lineage, security |
| [compute-capabilities.md](compute-capabilities.md) | The capability model and portability rules; resize, terminals, endpoints |
| [implementation-plan.md](implementation-plan.md) | Phases, the exact Phase 1 PR, conformance tests |

## 1. Executive summary

**Source material.** OpenComputer (`diggerhq/opencomputer`, checked at commit
`bd574a6`, version 0.6.0) is two products. The lower layer is *sandboxes*: a
control plane that provisions a full Linux VM per sandbox on worker nodes,
with `hibernate`/`wake`, named checkpoints (`restore`, `spawn`), runtime
scaling, PTY sessions, preview URLs, and sealed secrets. The upper layer is
*serverless agents*: agent projects, durable agent sessions, models, tools,
memory, schedules. The upper layer is an AI product and is out of scope here.

**What Compute already has** (verified in code, not inferred from
documentation): durable, owned, authorized *sessions* with a stop/resume/claim
lifecycle and per-environment capability reporting; *environments* as durable
computers whose desired contents (repositories, packages, supervised
processes) are reconciled continuously, with restart policy, readiness, and an
observed-versus-desired `reality`; explicit machine *replacement*; placement
that proves eligibility from capabilities; durable job reservations; and a
receipt for every execution. So OpenComputer's "persistent machine",
"long-running", and much of "hibernate/wake" are **existing or partial in
Compute**, and expressed in a stronger model (declarative contents plus
evidence) than a VM that merely stays up.

**What Compute is missing.** Verified absent (no code or docs mention them):

1. **Checkpoint / restore / fork** — nothing captures an environment's state
   or derives one environment from another. The only durable answer to "the
   machine was replaced or lost" is re-converging *declared* contents; files a
   workload wrote outside declared contents are gone.
2. **In-place resource change** — the only way to change CPU or memory is
   `compute environment replace`, which provisions a new machine and loses its
   undeclared state.
3. **Interactive terminals** — `SessionCapabilities.terminal` exists and no
   provider sets it; the Work terminal runs each command as a durable job.
4. **Reachable endpoints for sessions** — `SessionEndpointRequest` exists;
   every provider refuses `endpoints`; the container provider does not
   publish ports.
5. **Provider lifetime ceilings** — nothing lets a provider disclose that it
   destroys machines after N hours, which a "persistent" requirement must
   account for.

**What Compute should adopt** (reshaped): a **Checkpoint** of an environment's
*filesystem plus declared contents*, content-addressed and portable across
providers; **restore as seeded replacement** and **fork as a new environment
seeded from a checkpoint**, both with durable lineage; in-place **resize as an
optional provider capability** with replacement as the explicit portable
fallback; a **truthful lifecycle** (stop/resume documented and verified for what
they keep, plus idle policy and wake-on-use); terminals through the existing
`connect` contract; endpoints as a *state model*, not a promise of a public
URL; and a **lifetime-ceiling capability**.

**What Compute should reject:** VM snapshots as the portable checkpoint
(memory state is architecture- and kernel-bound and cannot cross providers);
"the sandbox is the unit" (Compute's unit is the environment, which is not a
VM); public-by-default preview URLs; hibernation as a distinct state that
silently discards what a name like "hibernate" promises; an in-VM secrets
proxy as a Compute primitive; agent-specific objects in the portable layer;
and adopting any OpenComputer API shape.

**Phase 1** is deliberately small: a deterministic, content-addressed archive of
a directory tree (`compute.checkpoint@1`) as a pure `compute-core` library,
mirroring how `DependencyCapsule` began, with no state, wire, provider, or
model change ([implementation-plan.md](implementation-plan.md#phase-1)).

## 2. Current Compute architecture

Compute's thesis: *build software once; run it wherever Compute can satisfy its
requirements.* The chain is:

```text
Application / project ── requirements ──▶ Placement ──▶ Provider ──▶ Machine
        │                    (what it needs)   (proves eligibility)  (how it runs)
        ▼
   Environment ── desired contents ──▶ Computer (durable session on a target)
        │                                   │
        └────────── execution (durable job) ┴──▶ Receipt ──▶ Evidence ──▶ Reality
```

The table below maps every concept the brief lists to its current
representation. "Portable?" asks whether the representation is independent of
the provider that hosts it.

| Concept | Current Compute representation | Source | Lifecycle | Durable? | Portable? | Evidence? |
| --- | --- | --- | --- | --- | --- | --- |
| Application | `ApplicationIdentity`; `ApplicationArtifact` (deterministic archive: manifest + canonical bundle); deployed as a project version | `compute-core/src/jobs.rs:20`; `compute-core/src/application_artifact.rs:127`; `compute-environment/src/daemon/applications.rs` | published → deployed → rolled out → active / rolled back | Yes (FeltDB `Version`, `Rollout`) | Artifact yes; a running application is on one computer | Receipt carries `ApplicationIdentity`; rollout events |
| Project | `ProjectSpec`, `ProjectAssembly`, `ProjectProposal`; state `ProjectRecord` / `VersionRecord` | `compute-core/src/computers.rs:532,1039,1054`; `compute-state/src/model.rs:413` | proposed → assembled → published as immutable version | Yes | Yes (source + declared build) | Job receipts for build/test; `OperationEvidence` |
| Environment | `EnvironmentRecord`; desired contents `EnvironmentContents` (repositories, packages, processes, projects, `generation`) | `compute-state/src/model.rs:259`; `compute-core/src/computers.rs:564` | created → contents changed (fenced by generation) → destroyed | Yes (FeltDB) | Yes: declarative, provider-free | Event per change (`environment.contents_changed`) |
| Computer (the machine of an environment) | `ComputerRecord`: status, `generation`, `spec_generation`, `target`, `placement_id`, `session_id`, `provider_resource`, `capabilities`, `observed`, `retired` | `compute-state/src/model.rs:289`; `ComputerStatus` at `compute-core/src/computers.rs:51` | pending → provisioning → running ⇄ stopping/stopped/resuming; unreachable, lost, expired, destroyed | Yes (FeltDB, fenced writes) | Record is portable; the machine is a target's | `observed` items each hold their job + execution + receipt |
| Runtime | `RuntimeKind`, `RuntimeDistribution`; pinned in `distribution/runtime-lock.json` | `compute-core/src/lib.rs:44,2164`; `compute-runtime/src/lib.rs:183` | resolved → prepared → executable | Prepared runtimes are on-disk, content-verified | Resolved per provider; identity is portable | `RuntimeIdentity` in every receipt |
| Execution | `ExecutionRequest` → `ExecutionResult`; as a durable job `ExecutionJob` under `JobManager` | `compute-core/src/lib.rs:525,1558`; `compute-core/src/jobs.rs:251`; `compute-provider/src/jobs.rs:71` | queued → waiting_for_capacity → reserved → admitted → running → terminal | Job record (job store) | Yes (`compute.remote@1`) | `ExecutionReceipt` |
| Session | `ComputeSession` written by `SessionManager` before any provider call | `compute-core/src/sessions.rs:377`; `compute-provider/src/sessions.rs:538` | requested → provisioning → ready ⇄ running; stopping/stopped/resuming; expiring/destroying → terminal | Yes (`<session-store>/sessions/<id>/session.json` + events) | Yes: "a portable, authorized, durable handle" | `SessionEvent` per transition; every command is a job with a receipt |
| Provider | `ComputeProvider` (execution), `SessionProvider` (environments); `LocalProvider`, `RemoteProvider`; `ProviderCapabilities` | `compute-provider/src/lib.rs:660,736,1559,331`; `compute-provider/src/sessions.rs:307` | discovered → healthy/unhealthy | Descriptors cached (`compute-placement/src/pool.rs`) | The contract is; implementations differ | `capability_version` recorded in placement |
| Machine / target | `ComputeTarget` (a pool member hosting sessions) with `target_features` (`kvm`, `gpu`, `containers`, …) | `compute-placement/src/targets.rs:18`; `compute-core/src/computers.rs:129`; `compute-placement/src/pool.rs:292` | discovered → placeable | Config (pool file), FeltDB placement records | Yes | Placement report lists every target and reasons |
| Capacity | `ProviderCapacity`, `ResourceRequirements`, `CapacitySnapshot`; reservations derived, not counted | `compute-core/src/lib.rs:471,480,518`; `docs/capacity.md` | reserve → admit → release | Reservation records (job store) | Normalized CPU/memory/disk/concurrency | `ReceiptReservation` (`receipt.rs:395`) binds the snapshot |
| Reservation | One durable reservation per job | `compute-core/src/jobs.rs`; `docs/capacity.md` | pending → reserved → released | Yes | Yes | In receipt |
| Distribution | `DistributionIdentity`; assembled and certified by `compute distribution` | `compute-core/src/receipt.rs:79`; `distribution/`; `compute-cli/src/distribution.rs` | built → certified | Files | Yes (identity is a hash) | `provenance` in receipt |
| Dependencies | `DependencyCapsule` (`compute.deps@1`): deterministic, content-addressed archive of a resolved payload | `compute-core/src/dependencies.rs:79` (`write` at 462, `validate_dependency_path` at 524) | created externally → verified → materialized under a fresh workspace | File / resident cache | Platform- and runtime-bound, verified | `ReceiptDependencies` |
| Runtime lock | Pinned runtime versions and digests | `distribution/runtime-lock.json`; `compute-runtime/src/lib.rs:183` | fixed per distribution | File | Yes | `runtime_lock` in receipt provenance |
| Receipt / evidence | `ExecutionReceipt` (`compute.receipt@1`); `OperationEvidence`; additive optional blocks `project`, `stack`, `app_bundle` on this branch | `compute-core/src/receipt.rs:184`; `compute-core/src/computers.rs:747`; `compute-core/src/project.rs`, `stack.rs`, `app_bundle.rs` | sealed once per execution; verified independently | Portable file; copies in job store / FeltDB `Receipt` | Yes | The evidence chain itself |
| Placement | `place_with_policy` over `PlacementRequirements`; `match_provider` yields structured `ReasonCode`s | `compute-placement/src/placement.rs:426`; `requirements.rs:120`; `matching.rs:185` | evaluated per submission | Report is portable; identity is hashed | Yes | `ReceiptPlacement` |
| Isolation | `IsolationProfile` (`process` < `sandboxed` < `strict`), `HostProfile`, `IsolationEvidence` | `compute-core/src/lib.rs:255,344`; `compute-core/src/host.rs:30`; `docs/isolation.md` | resolved before staging | — | Profiles are portable; enforcement per runtime | Recorded per execution |
| Persistence | Session/job stores on a `compute serve` node; FeltDB `Collection` for the daemon | `compute-provider/src/sessions.rs:538`; `compute-state/src/store.rs:16`; `docs/feltdb.md` | atomic write before each provider call | Yes | A model change requires `compute.flow` + `MODEL_GENERATION` (`AGENTS.md`) | Events |
| Hibernate / resume | `SessionProvider::stop` / `resume`; capabilities `suspend`, `resume`; `stop_step` / `resume_step` | `compute-provider/src/sessions.rs:350,357`; `compute-core/src/sessions.rs:129`; `compute-environment/src/daemon/computers.rs:3930,3988` | see [persistent-environments.md](persistent-environments.md#stop-and-resume-today) | Yes | Only as declared by capability | `SessionEvent`, `computer.stopped`; provider answer is advisory |
| Snapshots / checkpoints | **None.** No source or documentation mentions checkpoint, hibernate, or resize. (`ControlState::snapshot` and `CapacitySnapshot` are unrelated.) | searched `crates/`, `docs/`, `stacks/` | — | — | — | — |
| Networking | `compute-network`: domains, ingress, DNS providers, ACME certificates; endpoints for deployed processes | `crates/compute-network/`; `docs/networking.md` | domain → ingress route → certificate | Yes (FeltDB `Domain`, `DnsRecord`, `Certificate`) | Reachability is the daemon's, not the provider's | Network events |
| Ports / preview URLs | `SessionEndpointRequest`/`SessionEndpoint`; `ProcessSpec.port` + HTTP readiness | `compute-core/src/sessions.rs:258,272`; `compute-core/src/computers.rs:319`; providers refuse: `compute-provider/src/sessions.rs:432` | requested → (never realized for sessions today) | Endpoints on session record | Unsupported everywhere today, explicitly | Readiness recorded for processes |
| Terminals | `SessionCapabilities.terminal`; `SessionConnectionMode::{Terminal, Websocket, Ssh, …}`; `ProviderConnection` | `compute-core/src/sessions.rs:124,199`; `compute-provider/src/sessions.rs:291`; container `connect` at `containers.rs:258` returns a `compute session exec` command | connect issues short-lived material, never persisted | Not persisted (by design) | Contract portable; no provider implements a PTY | None (no transcript) |
| Filesystem state | Workspace directory per session (`WorkspaceSessionProvider`); container mounts it at `/workspace` (`ContainerSessionProvider`); capability `filesystem` | `compute-provider/src/sessions.rs:380`; `compute-provider/src/containers.rs:27` | kept across stop/resume; removed on destroy/expiry | Yes, on the node | Per provider | Observed only through jobs |
| Process state | `ProcessSpec`; `START_PROCESS`/`STOP_PROCESS`/`PROBE_PROCESSES`; observed state, readiness, restart counts | `compute-core/src/computers.rs:319`; `compute-environment/src/daemon/computers.rs:853,900,917`; `ProcessReality` `compute-environment/src/status.rs:541` | desired ↔ observed with bounded restarts | Yes (`ComputerRecord.observed`) | Yes: processes are *re-derived* from declaration | `OperationEvidence` per start/stop/probe |

## 3. OpenComputer concept mapping

| OpenComputer | Documentation source | Compute equivalent | Verdict |
| --- | --- | --- | --- |
| Sandbox = a full VM (KVM) | `docs/introduction.mdx`, `docs/how-it-works.mdx` | A *session* is a handle to "an execution environment — not a VM-specific abstraction" (`docs/sessions.md`); an environment's *computer* is a session on a target | **Reject as the unit.** Adopt nothing at this layer |
| Persistent, long-running (`timeout: 0`) | `docs/sandboxes/timeout.mdx` | Persistent environment: claimed session, no TTL (`compute environment create --persistent`) | **Existing** |
| Hibernate / wake (memory + disk) | `docs/sandboxes/overview.mdx`, `timeout.mdx` | `stop` / `resume` (disk retained; processes stopped and re-derived) | **Partial**; memory-inclusive suspend rejected as a portable promise |
| Auto-hibernate on idle; wake on any operation | `docs/sandboxes/timeout.mdx` | None | **Adopt, reshaped**: idle *policy* on environments; wake-on-use |
| Checkpoint (`oc checkpoint create`) | `docs/how-it-works.mdx`, `docs/sandboxes/checkpoints.mdx` | None | **Adopt, reshaped**: filesystem + declared contents, portable |
| Restore in place | `docs/sandboxes/checkpoints.mdx` | None (`replace` is the nearest) | **Adopt as seeded replacement** |
| Fork (`oc checkpoint spawn`) | `docs/how-it-works.mdx` | None | **Adopt**: fork = new environment seeded from a checkpoint |
| Checkpoints outlive their sandbox | `docs/sandboxes/checkpoints.mdx` | — | **Adopt**: checkpoints are durable artifacts owned by the environment's owner |
| `kind: "full"` refused rather than downgraded | `docs/sandboxes/checkpoints.mdx` | The Compute principle "unsupported is explicit" | **Adopt the principle** |
| Elastic memory/CPU (`169.254.169.254/v1/scale`) | `docs/how-it-works.mdx` | `replace` only (`replace_computer`) | **Reshape**: optional `resize` capability; replacement is the portable fallback |
| Interactive PTY over WebSocket | `docs/sandboxes/interactive-terminals.mdx` | `terminal` capability + `connect` contract; no implementation | **Adopt via the existing contract** |
| Preview URLs (public by default; optional bearer token) | `docs/sandboxes/preview-urls.mdx` | `SessionEndpoint*`; ingress in `compute-network` | **Reshape**: endpoint state model; private/authenticated by default |
| Secrets: sealed placeholders + egress proxy | `docs/sandboxes/secrets.mdx` | Credentials by *name*; values never in receipts | **Reject the mechanism**; keep the boundary rule |
| Signed upload/download URLs | `docs/sandboxes/signed-urls.mdx` | Job artifacts route (`/compute/jobs/{id}/artifacts`) | Out of scope |
| Templates / snapshots as images | `docs/sandboxes/templates.mdx` | `DistributionIdentity`, dependency capsules, stacks | **Existing** in a stronger form (content-addressed, verified) |
| Burst sandboxes (disk survives infra restarts; processes may restart) | `docs/sandboxes/burst-sandboxes.mdx` | `unreachable`/`lost` reality, restart policy | **Existing semantics**; useful as a provider *class* description |
| 8-hour hard lifetime (v2) | `docs/sandboxes/lifetime.mdx` | None | **Adopt as a capability** (`max_lifetime`) |
| Webhooks for lifecycle events | `docs/sandboxes/webhooks.mdx` | Events (`compute-state` `Event`) | Out of scope for this design |
| Agent sessions, models, tools, memory, schedules | `docs/agent-sessions/`, `docs/agents/` | None, deliberately | **Reject for Compute**; consumers live above it |

**What OpenComputer itself demonstrates about portability.** Its docs describe
two backends with different capabilities. On the older runtime a checkpoint can
include memory; on the current one the platform "exposes no snapshot or memory-
export operation at all", so checkpoints are filesystem-only, `kind: "full"` is
refused, in-place scaling returns `501`, mounts are unavailable, fork-from-
checkpoint "fails on v2", and sandboxes have a hard 8-hour ceiling that
hibernation does not pause (`docs/sandboxes/checkpoints.mdx`, `elasticity.mdx`,
`lifetime.mdx`, `migrating-from-v1.mdx`). That is Compute's portability
problem, in miniature: the same API over backends that cannot promise the
same things. It is the strongest argument for Compute's rule that a capability
is explicit, per environment, and never implied by the product name.

**Uncertainties, documented rather than guessed.**

- `docs.opencomputer.dev` is blocked from this environment; the documentation
  studied is the docs source in the repository at commit `bd574a6`. The hosted
  docs may differ.
- The hosted platform is not open source; internals (worker scheduling,
  storage) were not inspected beyond what the repository contains.
- The repository documents a retired backend ("v1") and a current one ("v2").
  Statements about behaviour are attributed to the backend the page names.
- How OpenComputer's fork identity/lineage is stored is not documented.

## 4. Gap analysis

| Need | State | Evidence | Note |
| --- | --- | --- | --- |
| Persistent environment identity | **Existing** | `ComputerRecord`, `EnvironmentRecord`; `docs/computers.md` | |
| Persistent filesystem across stop/resume | **Existing, unverified by conformance** | providers keep the workspace (`sessions.rs:498-513`; container `stop`/`start`) | No test proves it for every provider; `persistent_storage` is `false` for both and means something else |
| Persistent process state | **Deliberately re-derived** | `stop_step` stops processes as durable jobs; contents reconcile after resume | Not a gap: a design choice |
| Resumable sessions | **Existing** | `SessionStatus::{Stopped, Resuming}`; `resume` capability | |
| Machine lifecycle independent of execution | **Existing** | `ComputerRecord` outlives every job | |
| Checkpoint / restore / fork | **Missing** | no source or docs | The main gap |
| Memory-inclusive suspend | **Missing; deliberately not portable** | — | See [persistent-environments.md](persistent-environments.md#why-not-memory) |
| Idle policy, wake on use | **Missing** | `docs/environment-control-plane.md`: only TTL / claim | Cost-saving, small |
| In-place resize | **Missing; replacement is explicit** | `replace_computer` (`daemon/computers.rs:1856`) | |
| PTY terminals | **Missing** | `terminal` capability set nowhere; `environment-control-plane.md` Limitations | Contract exists |
| Reachable endpoints for sessions | **Missing** | providers refuse `endpoints` | Applications' ingress exists |
| Lifetime ceilings disclosed | **Missing** | — | |
| Lifecycle *evidence* stronger than events | **Partial** | stop/resume/claim/destroy emit `SessionEvent` only | See §8 |

## 5. Proposed target architecture

OpenComputer's model is a flat stack:

```text
Agent → Sandbox → VM
```

Compute's model keeps a separation the OpenComputer model does not have:

```text
Application ─ requirements ─▶ Placement ─▶ Provider ─▶ Machine
Environment ─ desired contents ─▶ Computer (a session on a target)
Execution ─▶ Receipt ─▶ Evidence ─▶ Reality
```

The target model adds **state that can be captured and carried**, without
turning the environment into a VM:

```text
Environment ────────────── desired contents (declared, portable)
    │                             │
    │  checkpoint                 │ seeded from
    ▼                             ▼
Checkpoint  ── fork ──▶  Environment'  ── Computer' (placed anew)
(filesystem + contents            (lineage: parent, checkpoint)
 + provenance, content-addressed)
    ▲
    └── restore = replacement of the Computer, seeded from the checkpoint
```

Principles:

1. The **environment** is the durable logical thing; the **Computer** is its
   current machine; neither is a VM.
2. State is **declared contents + filesystem**. Process and memory state are
   re-derived from declaration (Compute's existing recovery model).
3. Every new operation has a **portable contract** and an **optional provider
   capability** for a faster native path; no provider feature is implied.
4. Every state change is **placed, authorized, fenced, and evidenced** by the
   existing chain (job → receipt → observed → reality).

## 6. New and extended primitives

Only primitives the audit justifies. Detail is in the companion documents.

| Primitive | New or extended | Owner layer | Identity | State | Lifecycle |
| --- | --- | --- | --- | --- | --- |
| **Checkpoint** | New | Environment (daemon; FeltDB authority) | `sha256:` of the archive (`compute.checkpoint@1`) plus a record id | immutable; `pending → ready → deleted` | captured by a job; verified; retained until deleted |
| **Lineage** | New fields on existing records | Environment / Computer | `parent_environment_id`, `checkpoint_id`, `checkpoint_digest` | immutable once set | set at fork/restore |
| **Seed** | Extension of *replacement* | Computer | the checkpoint digest | `seeding → verified` | part of provisioning a computer generation |
| **`resize` capability** | Extension of `SessionCapabilities` + `SessionProvider` | Provider | — | capability bool + verified limits | optional in-place change |
| **`max_lifetime` capability** | Extension of provider/target descriptor | Provider / placement | — | seconds, or none | disclosed; placement enforces |
| **Idle policy** | Extension of environment lifetime | Environment | — | `idle_timeout` | evaluated by the controller |
| **Endpoint state** | Extension of `SessionEndpoint` | Network layer | endpoint id | `unavailable / internal / published` | requested → observed |
| **Terminal** | No new object: implement `terminal` + `connect` | Provider + node route | connection id (short-lived) | connected/closed events | not persisted |

Explicitly **not** new: a "Sandbox" object, a "Snapshot" object separate from
Checkpoint, a "Machine" object, a hibernate state, an agent object.

## 7. Capability model

Summarized here; the full matrix and portability rules are in
[compute-capabilities.md](compute-capabilities.md). The rule: **Compute
promises a contract everywhere it can prove one, and merely *exposes* a
capability where a provider advertises it — and says `unsupported` otherwise.**

## 8. Receipt / evidence model

The existing chain is: durable job → `ExecutionReceipt` (hash-addressed,
independently verifiable) → `OperationEvidence` recorded per observed item →
`reality` (desired vs observed, with `unverified`/`unreachable`/`lost`).
Session lifecycle transitions today produce `SessionEvent`s (sequence,
generation) but **no receipt**: `stop`, `resume`, `claim`, `destroy` are
provider calls recorded as events. The design principle is *a claimed state is
not a verified state*, so a new lifecycle operation is evidenced by **the
receipt of the job that performed or verified it**, not by a second receipt
system.

| Operation | Receipt / evidence today | Proposed evidence | Placement | Runtime | Env identity | Lineage | Provider | Actual vs requested |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| create | Provisioning readiness job receipt; `SessionEvent`; computer `OperationEvidence` | unchanged | `ReceiptPlacement` ✓ | in receipt ✓ | `ReceiptScope` ✓ | — | `provider` ✓ | requested resources vs `provider_resources` ✓ |
| start / resume | event; provider answer advisory | **verification job** (workspace marker digest + process probe) with a receipt; result sets `observed` | ✓ | ✓ | ✓ | — | ✓ | observed by probe; else `unverified` |
| stop | `stop_step`: `STOP_PROCESS` job receipts; provider `stop` advisory | keep; add liveness confirmation (`inspect`) | — | — | ✓ | — | ✓ | `stopped` shown as *provider-reported* until confirmed |
| hibernate | n/a (not a separate operation) | n/a | — | — | — | — | — | — |
| checkpoint | none | **capture job receipt**: outputs record the archive digest (`OutputReceipt.sha256`); executable identity of the archiver is in the receipt | ✓ | ✓ | ✓ | new `lineage` block: environment, contents generation | ✓ | verified: archive re-hashed |
| restore | none | **seed job receipt** on the replacement computer + **tree-digest verification job** | ✓ (new machine) | ✓ | ✓ | `lineage`: checkpoint digest, prior computer | ✓ | verified: seeded tree digest == checkpoint tree digest |
| fork | none | same as restore, for a new environment | ✓ | ✓ | new environment id | `lineage`: parent environment + checkpoint | ✓ | verified as restore |
| resize | none | verification job reads applied limits; else `unverified` | ✓ | ✓ | ✓ | — | ✓ | requested vs observed limits; mismatch recorded, not hidden |
| execute | job receipt ✓ | job receipt ✓ + optional `lineage` block when the environment derives from a checkpoint | ✓ | ✓ | ✓ | ✓ (new) | ✓ | — |
| terminate | events; teardown retried until provider confirms | keep; `inspect` confirms `Missing` | — | — | ✓ | — | ✓ | `destroyed` only after provider confirms |

Status vocabulary for every new operation, reused from the environment
`reality`: **verified** (a Compute-run probe observed it), **provider-reported**
(the provider said so; shown as `unverified`), **unavailable** (could not be
checked), **unsupported** (the capability is absent; refused before anything
is sent). A receipt never records an operation whose verification *failed*;
the operation is recorded as failed.

Additive receipt change (later phase): an optional `lineage` block, exactly
like the optional `project`, `stack` and `app_bundle` blocks: absent means
byte-identical old receipts.

## 9. Security and isolation implications

Detail is in [checkpoint-fork-design.md](checkpoint-fork-design.md#security).
Summary:

- **A checkpoint is a copy of files.** It can contain secrets the workload
  wrote. Compute itself never writes credentials into a workspace (target
  credentials and connection material are never persisted; environment values
  are process-scoped), so what a checkpoint can leak is what the *workload*
  put on disk. Therefore: checkpoint and fork are separately authorized
  operations (`ProviderOperation` variants, owner-only), archives are owned
  artifacts, capture applies a declared, recorded exclusion list, and a fork
  is **never** given the source's credentials or connection material — it
  starts with only what its own requirements and the caller's explicit
  `--env` supply.
- **OpenComputer's secrets design is instructive, not adoptable.** Its sealed
  placeholders and egress proxy (`docs/sandboxes/secrets.mdx`) keep secret
  values out of the guest, at the cost of a root process *inside* the guest on
  its current backend — the page states a guest privilege escalation can reach
  that secret. Compute's boundary today is: values are supplied to a process
  and are not in receipts, stacks, or logs. A proxy that substitutes values is
  an agent-platform mechanism; it belongs above Compute if anywhere.
- **Isolation is unchanged.** Checkpoint capture reads through the same
  boundary a job has; restore and fork provision new computers under the same
  isolation profile and admission as any environment. Nothing weakens a
  profile to make a seed fit.
- **AuthBoundry is not required.** Authority for these operations is the
  existing operator/target authority. AuthBoundry may sit in front as an
  application-level authority boundary; Compute does not depend on it.

## 10. CLI / UX proposal

The CLI exposes the durable Compute model, never provider machinery.

| Command | Justified? | Meaning |
| --- | --- | --- |
| `compute up` | Exists | Control plane + this machine's host |
| `compute environment create/start/stop/replace/destroy` | Exist | Unchanged |
| `compute environment checkpoint create <env> [--name N]` / `list` / `inspect` / `verify` / `delete` | **Yes** | Durable, named checkpoints |
| `compute environment restore <env> <checkpoint>` | **Yes** | Seeded replacement |
| `compute environment fork <env> <new> [--checkpoint C]` | **Yes** | A new environment seeded from a checkpoint |
| `compute environment resize <env> --cpu N --memory M` | **Yes, later** | In place if the machine advertises `resize`; otherwise `operation_unsupported`, naming `replace` |
| `compute environment exec … [--resume]` | Yes, small | Start a stopped environment on demand |
| `compute environment lifetime <env> --idle 30m` | Yes, small | Idle policy (extends the existing `lifetime`) |
| `compute environment connect --terminal` | Yes, later | Uses the `terminal` capability |
| `compute environment endpoint add/list` | Yes, later | Endpoint state model |
| `compute session …` | Unchanged | The primitive an environment's machine is made of; **no checkpoint on raw sessions** |
| `compute hibernate`, `compute resume`, `compute checkpoint` (top level) | **No** | `stop`/`start` exist; checkpoints belong to an environment |

Workflow:

```text
compute up
   ▼ usable computer:   compute environment create work --persistent
   ▼ install / build:   … repo add · package install · exec
   ▼ checkpoint:        compute environment checkpoint create work --name built
   ▼ fork:              compute environment fork work try-a --checkpoint built   (× N)
   ▼ experiment:        compute environment exec try-a -- … ; test ; receipts
   ▼ promote:           publish the winning source as a Version and `compute promote`
                        (or `restore work` from try-a's checkpoint)
```

"Promote" deliberately uses the existing version/rollout machinery: what is
promoted to production is *source and declared build*, never a machine image.

## 11. Phased implementation plan

Small, independently shippable, ordered by capability unlocked per unit of
risk. Full detail (files, symbols, migrations, acceptance criteria) is in
[implementation-plan.md](implementation-plan.md).

| Phase | Goal | Unlocks |
| --- | --- | --- |
| **1** | `compute.checkpoint@1`: deterministic archive of a directory (library only) | Identity and verification for everything after |
| 2 | Capture as a receipted job; durable `Checkpoint` record; `checkpoint create/list/inspect/verify` | Named, verified checkpoints |
| 3 | Restore as seeded replacement | State-preserving replacement (also the portable resize path) |
| 4 | Fork with durable lineage | Parallel experiments, reproducible failures |
| 5 | Lifecycle evidence, idle policy, wake-on-use | Cost control; verified resume |
| 6 | `resize` capability | In-place resource change where a provider can |
| 7 | Terminals through `connect` | Interactive work |
| 8 | Endpoint state model | Reachable services, honestly |
| 9 | `max_lifetime` capability | Safe persistence on ceilinged providers |

## 12. Test and conformance strategy

A provider-neutral conformance suite for the *session/environment contract*
(the `SessionProvider` trait), run against the workspace provider, the
container provider (skipped where no engine exists), and the fake provider,
in the manner of `compute-state`'s `conformance.rs` and
`compute-runtime-conformance`. Its cases — persistence, checkpoint, fork,
receipt lineage, unsupported-is-explicit, provider-side failure — are
specified in [implementation-plan.md](implementation-plan.md#conformance).

## 13. Architecture invariants

Only invariants the resulting architecture supports:

1. **A logical environment is not a VM.** Its identity is a record; its machine
   can be replaced (`replace`) without changing it.
2. **Provider-specific capabilities never appear in a portable contract.** They
   appear as named capabilities a provider advertises, with an explicit
   `unsupported` otherwise.
3. **Requested state is never represented as verified state.** `reality`
   distinguishes desired, provider-reported, and verified.
4. **Every state-changing lifecycle operation has observable evidence:** a
   durable event and, for checkpoint/restore/fork/resize, the receipt of the
   job that performed or verified it.
5. **Lineage is durable and immutable.** A fork's parent and checkpoint digest
   cannot be rewritten.
6. **Authority and credentials do not propagate through checkpoints.** A fork
   receives no source credentials or connection material.
7. **Unsupported capabilities are explicit** and refused before anything is
   sent to a provider (as `operation_unsupported` already is).
8. **Agents, CI, services and humans are consumers of the same primitives.**
   Nothing agent-specific enters the portable layer.
9. **A checkpoint is a filesystem-and-contents artifact, never process or
   memory state.** A provider-native memory capture, if one ever exists, is a
   different, provider-scoped capability with a different name.
10. **A durable environment is independent of the machine hosting it** — the
    property `replace` already has, made state-preserving by seeded restore.

## 14. Explicit non-goals

- Adopting OpenComputer's API, SDK shape, or naming.
- VM management, VM snapshots, or memory-inclusive checkpoints as a portable
  promise.
- A "sandbox" as Compute's unit.
- Public-by-default endpoints, or any guarantee that a public URL exists.
- An in-guest secrets proxy or sealed-placeholder mechanism in Compute.
- Agent frameworks, models, tools, memory, schedules, or brain/hands
  separation in the portable layer.
- Making AuthBoundry mandatory.
- A second receipt system, a second durable store, or hidden persistent state.
- Checkpoints of raw `compute session`s (the environment is the durable unit).
- Implementing any of this in the change that introduces this document.
