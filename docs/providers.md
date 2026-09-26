# Compute providers

A provider decides where a portable workload is executed. It does not change
what the workload means. `LocalProvider` calls the local Compute engine;
`RemoteProvider` transports the same canonical artifact to a Compute server
speaking `compute.remote@1`.

Providers are grouped into caller-owned [provider pools](provider-pools.md),
and [placement](placement.md) selects the provider that can prove it satisfies
a workload's contract.

## Provider model

```text
Workload ──▶ Provider Pool ──▶ Runtime resolution ──▶ Acquire / verify / prepare
             (placement)                                │
Result + Receipt ◀──────────── Execution ◀──────── Admission
```

- The **pool** evaluates and selects. It never executes.
- The **provider** executes through the existing provider contract.
- The **receipt** proves which provider executed, and under which placement.

## Capability model

Every provider answers the capability API (`GET /compute/capabilities`
remotely, `LocalProvider::capabilities` locally). The response describes:

| Field | Meaning |
| --- | --- |
| `protocol`, `provider` | Protocol version and the identity bound into receipts |
| `inventory` | Runtimes, pinned and observed versions, adapter capabilities |
| `runtime_artifacts` | Content identity of each runtime artifact |
| `distribution_id`, `inventory.platform` | Exact distribution and `<os>-<architecture>` |
| `isolation_profiles`, `network_policies` | Offered isolation and network policies |
| `max_timeout_ms`, `max_memory_bytes` | Upper bounds on requested limits, when bounded |
| `resources.capacity`, `resources.available` | Configured/detected CPU, memory, and disk ceiling, and the allocatable snapshot used by placement |
| `dependency_capsule_formats`, `dependency_capsules` | Accepted capsule formats and capsules already resident |
| `artifact_modes`, `max_request_bytes`, `max_output_bytes` | Artifact transport limits |
| `max_concurrent_jobs`, `job_retention_seconds` | Durable job limits, when jobs are accepted |
| `execution` | The execution modes the provider accepts: `run` (synchronous workloads), `jobs` (durable asynchronous jobs), `deployments` (durable applications) |

