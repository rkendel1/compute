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
| `dependency_capsule_formats`, `dependency_capsules` | Accepted capsule formats and capsules already resident |
| `artifact_modes`, `max_request_bytes`, `max_output_bytes` | Artifact transport limits |
| `max_concurrent_jobs`, `job_retention_seconds` | Present when durable jobs are accepted |

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
protocol, selection mode (`explicit` or `pool`), and selection reason.
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

Node 24 is the first acquired runtime. Linux x86-64 and arm64 artifacts are
pinned to official release digests in `distribution/runtime-lock.json`.

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

AppPort exposes placement as a capability; the placement logic stays in
Compute. These capabilities do not require AppBoundry.
