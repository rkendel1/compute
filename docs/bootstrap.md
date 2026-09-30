# Configured environment bootstrap

**Status:** implemented. There is no new provisioning engine: bootstrap is the
controller's existing contents reconciliation, made honest and inspectable.
`compute environment inspect NAME` and `GET /environments/{name}` carry
`computer.bootstrap`; derivation in
`crates/compute-environment/src/daemon/bootstrap.rs`; tests in
`crates/compute-environment/tests/bootstrap.rs`.

> **Bootstrap is configuration. Readiness is verification.**

```text
Recipe        desired policy (data; no commands, packages, or secrets)
  ↓ resolve
ComputerRequest   resolved execution requirements
  ↓ placement
Computer      a machine on a target
  ↓ bootstrap     the environment's declared contents are applied to it
Configured environment
  ↓ readiness     verified independently, against the target, now
Workload      admitted only when ready
```

Nothing is marked ready because bootstrap returned without an error:
readiness is evaluated from the target and the observed state
([readiness.md](readiness.md)).

## The audit: what already configured a computer

1. **What configures a Computer?** The controller's driver, one task per
   environment computer. After the machine is provisioned on its placed
   target (`ComputerStatus::provisioning → running`), `running_step` calls
   `plan(contents, observed)` and `apply_action` repeatedly. Each action is a
   durable job in the computer's session: `SyncRepository`,
   `InstallPackage`, `Build`, `StartProcess`. Runtimes come from the
   requirements (`ComputerRequirements.runtimes`, placement and the target's
   distribution) and from a process's runtime resolution, which is verified
   against the target's receipt.
2. **Which operations are authoritative?** `EnvironmentContents` (the
   environment record) is the declared configuration; the reconciler above is
   the only thing that applies it; `reconcile` is the only retry.
3. **What is persistent?** The environment record: requirements, declared
   contents, recipe evidence, and its `generation`. The computer record's
   `observed` contents: per item, a fingerprint and the job that applied it,
   and `converged_generation`. The workspace on the machine.
4. **What is ephemeral?** Processes, pids, restart state, provider-local
   handles: re-derived after a stop or a restart.
5. **What does `converged` prove?** It proves the computer *held* the
   declared contents at the recorded generation. **It did not** prove
   success: a failed repository, package, or build recorded its fingerprint,
   so the item counted as "attempted", and `converged_generation` advanced
   anyway. That is fixed here: the generation does not advance, and the view's
   `converged` is false, while any such item has failed.
6. **When is `ready_at` valid?** It is set when provisioning completes and
   the machine is running, not when configuration completes. It is machine
   evidence, not readiness; nothing reads it as readiness.
7. **What makes a Computer ready?** Nothing did: a running machine admitted
   workloads. Readiness ([readiness.md](readiness.md)) now requires a
   succeeded bootstrap among its conditions.
8. **Can it be repeated safely?** Yes. Items are keyed by fingerprint, so a
   converged environment applies nothing; `reconcile` on it writes only an
   event. A retry re-applies only failed items.
9. **A failure halfway?** The failed item's evidence, job, and error are
   kept, the computer's `failure` is `reconciliation / item_failed`
   (`retryable`), earlier items stay applied, and nothing retries by itself.

## Bootstrap state

Derived from the observed contents and the computer's failure; not stored.

| State | Means |
| --- | --- |
| `not_started` | No machine is running yet, so nothing has been applied |
| `running` | The machine runs and is being brought to what is declared |
| `succeeded` | Everything declared was applied and is held (`converged_generation == contents_generation`) |
| `failed` | Something declared failed, or the machine could not be established. The environment stays inspectable; retry with `reconcile` |

`steps` lists each declared item (`repository`, `package`, `build`,
`process`) with `pending`/`succeeded`/`failed`, the job and execution that did
it, and the error. `completed_at` is the last applied step's time once it has
succeeded.

## Failure classes

| Class | From | Readiness | Retry |
| --- | --- | --- | --- |
| `requirements_unsatisfied` | The target no longer satisfies the requirements (placement's reasons; at creation, placement refuses and records nothing) | `unavailable` | replace, or change requirements |
| `configuration_failed` | A repository sync, package install, or build failed | `failed` | `reconcile` |
| `provider_failed` | The target accepted the machine and could not provision or resume it | `failed` | `reconcile` / replace |
| `runtime_failed` | A declared process could not start or resolve its runtime | `degraded` (workloads admitted; a service its restart policy manages is an impairment) | `reconcile` |
| `bootstrap_cancelled` | Stopped or destroyed before configuration completed | `unavailable` | start; the item is applied again |
| `destruction_failed` | The machine could not be removed | `unavailable` | destroy again |

Each failure names the existing operation that failed (`package install`,
`repository sync`, `build`, `process start`, `provisioning`, `destroy`), and
carries the job when there is one. No provider internals are exposed.

## Lifecycle

- **Create.** `create` is the bootstrap: the driver applies the contents as
  soon as the machine runs. There is one authoritative lifecycle and no
  separate endpoint. `not_started → running → succeeded`, then readiness.
- **Bootstrap again.** `compute environment reconcile NAME` (existing) asks
  the driver to retry failed items; on a converged environment it changes
  nothing (the package is not run again, the machine is not replaced).
- **Stop or destroy during bootstrap.** The in-flight job is cancelled and
  confirmed ended (the lifecycle guarantees of [lifecycle.md](lifecycle.md)),
  so no bootstrap process survives; the attempt is not recorded as a failure,
  the state is `failed / bootstrap_cancelled` while stopped, and a start
  applies the item again automatically.
- **Stop then start.** The configuration persists (`succeeded`) and is not
  re-applied; readiness is re-verified before workloads are admitted.
- **Controller restart.** The evidence is durable: a failed bootstrap stays
  failed, a finished one is `succeeded`, and readiness is evaluated again. A
  restart during an install has no record that it finished, so the driver
  applies the item again: a declared package must be an idempotent install.
  Success is never reported without evidence.

## What bootstrap does not do

It applies only what the environment declares: no runtimes, packages,
capabilities, isolation, or provider features are added on its own, and
nothing is inferred from a recipe (a recipe declares requirements and
lifecycle; it holds no commands or scripts). Recipe provenance
(`name`, `version`, `digest`) is untouched by bootstrap. It does not repair
drift, manage secrets, check out source, or install PAX dependencies.

## For a consumer such as Foundry

```text
Recipe → resolve → create → wait: bootstrap.state == succeeded and
                            readiness.state == ready → run a workload
```

The consumer needs no knowledge of how runtimes were installed, how
distributions were acquired, or how readiness was verified: it reads
`computer.bootstrap`, `computer.readiness` (with `class`, `explanation`, and
`conditions`), and Compute refuses a workload the environment cannot run.
