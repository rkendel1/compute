# Factory: a thin GitHub control plane over Compute

**Status:** design and implementation plan. **Nothing in this document is
implemented, and this change implements neither Factory nor GitHub Actions
support.** Evidence: [local-ci-audit.md](local-ci-audit.md),
[github-runner-protocol.md](github-runner-protocol.md),
[factory-compute-gaps.md](factory-compute-gaps.md).

> Factory should make Compute look like the execution substrate GitHub
> expects. Compute should not become a GitHub Actions runner.

## 1. The two systems

| | Compute | Factory (as scoped here) |
| --- | --- | --- |
| Is | A portable execution fabric: placement, admission, sessions, environments, jobs, receipts | A control plane that speaks GitHub Actions semantics and asks Compute to execute |
| Owns | *Execution semantics*: where work may run, that it ran, what it consumed and produced, what evidence exists | *GitHub semantics*: repositories, workflows, runs, jobs, steps, runners, labels, dispatch, GitHub-shaped status, logs, artifacts |
| Durable state | FeltDB through `compute-state` (environments, computers, decisions, evidence); job/session stores on targets | FeltDB, in Factory's **own** collections; never Compute's |
| Never | Know what a workflow, a label or a check run is | Schedule onto machines, pick providers, define capabilities, run a process itself |

Two facts about the existing `rkendel1/factory` repository, read from a
clone, that this design has to reconcile rather than ignore:

- It is a Node/TypeScript "Software Factory Runner"/Actions Orchestrator for
  *operational* actions with its own `.flow` contracts, FeltDB state,
  AuthBoundry authority, and **its own native/PAX execution boundary**
  (`src/execution.ts`, `src/adapters.ts`, `src/workspace.ts`,
  `src/authority.ts`, `docs/real-execution.md`). That is an execution path
  parallel to Compute's, which contradicts "Factory is not an execution
  fabric".
- Its unit is called an *Action*, colliding with GitHub Actions vocabulary.

**Decision needed from the owner of Factory, not made here:** this design
treats the GitHub control plane as a new Factory *layer* (or module) whose
only execution path is Compute; the existing native execution path is out of
scope and would be retired behind the same Compute boundary in a separate
change. The naming collision should be resolved by calling the GitHub-shaped
objects `Workflow*`/`Job`/`Step` (as below) and leaving Factory's operational
"action" untouched.

## 2. Architecture

```
                        GitHub-shaped world                 |            Compute world
                                                            |
 webhooks / REST                                            |
 ┌───────────────┐   ┌─────────────────────────────────┐    |    ┌──────────────────────────────┐
 │ GitHub (or a  │──▶│ Factory                         │    |    │ Compute                      │
 │  user, or CLI)│   │  Repository, Workflow, Run,     │    |    │  Application, Environment,   │
 └───────────────┘   │  Job, Step, Runner, RunnerGroup │    |    │  Runtime, Workspace, Session,│
                     │  Label, JobDispatch, JobStatus  │    |    │  Execution, Process,         │
                     │                                 │    |    │  Placement, Capability,      │
 ┌───────────────┐   │  Runner-service emulation       │    |    │  Input, Output, Event,       │
 │ official      │◀─▶│  (registration, sessions, poll, │    |    │  Receipt                     │
 │ actions/runner│   │   timeline, logs, outputs,      │    |    │                              │
 │  (a workload) │   │   artifacts, cache)             │    |    │                              │
 └──────▲────────┘   │                                 │    |    │                              │
        │            │  label policy ─────────┐        │    |    │                              │
        │            └────────────────────────┼────────┘    |    └──────────────▲───────────────┘
        │                                     ▼             |                   │
        │                        ExecutionRequest ──────────────────────────────▶│  compute.remote@1
        │                        (placement requirements,    |                   │  (POST /compute/jobs,
        │                         inputs, env, network,      |                   │   sessions, cancel)
        │                         timeout, correlation id)   |                   │
        │                                                    |                   │
        │                        status / events / logs / artifacts / receipt ◀──┘
        └────────── runs inside the machine Compute chose ───|
```

