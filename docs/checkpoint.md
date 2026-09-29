# Checkpoint: durable portable captured state

**Status:** capture implemented. `compute environment checkpoint NAME [--parent ID]`,
`compute environment checkpoints NAME [ID]`;
`POST /environments/{environment}/checkpoint`,
`GET /environments/{environment}/checkpoints[/{checkpoint}]`. Code:
`crates/compute-environment/src/checkpoint.rs` (the format),
`daemon/checkpoint.rs` (the operation). **Restore is the next consumer and is
not built here.**

> A Checkpoint is immutable portable captured state derived from a verified
> Environment state. It is not an Environment, Computer, Session, or authority
> boundary.

```text
Environment ─ verified workspace ─▶ Checkpoint ─ durable portable state ─▶ restore / fork (next)
```

It is not a machine snapshot, a VM image, a process or memory checkpoint, a
provider snapshot, or a copy of controller state. Nothing about the machine is
the portable abstraction.

## Three kinds of state

| Kind | Held as | Authority |
| --- | --- | --- |
| Declared | the Environment record (FeltDB) | the environment |
| Observed | Reality and evidence: the Computer record, jobs, receipts, events | what was proved |
| **Captured** | a Checkpoint record (FeltDB) + an immutable artifact | none |

The environment stays authoritative for declared state. The artifact holds no
declaration, and the record keeps the contents generation for provenance only.

## Ownership of the pieces

| Piece | Where | Why |
| --- | --- | --- |
| the **Checkpoint record** | FeltDB (`Checkpoint`, model generation 10) | it is semantic and authoritative |
| the **bytes** | the existing content-addressed artifact store, kind `checkpoint` | immutable, verified on every read; no new store |
| filesystem rules | the workspace module | one implementation |

There is no checkpoint database, cache, registry, or directory. No new storage
primitive was needed: the artifact store already provides durable,
content-addressed, digest-verified bytes.

## The artifact: `compute.checkpoint@1`

A canonical tar: `manifest.json`, then each file as `files/<path>`, sorted, with
zero timestamps and ownership and modes `0644`/`0755`. The manifest carries:

| Field | |
| --- | --- |
| `format`, `workspace_identity` | `compute.checkpoint@1`, `compute.workspace@1` |
| `tree_digest` | the workspace digest the files must reproduce |
| `source` | environment id, computer generation, contents generation, platform (`Linux-x86_64`) |
| `exclusions` | the paths not captured, and a note on what cannot be known |
| `parent` | lineage |
| `entries` | empty directories, then files: path, executable bit, size, SHA-256 |

**Identity.** `artifact_digest` is the SHA-256 of the bytes; `checkpoint_id` is
`ckp_` + that digest; `tree_digest` is the workspace digest. All derive from
content. Capture time, machine, session, provider, target, and temporary paths
are not inputs: the same state captured twice is the same bytes. Provenance
(environment id, generations, platform, parent) *is* content, so the same tree
from another environment, generation, platform, or parent is a different
artifact with the same `tree_digest`.

**Validation is total.** `validate` parses the artifact, reproduces the
workspace digest from its files, and requires the bytes to be exactly the
canonical rendering it would produce. Truncated, corrupted, reordered, extended,
or non-canonical artifacts are refused, as is one whose files do not reproduce
its digest.

## Filesystem safety

Unchanged and shared: the workspace module (`read_workspace`) is the authority,
and a checkpoint is built from an archive it validated; `validate` runs the
files back through it. Symbolic and hard links, devices and pipes, absolute or
escaping paths, control characters and backslashes, controller or re-derived
state, a path given twice, and a path that is both a file and a directory are
all refused, in the live workspace (`workspace_check`, tar link entries) and in
crafted archives (unit tests). The executable bit and empty directories are
preserved. A file/directory collision check was added to the shared reader.
Case-insensitive collisions are not part of the workspace contract.

## Capture

```text
authorize: the environment's owner                      before anything runs
export the workspace (a durable job; digest before and
  after; refused if it changed while captured)          workspace export
build the artifact; validate it (it must reproduce the digest)
store it; read it back, verified
PUBLISH: one write creates the record and its event     FeltDB
```

Evidence: the export is a durable job with a receipt (`capture_job_id`), recorded
as `workspace.export`; the record's creation and `checkpoint.captured` commit
together. A failure records `checkpoint.failed` with its phase and
`published: false`.

**Nothing is published until everything is verified.** The record is the last
write. A capture that fails or is interrupted earlier (an unsafe entry, a
workspace that changed, the artifact store down or returning bad bytes, the
state store refusing the record, a controller restart mid-capture) leaves no
record. An artifact already stored is content-addressed bytes no record names:
unreachable, not authoritative, and identical to what a retry stores. The
environment is never stopped, marked, or otherwise changed, and the next capture
is unaffected. A record never vouches for bytes that stop validating:
`checkpoints ID` reads and validates the artifact each time.

Capturing state that has already been captured returns the existing record
(`existing`), unchanged: a checkpoint is named by its content and is never
rewritten. Two captures of one environment differ only when the state or the
provenance does.

## Credentials

Compute writes no credential, token, session, connection, or endpoint material
into a workspace, so none is captured; configuration values are environment
record fields, not workspace files, and are not in the artifact (tested). Files
a workload writes into its own workspace are workspace content and are captured,
secrets included: Compute cannot recognise them, and does not claim to. The
exclusion policy and this statement are recorded in every manifest. Capture is
owner-authorized through the existing environment ownership model, and needs
`compute.operate`; reading needs `compute.read`. AuthBoundry is not involved.

## Lineage

`--parent` (or `CheckpointRequest.parent`) records the checkpoint this one was
derived from, which must be a checkpoint of the same environment. Records are
never mutated, so `C1` can have independent children `C2` and `C3`. Lineage is
provenance: a child inherits no credentials, sessions, execution, or provider
authority. Nothing sets a parent automatically until restore exists.

## Not captured

Memory, processes, PIDs, sockets, kernel or machine state, `repos/` (re-derived
from declared revisions), controller state, configuration values, sessions and
connections, provider identity. Not built: restore, retention and deletion,
quiescing, VM or provider snapshots, streaming (the 8 MiB workspace bound
applies to what can be captured), secret scanning, and any change to
[fork](fork.md), which keeps its direct export/seed path.

## Restore (next)

```text
Checkpoint ─▶ new Computer ─▶ seed (verified) ─▶ apply declared state ─▶ reconcile ─▶ verify
```

Not an in-place overwrite; a failed restore leaves the existing environment and
computer authoritative. For an existing environment it is
[replace](replace.md) seeded from the checkpoint (same environment, new
computer); for a new environment it is [fork](fork.md) seeded from it (new
environment, new computer). Both already exist over an archive; restore adds
reading the artifact back, which `Checkpoint::read_files` and the shared reader
already do.
