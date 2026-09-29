# Persistent environments

**Status:** analysis of what exists, and the design decisions it supports.
Nothing here is implemented by the change that adds this document. See
[opencomputer-evaluation.md](opencomputer-evaluation.md) for the overall
evaluation.

> A Compute environment survives its executions, its machine's stops, and its
> controller's restarts. It does **not** survive by keeping a machine's memory:
> it survives because what it *is* is written down — declared contents, a
> workspace, and evidence — and because the machine is re-derived from that.

## What "persistent" means in Compute

Three things are easy to conflate:

| | Identity | Owner of truth | Sources |
| --- | --- | --- | --- |
| **Environment** | `environment_id`, name | FeltDB `EnvironmentRecord` (desired contents, `generation`) | `compute-state/src/model.rs:259`; `compute-core/src/computers.rs:564` |
| **Computer** | record derived from the environment (`cmp_…`) | FeltDB `ComputerRecord` (status, `generation`, `spec_generation`, target, session, `observed`) | `compute-state/src/model.rs:289` |
| **Session** (the machine's handle) | `session_id` (Compute's); `provider_session_id` is the provider's handle, never an identity | the target's session store | `compute-core/src/sessions.rs:377`; `compute-provider/src/sessions.rs:538` |

A persistent environment is a computer whose session is **claimed** (no TTL;
`compute environment create --persistent` requires the `claim` capability;
`compute environment lifetime --keep|--temporary` changes it in place). A
target that cannot claim cannot host a persistent environment, and placement
refuses it with a reason — nothing is written.

Machine lifecycle is independent of any execution: `ComputerStatus` is
`pending → provisioning → running ⇄ stopping/stopped/resuming`, with
`unreachable`, `lost`, `expired`, `failed`, `destroying`, `destroyed`
(`compute-core/src/computers.rs:51`). A job ending changes none of it.

## What survives what

| Event | Filesystem (workspace) | Declared contents | Processes | Evidence | Source |
| --- | --- | --- | --- | --- | --- |
| An execution ends | kept | kept | unaffected | receipt | jobs never own the workspace |
| `stop` then `resume` (same machine) | **kept** | kept | stopped by `STOP_PROCESS` jobs, then **re-started from declaration** after resume | `STOP_PROCESS` receipts; `computer.stopped` / `computer.resumed` events | `daemon/computers.rs:3930` (`stop_step`), `:3988` (`resume_step`) |
| Controller (daemon) restart | kept | kept (FeltDB) | left running; observed by the next probe; failures handled by restart policy | writes are fenced by record version; a recorded `starting` is resumed, never re-decided | `docs/computers.md` "Durability and fencing" |
| Target (`compute serve`) restart | kept, if the provider keeps it | kept (control plane) | provider-dependent | session store reconciles: in-flight transitions repeat; an environment the provider lost becomes `failed`/`environment_lost` | `docs/session-architecture.md` "Consistency" |
| Target unreachable | unknown | kept | unknown | computer `unreachable`, then `running` again when it answers | `docs/computers.md` "Observed reality" |
| Provider lost the machine | **gone** | kept | gone | computer `lost`; never re-provisioned silently | same |
| `replace` (new requirements) | **not carried**: a new workspace; contents re-converge (repos re-cloned, packages re-installed) | kept | restarted on the new machine | new session; old one `retired` after the new one runs | `daemon/computers.rs:1856` (`replace_computer`) |
| Destroy / expiry | removed | record kept | ended | the record and evidence remain | |

The last two rows of the table are the real persistence gap: **anything a
workload wrote outside declared contents is lost when a machine is lost or
replaced.** That, not "the VM went away", is the problem checkpoints solve
([checkpoint-fork-design.md](checkpoint-fork-design.md)).

## Classification (the brief's concepts)

| Concept | Classification | Reasoning |
| --- | --- | --- |
| Persistent filesystem across stop/resume | **Existing** (unverified by conformance) | Workspace provider keeps the directory (`sessions.rs:498-513`); container provider `stop`/`start` (`containers.rs:277-296`). No shared test proves it; both providers set `persistent_storage: false`, which means "storage that outlives the session", a different property |
| Persistent process state | **Deliberately out of scope** | Processes are declared (`ProcessSpec`) and re-derived with restart policy and readiness; memory is not captured |
| Persistent environment identity | **Existing** | FeltDB records; survives everything except destroy |
| Resumable sessions | **Existing** | `SessionStatus::{Stopped, Resuming}`; provider `resume` never substitutes a new machine |
| Machine lifecycle independent of execution | **Existing** | `ComputerRecord` |
| State that survives machine loss or replacement | **Missing** | [checkpoint-fork-design.md](checkpoint-fork-design.md) |

## Stop and resume today

`stop` is *not* hibernation. Precisely (all in code):

1. `stop_step` stops every observed process as a durable job (`STOP_PROCESS`,
   receipt in `OperationEvidence`), clears retry timers and readiness, then
   asks the target to stop the session.
2. The **workspace provider**'s `stop` validates the directory and does
   nothing else (`compute-provider/src/sessions.rs:498`); the **container
   provider** runs `docker stop`. Neither preserves a running process.
