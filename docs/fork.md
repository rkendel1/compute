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
| policy (environment state in the model) | configuration values, where credentials live ([configuration.md](configuration.md)). `--copy-config` opts in, and the names left behind are reported |
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

## Machine fork (`state: machine`)

`compute environment fork SOURCE NAME --machine`
(`POST /environments/{environment}/fork` with `"state": "machine"`) forks the
running machine itself: memory and disk, on the source's own target, so
processes continue in NAME from where they were. It is a different
operation from the workspace fork above, asked for by name. Neither is ever
substituted for the other.

```text
fork SOURCE NAME --machine
  refuse: a reserved name, a name in use, a non-owner, a computer that is not
    running, one whose backend lacks `fork`, a --target other than the
    source's                                                  (nothing created)
  candidate: an environment whose computer spec says "fork SOURCE's session"
    (ComputerSpec.fork); the controller's ordinary provisioning asks the
    target for one child of that session, named by the computer's reference,
    so a controller that restarts gets the same child back
  wait until it runs and its declared contents (SOURCE's) are held
  HANDOFF, as above
```

Same identity invariant: a new environment, a new computer and a new session,
owned by the operator who forked. What differs is what is inherited: the
machine's memory and disk instead of the portable workspace. The target's
checkpoint is recorded in the child's lineage and in the report. The backends
that can do it are listed in
[session-backends.md](session-backends.md#machine-fork).

### What a machine fork carries, and what it does not

| | Machine fork |
| --- | --- |
| **Configuration values** | Carried. The source's processes were started with them and hold them in memory. A source with configuration is therefore refused unless `--copy-config` is given. The fork's record then holds the same values, and nothing is reported as left behind when it is in memory. |
| **Files, including any secret a process wrote to disk** | Carried, as in the workspace fork. |
| **Egress credentials** (`compute egress`) | Never inside a machine: the host's gateway adds them on the way out. They aren't in the checkpoint, can't be revived from one, and a change at the gateway applies to source and fork alike. The gateway's rules are the node's. A fork can reach what its source could; there is no per-fork egress binding. |
| **Operator and target credentials** | Never inside a machine. The fork is authorized as a new environment owned by the operator who forked, and the target admits the child as a new session. |
| **The kernel's random state** | Reseeded in each child before it is used. |
| **User-space state that should be unique** (a process's own PRNG, in-memory TLS session keys, UUID generators seeded at start) | Duplicated, as in any memory fork. A workload that must not share these across forks has to re-derive them on resume. Compute cannot do this for it. |
| **Machine checkpoints** | Transient. Released once every child has started, so there is no old memory image to restore later. |

### When the controller stops mid-fork

A controller that stops after the candidate's computer was recorded leaves the
candidate. Its computer names the child by reference, so the next controller
gets the same child from the target and never a second one
(`a_machine_fork_interrupted_by_a_controller_restart_makes_one_child`). The
candidate keeps running until the fork is asked for again, which clears it
first, or until it is destroyed. The workspace fork, restore and replace
behave the same way after a crash. Only a failure they see themselves stops
their candidate.

## Not built

Streaming artifacts. [Checkpoint](checkpoint.md) and [restore](restore.md) makes the same
portable state durable; fork keeps the direct export/seed path, which stays
the fast path for a transient transfer. The 256 MiB `WORKSPACE_ARCHIVE_LIMIT` bounds the workspace
that can be forked (it travels in chunks); a larger one is refused at the export.