Factory is the *only* thing that talks GitHub, and the *only* Compute client
in this picture. The runner is a workload running on a machine Compute
placed; it reaches **Factory** (not Compute) over the network as if Factory
were GitHub.

## 3. Responsibility matrix

Objects are grouped by owner. "Crosses" lists what is ever exchanged.

| Object | Owner | Crosses the boundary? | How |
| --- | --- | --- | --- |
| Repository | Factory | Only as a **reference** (`repo`, `sha`) inside an input | Compute never stores repository identity beyond declared environment repositories |
| Workflow, WorkflowRun | Factory | No | |
| Job (GitHub sense) | Factory | Only its `job_id` as a **correlation id** | Compute `request_id` / idempotency key (`compute-provider/src/lib.rs:1694`, `jobs.rs:237`) |
| Step | Factory (runner-reported) | No. Step records come from the runner's timeline callbacks | |
| Runner, RunnerGroup, RunnerLabel | Factory | No | Labels are translated *before* the boundary (§6) |
| JobDispatch | Factory | No | Answering a long poll is Factory-internal |
| JobStatus (GitHub) | Factory | No | Derived from Compute status + runner reports |
| GH logs / events / artifacts | Factory | No | Reconstructed (§8, §9) |
| Application | Compute | Factory names one per run **only if** it needs Compute's application identity | `ApplicationIdentity` (`compute-core/src/jobs.rs:20`) |
| Environment / Computer | Compute | Factory refers by id | `EnvironmentRecord`, `ComputerRecord` (`compute-state/src/model.rs:259,289`) |
| Runtime | Compute | Factory names a runtime requirement | `RuntimeKind::Dotnet`/`Native` (`compute-core/src/lib.rs:44`) |
| Workspace | Compute | Factory names a workspace, never mounts one | session/computer workspace |
| Execution / Job | Compute | Factory submits, cancels, reads | `ExecutionJob` (`jobs.rs:251`) |
| Session | Compute | Factory creates/holds/destroys through Compute authorization | `SessionManager` |
| Process | Compute | Factory declares the runner as a process | `ProcessSpec` |
| Placement, Capability | Compute | **Factory sends requirements, never a provider** | `PlacementRequirements` (`compute-placement/src/requirements.rs:120`) |
| Input / Output | Compute | Digest-addressed | `DependencyCapsule`, `JobArtifact` |
| Event | Compute | Compute → Factory | `JobEvent`, `SessionEvent`, environment event bus |
| Receipt | Compute | Compute → Factory | `ExecutionReceipt` |

**Every object crossing the boundary, exhaustively:**

Factory → Compute:

1. *ExecutionRequest*: placement requirements (translated from labels), the
   runner distribution as a digest-pinned input, argv/env, network policy,
   timeout, a correlation id, and a delegation grant id (§10).
2. *Cancel(job)*.
3. *Session lifecycle*: create/hold/release/destroy, for hold-on-failure (§7).
4. *Reads*: job status, events, logs, artifacts, receipt by identity.

Compute → Factory:

5. *Job status* (`JobStatus`, terminal states, `failure` reason).
6. *Job events* (sequence-numbered).
7. *Logs* (see gap G3: today only whole-at-completion).
8. *Artifacts* (digest, size, bytes).
9. *Receipt* (compute.receipt@1).
10. *Refusal reasons* (`ReasonCode` from placement, `operation_unsupported`).

Nothing else crosses. In particular: no label, workflow, run id, or check
run id is stored *in* Compute other than as an opaque correlation string
Factory chooses (§6.4).

## 4. Local CI as evidence, and the three models

What Local CI shows ([audit](local-ci-audit.md)): the unmodified runner is a
step interpreter that needs a resolved job message, a server that speaks the
distributed-task API, a workspace, and network. It shows nothing about
placement, durability, or evidence, because it has none.

