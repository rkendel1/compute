# Admission

Admission is Compute's deterministic, fail-closed boundary between "can run"
and "runs":

```text
Workload → Requirements → Provider Pool → Capability Match
        → Admission Policy → Selected Provider → Execution → Result + Receipt
```

## The evaluator

The evaluator is a pure function:

> Policy + ExecutionContract + ProviderFacts + CapabilityStatus → AdmissionDecision

It performs:

- no file reads
- no network calls
- no environment inspection
- no clock
- no randomness
- no global state

It lives in the `compute-policy` crate, which has no provider, runtime,
HTTP, or filesystem dependency.

- **ExecutionContract** is the canonical, explicit statement of what the
  workload asks for: workload and bundle identities, runtime, dependency
  capsule, isolation, network, resources, platform, input and artifact sizes,
  and output classes.
- **ProviderFacts** state where the execution would run: provider identity,
  distribution identity, platform, and the runtime version the provider
  reports.
- **CapabilityStatus** is the capability check's result (`compatible`,
  `incompatible`, or `unknown`). It is carried beside the policy evaluation,
  never merged into it.

## Decisions

```json
{
  "admission_version": "compute.admission@1",
  "admission_id": "sha256:...",
  "status": "denied",
  "admitted": false,
  "policy_id": "sha256:...",
  "provider": { "identity": { "kind": "local", "id": "local" }, "distribution_id": "sha256:..." },
  "capability": { "status": "compatible" },
  "reasons": [
    {
      "code": "network_denied",
      "kind": "policy",
      "dimension": "network",
      "requested": "network",
      "allowed": ["none"],
      "message": "requested network \"network\" is not allowed by policy"
    },
    {
      "code": "runtime_denied",
      "kind": "policy",
      "dimension": "runtime",
      "requested": "ruby",
      "allowed": ["python", "wasm"],
      "message": "requested runtime \"ruby\" is not allowed by policy"
    }
  ],
  "contract": { "contract_version": "compute.contract@1" }
}
```

Every denial explains itself. The evaluator reports **every** violation it
can determine, ordered by kind and then code, rather than stopping at the
first.

| Kind | Codes |
| --- | --- |
| contract | `resource_request_invalid`, `platform_conflict` |
| capability | `capability_mismatch`, `capability_unknown` |
| policy | `runtime_denied`, `runtime_version_denied`, `runtime_version_unknown`, `distribution_denied`, `distribution_unknown`, `dependency_denied`, `isolation_below_minimum`, `network_denied`, `timeout_unbounded`, `timeout_exceeds_policy`, `memory_unbounded`, `memory_exceeds_policy`, `output_unbounded`, `output_exceeds_policy`, `input_exceeds_policy`, `artifact_exceeds_policy`, `platform_denied`, `architecture_denied`, `platform_unknown`, `output_class_denied` |

**Fail-closed.** Unknown capabilities, unknown distributions or versions
under a policy that restricts them, unbounded resources under a limit,
invalid policies, and impossible contracts are all denials.

## Identity

`admission_id` is the SHA-256 of:

- the contract
- the provider facts
- the capability status
- the `policy_id`

It never includes timestamps, job IDs, credentials, or transport metadata.
The same inputs always produce the same decision and identity. Placement
computes the admission it expects, and the provider computes it again
independently. The receipt must carry that same `admission_id`.

## Where admission happens

| Path | Admission |
| --- | --- |
| `compute run` | Locally, before the runtime starts |
| Remote execution | By the server: verify artifact → resolve requirements → capability check → policy admission → execute |
| Durable jobs | At submission; a denied request never becomes a job |
| Provider pools | For every provider, independently of capability |

In a provider pool, a provider is a candidate only if it is both capable and
admitted:

| Capability | Policy | Status |
| --- | --- | --- |
| compatible | admitted | `compatible`: a candidate |
| compatible | denied | `policy_denied`: excluded |
| incompatible | admitted | `incompatible`: excluded; notes that policy would admit |
| incompatible | denied | `incompatible`: excluded; both facts preserved |

**Explicit selection** (`--provider production`) means "use this provider if
it satisfies the contract", never "bypass policy". A capable provider whose
policy denies the workload fails with `explicit_provider_denied`, and nothing
else is substituted.

When admission denies:

- nothing executes
- the provider returns `admission_denied` (HTTP 403) with the complete
  decision as the rejection evidence
- `compute run` prints the decision (`--json`) and exits `2`

Each provider's health reports `executions_started`, and a denied request
never increases it.

## Evidence

Admitted executions bind admission into:

- **Execution results:** `admission.policy_id`, `admission.admission_id`,
  `admission.admission_status`.
- **Jobs:** `admission` together with `placement_id`, `provider_id`,
  `request_id`, and `execution_id`. The job's stored request also keeps the
  decision and its policy snapshot.
- **Receipts:** `compute.receipt@1` gains `policy_id`, `admission_id`, and
  `admission_status`.

A receipt therefore proves:

- the workload, bundle, and dependencies
- the runtime and distribution
- the provider and placement
- the policy and admission
- the execution itself

Receipt verification rejects:

- partial admission evidence
- any `admission_status` other than `admitted`, since receipts exist only
  for executions

## Commands

```sh
compute policy inspect                 # effective policy and its sources
compute policy validate policy.json    # static validation and policy_id
compute policy check ./script.py       # decision only; exits 2 when denied
compute policy explain --bundle workload.compute
compute explain ./script.py            # the whole decision chain
```

`policy check --json` returns `policy`, `policy_id`, `requirements`,
`provider`, `admission`, `reasons`, `effective_policy`, and the full
`decision`. `compute explain` shows these stages:

1. workload
2. runtime, dependency, and isolation requirements
3. provider capabilities
4. policy
5. per-provider admission
6. placement

Add `--provider ID` to check against a pool member; the member's own server
policy applies.

## AppPort

These capabilities are public observations, and none of them executes:

- `compute.policy.inspect@1`
- `compute.policy.check@1`
- `compute.policy.explain@1`
- `compute.admission@1`

Existing capabilities are unchanged. AppPort transports Compute's decisions
and never redefines them. A denied `compute.run` reports `admission_denied`,
or `capability_denied` when only capability reasons apply.

## Non-goals

Admission is not:

- AuthBoundry, identity, authorization, RBAC, or ABAC
- a policy server, registry, or marketplace
- a scripting language, Rego/OPA, or AI-made decisions
- workflow, scheduling, or billing rules

Compute policy governs execution constraints, not business authorization.
