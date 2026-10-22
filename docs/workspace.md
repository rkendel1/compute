# Workspace state

**Status:** implemented. `compute environment workspace export | seed | verify`,
`POST /environments/{environment}/workspace/{export,seed,verify}`, in
`crates/compute-environment/src/daemon/workspace.rs`.

```text
Computer A ── export ──▶ portable workspace state (archive + digest)
                              ├── seed ──▶ Computer B ── verify ✓
                              ├── seed ──▶ Computer C ── verify ✓
                              └── verify anywhere against the digest
```

Workspace state is a Compute capability, not a feature of any one workflow.
[`fork`](fork.md) and [`replace`](replace.md) are composed from it; neither is
why it exists.

## Invariant: workspace state is context-independent

The primitives take an environment, an operator, and an archive or a digest.
They do not know, and cannot be told, whether the computer is for development,
a demo, tests, CI, staging, production, a customer, or an AI workload, or
whether it is local or remote. Those are compositions and policies above
Compute. The same export seeds a persistent computer and an ephemeral one with
different requirements (tested).

## What this is not

Not a checkpoint (no declared-state generation, provenance, lineage, or
lifecycle meaning), not a backup, not deployment, not a development
environment, not restore or fork. Those are consumers. Nothing is stored: no
record, no cache, no artifact. A workspace is described by a digest.

## The identity contract: `compute.workspace@1`

"The same workspace" means the same digest of this text:

```text
compute.workspace@1
dir <path>                    each EMPTY directory, sorted
file <x|-> <sha256> <path>    each regular file, sorted
link <n> <path> <target>      each symbolic link, sorted; n = the path's length in bytes
```

A workspace with no symbolic links has no `link` lines, so its digest is
unchanged from before links were supported.

Paths are relative, `/`-separated, sorted bytewise. The digest is
`sha256:` of that text.

| | In the identity? |
| --- | --- |
| relative paths | yes |
| file contents | yes |
| empty directories | yes |
| non-empty directories | no (implied by their files and links) |
| executable bit | **yes**: the *owner* execute bit, `x` (workloads depend on it; git tracks it) |
| other permission bits | no |
| ownership | no |
| timestamps | no |
| extended attributes | no |
| symbolic links | **yes**: path and target, as written (not resolved). Nothing is read or written *through* a link: a path under one is refused |
| hard links | as files: each name is a `file` line; a capture restores them as separate files |
| devices, sockets, pipes | **unsupported: refused** |
| paths or link targets with control characters or backslashes, link targets of 100 bytes or more | **unsupported: refused** |

Tar semantics are not inherited: the archive is only a transport, and a
receiver validates it against this contract (it accepts only regular files,
directories, and links at safe relative paths) before anything is sent anywhere. The
digest is computed twice, independently: inside a computer (shell, over the
live tree) and from an archive (Rust, without extracting), and the two agree.

### What is not in a workspace

* **`repos/`.** Declared repositories are declared state. A workspace never
  contains them; they are re-derived from their declared revision by the
  reconciler. (A caller who wants uncommitted repository changes carried has
  to ask for that deliberately; this capability does not.)
* **Controller state.** Everything under `.compute/` except
  `.compute/sources` (imported source repositories that declared contents
  refer to): process pid/log/exit files and in-flight imports.
* **Configuration values and credentials.** The primitives read files; they
  have no access to environment configuration, sessions, tokens, or controller
  records. A secret an application wrote into an ordinary workspace file *is*
  workspace state and travels with it.

## The three operations

**export** (`Execute`): durable jobs on the computer's target: one makes
the archive, then one per 6 MiB chunk reads it back. The first
refuses unsupported entries, measures the workspace digest **before and
after** making the archive and refuses if they differ (the workspace changed
while it was captured, whatever `tar` noticed), refuses an archive over the
transport bound, and returns the archive, its digest, and the workspace digest.
The daemon then checks the archive against the digest the computer printed for
it, validates it, and requires that it describe the very workspace the
computer measured.

**seed** (`Operate`): the archive is validated and its digest computed, and
compared with the caller's expected digest if given, *before* anything is
sent. A durable job extracts it into a workspace that must hold nothing (a
non-empty workspace is refused and left exactly as it was), and another job
measures the digest inside the computer, which must equal the archive's. A
failed extraction removes exactly what it wrote.

**verify** (`Execute`): measure the workspace digest and compare it with an
expected one. A mismatch is a result (`verified: false`), not an error, so a
caller can act on it. Without an expected digest it only measures.

Each is authorized to the environment's owner, and each is recorded as an
`environment.command` event with its jobs.

## Transport bound

An archive travels in chunks: export reads it back 6 MiB at a time through
job output, and seed sends it 8 MiB of base64 at a time on job standard
input, so no single job carries more than a chunk.
`WORKSPACE_ARCHIVE_LIMIT` (256 MiB) bounds the whole; a larger workspace is
refused with that reason, never truncated. Each chunk is an ordinary durable
job with its own evidence.

## Lifecycle rule for compositions

A composition that fails after it has created something must never present an
unverified environment as runnable. `abandon_composition` (used by `fork` and `replace`)
stops the candidate it was preparing, so Reality says `stopped`, and records
the failure with its phase and `workspace_verified: false`. The requested name
is never occupied by an unverified environment; the inert candidate stays until
the next attempt clears it. Nothing is rolled back.

## Consumers (not built here)

[`fork`](fork.md) (a new environment from portable state) and
[`replace`](replace.md) (a computer replaced while the environment and its
workspace survive), [`checkpoint`](checkpoint.md) (the same state, made
durable) and [`restore`](restore.md) (that state, back into a new
environment) exist.  `restore`, backup,
migration, and checkpoint (workspace + declared-state generation + provenance +
lineage + receipt) are each a composition of these three operations; none needs
new mechanism.