### Model A: Factory-owned runner

Factory implements the step interpreter itself and executes steps.

- **Pro:** no dependence on a Microsoft protocol; full control of streaming.
- **Con:** Factory re-implements GitHub Actions step semantics (expressions,
  `uses:` resolution, composite/docker/JS actions, masking, `GITHUB_*` files,
  post-steps). That is the huge surface Local CI avoids by *not* doing it. It
  also makes Factory an execution engine, exactly what this design forbids,
  and puts step execution outside Compute's receipts unless Factory calls
  Compute once per step (thousands of durable jobs).
- **Verdict: reject.**

### Model B: real GitHub runner, Factory as control plane (recommended)

The official runner runs as a **workload** that Compute places and executes.
Factory serves the runner-facing protocol. This is Local CI's architecture
with the DTU's in-memory maps replaced by durable Factory state and Docker
replaced by Compute.

- **Pro:** exact step semantics for free; Compute stays generic (it runs a
  .NET process with env vars); the Local CI project is direct evidence it
  works.
- **Con (honest):** Compute's evidence covers the **runner process** (one
  receipt: where, when, exit status, inputs, resources). *Per-step* results
  are **runner-reported** through Factory's endpoints. Compute does not
  attest steps. The design must say so wherever step results are shown
  (§8). Also carries the protocol version risk in
  [github-runner-protocol.md](github-runner-protocol.md#evidence-quality).
- **Verdict: adopt.**

### Model C: Compute-native runner

Compute learns Actions: it parses workflows, resolves actions, maintains
steps.

- **Con:** puts GitHub semantics into Compute; a second workflow engine; the
  exact inversion of the guiding principle.
- **Verdict: reject.**

| | A | **B** | C |
| --- | --- | --- | --- |
| GitHub semantics in Compute | no | no | **yes** |
| Execution semantics in Factory | **yes** | no | no |
| Step fidelity | must be built | exact | must be built |
| Compute evidence granularity | per step if forced | per runner process | per step |
| New Compute primitives needed | many | few (§12) | many |
| Real-world proof | none | Local CI | none |

## 5. The "real runner" strategy in detail

Details, with what is proven vs. not. Protocol rows are in
[github-runner-protocol.md](github-runner-protocol.md).

1. **One runner = one ephemeral process = one Compute job**, per GitHub job.
   Mirrors Local CI (`run.sh --once`, `"ephemeral":true`). No long-lived runner
   fleet in v1; that removes runner GC, upgrade, and stale-registration
   problems and keeps "runner" a *Factory record*, not a Compute concept.
2. **Where the runner binary comes from:** a digest-pinned input (an
   artifact or `DependencyCapsule`-style input) of the official release, with
   `RuntimeKind::Native` or `Dotnet` and a `platform` requirement. Whether the
   capsule model fits a self-contained .NET tree is **unverified**; capsule
   creation currently refuses symlinks (see `docs/dependencies.md`) and the
   runner tarball's contents and size were not inspected, and a capsule
   travels inside a request capped at 64 MiB (`docs/stacks.md`), which a runner
   distribution may exceed. Gap G6.
3. **Registration and auth flow:** Factory mints per-job credentials on
   dispatch (registration token, runner credentials, runtime token), scoped to
   one job and expiring with it, and passes them as env/files in the
   ExecutionRequest. Whether the runner's just-in-time config mode can carry
   this against a non-GitHub server URL is **unverified**; Local CI pre-writes
   `.runner`/`.credentials` instead (`container-config.ts:182`) and the PoC
   would do the same.
4. **Polling and dispatch:** the runner long-polls Factory; Factory answers
   with the job message it has already built, keyed by the runner name it
   assigned (Local CI pins by name, `local-job.ts:747`). Dispatch is
   Factory-internal. Compute is not polled.
5. **Status callbacks:** lock renewal, timelines, logs, outputs, finish arrive
   at Factory; Factory persists them (§8).
6. **Log and artifact transport:** the runner uploads to **Factory**; Factory
   stores what it must in Compute's artifact path (§9). Compute never speaks
   the GitHub wire formats.
7. **Cancellation:** Factory cancels the Compute job
   (`POST /compute/jobs/{id}/cancel`); if the runner has time, Factory *first*
   answers the runner's next lock renewal with a cancel signal (protocol row
   7). Which cancel form a given runner honors is unverified.
