# Lifecycle: ownership, stop, destroy, cancel

> **Compute implements lifecycle. Recipes describe lifecycle intent
> ([recipes.md](recipes.md)). Nothing else does.** A stop, a destroy, a
> cancel, a replace, and a fork are the operations the environment and
> computer already have. This page states what each guarantees, what it will
> not claim, and what depends on the provider.

```text
Recipe        lifecycle intent (data)
  ↓
Computer      persistent execution target: a machine on a target
  ↓
Environment   configured persistent environment: workspace, contents, configuration
  ↓
Process       ephemeral execution workload (a durable job, or a process the computer runs)
```

| | Is | Survives `stop` | Survives `destroy` |
| --- | --- | --- | --- |
| **Computer** | the machine, its target session | yes: the same machine resumes | no: the machine is removed; the record stays as evidence |
| **Environment** | declared contents, configuration, workspace | yes: workspace, configuration, installed artifacts, declarations | the record stays; the workspace goes with the machine |
| **Process** | a running workload and its children | **no**: ended, not suspended | no |

## The contract

| Operation | Guarantee | Reported only when |
| --- | --- | --- |
| **Stop** | Stops execution and preserves persistent state. No process the machine owns is left running; the workspace and configuration are kept; the same machine resumes. | every owned process is confirmed gone (`stopped`) |
| **Destroy** | Terminates every Compute-owned workload, then removes the machine. Nothing is removed while a workload still runs. | the target confirms the machine is gone (`destroyed`) |
| **Cancel** | Terminates the requested execution's process tree and records a terminal result. Idempotent: repeating it changes nothing. | the runtime reports the tree ended (`cancelled`, `cancellation.effective`) |
| **Replace** | New machine, same environment. State carries over by the [Replace contract](replace.md); running processes do not transfer. The old machine's processes are ended with the old machine. | the old machine is confirmed destroyed |
| **Fork** | An independent environment on its own machine. Its own processes, its own lifecycle: destroying either never touches the other. | (unchanged; verified by tests) |

**State reflects confirmation, never a request.** `destroy requested` is
`destroying`, not `destroyed`; `cancel requested` is a running job with
`cancellation.requested`, and `effective` becomes true only once the job is
recorded `cancelled`. A stale or interrupted record cannot turn either into
the terminal state: the transition is written after the provider confirms it.

### Distinguishing outcomes

A destroy that is not complete says why, in the computer's `failure`
(phase `teardown`; `compute environment destroy` exits non-zero and prints
it):

| Code | Meaning | Status |
| --- | --- | --- |
| *(none)* | the target confirmed the machine gone | `destroyed` |
| `termination_failed` | a process the machine owns is still alive (or an execution did not end) | stays `destroying` |
| `destruction_failed` | processes are gone, and the target could not remove the machine | stays `destroying` |
| `target_unavailable` | the target did not answer | stays `destroying` |

`termination_failed` on a stop (phase `stopping`) leaves the computer
`stopping`. All are retried; none is ever hidden or reported as success.
Execution outcomes are `completed`/`failed`/`cancelled`/`timed_out`
(`JobStatus`), distinct from provider or infrastructure errors.

## Process ownership

Compute knows which processes belong to which environment and machine
without a second table:

* **Ownership marker.** Every command Compute runs in a session carries
  `COMPUTE_SESSION_ID` and `COMPUTE_SESSION_WORKSPACE` (a caller cannot set
  either), and every descendant inherits them. A process belongs to the
  workspace it names, at any depth, whether or not it left its parent's
  process group or session. On Linux the kernel is the record
  (`/proc/<pid>/environ`); nothing is stored that could drift.
* **Environment and computer.** The computer's record holds each declared
  process (`observed.processes`: state, pid, evidence) under its
  environment; the machine is the workspace the marker names. Different
  environments have different workspaces, so their processes never share an
  owner, a fork or replacement never inherits pid files
  (`.compute/processes` is excluded from workspace transfer), and destroying
  one cannot touch another.
* **Job trees.** A command runs in its own process group; cancelling or
  timing it out kills the group.

## Process tree semantics

Terminating an environment terminates its tree: parent, child, grandchild,
and a descendant that started its own session. `terminate_owned`
(`compute-provider/src/processes.rs`) asks owned processes to exit (SIGTERM),
kills survivors (SIGKILL after a grace period), and **confirms by scanning
again** that none is alive; new processes started during teardown are found
by the next scan. If any survives the deadline, the operation fails
`termination_failed`, naming the pids. Destroy runs it before removing
anything.

Limit: a process that scrubbed its environment (`env -i`) *and* left its
process group cannot be found on a host without a cgroup or container to
enclose it. That is a property of the provider, declared below.

## Provider guarantees

`SessionCapabilities.process_tree_termination` says whether the provider
keeps the contract. Require it like any capability
(`--require process_tree_termination`, or in a recipe's
`requirements.capabilities`): a target without it is refused before anything
is acquired, with `session_capability_unsupported`.