Placement turns this response into a canonical, validated **provider
descriptor** with a `capability_version` digest. See
[placement.md](placement.md#security).

Each runtime inventory entry binds availability and capabilities to its
platform, complete distribution identity, runtime payload identity, and
executable identity. Runtime support is therefore never reduced to a boolean
or assumed portable across platforms.

Runtime lifecycle is explicit: `installed` can execute now, `available` is a
pinned distribution the provider can obtain, `ready` is verified and
prepared, `unsupported` has no provider distribution, and
`unavailable`/`failed` cannot currently become runnable. Distribution IDs are
derived from artifact, version, platform, executable, and capability data;
mutable tags are not identities.

```sh
compute provider list --json
compute provider inspect production --json      # validated descriptor
compute provider capabilities production --json # raw capability response
compute provider capabilities http://127.0.0.1:8080
compute runtimes --json
compute runtimes --provider production
compute runtimes --provider production --json
```

A configured pool ID yields its descriptor. `local` or an `http(s)://`
endpoint outside the pool yields the raw capability response.

## Execution modes

Every provider says which submissions it accepts, explicitly, in
`execution`. Placement matches each submission mode against it, and a
provider that does not offer the mode is rejected before anything
executes, with a reason naming what was required and what is offered:

| Submission | Mode | Rejected with |
| --- | --- | --- |
| `compute run`, `compute pool run` | `run` | `run_unsupported` |
| `compute submit`, `compute pool submit` | `jobs` | `jobs_unsupported` |
| `compute deploy`, `compute application deploy` | `deployments` | `deployment_unsupported` |

```text
provider deployment-only: incompatible
  run_unsupported: required "run", available ["deployments"] (the provider does not run workloads on request)
```

What each kind of provider offers by default, and how to narrow it:

| Provider | Started with | Offers | Narrow with |
| --- | --- | --- | --- |
| `local` pool member | (this machine) | run, deployments (through this machine's daemon) | - |
| Compute server | `compute serve` | run, jobs | `--offer run` or `--offer jobs` |
| Compute daemon | `compute start` | run, jobs, deployments | `--offer deployments`, `--offer run,jobs`, ... |

A mode that is not offered is neither advertised nor accepted: a
`compute serve --offer jobs` endpoint refuses `POST /compute/execute` as
well as not advertising it. A server cannot offer deployments; a daemon
hosts them.

A daemon serves `compute.remote@1` (`/compute/execute`,
`/compute/jobs`, runtime resolution and preparation) on the same
provider and runtime store its deployments use: a run, a job, and an
application's service are the same execution on the same substrate, with
the same admission, runtime preparation, and receipts. A daemon in a run
pool therefore runs workloads. Its `compute.remote@1` routes need the
daemon's credential (`execute` scope for changes, `read` for discovery),
like the rest of its API.

A deployment is placed with `SubmissionMode::Deployment` and then released
through the selected daemon's authenticated `/applications` API (see
[applications.md](applications.md)). Nothing here is specific to where the
daemon runs: a laptop, a Linux host, a VM, or an Apple Container VM are all
the same provider to Compute.

## Restricting what a provider offers

An operator can narrow a server's offer. A restricted capability is both
withheld from discovery and **rejected at execution**, so advertised
capabilities never exceed enforced ones.

```sh
compute serve --listen 0.0.0.0:8080 \
  --allow-runtime python --allow-runtime node \
  --allow-isolation process --allow-isolation sandboxed \
  --allow-network network \
  --max-timeout 60s --max-memory 1g
```

For a provider with an operator-defined allocation budget (including test
fleets), advertise all three resource dimensions explicitly:

```sh
compute serve --resource-cpu 8 --resource-memory 16GiB --resource-disk 100GiB
```

A request that exceeds the restriction fails with `capability_mismatch` and
never runs.

## Execution policy

A server can enforce an execution policy with `compute serve --policy FILE`
or `[server.policy] path` in `compute.toml`. It advertises that policy in its
capabilities as `policy`. Every request is admitted before execution; a
denial returns `admission_denied` with the complete decision and never
reaches a runtime. See [policy.md](policy.md) and
[admission.md](admission.md).

## Guarantees

Capability discovery is descriptive. Compute never substitutes a runtime,
weakens isolation, removes a network requirement, or falls back to host
dependencies based on discovery results. An incompatible request fails before
runtime selection wherever its requirement can be evaluated.

Provider identity is included in every execution result and sealed into its
`compute.receipt@1` receipt. Remote receipts also bind `compute.remote@1`.
Placed executions also bind their placement: placement ID, pool provider ID,
protocol, selection mode (`explicit` or `pool`), selection reason, requested
and allocated resources, the provider capacity/availability snapshot, and the
execution platform.
Receipt verification is local and never contacts the provider.
Prepared-runtime receipts additionally bind the selected distribution,
verified artifact digest, prepared payload identity, and actual executable.

## Runtime provider lifecycle

Providers own installation behind resolve, prepare, and status operations.
Dispatch re-resolves the selected offer, refuses capability drift, requires
verified preparation before admission, and binds the distribution ID and
digest into expected execution evidence. Failed staging data is never
runnable; successful preparation survives provider restarts in
`COMPUTE_RUNTIME_STORE`.

The canonical catalog currently makes Node 24, Python 3.13, Deno, and Bun
provider-acquired runtimes on Linux x86-64 and arm64. Every artifact is pinned
to its upstream release URL and SHA-256 digest in
`distribution/runtime-lock.json`; the provider lifecycle contains no
runtime-specific acquisition branch. Workload aliases such as
`python >=3.12,<3.14` resolve to an exact catalog distribution before
preparation, and receipts bind that exact identity, digest, payload, and
executable.

## AppPort

AppPort keeps `compute.inspect@1`, `compute.run@1`, and `compute.submit@1`
unchanged and additionally exposes:

| Capability | Effect | Authorization |
| --- | --- | --- |
| `compute.provider.list@1` | observation | public |
| `compute.provider.inspect@1` | observation | public |
| `compute.provider.capabilities@1` | observation | public |
| `compute.provider.run@1` | consequential | `compute.run` |
| `compute.placement.inspect@1` | observation | public |
| `compute.pool.run@1` | consequential | `compute.run` |
| `compute.pool.submit@1` | consequential | `compute.submit` |
| `compute.application.deploy@1` | consequential | `compute.application.deploy` |
| `compute.application.rollback@1` | consequential | `compute.application.deploy` |
| `compute.application.stop@1` | consequential | `compute.application.stop` |
| `compute.application.status@1`, `.logs@1`, `.history@1` | observation | `compute.application.read` |

AppPort exposes placement as a capability; the placement logic stays in
Compute. The application capabilities speak of applications (identity,
version, provider, runtime, endpoint, status, evidence); which daemon hosts
one and how it is reached stay in Compute, which places and operates it
over the caller's pool exactly as `compute deploy` does. These
capabilities do not require AppBoundry.
