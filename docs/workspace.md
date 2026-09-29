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
```

Paths are relative, `/`-separated, sorted bytewise. The digest is
`sha256:` of that text.

| | In the identity? |
| --- | --- |
| relative paths | yes |
| file contents | yes |
| empty directories | yes |
| non-empty directories | no (implied by their files) |
| executable bit | **yes**: the *owner* execute bit, `x` (workloads depend on it; git tracks it) |
| other permission bits | no |
| ownership | no |
| timestamps | no |
| extended attributes | no |
| symbolic links, hard-link entries | **unsupported: refused**, never approximated |
| devices, sockets, pipes | **unsupported: refused** |
| paths with control characters or backslashes | **unsupported: refused** |

Tar semantics are not inherited: the archive is only a transport, and a
receiver validates it against this contract (it accepts only regular files and
directories at safe relative paths) before anything is sent anywhere. The
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

**export** (`Execute`): one durable job on the computer's target.
It refuses unsupported entries, measures the workspace digest **before and
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

An archive travels through job output (export) and job environments (seed),
so the capability is **currently bounded by job/output transport**:
`WORKSPACE_ARCHIVE_LIMIT`, 8 MiB. Larger workspaces are refused with that
reason, never truncated. Measured on this development machine (debug build,
one workspace provider, an incompressible file):

| Archive | Export | Seed |
| ---: | ---: | ---: |
| 1 MiB | 0.4 s | 11 s |
| 4 MiB | 1.4 s | 41 s |
| 8 MiB | 2.5 s | 84 s |
| 12 MiB | 3.3 s | not completed (disk exhausted by the measurement itself) |

Export is bounded by the 16 MiB job output cap (base64 of ~12 MiB); seed is
slow, about 10 s per MiB, because each 96 KiB chunk is a durable job
environment. The bound is set where seed stays under about a minute and a half.
It is not optimized: the next workload that needs a bigger workspace should
force a streamed artifact capability, not a bigger constant.

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
workspace survive) and [`checkpoint`](checkpoint.md) (the same state, made
durable) exist.  `restore`, backup,
migration, and checkpoint (workspace + declared-state generation + provenance +
lineage + receipt) are each a composition of these three operations; none needs
new mechanism.
