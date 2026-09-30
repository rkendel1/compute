# Configured environment readiness

**Status:** implemented. `compute environment inspect NAME` and
`GET /environments/{name}` carry `computer.readiness`; code in
`crates/compute-environment/src/daemon/readiness.rs`, types in `status.rs`,
tests in `crates/compute-environment/tests/readiness.rs`.

> `ready` means Compute has verified, against the environment's own target
> and now, that the machine is confirmed, the requirements hold, and the
> declared contents are held. A computer that exists is not a ready
> environment.

```text
Recipe        "what I want"                  policy, versioned data
  ↓ validate / resolve
Resolution    "where and how it can run"     read-only; nothing exists
  ↓ create
Environment   "what actually exists"         Computer + configured contents
  ↓ verify
Readiness     "whether it is usable now"     observed, never stored
  ↓ admit
Workload      "what is running"
```

A recipe describes desired execution policy. Resolution answers whether that
policy can be placed. Readiness describes whether the environment that was
created still satisfies it. They are separate answers and are not merged.

## What Compute already knew (the audit)

Nothing here was added as a second model.

| Concern | Existing contract | Before this change |
| --- | --- | --- |
| Desired requirements | `ComputerRequirements` (cpu, memory, architecture, network, isolation, capabilities, features, runtimes) on the environment's `ComputerSpec` | Unchanged. Readiness never restates it |
| Can a target satisfy them | Placement: `match_provider`, admission, `place_with_policy`, with `ReasonCode`s (`runtime_unavailable`, `runtime_version_mismatch`, `isolation_unsupported`, `network_unsupported`, `session_capability_unsupported`, `target_feature_unsupported`, …) | Evaluated once, at creation |
| Machine state | `ComputerStatus` (`pending` … `running` … `lost`), `ComputerReality` (`observed`, `unverified`, `reconciling`, `explanation`), `converged`, `ready_at` | Told whether the *machine* ran, not whether the environment could execute |
| Process readiness | `ObservedReadiness` per process (`ready`/`unready`) | Per process only |
| Workload admission | `exec` required `status == running` | A computer that ran was enough; the target's capabilities were not re-checked |
| Runtime and distribution | Placement descriptors carry the target's runtimes, distribution, platform and lock identity | Read at placement only |

The gap: nothing said, after creation, that the target *still* satisfies
what the environment requires, and a running computer was treated as a usable
environment.

## The states

| State | Means | Admits workloads |
| --- | --- | --- |
| `created` | The environment exists; its machine is not being provisioned yet | no |
| `starting` | Its machine is being provisioned or resumed, or it is still being brought to its declared contents | no |
| `ready` | Verified: machine confirmed by its target, requirements satisfied by that target now, declared contents held, no declared process impaired | yes |
| `degraded` | It runs and its requirements hold, but a declared process is impaired, the target has not confirmed the machine recently, or the requirements could not be re-verified. It is never reported `ready` | yes |
| `unavailable` | It cannot run workloads: stopped, stopping, unreachable, lost, destroyed, or its target no longer satisfies its requirements (placement's reasons are listed) | no |
| `failed` | Establishing it failed (a provisioning failure). Distinct from unsatisfied requirements | no |

Bootstrap ([bootstrap.md](bootstrap.md)) is its own condition: a failed
repository, package, build, or provisioning makes the environment `failed`
with a class (`configuration_failed`, `provider_failed`) and the operation
that failed; a declared process that will not start is `degraded`
(`runtime_failed`). A machine that is running but still being configured is
`starting`.

**Unsatisfied is not failed.** A target that cannot satisfy the requirements
is `unavailable` with `unsatisfied` reasons (`session_capability_unsupported`,
`runtime_unavailable`, …); at creation it is refused by placement and nothing
is recorded. `failed` is only for an attempt to establish the environment
that the target accepted and could not complete.

## Requirements and readiness

```text
Needs:       1 CPU, 64.0 MiB, network network
Readiness:   ready
             mine is ready: verified against target-a.
             ✓ machine: running on target-a, confirmed by the target
             ✓ requirements: target-a satisfies the requirements now
             ✓ contents: holds what the environment declares
             ✓ processes: every declared process that should run is running
```

`requirements` (on the computer view) is what it must satisfy. `readiness`
is what was verified. The `requirements` condition is placement's own
evaluation restricted to the computer's target, so its reasons are
placement's `ReasonCode`s. Capacity is not part of readiness: the running
machine already holds its capacity. The recipe, when there is one, is shown
as provenance (`Recipe:`), unchanged.

## Read-only, and never stored

Evaluating readiness creates no computer, starts no process, installs
nothing, changes no record or recipe, and acquires no other target. The only
side effect is re-discovering the computer's own target's capabilities into
the capability cache, at most once per `read_cache` (1 s) for a view and
always for a workload admission
([feltdb.md](feltdb.md) working-state table). Readiness is derived on every
read from the `Computer` record, the target's confirmation, and the target's
current capabilities, and is never written: after a controller restart it
is evaluated again, and a "ready" from before the restart cannot be read
back. Repair, if it is ever wanted, is a separate explicit operation.

## Lifecycle

| Operation | Readiness |
| --- | --- |
| create | `created` → `starting` → `ready`, only once the machine is confirmed, converged and verified |
| stop | `unavailable` (stopped); no workload is admitted; no process runs |
| start | `starting` → re-verified. If the target no longer satisfies the requirements, `unavailable` with the reason, not `ready` |
| controller restart | evaluated again; identity, requirements, and recipe evidence survive; a stopped environment stays stopped with no process |
| replace / fork | the new machine is verified on its own target |

## Workload admission

Compute owns the boundary. `exec` and project commands ask the environment
to be `ready` or `degraded`, verified against the target now, and otherwise
are refused with the state and why:

```text
environment dev is unavailable, not ready for workloads: dev runs on target-a,
but target-a no longer satisfies its requirements:
session_capability_unsupported. No workload is admitted; replace the computer
or change its requirements.
```

There is no fallback to another machine. An unreachable or lost machine
keeps its existing error. Setup Compute does itself (importing sources,
applying declared contents, checkpoints) is not a user workload and does not
come through this check.

## For a consumer such as Foundry

```text
Recipe → compute environment create → wait for readiness.state == ready
       → compute environment exec / project command
```

The consumer needs no knowledge of how readiness is established: it reads
`computer.readiness.state` (and `explanation`, `conditions`, `unsatisfied`)
from `environment inspect` or the API, and Compute refuses a workload the
environment cannot run.

## What this does not do

No repair or drift correction, no package installation, no new provider or
scheduler behavior, no new recipe semantics, and no second requirements or
runtime inventory: readiness reuses placement's evaluation of the
distribution and runtimes the target reports.
