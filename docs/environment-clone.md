# Environment clone

**Status:** implemented (`compute environment clone SOURCE NAME`,
`POST /environments/{environment}/clone`). This is the first OpenComputer-
inspired composition, built to find out what Compute already has and what it
lacks before building a checkpoint subsystem. It is a proof, not the
checkpoint primitive: no archive is stored, and there is no restore, fork, or
lineage.

```text
compute environment clone origin copy
   export   a durable job in origin's computer tars its workspace
   verify   the daemon checks the archive against the digest the computer printed,
            validates every entry, and computes the tree digest
   create   a new environment: same requirements, same policy, no contents
   seed     upload; a durable job extracts; another recomputes the tree digest in the
            new computer; it must equal the one computed from the archive
   start    apply origin's declared contents; the ordinary reconciler starts them
   verify   the new computer converged; the commits on both sides are reported
```

Code: `crates/compute-environment/src/daemon/clone.rs`. Test:
`cloning_an_environment_seeds_a_new_computer_with_the_same_workload_state`
(`tests/computers.rs`), run against a real workspace provider behind a real
`compute.remote@1` target.

## 1. What composes cleanly (existing, unchanged)

| Need | Existing primitive |
| --- | --- |
| A new environment with the same needs | `create_computer_environment`; requirements and policy copied from the source record |
| Choosing where it runs | placement, exactly as for any environment (`--target` only constrains) |
| Running work inside a computer, evidenced | `run_in_computer_command`: durable job, job and execution ids, outcome |
| Getting bytes into a computer | the archive transport `import_source` already used (base64 chunks in job environments, digest-checked in the target) |
| Declared contents and starting them | `change_environment` + the reconciler (repositories, packages, processes) |
| Waiting for reality | `await_computer`, `ComputerView.converged`, `observed.repositories[..].commit` |
| Authorization and ownership | `owned_environment` (only the owner clones), scopes (`Operate`) |
| Evidence | an `environment.command` event naming the source, digests, and jobs |

Nothing here needed a provider change, a new state record, a FeltDB model
change, or a new `SessionCapabilities` field.

## 2. What was missing

Three things, all generic:

1. **Reading a tree out of a computer.** Every existing path moved bytes *in*
   (`import_source`) or read small text; nothing exported a workspace.
   → `Daemon::export_workspace`.
2. **Placing a tree into an empty workspace.** `import_source` commits an
   archive as a git repository under `.compute/sources/`; it cannot lay files
   into the workspace. → `Daemon::seed_workspace`, which refuses a non-empty
   workspace.
3. **A tree digest both sides compute identically**, so "the same state" is
   checked rather than assumed. → `summarize` (from an archive, in Rust) and
   `TREE_DIGEST` (inside a computer, in shell), defined as SHA-256 over
   `"<sha256>  ./<path>\n"` for each regular file, sorted bytewise.

Plus one refactor: the chunked upload loop moved out of `import_source` into
`upload_archive` so both use it.

Findings that shaped the design and remain limits:

- **`run_in_computer_command` returns text and no truncation flag.** A
  truncated export could have looked like a smaller archive. The export
  therefore prints the archive's own digest as a second line and the daemon
  rejects anything else.
- **The archive travels through job output and job environments**, base64,
  held in daemon memory. The ceiling was not measured; the test uses a small
  tree. A large workspace needs the streamed artifact route, which is the
  real missing primitive for anything bigger.
- **`tar` of a live workspace is crash-consistent, not quiesced.** A file
  changing during the read makes `tar` exit non-zero and the export fails
  closed. Quiescing (stop processes, capture, restart) was not built.

## 3. Lines of generic Compute code

Counted as non-blank, non-comment lines in this change:

| | Lines |
| --- | ---: |
| `clone.rs`: three shell scripts (~35), `summarize` (~65), `export_workspace`, `seed_workspace`, and their types | 199 |
| `upload_archive` in `computers.rs` (net of the code it replaced in `import_source`) | 21 |
| `await_computer` visibility, scope line for the route | 2 |
| **Generic total** | **≈ 222** |

## 4. Lines of composition / glue

| | Lines |
| --- | ---: |
| `clone_environment` | 132 |
| imports in `clone.rs` | 13 |
| `CloneRequest`, `CloneReport` (`model.rs`) | 24 |
| route entry and handler arm (`api.rs`) | 4 |
| CLI subcommand and output (`computer_cmd.rs`) | 52 |
| **Glue total** | **≈ 225** |

Plus 139 lines of test and two documentation table rows. The split is about
even. The point is not the ratio but that the glue is a linear script over
existing operations: no state machine, no new durable record, no retry logic
of its own.

## 5. Is the result useful outside this demo?

Each generic piece has a use that is not clone:

- `export_workspace` + `summarize`: back up or inspect any computer's
  workspace; attach it to an incident; diff two computers.
- `seed_workspace` + the tree digest: seed any computer from any archive, with
  proof it landed. It generalizes `import_source`'s transport.
- Together they make **state-preserving `replace`** (export → replace → seed)
  a small addition: today replacement drops undeclared files
  ([persistent-environments.md](persistent-environments.md)).
- If the archive were stored (an artifact) instead of streamed to the next
  step, this is checkpoint / restore / fork with the same code. That is the
  step this PR deliberately did not take; what it establishes is that the
  next PR is *storage and a record*, not new mechanism.

## What a clone carries, and does not

Carries: the workspace's files (including empty directories), the same
declared repositories at the same revisions, packages, and processes, the same
requirements and policy.

Does **not** carry:

- **`repos/`**: declared repositories are re-derived from the declared contents.
  Uncommitted changes and untracked build output inside a checkout are lost.
  This is the sharpest limit; carrying them needs either committing them or
  treating `repos/` as ordinary files, which fights the reconciler.
- Memory, processes, the machine, the provider: never (a clone is a new
  machine running declared processes from declared contents).
- Symbolic links and special files: the export is **refused**, and nothing is
  created (`… holds something other than files and directories`).
- Controller state: `.compute/processes` and `.compute/imports` are excluded;
  `.compute/sources` (imported source repositories the contents refer to) is
  carried; any other `.compute` path in an archive is refused.
- Configuration *values*: not copied unless `--copy-config`, because that is
  where credentials live; the names left behind are reported.
- Executable bits in the digest: the archive keeps modes, the digest covers
  contents and paths only (and not empty directories).

## Failure

Phases are durable and evidenced. The new environment is created with no
contents; contents are applied only after the seed is verified, so a failed
clone leaves an inert environment whose error names the phase
(`provisioning`, `seeding`, `applying contents`, `starting`). Nothing is
retried or removed silently. Export failures (unsupported files, a changing
file, a cut-short output) happen before anything is created.

## Not done

- The CLI command is compiled and runs through the same daemon method the test
  exercises, but no automated test drives the CLI or the HTTP route.
- No stored checkpoint, no lineage, no quiesce, no size limit measurement, no
  clone across daemons.
- A deterministic checkpoint archive format was drafted first and set aside
  because this composition did not need it: the archive here is a transport,
  identified by its digest and its tree digest, not a durable artifact.
