# Execution policy

Capability asks:

> "Can it run?"

Policy asks:

> "May it run?"

Compute executes only when both answers are yes.

## Capability versus policy

Capability, admission, and execution are separate steps with separate
evidence:

| Step | Question | Fails with |
| --- | --- | --- |
| Capability | Can this provider execute the contract? | `capability_mismatch` |
| Admission | Is this execution permitted? | `admission_denied` |
| Execution | Run it | an execution result |

For example:

- A provider supports strict isolation, the policy requires strict, and the
  workload requests strict. Result: **admitted**.
- A provider supports only process isolation and the workload requires
  strict. Result: **capability mismatch**.
- A provider supports the workload, the workload requests network access, and
  the policy denies network access. Result: **admission denied**.

Admission reasons carry a `kind`, which is `contract`, `capability`, or
`policy`, so the two facts are never collapsed into one opaque decision.

## `compute.policy@1`

A policy is a small, versioned JSON document. There are no expressions,
scripts, rules, or inheritance.

```json
{
  "version": 1,
  "name": "production-policy",
  "defaults": { "network": "none", "isolation": "strict" },
  "limits": {
    "max_timeout_ms": 300000,
    "max_memory_bytes": 1073741824,
    "max_input_bytes": 10485760,
    "max_output_bytes": 1048576,
    "max_artifact_bytes": 67108864
  },
  "allowed_runtimes": ["wasm", "python", "node"],
  "allowed_runtime_versions": { "python": ["3.13"] },
  "allowed_distributions": ["sha256:..."],
  "allowed_dependencies": ["sha256:..."],
  "minimum_isolation": "sandboxed",
  "allowed_networks": ["none"],
  "allowed_os": ["linux"],
  "allowed_architectures": ["x86_64"],
  "allowed_output_classes": ["stdio", "files"]
}
```

| Field | Restricts |
| --- | --- |
| `allowed_runtimes` | Runtime kind |
| `allowed_runtime_versions` | Per runtime, the version the executing runtime reports must contain one of these prefixes |
| `allowed_distributions` | The executing provider's distribution identity |
| `allowed_dependencies` | Dependency capsule identity. Workloads without a capsule are unaffected |
| `minimum_isolation` | The weakest isolation profile allowed (`process` < `sandboxed` < `strict`) |
| `allowed_networks` | The workload's network policy |
| `limits.max_timeout_ms`, `limits.max_memory_bytes` | Declared wall-time and memory limits |
| `limits.max_input_bytes` | Entrypoint plus declared inputs |
| `limits.max_output_bytes` | Declared stdout plus stderr bounds |
| `limits.max_artifact_bytes` | The transported `.compute` artifact, including any dependency capsule |
| `allowed_os`, `allowed_architectures` | The executing platform |
| `allowed_output_classes` | `stdio` (always produced) and `files` (declared outputs) |
| `defaults` | See [below](#unset-versus-none) |

### Unset versus none

An absent field places no restriction from that policy. It never means
"allowed" on its own: the effective policy always includes the
[baseline](#baseline-policy), which states every dimension explicitly.

A present limit is a requirement to be bounded. If a policy sets
`max_timeout_ms` and the workload declares no timeout, admission fails with
`timeout_unbounded`; it does not treat the missing value as zero or as
acceptable. The same applies to memory and output.

The canonical execution contract always states `network` and `isolation`
explicitly. The workload default is `network: none` and `isolation: process`.
`defaults` apply only when Compute generates a workload (`compute run
./script.py`) and neither the caller, the flags, nor `compute.toml` chose a
value. Defaults never rewrite a stated value, and never apply to a
`WorkloadSpec` file or bundle.

### Validation

`compute policy validate FILE` and every load of a policy reject:

- unsupported versions (anything other than `"version": 1`, including the
  string `"1"`)
- unknown fields
- invalid runtime, isolation, or network names
- negative or zero limits
- `max_input_bytes` greater than `max_artifact_bytes`
- malformed distribution or dependency identities
- empty or duplicated allow-lists
- runtime versions for a runtime that is not allowed
- `defaults` that the same policy would deny

Validation happens before any evaluation or execution. A future policy
version is never interpreted as version 1.

### Identity

`policy_id` is the SHA-256 of the canonical serialization: fields in
declaration order, lists sorted, duplicates impossible. Reordering a
document keeps its identity, and any change to its meaning, including its
`name`, changes it.

```sh
compute policy validate production.json --json
```

## Baseline policy

When nothing is configured, Compute uses `compute-baseline@1`:

- all runtimes are allowed
- all network policies are allowed
- `minimum_isolation` is `process`
- both output classes are allowed
- defaults are `network: none` and `isolation: process`
- there are no limits

The baseline admits everything Compute can execute, so local execution needs
no policy file. It is still recorded: every admission and receipt names a
`policy_id`.

## Composition

Policies come from:

| Source | Where it is configured |
| --- | --- |
| Baseline | Always present |
| Local | `compute.toml` `[policy] path = "compute-policy.json"`. The file is found through `--config`, then `$COMPUTE_CONFIG`, then `./compute.toml` |
| Server | `compute serve --policy FILE`, or `[server.policy] path` in that `compute.toml` |
| Provider | The server policy a remote provider advertises in its capabilities |
| Environment | An environment's `policy`, for executions the Compute daemon runs in that environment ([docs/environments.md](environments.md#admission)) |
| Explicit | `--policy FILE` on `run`, `pool`, `placement`, `policy`, and `explain` |

The workload is not a policy source. Its requirements are the contract being
admitted.

The **effective policy is the intersection** of all applicable sources:

- allow-lists intersect
- limits take the minimum
- `minimum_isolation` takes the maximum
- defaults take the more restrictive value

The result is never less restrictive than any source, and source order does
not matter. The effective policy is unnamed; its `sources` list the
`policy_id` and label of each input.

```sh
compute policy inspect --json
```

The contract is never transformed to fit the policy. The effective execution
contract is:

> Workload requirements ∩ Provider capabilities ∩ Execution policy

If the workload asks for something the policy does not allow, the result is
a denial, not a rewritten workload.

## Policy changes

Every admission records the exact policy snapshot it evaluated. A provider:

- executes an admitted request only after re-evaluating it against **that
  snapshot** and reproducing the identical decision
- evaluates a new request against the current policy

A durable job stores its snapshot when it is accepted. If the server's policy
changes while the job is queued, the job still runs under the policy it was
admitted under. New submissions use the new policy. The `policy_id` in the
receipt makes this explicit.

## Policy is not authorization

Compute policy governs execution constraints. It has no concept of users,
identities, API keys, organizations, roles, tokens, or sessions. The provider
authorization boundary (`ProviderAuthorizer`) decides who may request an
operation; policy decides what execution constraints apply. Keep them
separate.

## Limitations

- The language is deliberately small, with no expressions, conditions, or
  per-identity rules.
- `max_output_bytes` constrains declared stdout and stderr bounds.
  Declared output files are governed by `allowed_output_classes`, not by
  size.
- Runtime version restrictions compare against the version string the
  executing runtime reports.
- `compute exec` is a non-portable diagnostic path with host mounts. It is
  not admitted; use `compute run`.

See [admission.md](admission.md) for decisions, identities, and evidence.
