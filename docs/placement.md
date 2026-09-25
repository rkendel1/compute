# Placement

Compute does not ask:

> "Where would this probably work?"

Compute asks:

> "Which provider can prove it satisfies this workload contract?"

```text
Workload
   ↓
Requirements
   ↓
Provider Pool ──▶ capability discovery
   ↓
Capability Matching
   ↓
Selected Provider
   ↓
Resolve runtime distribution
   ↓
Acquire → verify → prepare
   ↓
Admission → Execution Job
   ↓
Result + Receipt
```

## Core invariant

**Provider selection never alters workload semantics.** Given a workload's
runtime, distribution, dependency capsule, isolation, network, and resources,
a provider is either compatible or incompatible. Compute never does any of
the following:

- substitute a runtime
- replace a dependency
- downgrade isolation or relax network policy
- fall back to the host
- substitute an architecture or version

If no provider satisfies the contract, the result is `placement_failed`, and
nothing executes.

Use `compute run APP --provider auto` to select by compatibility and the
pool's deterministic priority ordering. `--provider local` and
`--provider remote` filter only the eligible set; `--provider provider:ID`
requires one configured provider and never falls back. Legacy bare IDs remain
accepted. The established
read-only forms are `compute placement APP` (an alias for
`compute placement inspect APP`) and `compute placement explain APP`.

## Requirements

Placement requirements are derived from the canonical workload bundle. They
contain only what affects execution compatibility:

```json
{
  "requirements_version": "compute.placement.requirements@1",
  "runtime": { "kind": "python", "version": "3.13" },
  "distribution": { "id": "sha256:..." },
  "dependencies": {
    "id": "sha256:...", "format": "compute.deps", "embedded": true,
    "runtime_version": "3.13.15",
    "platform": { "os": "linux", "architecture": "x86_64" }
  },
  "isolation": "strict",
  "network": "none",
  "resources": {
    "cpu_count": 2,
    "memory_bytes": 2147483648,
    "disk_bytes": 5368709120,
    "timeout_ms": 30000
  },
  "architecture": "arm64",
  "platform": { "os": "linux", "architecture": "x86_64" },
  "artifact": { "mode": "bundle", "request_bytes": 9486, "submission": "synchronous" }
}
```

Where each requirement comes from:

| Requirement | Source |
| --- | --- |
| Runtime kind and version | The workload |
| CPU, memory, disk, architecture | The workload (`compute.toml` or portable workload specification) |
| Runtime artifact identity | `--runtime-artifact` |
| Distribution | `--distribution` |
| Isolation | The workload's profile. `--isolation` may strengthen it, never weaken it |
| Platform | The dependency capsule, the ELF header of a native entrypoint, or `--platform`. Conflicting sources are an error |
| `submission` | `job` for `compute pool submit` or `--submit` |

## Matching

`match_provider(requirements, descriptor)` evaluates every dimension and
reports **every** incompatibility, each with a stable code, the required
value, and the available value:

```json
{
  "compatible": false,
  "reasons": [
    {
      "code": "isolation_unsupported",
      "dimension": "isolation",
      "required": "strict",
      "available": ["process", "sandboxed"]
    }
  ]
}
```

| Dimension | Codes |
| --- | --- |
| runtime | `runtime_unsupported`, `runtime_unavailable`, `runtime_version_mismatch`, `runtime_artifact_mismatch` |
| distribution | `distribution_mismatch` |
| platform | `platform_mismatch`, `architecture_mismatch` |
| dependencies | `dependency_format_unsupported`, `dependency_capsule_missing`, `dependency_capsule_mismatch`, `dependency_runtime_mismatch`, `dependency_platform_mismatch` |
| isolation | `isolation_unsupported` (with the failing boundary, such as `filesystem_isolation_unavailable`) |
| network | `network_unsupported` |
| resources | `cpu_unavailable`, `memory_unavailable`, `disk_unavailable`, `timeout_unenforceable`, `timeout_exceeds_limit`, `memory_unenforceable`, `memory_exceeds_limit`, `cpu_limit_unenforceable`, `process_limit_unenforceable`, `output_limit_unenforceable` |
| artifact | `artifact_mode_unsupported`, `artifact_too_large`, `output_exceeds_limit`, `jobs_unsupported` |

Runtime isolation is resolved by the same function execution uses, so
placement never admits a request that execution would reject.

An immediately executable runtime is compatible as `installed` or `ready`.
A pinned provider distribution is also compatible as `available`; placement
records its canonical distribution and lifecycle state. After selection,
dispatch resolves it again and prepares it before admission. Resolution
drift, acquisition failure, digest mismatch, or unverified preparation fails
on the selected provider without fallback.

**Exact distribution matching.** A workload bound to `distribution_id = X`
matches only providers advertising exactly `X`. The same runtime version is
not the same distribution.

**Exact dependency matching.** A capsule `X` matches when it is embedded in
the bundle and the provider accepts `compute.deps` capsules (it is
transferred and verified at execution), or when the provider already holds
`X`. "Python 3.13" is never treated as "the same dependency environment".
There is no host dependency fallback.

## Selection

Eligibility and selection are separate phases. `local`, `remote`, and
`provider:ID` never alter capability matching or make an ineligible provider
eligible.

1. Only providers that are **compatible** and **admitted by policy** are
   candidates. Admission is evaluated per provider, independently of
   capability, under the caller's policy intersected with the provider's
   advertised policy; see [admission.md](admission.md).
2. Candidates are ordered by `priority` (descending), then provider ID
   (ascending).
3. The first candidate is selected.