| Provider | Tree termination | Stop / resume keep state | Confirmation |
| --- | --- | --- | --- |
| Workspace, Linux (`compute serve`, this machine) | **yes**: owner-marker scan + process groups | yes (the directory) | scan after signalling |
| Workspace, other Unix | **no**: `process_tree_termination = false`; no scan is possible, so nothing is claimed | yes | none |
| Container | yes: the engine ends the container's processes | yes (`stop`/`start`) | container state read back; **unverified against a real engine** |

Not provider-dependent: idempotent cancel, the terminal-state rules, the
`termination_failed`/`destruction_failed` distinction, and that a destroy is
never reported over a failed termination.

Bootstrap ([bootstrap.md](bootstrap.md)) is a lifecycle operation, and stop
and destroy never wait on it. While a bootstrap job (a repository sync, a
package install, a build) runs, the driver watches for the environment being
stopped or destroyed; when it is, the job is cancelled through the existing
confirmed cancellation, the target confirms every process of it gone (the
guarantees above), its claim is cleared so a start applies it again, and the
stop or destroy proceeds. Bootstrap work is never left running, and nothing
waits an hour for it.

## Persistent, ephemeral, and provenance

An environment is one durable object. What it *is* is written down; what it
is *doing* is not.

| Persistent (survives stop, replace, and a controller restart) | Ephemeral (never survives; re-derived or ended) |
| --- | --- |
| environment id, owner, name | processes and pids, running jobs |
| configured requirements, lifecycle, policy, declared contents | readiness and restart state, observed contents |
| the workspace (kept by stop; moved by replace/fork) | cancellation state, provider-local handles |
| recipe evidence `{name, version, digest}`, while true | the machine's session and connection |

**Recipe evidence** is provenance: which recipe version produced the
configuration. It is carried only while it is still true, by one rule:

> An operation keeps the recipe only if that version still resolves to the
> configuration now in force (only `target` may differ). Otherwise it
> releases it. The creation event keeps the history either way.

| Operation | Recipe | Why |
| --- | --- | --- |
| stop / start / controller restart | kept | same environment, same configuration |
| replace, same requirements | kept | same configuration on a new machine |
| replace, different requirements | **released**; `computer.replaced` names it (`recipe_released`) | a different configuration; the record must not claim otherwise |
| fork | kept if it still resolves to what is inherited; a new environment with its own identity | same configuration, new identity |
| restore | same rule, against the source's current configuration | derived through the same path as fork |
| destroy | the record stays as evidence | nothing is rewritten |

`compute environment inspect` shows the recipe (and the workload count) and
`GET /environments/{name}` returns it as `recipe`; there is no separate
provenance API. Tests: `crates/compute-environment/tests/provenance.rs`.

## Recovery

* A session's state is written before the provider acts. A `destroying`
  session, or a computer `destroying`, is resumed after any restart and
  repeats the (idempotent) terminate-then-remove; it is never marked
  destroyed by a restart.
* A running job interrupted by a restart is recorded `failed`
  (`provider_interrupted`), never `cancelled` or `succeeded`. Its process may
  still be alive; the session's stop or destroy ends it.
* Observed process state is applied to the record only after the target
  confirms the stop, so a controller that dies mid-stop cannot leave "stopped"
  over a running process.
* Node environments (bundle projects on the daemon host) keep their earlier
  model: their delete waits for each unit's task (30 s) and is not covered by
  the confirmation contract. That is the documented legacy path
  ([architecture.md](architecture.md)); new work belongs in computers.

## Audit: what was true before this change

| Question | Before |
| --- | --- |
| Destroy with a running process | Marked processes `stopped` in the record, destroyed the session, and the workspace provider only deleted the directory: detached processes (started with `setsid`) kept running |
| Stop | Ran a per-process stop script and ignored its outcome; the provider's stop did nothing for detached processes |
| Cancel | Killed the job's process group; but `effective` was set at request time, a repeat rewrote the record (even a finished job showed `requested`), and stopping or destroying a session did not wait for cancelled jobs |
| Ownership | Recorded per process (pid) on the computer; nothing tied a running descendant to its workspace |
| Children | The group was killed; a descendant in another session escaped |
| Persistence | Stop kept the workspace (already true; now tested) |
| Replace / fork | Independent by construction (pid files not transferred); untested for process trees |
| Recovery | Sessions and computers resumed `destroying`; a failed teardown was recorded as `target_unavailable` whatever the cause |

## A latent bug this work found: adopted sessions were swept

After a fork, restore, or replacement the machine's session keeps the
`reference` of the candidate that provisioned it, and the candidate's records
are deleted. The orphan sweep (every 60 s) looked the owner up by that
reference, found nothing, and destroyed the live machine's session as an
orphan: the environment became `lost` within a minute of being forked or
replaced. It went unseen because tests finish inside the first sweep interval.
The sweep now treats a session as owned when *any* live computer holds it
(`a_session_a_fork_or_replacement_took_over_is_never_swept_as_an_orphan`, with
the sweep interval configurable and set to 1 s in the test harness).

## What this does not change

No developer-environment subsystem, CI/deployment/agent lifecycle, scheduler,
process database, or provider abstraction was added. Foundry consumes the
same operations (computer → process → cancel/complete → destroy) and no
longer needs its own cleanup: destroy is confirmed by Compute. Foundry is not
in this repository, so its integration tests were not run here.
