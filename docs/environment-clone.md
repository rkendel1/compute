# Environment clone

**Status:** implemented (`compute environment clone SOURCE NAME`,
`POST /environments/{environment}/clone`), as the **first consumer** of
[workspace state](workspace.md). Clone is a composition; the mechanism it uses
is the generic capability, and exists independently of it.

```text
clone SOURCE NAME =
  export SOURCE's workspace           workspace.rs
  create NAME (same requirements and policy, no contents)
  seed NAME, which verifies           workspace.rs
  apply SOURCE's declared contents    the ordinary reconciler starts them
  report commits on both sides
```

Code: `crates/compute-environment/src/daemon/clone.rs`. Tests: `cloning_an_
environment_seeds_a_new_computer_with_the_same_workload_state` and
`a_failed_clone_never_leaves_an_unverified_environment_running`
(`tests/computers.rs`).

## What clone adds to the primitives

Only composition: the order above; creating the destination from the source's
requirements and policy; withholding configuration *values* unless
`--copy-config` (names left behind are reported); applying declared contents
only after the seed is verified, so nothing runs before that; re-deriving
declared repositories from their declared revision (they are not workspace
state); and the failure rule.

## What clone does not carry

* `repos/`: uncommitted or untracked changes inside a checkout are lost.
  Declared repositories are re-cloned at the declared revision.
* Memory, processes, the machine, the provider.
* Symbolic links and special files: the export is refused and nothing is
  created.
* Configuration values, unless asked for.

## Failure

The destination is created with no contents; contents are applied only after
the seed is verified. If any phase after creation fails, `abandon_composition`
stops the environment (Reality: `stopped`, never running), records the phase
and `workspace_verified: false`, and the error says so. There is no rollback.
Failures before creation (export refused, an unsupported entry, a changing
workspace, an oversized archive) create nothing.

## Size of the composition

Counted as non-blank, non-comment lines:

| | Lines |
| --- | ---: |
| generic workspace capability (`workspace.rs`: shell, identity, export, seed, verify, `abandon_composition`) | ~430 |
| clone (`clone.rs`, including imports, phase tagging, and the report) | ~150 |
| request/response types, route entries, CLI (clone and workspace) | ~230 |

The clone composition is roughly a third of the size of the mechanism under
it. It did not get smaller in absolute terms than the first version (about
145) because it now also carries the failure rule's call sites and a richer
report; the failure rule itself (about 35 lines) was moved into the generic
module because any composition needs it.