Selection never uses randomness, response time, latency, hidden heuristics,
or health scoring. Health affects selection only when the pool explicitly
sets `require_healthy`. The same pool configuration, capabilities, policy,
and requirements always produce the same selection.

Every evaluated provider has one status:

| Status | Meaning |
| --- | --- |
| `compatible` | Satisfies every requirement |
| `incompatible` | Proven unable, with reasons |
| `capabilities_unknown` | Stale capabilities; see [provider-pools.md](provider-pools.md#capability-cache-and-freshness) |
| `provider_capabilities_invalid` | Malformed or contradictory capability data |
| `provider_unavailable` | Capability discovery could not reach it |
| `excluded_unhealthy` | Compatible, but excluded by `require_healthy` |
| `policy_denied` | Compatible, but policy does not admit the execution |

The report lists `compatible_providers`, `incompatible_providers` (proven
incompatible), and `excluded_providers` (compatibility not established).

## Explicit provider

```sh
compute pool run --provider provider:production ./script.py
```

Only the named provider is evaluated. If it is compatible, the workload runs
there with `selection_mode = explicit`. If it is incompatible, unknown,
invalid, or unavailable, placement fails with
`explicit_provider_incompatible`, and no other provider is substituted. An
unconfigured name fails with `provider_not_configured`.

## Placement identity

`placement_id` is a digest of:

- the requirements
- the evaluated pool configuration (IDs, kinds, endpoints, priorities)
- each provider's `capability_version` and status (plus health, when
  `require_healthy` is set)
- the selection policy and selection mode

It never includes timestamps, job IDs, credentials, or transient network
metadata. Re-running placement with identical inputs yields the same
`placement_id`, selection, and explanation.

## Inspecting and explaining

```sh
compute placement inspect ./script.py --json
compute placement inspect --bundle workload.compute --refresh
compute placement explain ./script.py
```

No execution occurs. `inspect` returns the requirements, the providers
evaluated with their statuses and reasons, the compatible and incompatible
providers, and the selected provider. `explain` answers four questions:

1. What does this workload require?
2. Which providers were considered?
3. Why is each provider compatible or incompatible?
4. Why was the selected provider selected?

Explanations use factual terms such as "selected provider", "selection
priority", and "compatible". Both commands exit `0` when a provider is
selected and `2` when placement fails.

## Pool execution

```sh
compute pool run ./script.py --receipt receipt.json
compute pool submit --bundle workload.compute --idempotency-key build-42 --json
```

Both commands run these steps:

1. Construct the canonical workload and resolve its dependency capsule.
2. Discover provider capabilities.
3. Evaluate the requirements and select a compatible provider.
4. Report the placement: stderr for text, a `placement` field for `--json`,
   and optionally `--placement-output FILE`.
5. Execute through the existing provider contract.
6. Return the result or the job.

There is no second execution engine. The selected provider binds the
placement into the receipt it seals. Compute then verifies that the receipt
names the selected provider, the placement, and the required distribution,
dependency, runtime, and isolation.

`pool submit` requires a job-capable provider. The local provider has no
durable jobs, so it is `incompatible` with `jobs_unsupported`. The output
names the provider and endpoint that own the job, for example:

```sh
compute remote status --provider <provider_id> <job_id>
```

## Failure semantics

| Situation | Result |
| --- | --- |
| No compatible provider | `placement_failed` / `no_compatible_provider`; nothing executes |
| Explicit provider incompatible | `placement_failed` / `explicit_provider_incompatible`; nothing executes |
| Provider disappears during discovery | That provider is `provider_unavailable`; the others are still evaluated |
| Malformed or contradictory capabilities | That provider is `provider_capabilities_invalid` and excluded |
| Stale capabilities | That provider is `capabilities_unknown` and excluded |
| Provider unavailable **after** selection | `provider_unavailable`, naming the placement and provider, with `retried: false` |

**No automatic retry across providers.** Once a provider is selected,
Compute never sends the workload elsewhere, even if the provider fails. This
matters for workloads with side effects. A caller that wants failover can
make a new, explicit placement and submission, and the receipt of each
attempt names its actual provider.

## Security

Provider capability responses are untrusted input. Before a provider can be
matched, Compute validates:

- **Protocol:** `compute.local@1` for local providers, `compute.remote@1`
  for remote ones.
- **Identity:** the reported kind must match the configured kind, and
  identifiers and endpoints must be well formed.
- **Runtime descriptors:**
  - no duplicate runtimes, and well-formed versions
  - internal consistency: a runtime that claims a filesystem boundary,
    timeout enforcement, or memory, CPU, or process enforcement must declare
    the matching capability, and a network boundary requires support for
    `network: none`
- **Identities:** distribution, runtime artifact, and resident capsule
  identities must be `sha256:` identities. Artifact identities must name
  listed runtimes.
- **Profiles and policies:** isolation and network lists must be non-empty
  and free of duplicates.
- **Limits:** artifact and resource limits must be positive.

A provider whose descriptor fails validation is
`provider_capabilities_invalid` and excluded. It cannot claim compatibility
through malformed or contradictory data.

## Library use

```rust
use compute_placement::{place, dispatch, PlacementRequirements, ProviderPool, PoolConfig};

let pool = ProviderPool::from_config(&PoolConfig::load(path)?)?;
let records = pool.capabilities(&mut cache, DiscoveryMode::PreferCache, None, Utc::now()).await;
let report = place(&pool.configs(), pool.policy(), &records, &requirements, None);
let response = dispatch::execute(&pool, &report, request).await?;
```

`place` is a pure function of its inputs, so placement can be tested and
audited without contacting providers.
