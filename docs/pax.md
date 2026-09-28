# PAX projects

**PAX defines project and environment requirements. Compute executes those
projects on targets capable of satisfying those requirements.**

```text
             PAX
      project requirements
              │
              ▼
        ┌───────────┐
        │  Compute  │
        │   Plan    │
        └─────┬─────┘
              │
       capability match
              │
       ┌──────┴──────┐
       ▼             ▼
    Local          Remote
    Target         Target
       │             │
       └──────┬──────┘
              ▼
         Environment
              │
              ▼
          Execution
              │
              ▼
       Receipt / Reality
```

PAX is an integration boundary, not a Compute implementation dependency.
Compute does not link, vendor, or embed PAX. It observes a project by running
the external, read-only `pax` executable and reading its versioned JSON
(`schemaVersion: "1"`). Only one module knows how PAX represents a project:
the adapter in `crates/compute-project/src/pax.rs`. Everything after it —
planning, placement, materialization, receipts — consumes Compute's own
`ProjectRequirements` (`crates/compute-core/src/project.rs`).

| | PAX | Compute |
| --- | --- | --- |
| Owns | what the project requires: ecosystem, package manager, declared dependencies, commands | where and how it runs: placement, runtimes, isolation, capacity, execution, evidence |
| Does not own | targets, placement, execution | dependency resolution, project authoring |
| CLI | `pax info`, `pax install`, `pax add`, … | `compute run`, `compute placement`, `compute receipt` |

Compute adds no PAX commands. Author the project with PAX; run it with Compute.

## The developer experience

```sh
cd my-project
compute run                       # discover, place, materialize, execute, prove
compute run --command check       # a different project command
compute run --project ../other    # a project elsewhere
compute run --dry-run             # plan and place; never execute
compute placement inspect --project .
```

With no `PATH`, `--workload`, or `--bundle`, `compute run` runs the PAX project
in the current directory. The project root is exactly the directory named —
Compute does not search upward, read ambient state, or remember earlier runs.
`compute run <path>`, `--workload`, and `--bundle` are unchanged and never
consult PAX. `compute up` is unrelated: it launches the control plane.

`compute run --project` uses the same placement as `compute run <path>`:
`--provider`, `--policy`, `--prefer-provider`, and a remote pool
(`--pool-config`) apply unchanged, so the same project runs on the local
target or a remote one, with the same evidence.

## What Compute derives

The adapter turns PAX's `info`, `deps`, and `scripts` documents into
normalized requirements:

| Requirement | From PAX | Notes |
| --- | --- | --- |
| project identity | `project.name`, schema version | recorded in the receipt |
| runtime | `ecosystem` (`javascript` → `node`, `python` → `python`) | a command's own runtime (`bun x.js`) refines it |
| tool | `manager.name` / component `tool`, `manager.version` | a **provisioning** tool: it built the environment; Compute never runs it |
| dependencies | `dependencies`, `devDependencies`, `optionalDependencies`, `peerDependencies` | only runtime dependencies must be in the capsule |
| commands | `scripts` | see below |
| platform | — | PAX reports none. State one to Compute: `[runtime] architecture` in `compute.toml`, or `--platform` |
| runtime version | — | PAX reports none. State one to Compute: `[runtime] version` in `compute.toml` |

Anything PAX declares that Compute cannot normalize is **never dropped**: an
ecosystem Compute does not run as a runtime workload (Rust, Docker) and
dependencies PAX reports only as ecosystem-native groups (today, Python) make
the project fail with `requirements_unresolved`.

### Commands

Compute executes a runtime on an entrypoint file. A project command runs when
it has that shape — `node index.js --flag`, `python3 main.py` — and is refused
otherwise (`tsc -p .`, `npm run x`, `a && b`): Compute does not run a shell.
Without `--command`, the `start` command is used; a project without one falls
back to Compute's own entrypoint conventions (`compute.toml [run].entrypoint`,
`package.json` `main`, `main.py`, …). Arguments after `--` follow the command's.

## Environment materialization

The requirements become the workload Compute already knows how to run:

```text
PAX requirements ─▶ runtime + version constraint + platform ─▶ WorkloadSpec ─▶ placement
                └─▶ runtime dependencies ─▶ dependency capsule (verified) ──▶ eligibility
                └─▶ command ─▶ entrypoint + arguments                       ─▶ execution
```

Compute consumes dependency capsules and never resolves or installs
dependencies (see [dependencies.md](dependencies.md)). Supply the capsule your
package manager's output produced with `--deps` or `compute.toml`
`[dependencies] capsule`. Before placement, every runtime dependency PAX
declares must be present in the capsule's inventory, with an exact version
where PAX pins one. Environment variable *names*, the entrypoint, and the
capsule identity are carried in the receipt; values never are.

## Planning and failures

Placement evaluates the project's resolved requirements against every target's
advertised capabilities, so a target is eligible only if it offers the project's
runtime (and version), platform, and capsule — not merely *a* runtime. It
selects among eligible targets exactly as for any workload (priority, capacity,
then identifier), so the same inputs select the same target.

Failures name the stage that failed and never collapse into "execution
failed":

| Code | Meaning |
| --- | --- |
| `project_discovery_failed` | no project at that directory, or `pax` cannot run |
| `pax_metadata_invalid` | PAX output is malformed, of another schema, or PAX could not parse the project |
| `requirements_unresolved` | the project declares something Compute cannot run or normalize |
| `no_target_satisfies_requirements` | every target lacks something the project requires |
| `runtime_unavailable` | every target lacks the required runtime |
| `dependency_unavailable` | the capsule is missing, or lacks a required dependency |
| `environment_materialization_failed` | the environment the requirements describe could not be built |
| `execution_failed` | the workload ran and failed (its exit code and receipt say how) |
| `receipt_evidence_incomplete` | the run's receipt does not identify or prove the project |

```text
no_target_satisfies_requirements: no target can run project `app`
  required:
    architecture = riscv64
    runtime = node
    tool npm = provisioning (not needed on a target)
  target:
    target local = incompatible; platform linux-x86_64; architecture_mismatch (required "riscv64", available "x86_64")
  result: unsupported
```

With `--json`, the same is `project_failure` beside the full `placement` report.
Nothing executes, and no receipt is written, when a project is unsupported.

## Evidence

`compute.receipt@1` gains an optional `project` block; receipts without one are
byte-for-byte what they were. The block separates four things:

- **declared** — what PAX said: runtimes, tools, and a count and identity of
  runtime dependencies;
- **resolved** — what Compute derived: runtime, version constraint, platform,
  the capsule, the command, environment variable names;
- **verified** — what *this execution's own evidence* shows, per requirement:
  `satisfied`, `unsatisfied`, `not_required`, or `not_evaluated`;
- **actual** — the receipt's existing `placement`, `runtime`, `dependencies`,
  `request`, and `execution` sections.

A requirement is `satisfied` only when the receipt shows it: the observed
runtime and version, the platform executed on, a capsule verified at
execution, the entrypoint that ran. A declared tool is `not_evaluated`; it is
never claimed. `compute receipt verify` recomputes `verified` from the receipt
and rejects any receipt whose `verified` does not follow, or that records an
`unsatisfied` requirement. `compute receipt inspect` prints the block.

## No hidden state

Discovery, requirement extraction, and planning are pure functions of the
project directory, the `pax` observation of it, and the command line. Compute
keeps no PAX cache, database, or sidecar file, and a project run writes nothing
but the receipt you ask for. `COMPUTE_PAX` selects the `pax` executable (default:
`pax` on `PATH`); it selects a program, not state.

## Not yet supported

- **Target-advertised tools.** No Compute target reports the tools it has, so a
  project cannot require an *execution* tool on its target; PAX's package
  manager is a provisioning tool and is recorded, not matched. The adapter and
  materializer already reject an execution tool no target offers.
- **Version and platform constraints from PAX.** PAX v1 reports neither, so they
  come from `compute.toml` and `--platform`.
- **Python dependencies, Rust, Docker.** See "What Compute derives".
- **Older remote targets.** A target that predates the `project` execution
  option rejects a project request; upgrade it or run on a current target.
- **Workspaces.** Multi-package workspaces run as the single project PAX reports
  at the given directory.