3. The capability that advertises this is named `suspend`
   (`SessionCapabilities.suspend`, `compute-core/src/sessions.rs:129`) and is
   `true` for both providers. Its documented meaning is only "the provider can
   suspend the environment on `stop`" (`docs/sessions.md`).
4. `resume_step` asks the target to resume; when the provider answers success
   the record becomes `running` and an event is written. **No probe
   verifies the resumed machine**; the answer is the provider's. The
   controller's normal reconcile then restarts declared processes.
5. If a provider cannot resume, `resume_step` records `resume_unsupported`,
   keeps the computer `stopped`, and creates nothing in its place.

Two consequences for the design:

- `suspend` is honest only for the state it keeps: **disk**. This document
  keeps the name and tightens the *documented* meaning; it does not add a
  "hibernated" state.
- Resume is *provider-reported*. Later phases verify it with a probe job
  ([implementation-plan.md](implementation-plan.md), Phase 5).

## Hibernate and resume

**Should Compute support `running → hibernated → resuming → running`?** It
already supports `running → stopped → resuming → running`. What differs from
OpenComputer's hibernate is *what is kept*: OpenComputer snapshots memory and
disk on its own hypervisor, and resumes "with a cold-boot fallback if snapshot
restore is unavailable" (`docs/how-it-works.mdx`, `docs/sandboxes/timeout.mdx`).

<a name="why-not-memory"></a>**Decision: no portable memory-inclusive
hibernate.**

- **State that survives:** filesystem and declared contents. Processes are
  re-derived. This is the same result OpenComputer's cold-boot fallback gives,
  and it is the only one that is identical on every provider.
- **Where state lives:** the workspace on the target; contents and evidence in
  FeltDB. Nothing new is stored.
- **Provider support required?** No, beyond the existing `resume` capability.
- **Belongs in the provider abstraction?** Only as a *different, optional,
  provider-scoped* capability if a provider ever offers memory suspend. A
  memory image is bound to the machine's architecture and kernel; it cannot
  move between providers, so it can never be a portable Compute promise. It
  would be advertised under its own name (never `suspend`) and never implied.
- **When a provider cannot support it:** `operation_unsupported`, refused
  before anything is sent — already the behaviour for `stop`/`resume`
  (`SessionProvider` defaults, `sessions.rs:350-362`).
- **What is added instead** (small, portable): an *idle policy* and
  *wake-on-use*. Today `running()` (`daemon/computers.rs:1931`) answers a
  command against a stopped computer with `conflict: … is stopped`. A policy
  `idle_timeout` on the environment's lifetime, evaluated by the controller
  from execution submission times that are already recorded
  (`SessionExecution.submitted_at`, `compute-core/src/sessions.rs:363`; connections
  record no time today, so idle would count executions only), plus
  `compute environment exec --resume`, gives OpenComputer's "laptop lid"
  behaviour with Compute's semantics. Every automatic stop or resume is an
  event with its cause.

## Long-running workloads

A workload may live for minutes, hours, days, or weeks. What Compute
**guarantees today**, and what it does not:

| Concern | Guarantee | Source |
| --- | --- | --- |
| Process persistence | A desired-running process is supervised: started with `setsid`, pid/log/exit recorded, probed every 15 s (1 s until ready), restarted per policy (`never`/`on_failure`/`always`, bounded, exponential backoff) | `docs/computers.md` "Processes"; `START_PROCESS`, `PROBE_PROCESSES` |
| Reconnect | Every command is a durable job with `job_id`/`execution_id`; status, logs, result and receipt are retrievable by identity after the client is gone | `docs/session-architecture.md` "Execution" |
| Hibernation | Not applicable (see above); stop/resume keeps disk | |
| Restart of the controller | Fenced writes; recorded `starting` is resumed without being re-decided; a running process is left alone | `docs/computers.md` |
| Restart of a job's provider | The job becomes `provider_interrupted`, **never a fabricated success**; the session returns to `ready` | `docs/sessions.md` "Durable execution semantics" |
| Failure | A machine that is gone is `lost`, an unreachable one `unreachable`; each is a durable transition with an event; work against them fails with a named reason | `docs/computers.md` |
| State recovery | Declared contents reconverge; **undeclared files survive only while the machine does** | this document |
| Receipts / evidence | Each start/stop/probe/sync/install carries its job, execution and receipt | `OperationEvidence` (`compute-core/src/computers.rs:747`) |

Not guaranteed: memory state; survival past a provider's own lifetime limit
(nothing discloses one yet — the `max_lifetime` capability in
[compute-capabilities.md](compute-capabilities.md#max-lifetime)); an interactive
terminal surviving a disconnect (there is no PTY today).

OpenComputer's v2 documentation shows why the last point matters: sandboxes
there "have a hard end time" of 8 hours that counts hibernated time, and the
prescribed design is *checkpoint the filesystem and recreate* or *externalise
the state* (`docs/sandboxes/lifetime.mdx`). Compute reaches the same place by
its own route: a persistent environment on a provider with a ceiling needs
either declared contents that reconverge or a checkpoint to seed the next
machine, and placement must know the ceiling exists.

This is independent of any AI agent framework: nothing above is agent-specific.
An agent, a CI job, a service, or a person is a consumer of the same
environment, process, job and receipt primitives.