8. **Labels:** a runner registers with labels; Factory generates them from
   the placement it received (§6.3). The runner never chooses its own.
9. **Env vars and filesystem:** `GITHUB_*` are constructed by Factory into the
   job message (`generators.ts:260` shows what Local CI supplies). The
   workspace is a Compute workspace; the repository at `sha` is a declared
   input (repository at commit is already an environment content type).
10. **Service/container expectations:** `docker.sock`, `container:` and
    `services:` are the hard part. Compute does not provide docker-in-docker
    and this design does not add it; workflows that need it are **refused with
    a named reason** in v1 (the same way Local CI refuses macOS/Windows,
    `runs-on-compat.ts:62-85`).
11. **Network:** the runner must reach Factory. That is a *declared*
    requirement (Factory's endpoint) in the ExecutionRequest's network
    policy, not an ambient assumption (Compute has `NetworkPolicy`,
    `jobs.rs:227`; a per-destination allow is gap G7).

**GitHub-specific emulation that stays in Factory:** everything in the
"GitHub-specific emulation" section of the protocol map.

## 6. `runs-on` → Compute placement

### 6.1 Options

| Option | Meaning | Verdict |
| --- | --- | --- |
| Labels opaque | Factory keeps `runs-on` strings, matches them to pre-provisioned runners | Rejected as *the* mechanism: recreates a second scheduler/capability model |
| Labels = capabilities | A label is a Compute capability name | Rejected: Compute capabilities are a closed set that *errors on unknown names* (`SessionCapabilities::validate_names`, `docs/compute-capabilities.md` rule 2); GitHub labels are open-ended (`self-hosted`, `gpu-large`, any team string) |
| **Translation** | Factory owns a small, declarative **label policy** that maps a label set to `PlacementRequirements`; Compute matches those exactly as it matches any workload | **Adopt** |

### 6.2 What the translation can target (all existing fields)

| GitHub label | Compute field (`PlacementRequirements`, `requirements.rs:120`) |
| --- | --- |
| `ubuntu-*`, `linux` | `platform` (`PlatformIdentity` OS) |
| `x64`, `arm64` | `architecture` |
| `macos-*`, `windows-*` | `platform`; refused with reason if no target matches (as Local CI skips them) |
| `self-hosted` | no requirement; means "not Factory-default pool" — Factory policy, not Compute |
| `gpu`, `kvm`, `firecracker` | `target_features` (closed list `TARGET_FEATURES`) |
| size labels (`4-core`) | `resources` |
| unknown label | **refused with `unknown_label`** unless the label policy maps it |

### 6.3 Rules

1. The policy is **declarative repository data**, versioned and hashed the
   way stacks are (`docs/stacks.md`), not hard-coded. It lives with Factory.
2. It can only emit fields Compute already defines. If a label cannot be
   expressed, that is a Compute gap to raise, never a Factory-side capability.
3. Runner labels shown to GitHub are **generated from the placement result**
   (what was matched), so a runner cannot claim what Compute did not verify.
4. The correlation id (GitHub job id) is an opaque string; Compute never
   interprets it.
5. Placement refusals (`ReasonCode`) surface as a queued/never-started job
   with the reason, not as a silent wait.

Translation is lossy by design: GitHub cannot ask for anything Compute cannot
place. That is the intended direction of the loss.

## 7. Failure and resumability

Question: after a job fails, is the environment inspectable and can the step
be retried, or is it destroyed?

What Compute has: a session that outlives an execution (jobs never own the
workspace; `docs/persistent-environments.md`), `stop`/`resume` keeping disk,
`claim` to make a session non-expiring, ephemeral sessions with a TTL. What
it lacks: any policy that says "keep this machine because the job failed".

**Minimum Compute primitive: none new for v1.** Factory holds the failed job's
session by *claiming it with a Factory-chosen expiry* (an existing
capability, `claim`, `sessions.rs:129`) instead of destroying it, and
releases it later. Consequences:

- Inspectability is delivered by existing session `exec`/`logs`
  (`docs/sessions.md`); authorization is Compute's per operation.
- **Step retry in place** (Local CI's pause/retry) needs the *runner process
  to remain alive* with its state. Compute does not snapshot processes and this
  design assumes no VM snapshots. So in-place retry of a *step inside the same
  runner process* is **not** offered. What is offered: **job retry on the same
  workspace** — Factory dispatches a fresh ephemeral runner into the *held*
  session so the workspace survives. That covers "fix the file and rerun"
  without any new primitive, at the cost of re-running earlier steps.
- Resource cost of holding is real; hold has a Factory policy timeout and
  the held session is visible as `claim`ed in `reality`.
- If the provider lacks `claim`, hold is refused with `operation_unsupported`
  and the session is destroyed after failure. Says so; nothing degrades
  silently.

If later evidence shows hold-on-failure needs Compute to *refuse* TTL expiry
on a failed execution, that is one small, generic addition ("do not expire on
non-zero exit") and is recorded as gap G5, not assumed.

## 8. Events, logs, receipts

Rule: **Compute is authoritative for execution evidence; the GitHub view is
reconstructible.**

| GitHub-visible thing | Source of truth | Reconstruction |
| --- | --- | --- |
| "Runner process started/finished, exit code, machine, timing, inputs" | Compute `ExecutionReceipt` + `JobEvent`s | Factory reads by job id |
| Job `conclusion` | Compute terminal `JobStatus` **and** runner-reported result | Factory maps; a runner-reported `success` with a Compute `failed`/`timed_out` is shown as failed |
| Step records (timeline) | Runner-reported, persisted by Factory as `reported` | Not reconstructible from Compute; Factory's records are the *only* source, labelled as runner-reported |
| Log text | Runner-reported lines persisted by Factory; Compute's job logs are the process's stdout/stderr | Two different things; Compute logs are the fallback if Factory lost the lines |
| Cancellation | Compute `JobCancellation {requested, effective, phase}` | Direct |

Factory's job-status state machine derives from Compute's `JobStatus`
(`created … running … succeeded|failed|cancelled|timed_out|rejected`,
`jobs.rs:191`) plus its own pre-Compute states (`queued-for-label-policy`,
`waiting-for-runner-poll`). A GitHub "completed" is emitted only after
Compute reports a terminal status, so Factory cannot claim completion of
something Compute has not finished.

Consequence: Compute receipts do **not** contain steps, and this design does
not add them (Model C would). If per-step attestation is ever required, the
mechanism is the runner-reported timeline sealed into an artifact whose digest
is referenced from Factory's record — never a Compute concept.

## 9. Artifacts and caches

- **Artifacts:** the runner uploads via the GitHub artifact wire protocol to
  Factory. Factory names, scopes, retains, and lists them (all GitHub
  semantics) and stores the **bytes** as Compute job artifacts/`ArtifactStore`
  objects addressed by digest. Gap: Compute job artifacts are returned inline as bytes in one JSON
  response (`JobArtifact.data`, `jobs.rs:310`), not streamed; request bodies,
  which is how inputs such as capsules travel, are capped at 64 MiB
  (`DEFAULT_MAX_REQUEST_BYTES`, `compute-provider/src/lib.rs:55`). Whether a
  response cap exists was not checked. See G4.
- **Actions cache:** key/restore-key semantics are GitHub's, so the key
  namespace is Factory's. **No GitHub-specific persistent storage is added to
  Compute.** v1 either omits cache (steps that use `actions/cache` simply miss,
  which is correct behaviour) or stores cache blobs as artifacts. Compute's
  content-addressed `DependencyCapsule` is a better cache for what people cache
  most (dependencies) and is used via *project requirements* (`docs/pax.md`),
  not via the cache protocol. Only if measurements show that cache misses make
  real workflows unusable is a byte store beyond artifacts justified.
- **Action tarballs (`uses:`)** are fetched from github.com through Factory
  (as Local CI's proxy does, `index.ts:881-923`). The `resolvedSha` must be the
  real one, unlike Local CI's hash.

## 10. Security and authority

1. Factory holds **no ambient authority** over Compute. Every ExecutionRequest
   carries an explicit **delegation**: which repository/workflow/run, which
   Compute operations (`ProviderOperation` set), which placement ceilings
   (resources, network, isolation), expiry. Compute authorizes each operation
   against that grant as it does any caller
   (`docs/product-surface.md` route table; `ProviderOperation`).
2. Factory may *strengthen* isolation, never weaken it (the same rule
   `RequirementOptions.isolation` already has, `requirements.rs:154`).
3. Runner credentials are per-job, short-lived, minted by Factory, never
   written to receipts, events, logs, or Factory's persisted evidence; only
   their *names* appear (same rule as stack credentials, `docs/stacks.md`).
4. The runner is untrusted code on a machine Compute controls: it gets only
   the network destinations the ExecutionRequest allows (Factory's endpoint
   plus what the workflow's policy permits).
5. Fork PRs / untrusted workflows map to a **stronger isolation profile**
   through the label policy, never a weaker one.
6. Factory's own authority (AuthBoundry, in the existing repository) decides
   *who may cause a run*; Compute's authorization decides *what the run may do
   to Compute*. They are separate checks.

## 11. Compute gap analysis (summary)

Full list, with the code each cites, in
[factory-compute-gaps.md](factory-compute-gaps.md). Headlines:

| ID | Gap | Needed for | New generic Compute primitive? |
| --- | --- | --- | --- |
| G1 | No per-job clean workspace identity | Isolation between jobs on a held session | small |
| G2 | Session exec has no cwd, stdin, or streaming (`SessionCommand`, `sessions.rs:491`) | Running the runner in a chosen dir | small |
| G3 | Job logs only whole-at-completion (`jobs.rs:339`) | Live GitHub logs | followable log (generic) |
| G4 | Artifacts inline and size-capped (`jobs.rs:310`) | Real artifact sizes | streamed artifact (generic) |
| G5 | No hold-on-failure policy | Failure inspection | none for v1 (§7) |
| G6 | Runner distribution as a pinned input | Runner binary | unverified fit |
| G7 | No per-destination network allow | Runner → Factory only | maybe |
| G8 | Job events carry type+time only (`provider/src/jobs.rs:63`) | Richer Factory view | optional |

None of these is GitHub-specific. That is the acceptance test for each.

## 12. Implementation sequence

Ordered so that each step is independently reviewable and Compute changes
come first and are justified without reference to GitHub.

1. **Compute (generic):** G2 (cwd and env on session exec), G3 (followable
   job logs), G1 (workspace identity on a job). Each with conformance tests in
   `docs/`'s existing conformance style. No GitHub words in code or docs.
2. **Factory core (no runner yet):** FeltDB collections for Repository,
   Workflow, WorkflowRun, Job, Step, Runner, JobDispatch; a Compute client
   using `compute.remote@1`; job-status derivation from Compute status. Test:
   a "shell job" (no runner) runs via Compute and a run completes with a
   receipt.
3. **Label policy:** declarative file + translator to
   `PlacementRequirements`, with refusal reasons. Test against existing
   placement tests' style (`compute-placement/tests`).
4. **GitHub runner-service emulation, minimum:** registration, session, long
   poll, job message, lock renewal, timeline, logs, outputs, finish (rows 1–10
   of the protocol map). Port from Local CI's behaviour, with real tokens.
5. **Persistence and restart:** every state above in FeltDB; kill Factory
   mid-job and resume from durable state (Local CI cannot).
6. **Security:** delegation grants, per-job credentials, isolation mapping.
7. **Observability:** GitHub view reconstructed from receipts; consistency
   checker (Compute terminal vs runner-reported).
8. **Artifacts (G4) and cache decision**, then `uses:` actions.
9. **Integration validation:** PoC below, then a suite of real workflows.

## 13. Proof of concept

Smallest thing that proves the architecture:

```
workflow (one job, runs-on: ubuntu-latest, steps: [run: echo hello])
  → Factory: label policy → PlacementRequirements(platform=linux/x64)
  → Compute: POST /compute/jobs (runner as a pinned input, ephemeral)
  → real actions/runner (pinned version) registers with Factory,
    long-polls, receives the job message, executes `echo hello`,
    reports timeline/logs/finish to Factory
  → Compute: job succeeded, ExecutionReceipt sealed
  → Factory: GitHub-shaped run = success, derived from BOTH the receipt
    and the runner's report; logs contain "hello"
```

Pass criteria (each testable): the process executed **under Compute**
(receipt has placement and provider); Factory contains no execution code
path; killing the runner mid-job yields `failed` in both views; killing
Factory and restarting resumes the run; the receipt contains no credentials.
The workflow uses only `run:` (no `uses:`, no artifacts, no cache, no
containers) and a pre-seeded workspace, which removes rows 11–14 of the
protocol map.

## 14. Non-goals

- Implementing Factory or GitHub Actions support (this change).
- Replacing Compute's runtime, session, environment, or receipt model.
- A second scheduler or capability model; labels never bypass placement.
- A Factory VM/machine abstraction.
- Forking, patching, or rewriting the official runner.
- GitHub-specific storage, tables, states, or routes in Compute.
- Speculative snapshot infrastructure; process/VM checkpoints.
- docker-in-docker and `docker.sock` passthrough.
- Windows/macOS jobs (refused with a reason).

## 15. The twelve questions

| # | Question | Answer |
| --- | --- | --- |
| 1 | What is Factory? | The GitHub-semantics control plane: it decides *what* to run for a workflow and *speaks GitHub*; it does not execute. |
| 2 | What is Compute? | The portable execution fabric that decides *where*, runs it, and produces evidence. |
| 3 | What does Local CI teach? | An unmodified runner works against an emulated server; job description and step interpretation are separable; pause-and-retry is the valued workflow. It also shows the missing durability, identity, and evidence. |
| 4 | Which primitives does Compute adopt? | None GitHub-specific. Generic ones from the gaps: cwd/env on exec, followable logs, workspace identity, later streamed artifacts. |
| 5 | Which are Factory-only? | Runner, RunnerGroup, labels, dispatch, workflow/run/job/step, GitHub status/logs/artifacts/cache namespaces, runner-protocol emulation. |
| 6 | Can the real runner execute against Factory? | Local CI proves it can against an emulation of the older protocol; whether the *current* release accepts that flow is unverified, hence the pinned version and PoC. |
| 7 | Required Compute APIs? | Existing: submit, status, cancel, events, logs, artifacts, receipt, session create/claim/exec/destroy. New: G1–G4. |
| 8 | Durable state? | Compute: FeltDB via `compute-state`. Factory: its own FeltDB collections. Nothing in memory that cannot be rebuilt. |
| 9 | After failure? | Session is held by `claim` with an expiry; inspectable through session exec; job retry into the same workspace. In-place step retry is not offered. |
| 10 | Where is execution evidence authoritative? | Compute receipts and events. Step records are runner-reported and labelled so. |
| 11 | How do labels become placement? | Factory's declarative label policy emits `PlacementRequirements`; unknown labels are refused. |
| 12 | Smallest proving implementation? | §13. |
