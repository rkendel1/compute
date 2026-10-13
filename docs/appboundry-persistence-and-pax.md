# AppBoundry persistence and PAX evidence: disposition

Status: **both are blocked, with separate narrowly-scoped changes proposed.** Nothing here is implemented, and nothing
is relabelled: JSON records are not FeltDB-authoritative state, and no check below is described as PAX verification.
Facts come from reading `rkendel1/appboundry@4e50aeb` and `rkendel1/pax@31b4871` (0.5.0); line numbers are in
`packages/cli/src/commands.ts` unless noted.

## 1. AppBoundry persistence

### What is true today

The AppBoundry host (`appport host add|start`, the process serving `/apps/<selector>/mcp` and the capability HTTP
routes) keeps its application state and its invocation records in JSON files under `APPPORT_RUNTIME_DIR`:

| Datum | Where | Written by |
|---|---|---|
| `AppPort/application-invocation/1` records, usage records, verification and deployment records, idempotency effects, collections (todos), host lifecycle/policy | `state/<bundleKey>.json` (`AppPort/application-state/1`) | `appendInvocationRecord` (4444), `appendUsageRecord` (4451), `applyPortableStoragePlan` (3935), `writeExternalState` (4299), `writeDurableHostAuthority` (4354) |
| Catalog, runtime, provider bindings, idempotency ledger | `host/catalog.json`, `runtime/<bundleKey>.json`, `host/provider-bindings.json`, `host/idempotency.json` | various |

- Writes are a whole-file read-modify-write with an atomic rename (`writeJsonFile`, 4368): no fsync, no lock, no
  compare-and-swap, so concurrent processes can lose updates. `appendVerificationRecord` (4530) is a plain, non-atomic
  `writeFile`.
- Only `completed` invocations get a record. A failure writes a usage record and nothing else.
- Nothing signs or hash-chains records. They are authority by convention. (Deployment activation does recompute
  provenance from the bundle, `deploymentProvenanceVerified` 3697, but invocation records have no such check.)
- The host does **not** use `ExecutionEngine`, `LedgeredExecutionEngine`, `ExecutionLedger` or
  `ApplicationStateTransaction`. It instantiates WASM directly (`invokePackagedWasm`, 3895) with its own storage-plan
  format. `packages/cli` does not depend on `@feltdb/appport`; the string "feltdb" in the host is only a provider-kind
  allowlist.
- A FeltDB-backed path **exists as a library** (`integrations/feltdb`): `FeltDbExecutionLedger` (collection
  `AppPortState`, write-once `begin`, `complete` with `expectedVersion`, atomic `completeWithState`), authorization,
  operation and registration stores, and `RealFeltDbRuntimeAdapter.create({path|server})`. It is proven by
  `integrations/feltdb/test` (`pr28-execution-ledger`, `pr33-crash-consistency`, `feltdb-restart`, …). No environment
  variable or configuration selects it for the host.

So the statement "FeltDB remains the durable application authority" is true for Attn's operation state and is **not
true for the AppBoundry host's application state or invocation records.**

### Smallest correct change (separate PR, in AppBoundry)

1. A persistence seam for the operations the host already performs (read/write external state, append invocation and
   usage records, deployments, idempotency effects, lifecycle and policy, `durableApplicationStates`). The existing JSON
   functions become the default backend, so behaviour without configuration is unchanged.
2. A FeltDB backend on `RealFeltDbRuntimeAdapter`, selected once at host start by explicit configuration. The invocation
   record is written with `requireAbsent` keyed by `operationId` **in the same transaction** as the state mutation and
   the idempotency effect, replacing the read-modify-write at 4444/3974-4020.
3. With FeltDB selected, JSON is not written: **no dual write, no second source of truth.**

Decisions the owner must make first: migration of existing `state/*.json` (one-time import with a marker, refuse to
start on a mix, or read-only legacy; not dual-write); whether the catalog/runtime/idempotency JSON also moves
(otherwise `currentDeploymentId` is still JSON-authoritative); document granularity and `expectedVersion` use;
whether `recoverIdempotentInvocation` (3745) stays; whether to adopt `ExecutionLedger.begin/complete` so running and
failed executions are recorded (needs a binding and authority wiring the host lacks, a larger change).

