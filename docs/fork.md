# Fork an environment from portable state

**Status:** implemented. `compute environment fork SOURCE NAME [--target T] [--copy-config]`
(`POST /environments/{environment}/fork`); code in
`crates/compute-environment/src/daemon/fork.rs`. It composes the
[workspace primitives](workspace.md) and the [candidate](#the-candidate) (shared
with [restore](restore.md) as `Derivation`); it adds no copy mechanism. It replaces the earlier `clone`, which was the same
operation with weaker failure semantics; there is one operation, not two.

## The identity invariant

| | Environment | Computer |
| --- | --- | --- |
| [**replace**](replace.md) | same | new |
| **fork** | **new** | **new** |

> A new Environment identity can inherit portable workspace state without
> inheriting the source environment's authority or machine identity.

Fork is not "copy the environment". It creates a new environment **from
portable state produced by another environment**.

| Inherited (state) | Not inherited |
| --- | --- |
| workspace files, moved by export / seed / verify (`compute.workspace@1`) | environment id, computer id, machine, session, connection |
| declared contents (repositories, packages, processes), which the **new** controller reconciles, so a process runs in B because B started it, not because one was copied | running processes and their pids, readiness and restart state |
| policy (environment state in the model) | configuration values, where credentials live. `--copy-config` opts in, and the names left behind are reported |
| requirements and lifecycle kind | provider / target choice: placement decides unless the caller names a target |
| | `repos/` (re-derived from declared revisions), controller state, endpoints |
| | the source's events and receipts: the fork is recorded on the new environment, the failure on both, and nothing is written to the source on success |

Ownership: B is owned by the operator who forked, through the existing
environment ownership model. Only the source's owner may fork it, so the new
owner is that operator; no new authority object is introduced. Delegated
authority is something an application composes on top.

Fork is context-independent: nothing in it can be told whether B is for
development, CI, tests, staging, production, a demo, or an agent.

## The sequence

```text
fork SOURCE NAME
  refuse: a reserved name, a name in use, a non-owner        (nothing exported)
  export SOURCE's workspace                                   workspace export
  prepare a candidate: seed + verify inside it, apply SOURCE's
    declared contents, wait until they are held               candidate.rs
  HANDOFF, one fenced transaction: create NAME's environment and computer
    records on the candidate's machine, delete the candidate's
```

NAME does not exist until the workspace is verified and the contents are
reconciled. A failure at any earlier point never touches NAME.

## The candidate

A candidate is **not a Compute primitive**. It is a temporary composition of
existing Environment and Computer machinery, used to make a change atomic from
the environment's perspective: an ordinary environment, reconciled by the
ordinary controller, with the ordinary lifecycle and Reality, whose name ends
`--candidate` (which no operator can use). A composition prepares a machine in
it while its target environment does not yet exist (fork) or is untouched
(replace), then finishes with one fenced transaction over the records
involved, deleting the candidate's. Nothing of it outlives success; after a
failure only an inert, stopped candidate remains, and the next attempt at the
same name clears it. See `daemon/candidate.rs`.

## Failure

Before the handoff: NAME does not exist (no poisoned name), the source is
unchanged and not written to except for the recorded failure, and the
candidate is stopped (Reality says `stopped`, never running or ready). The
failure is recorded, with its phase and `workspace_verified: false`, on the
candidate and on the source. Phases: `provisioning`, `seeding` (an extraction
or a digest mismatch inside the new computer), `applying contents`,
`reconciling`, `handing off`. A source that changes while it is captured, an
unsupported workspace entry (symlink, special file), and an oversized archive
are refused at the export, before anything is created. There is no rollback.

## Not built

Streaming artifacts. [Checkpoint](checkpoint.md) and [restore](restore.md) makes the same
portable state durable; fork keeps the direct export/seed path, which stays
the fast path for a transient transfer. The 8 MiB `WORKSPACE_ARCHIVE_LIMIT` bounds the workspace
that can be forked; a larger one is refused at the export. That is a
documented limit, not a reason to build streaming here.
