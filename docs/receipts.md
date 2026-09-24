# Verifiable execution receipts

`compute.receipt@1` is portable, hash-addressed evidence about one Compute
execution. It identifies the workload (and bundle, when present), exact Compute
distribution, selected and observed runtime, normalized policy, input and
output digests, and terminal outcome. It is evidence, not authorization,
signing, remote attestation, or durable storage.

## Creating and inspecting receipts

```sh
compute run --workload workload.json --receipt receipt.json
compute run --bundle app.compute --receipt receipt.json
compute exec ./main.py --runtime python --receipt receipt.json
compute receipt inspect receipt.json
compute receipt inspect receipt.json --json
```

`compute run --json` and `compute exec --json` also include the structured
receipt in the execution result. The execution engine constructs the evidence;
the CLI only writes its canonical bytes.

## Identity and canonicalization

Workload, bundle, distribution, runtime payload, executable, provenance, and
receipt identities use the explicit form `sha256:<64 lowercase hex digits>`.
The receipt hash is SHA-256 over compact UTF-8 JSON containing every receipt
field in schema order except `receipt_hash`. Arrays whose order is not
execution-significant (inputs, outputs, and environment names) are sorted.
Optional values are encoded as JSON `null`; integers use JSON decimal notation;
timestamps use chrono's RFC 3339 UTC representation. The portable file encoding
is the compact complete JSON object followed by one newline.

Verification rejects unknown fields, malformed digests, unsupported receipt
versions, noncanonical array or file encoding, inconsistent runtime identities,
distribution/provenance disagreement, and receipt-hash mismatches. An execution
ID and timestamps intentionally make each execution receipt unique. Consumers
seeking semantic equivalence should compare workload, bundle, distribution,
runtime, policy, input/output identities, and outcome while excluding those
execution-specific fields.

## Verification

```sh
compute receipt verify receipt.json
compute receipt verify receipt.json --distribution dist/compute-distribution
compute receipt verify receipt.json --artifacts exported-output
```

Basic verification is independent of the machine that executed the workload.
Distribution binding first verifies the local distribution and then requires
its immutable distribution ID to match the receipt. A missing distribution is
reported as `unavailable`, which is distinct from an invalid or mismatched
distribution.

Artifact verification safely resolves each collected logical output below the
supplied directory, checks byte length and SHA-256, and fails for missing or
modified referenced outputs. Extra files do not invalidate the receipt.

## Schema summary

- `receipt_version`, `execution_id`, and `receipt_hash`
- workload and optional bundle identities
- distribution ID, platform, and manifest schema version
- declared, selected, and observed runtime plus payload/executable identities
- sanitized request summary and effective execution policy
- requested/effective isolation profile and concrete boundary evidence
- sorted input and output identity records
- terminal lifecycle status, exit code, and typed failure classification
- start/finish timestamps and compact immutable provenance references

Receipts bind to the certified distribution ID from `runtime-manifest.json`.
Source-development runs use an explicitly development-scoped identity derived
from the Compute version, platform, and embedded runtime lock; they do not
claim to be a certified distribution.

## Security and compatibility

Receipts never include environment values, input contents, host absolute paths,
mount sources, or full command lines. They record environment names, logical
portable paths, byte sizes, counts, and content digests. Arguments and stdin
may contain secrets, so only their count/size and stdin digest are recorded.
Runtime error messages are reduced to stable typed codes.

Receipt versions are independent of workload, bundle, CLI, and distribution
versions. Verifiers fail closed unless the major version is exactly
`compute.receipt@1`. A future minor-compatible extension cannot change identity
semantics without an explicit version rule.
