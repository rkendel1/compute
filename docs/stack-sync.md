# Keeping a stack reproducible and in sync

A stack (`stacks/<name>/stack.toml`) says what a Computer is configured with
([stacks.md](stacks.md)). This page covers how the stack is **assembled**:
which source repositories it is built from, at which commits, which published
packages at which exact versions, which packages and protocols must agree, and
what has to pass before a revision is known-good. `compute stack` keeps that
reproducible, shows drift as it appears, and changes the known-good revision
only through a tested candidate and a named human's approval.

```text
stack.toml (policy)             stacks/<name>/
  sources: track main / ref       stack.lock                the known-good revision
  components: npm:… constraints   candidates/<digest>.lock  resolved, not yet promoted
  relationships, copies,          evidence/<digest>.json    suite results for exactly that lock
  divergences, acceptance         history/<digest>.lock     every promoted revision
                                  history/<digest>.approval.json  who approved it, over which evidence
```

The lock and its history are versioned files, reviewed like code. They are not
runtime state, and FeltDB stays the only durable runtime store
([feltdb.md](feltdb.md)).

## The commands

| Command | What it does | Changes |
| --- | --- | --- |
| `compute stack check [--offline] [--fail-on drift\|violation]` | Compares the known-good lock with the manifest, and (unless `--offline`) with the sources' heads and the registry. Lists the lock's inventory findings. | nothing |
| `compute stack diff [FROM] [TO]` | Shows what changed between two revisions: commits, lockfiles, toolchains, contract paths, packages, relationships, findings. Lists the affected sources and the suites to run. `head` resolves the manifest again. | nothing |
| `compute stack update --candidate [--source NAME=REF] [--package PKG=VER]` | Resolves a candidate into `candidates/<digest>.lock`. | a candidate file only |
| `compute stack test [REV] [--suite S] [--impacted-from REV] [--record]` | Clones every source at its locked commit from its declared repository and checks the tree and lockfiles against the lock. Then runs the suites in `compatibility/stack/`. | `--record` writes `evidence/<digest>.json` |
| `compute stack promote REV --approve --approved-by NAME` | Makes a candidate the known-good lock (details below). | `stack.lock`, `history/` |
| `compute stack reproduce [REV] [--into DIR]` | Restores every input of a revision from its declared sources and the registry, and verifies each one: commits, trees, lockfile digests, and package integrity recomputed from `npm pack`. | a work directory |
| `compute stack inventory [REV] [--package P] [--consumer C] [--status S]` | Lists every use of every provided package at the revision's locked commits, and the findings. | nothing |
| `compute stack consumers PKG [--target VERSION\|COMMIT]` | Lists who uses a package. With `--target`, lists what each consumer would have to change to move there. | nothing |

`REV` is `current` (the known-good lock), `head`, a digest prefix, or a lock
file. Exit codes: `0` in sync or passed; `1` drift; `2` failed or invalid;
`3` (promote only) approval required.

## The lock

`compute.stack-lock@1` is canonical JSON: sorted keys, pretty-printed, a final
newline. Its digest is the SHA-256 of those exact bytes. A lock that is not in
canonical form is refused, so a hand edit is detected rather than trusted. The
lock holds no timestamps, paths, or machine facts. Resolving the same inputs
twice yields the same bytes. For each source the lock records:

- the commit and tree;
- the SHA-256 of each root lockfile and toolchain file;
- the toolchains the source declares;
- the version values its manifest asks to record.

For each package it records:

- the exact published version;
- the registry integrity;
- the source commit the publisher claims (npm `gitHead`). This is recorded as a
  claim, never as proof of what the artifact was built from.

It also records whether each relationship held, and the inventory.

## Relationships

| Kind | Meaning | Statuses |
| --- | --- | --- |
| `published-from` | A stack package is published from a source. Its version there is compared with the published one, and the claimed commit with the locked one. | holds, drift |
| `package-dependency` | One stack package's published dependency on another must be the stack's version (`same`), or a nested copy is accepted with a reason (`nested`). An undeclared mismatch is a violation. | holds, accepted, violated |
| `pin` | A source pins another source's commit in a file. `locked`: it must be the locked commit. `fixed`: it must exist, and the reason is recorded. | holds, drift, accepted, violated |
| `consumes` | A change to the provider affects the consumer. This drives impact selection. | holds |

## The inventory

At the locked commits, the inventory reads from git objects (no checkout, no
package manager):

- every `package.json`, `package-lock.json`, `pnpm-lock.yaml`, `yarn.lock`,
  `Cargo.toml` and `Cargo.lock`;
- every tracked `node_modules` entry;
- every submodule;
- every directory that holds a package with a provided package's name.

For each package a source `provides` (`npm:<name>` or `cargo:<crate>`), it
records:

- **canonical:** the providing source, its locked commit, the package's directory,
  version and tree;
- **uses:** for each consumer and file:
  - the kind: `declared`, `resolved`, `installed` or `copy`;
  - the scope: production, development, peer, optional or build;
  - the context: package, test or example;
  - whether the use is direct or transitive;
  - the specifier and the resolved version;
  - the origin: registry, workspace link, path, git or copy;
  - the digest: the registry integrity or checksum, or the copy's tree;
  - for copies, whether it matches the canonical source;