A contract test for that PR must show: after `host add`, `start`, invoke and a restart, the record (operationId,
capability, input, output, `artifactHash`, `instantiatedArtifactHash`, `executionMode`, `authority`) is readable from
FeltDB by a fresh adapter in a second process, and **no `state/<bundleKey>.json` is created or modified**; the same todo
ids and idempotency effect replay across restart; N parallel invocations from two processes are all persisted; crash
injection leaves no phantom record and loses none; and the default (no configuration) suites stay green.

## 2. PAX evidence for an AppBoundry invocation

### What PAX can do today (0.5.0)

- Observation schema `pax.observation.v1`: facts with provenance strength **declared | observed | resolved** only;
  `verified` is never emitted and does not exist in the schema (`docs/PAX_OBSERVATION.md`).
- Scopes: `project`, `crate:`, `module:`, `file:<path>`, `path:<prefix>`; `file:`/`path:` accept Rust sources and
  manifests only. Observation is Rust-only (`syn`, `cargo metadata`); other ecosystems get `artifact.exists` and a
  `source_structure_unsupported` diagnostic.
- No JSON evidence ingestion, no hashing or digest verification, no `verify` command.
- `pax.execution-result.v1` reports PAX's own delegated `pax test`; it cannot carry a foreign operation's outcome.
- `docs/PAX_BOUNDARY.md`: PAX has no knowledge of AppPort or Compute, runs nothing under `observe`, writes nothing.

A `.app` directory (`manifest` plus `application.wasm`) is not a PAX "project", and `compute.receipt@1`'s `project` block
describes a project Compute ran, not an AppBoundry invocation.

### What an invocation produces, and what can be independently checked

| Item | Recomputable offline from the bundle? |
|---|---|
| `application.wasm` sha256 vs `manifest.artifact.hash` | Yes (AppBoundry already does this at load: `verifyPackagedWasm`) |
| `packageIdentity.contentAddress`, contract fingerprint vs manifest | Yes (`resolveApplicationPackageMetadata`, 1386) |
| Record's `artifactHash`, `contractFingerprint`, `applicationId` vs manifest | Yes |
| `executionUnitId` linking the MCP `_meta["appboundry/evidence"]` to the persisted record | Referential only |
| AuthBoundry decision ids in `authority` | Needs a live AuthBoundry evidence call and an operator session |
| That the WASM actually ran | **No.** Host-asserted |
| That `output` came from `input` and that module | **No.** Host-asserted |
| That the record was not edited afterwards | **No.** No signature, no hash chain |

Two gaps in the *shape*: the MCP evidence meta lacks `operationId` and `instantiatedArtifactHash` (it joins the record
only by `executionUnitId`), and failures leave no invocation record at all.

### Minimal contract that would make a narrow claim true

The claim it could support: *"record R (operationId X, executionUnitId U) names artifact A, and the bundle's bytes hash
to what R and the manifest state."* It would **not** prove the WASM ran or that the record is unaltered; only
re-execution by a verifier with the same bytes and input, or a host attestation (none exists today), can.

- **AppBoundry (owner of the evidence):** add `operationId` and `instantiatedArtifactHash` to
  `_meta["appboundry/evidence"]` (additive); document a stable `AppBoundry/invocation-evidence/1` document; optionally an
  offline `app verify-evidence <bundle> <record>` reusing the existing recompute code.
- **PAX (owner of observation):** an opt-in, read-only, bounded `bundle:<dir>` scope (optionally `--evidence <json>`)
  emitting only `observed` and `resolved` facts (`artifact.sha256` resolved by recompute, and a resolved fact for
  record-to-manifest match, with a diagnostic on mismatch). No `verified` strength. PAX must not interpret what the
  outcome means.

Anchors: `packageIdentity.contentAddress` and `artifactHash` for the bundle; `operationId` / `executionUnitId` for the
invocation. `AppPort/application-verification/1` and deployment provenance attest a *conformance run*, not a per-invocation
execution, so they are the wrong anchor.

### Disposition

Attn therefore reports `pax: { status: "not_supported" }` and words its "verified" as "the application reported success
with complete evidence; not independently verified by PAX". That stays until the two changes above exist and a test
shows PAX recomputing a digest from a bundle. **PAX verification is a blocker for full-stack certification.**
