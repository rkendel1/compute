# Replace a computer, keep the environment

**Status:** implemented. `compute environment replace NAME [requirement flags]`
(`POST /environments/{environment}/replace`); code in
`crates/compute-environment/src/daemon/replace.rs`. It composes the
[workspace primitives](workspace.md); it adds no copy mechanism of its own.

> Environment identity survives computer replacement. Computer identity does not.

| | Survives replacement | Does not |
| --- | --- | --- |
| **Environment** | id, owner, declared contents, configuration, policy, lifecycle, requirements (moved to the new ones), the workspace | |
| **Computer** | | the machine: target, session, provider resource, connection, observed contents, generation |
| **Session** (the handle to the machine) | | the old one is retired and torn down; a connection to it does not survive. A new session is the new machine's |

## The sequence

```text
E ── A (current, untouched, still serving)
1  authorize (owner); the computer must be running
2  export A's workspace                                  workspace export
3  create a candidate for the new machine                (an ordinary environment)
4  seed the candidate; it verifies the digest inside     workspace seed
5  apply E's declared contents; the controller starts them; wait until held
6  re-measure A: it must still be the captured workspace workspace verify
7  HANDOFF, one fenced transaction:
     E's computer takes the candidate's machine, A's session is retired,
     E's requirements and generation move on, the candidate's records go
8  the controller tears down A's retired session
```

Nothing of E's changes before step 7.

## Why there is a candidate

A computer record binds exactly one machine, and the controller reconciles
the record's machine. The existing replacement therefore switched first: it
retired the old session and reset the record to `pending` before the new
machine existed, so there was no moment at which the old machine was still
current and the new one could be checked. The generic gap is **an atomic
handoff between two machines of one environment**.

The smallest way to close it without a new store is to let the controller
prepare the second machine as it prepares any machine, in a record of its own
(an environment whose name ends `--candidate`, reserved for this; see [fork.md](fork.md) for what a candidate is and is not), and to make
the handoff a single fenced transaction over four records: E's environment
(fenced, with its new requirements), E's computer (the machine binding
swapped), and the candidate's environment and computer (deleted). No field,
collection, or `MODEL_GENERATION` changed. What a candidate is: an ordinary
environment, with the ordinary lifecycle and Reality, that no operator can
create and that a successful replacement removes.

The larger alternative, a candidate *slot* inside the computer record with the
controller generalized to reconcile either machine, was not needed to prove
the invariant and would touch every controller step.

## Failure

Before the handoff, nothing of E's has changed: A is current, converged,
running the same processes, with the same workspace. The candidate is stopped
(its Reality says `stopped`, never running or ready), and the failure is
recorded on both E and the candidate with its phase and
`workspace_verified: false`. The error says the source is unchanged. Phases:
`provisioning`, `seeding` (an extraction or a digest mismatch inside the new
computer), `applying contents`, `reconciling` (the declared contents did not
come up within `replacement_deadline`), `verifying the source` (A's workspace
changed after it was captured), `handing off` (E changed underneath, or a
record moved). After the handoff there is nothing to undo: A is a retired
session. There is no rollback and no record kept beyond the environment,
computer, and event records that already exist.

A failed candidate stays until the next replacement of E, which clears it
first (destroys its machine, waits, deletes its records).

## Evidence

The handoff records `computer.replacing` and `computer.replaced` on E:
from and to session and target, the workspace digest, `workspace_verified`,
the new spec generation, and every job that did the work (export, upload,
extract, measure). The jobs' own receipts are the target's.

## Properties to know

- **The captured state is what moves.** The source is re-measured after the
  new machine has converged; if it changed since it was captured (a process
  wrote a file), the replacement is refused, not silently lossy. Quiescing the
  source is the caller's choice. A residual window of milliseconds remains
  between that measurement and the transaction.
- **Overlap.** While the candidate is prepared, A keeps running, and so do the
  candidate's copies of the declared processes once started. A process that
  cannot tolerate two live instances (a fixed port on a target whose machines
  share a network, single-writer state) fails its readiness on the candidate
  and the replacement fails before the handoff.
- **Not preserved:** memory, processes' state, connections, endpoints,
  terminals. Declared repositories are re-derived from their declared
  revision (`repos/` is not workspace state).
- **Bounded transport.** The workspace travels through job output and
  environments: `WORKSPACE_ARCHIVE_LIMIT` (8 MiB). A larger workspace makes
  the replacement fail at the export, before anything is created. This is a
  documented limit, not a reason to build streaming here.
- **A computer that cannot be exported from** (lost, unreachable, failed,
  stopped) is replaced the older way, from declared contents alone.
- **Not a deployment command.** There are no strategy, force, or zero-downtime
  flags; those are control-plane concerns above this operation.