- **expectations:** what the canonical source says each consumer should run
  (FeltDB's `product-certification/feltdb/compatibility-manifest.json`). Consumers
  it names that are not sources of the stack are listed as **not inspected**.

Lockfile coverage follows each package manager: a workspace member is governed
by its workspace root's lockfile, and pnpm by `pnpm-lock.yaml`.

### Findings

| Kind | Checks | Status |
| --- | --- | --- |
| version drift | `consumer-version`, `fixture-version` (a consumer resolves another version than the stack locks, or than the canonical source is at); `expected-version` (other than the canonical source expects) | open |
| source drift | `stale-copy` (a copy exactly matches an older canonical commit); `modified-copy` (it differs from the commit it claims); `unexplained-file`; `undeclared-copy-differs`; `git-pin`, `submodule-pin` | modified and unexplained: **violated**; others open |
| resolution drift | `duplicate` (one lockfile resolves several versions); `lock-mismatch`, `missing-from-lock` (a lockfile contradicts its own manifest); `shadowed-lockfile`; `unreadable-lockfile` | contradictions in a package: **violated**; in a fixture or example, and the rest: open |
| compatibility drift | `excludes-stack-version` (a production or peer specifier does not admit the stack's version) | open |
| provenance gap | `undeclared-copy`, `copy-origin-unknown`, `unlocked` (no lockfile records what installs), `external-path`, `undeclared-git`, `floating-git`, `committed-install`, `no-integrity`, `no-source-claim`, `undeclared-submodule` | open |

A finding is `accepted` when a `[[divergence]]` names it with a reason, or when
it is a stale copy behind a `fixed` pin. A **violated** finding blocks
promotion. An open finding makes `check` report drift; it does not block, and
it does not go away by itself.

### Copies and divergences

```toml
[[copy]]
name = "stack-agent-vendored-feltdb"
package = "cargo:feltdb"
consumer = "stack-agent"
path = "vendor/feltdb"
origin = "stack-agent-vendors-feltdb"           # a pin relationship (or commit = "<sha>")
map = ["src=crates/feltdb/src", "LICENSE=LICENSE"]   # must equal the canonical source at the origin
local = ["Cargo.toml", "rustfmt.toml", "VENDORED.md"] # written for the copy
reason = "…"                                     # required when `local` is not empty

[[divergence]]
name = "…"
package = "npm:@appport/*"     # an identity, or a prefix ending in `*`
consumer = "appboundry"        # optional
check = "version-drift"        # optional: one finding kind
path = "packages/"             # optional: only at or under this path
reason = "…"                   # required; recorded on every finding it accepts
```

A copy's mapped paths are compared by git tree with the canonical source at
the copy's origin (a mismatch is `modified-copy`), and with the locked commit
(a mismatch is `stale-copy`). Any file that is neither mapped nor local is
`unexplained-file`.

## Acceptance suites

`[[acceptance]]` names a suite. `covers` lists the sources and components
whose change makes it relevant. `mode` is deterministic, live, replay, mock or
model; a model suite cannot be mandatory. The implementation is
`compatibility/stack/<name>.sh`, run with `STACK_LOCK`, `STACK_SOURCES`,
`STACK_OUT`, `STACK_WORK` and `STACK_HARNESS`. A suite reuses the tests its
sources already have, at their locked commits, and writes `result.json`. A
suite passes only when its report says passed, nothing failed, and it exited 0.

## Upgrading a shared package

1. `compute stack consumers npm:@feltdb/core --target 0.12.1` lists every
   consumer, what it declares and resolves, and what would change. A copy with
   local files is `manual`: its local files are never overwritten.
2. Make each change in the consumer's own repository, on a branch, with its
   own package manager and its own tests. Each repository keeps releasing on
   its own schedule.
3. `compute stack update --candidate --source authboundry=<branch> …`. The
   candidate's inventory shows which consumers moved and which are still
   outstanding.
4. `compute stack test <candidate> --record`, then promote.

A partial upgrade stays visible: the consumers not yet moved remain as
version-drift findings until they move or a divergence explains them.

## Promotion

`promote` refuses unless all of the following hold:

- the manifest has not changed since the candidate was resolved;
- no relationship and no inventory finding is violated;
- the evidence is recorded for exactly this lock digest, produced by a
  committed (clean) harness;
- every source materialized from its declared repository;
- every mandatory suite ran in that evidence and passed. An impact-selected
  run (`--impacted-from`) is for fast feedback; promotion needs every
  mandatory suite.

It then asks for `--approve --approved-by NAME`. It keeps the previous lock in
`history/`, writes the new one with an approval record (who, when, and the
evidence digest), and replaces `stack.lock`. Nothing else ever writes
`stack.lock`.

## CI

`.github/workflows/stack.yml`:

- **contract** (pull requests and pushes that touch a stack or the tooling)
  runs the manifest, lock and inventory tests, then `compute stack check
  --offline --fail-on violation`.
- **candidate** (a pull request that adds a candidate lock, nightly, or on
  demand) resolves the candidate (or tests the added one) and runs `check`
  and `diff`. It then runs `test --impacted-from current --record` against
  fresh clones and uploads the evidence. It never promotes.

Private sources need a read-only token in `STACK_SOURCES_TOKEN`.

## What this does not establish

- **Packages not committed.** The inventory sees committed files. A dependency
  installed at build time without a lockfile, or a binary downloaded at run
  time, is invisible to it; where it can tell, it reports `unlocked`.
- **What a registry artifact was built from.** `gitHead` is a claim. A package
  published without one is a `no-source-claim` gap.
- **Consumers outside the stack.** A consumer the canonical source names but the
  stack does not include (for FeltDB: attn, factory, jev-ui, web-monitor) is
  listed as not inspected, never as in sync.
- **Ranges the grammar cannot read.** A specifier outside the version grammar
  (exact, `^`, `~`, comparators, `||`, `x`) is recorded and not judged.
