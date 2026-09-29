# Restore: a checkpoint becomes a new environment

**Status:** implemented. `compute environment restore CHECKPOINT NAME [--target T]`
(`POST /checkpoints/{checkpoint}/restore`); code in
`crates/compute-environment/src/daemon/restore.rs`, over the shared
composition in `daemon/fork.rs` (`Derivation`) and `daemon/candidate.rs`.
Restore is a consumer of a [checkpoint](checkpoint.md); it stores nothing.

> Restore creates fresh execution identity from immutable captured state. It
> does not restore machine identity, process identity, session identity,
> credentials, provider identity, or authority.

```text
Checkpoint C ─ validated ─▶ new Environment B ─ new Computer B ─ seed ─ verify ─ reconcile ─▶ Reality
```

| | Identity |
| --- | --- |
| [replace](replace.md) | same environment, new computer |
| [fork](fork.md) | new environment, new computer, from another environment's live workspace |
| **restore** | **new environment, new computer, from a durable checkpoint** |

Fork and restore are one composition with two inputs: fork exports a live
workspace, restore reads a checkpoint. Fork does not depend on checkpoints;
export is transient transfer, a checkpoint is durable captured state.

## The sequence

```text
authorize: the checkpoint's owner, who must also own the environment it was
  captured from (an unowned checkpoint is reported as not found)
resolve and VALIDATE, before anything is created:
  the record, its format, its artifact present; the artifact in canonical form,
  its digest equal to the record's, its checkpoint id equal to the record's,
  its workspace digest equal to the record's and to its own files
only then begin: refuse a name in use, clear an earlier attempt's candidate
prepare a candidate: seed the checkpoint's workspace, verified inside the new
  computer; apply the declared contents; reconcile until they are held
HANDOFF: one fenced transaction creates NAME and deletes the candidate
```

A corrupted, truncated, non-canonical, missing, substituted, or inconsistent
artifact fails at the first step and creates nothing, not even a candidate. A
failure after that stops the candidate (Reality says `stopped`), records the
phase and `workspace_verified: false` on the candidate, and leaves NAME free,
the checkpoint untouched, and the source's history unwritten. A retry clears
the leftover candidate. A controller that restarts mid-preparation finds no
environment under NAME; the checkpoint is intact and the restore can be
retried. This is the fork/replace recovery, not a new one.

## What is and is not restored

| Restored | Never restored |
| --- | --- |
| the workspace files, empty directories, and executable bits, digest-verified | machine, session, provider, target, or connection identity |
| the source's *declared* contents, policy, requirements, lifecycle kind, applied through the ordinary declaration path and reconciled | processes: pids, memory, sockets, terminals. A declared process runs because the new environment reconciled it, under a new pid |
| provenance: a `restore` event on the new environment | configuration values, credentials, tokens. The names and treatment the checkpoint recorded are reported as `configuration_required`; they are for the caller to supply with `compute environment config` |
| | endpoints, domains, receipts, the source's events |

**Declared state is the environment's, not the checkpoint's.** A checkpoint
records the contents generation it was captured under and nothing to apply.
Restore applies the source environment's *current* declarations and states the
difference: `declared_state` is `matches the state at capture` or `changed since
capture`, with both generations, in the report and the event. No second copy of
declarations is created, and none is treated as an authority. If the source
environment is gone, its declared state is gone and the restore is refused
rather than invented.

**Provenance.** The new environment's `restore` event names the checkpoint, its
artifact, the workspace digest, the source environment, and both generations.
This is the repository's existing convention for how an environment came to be
(as with fork); the checkpoint is provenance, never a parent authority, and no
new lineage mechanism was added.

**Immutability and reuse.** Restore reads the checkpoint and writes nothing to
it. Any number of restores of one checkpoint are independent environments with
their own ids, computers, and sessions and equal initial workspaces.

## Authorization

The existing ownership model: the operator must own the checkpoint (the
operator who captured it) and the environment it came from, and the route needs
`compute.operate`. Consuming a checkpoint transfers no authority. AuthBoundry is
not involved.

## Not built

Restore into an existing environment (compose checkpoint and [replace](replace.md)),
restore of a machine or process, lifecycle, terminals, endpoints, retention or
deletion, streaming (the 8 MiB workspace bound applies), and a separate way to
supply declarations.
