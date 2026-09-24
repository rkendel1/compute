# compute

`compute` is a runtime-neutral execution engine written in Rust.
It exposes a single CLI and library interface for inspecting runtimes
and running workloads across WASM and common process-backed runtimes.

Each execution returns a machine-readable result with a unique
`execution_id`, lifecycle states, exit status, captured output, artifacts,
and structured errors. `compute run ... --json` and `compute exec ... --json`
expose this contract without mixing human diagnostics into the JSON stream.
Use `compute doctor --json` to inspect the execution capabilities available
on the current backend. Unsupported resource or network boundaries are
reported rather than silently downgraded.

## Universal runtime distribution

Compute has first-class adapters for WASM, Python, Node, Bun, Deno, Ruby,
PHP, JVM artifacts, .NET assemblies, native Linux executables, and declared
shell scripts. Compiled Go programs use the `native` runtime; Compute does not
compile Go source or install language packages during execution.

`distribution/runtime-lock.json` is the single version authority. Official
distributions resolve interpreters from their own `runtimes/` tree, validate
the exact manifest and detected version, and never fall back to `PATH`.
Source-tree builds may use installed host tools for development and label
them `host_development` in the inventory so this cannot be mistaken for an
official distribution.

```sh
compute runtimes
compute runtimes --json
compute doctor --json
```

Ruby (`.rb`), PHP (`.php`), JVM (`.jar`), .NET (`.dll`), shell (`.sh`), and
native Linux entrypoints select their corresponding adapter. JavaScript and
TypeScript are deliberately ambiguous among Node, Bun, and Deno and require
an explicit runtime when more than one is intended. Bun and Deno execute
TypeScript directly; pinned Node executes the erasable TypeScript syntax its
native type-stripping mode supports. Compute never runs npm during execution.

Deno permissions are generated from the workload contract: filesystem access
is scoped to the staged work, temporary, and output directories, environment
access is restricted to declared variables plus Compute directories, and
`--allow-net` is absent for `network: none` (or scoped for localhost). Compute
policy remains the outer authority.

## Portable workloads

A **workload** is a versioned, declarative description of what Compute should
execute. An **execution** is one concrete attempt to run that workload and has
its own ID, lifecycle, output, artifacts, and status. An **application** is a
higher-level concept that Compute does not own.

```text
WorkloadSpec
    ↓
Input Materialization
    ↓
Runtime Execution
    ↓
Output Collection
    ↓
ExecutionResult
```

Compute workload specifications are JSON, use schema version `1`, and contain
only portable execution intent. Entrypoints and inputs are resolved relative
to the workload file and cannot be absolute or traverse outside that context.
Environment inheritance remains disabled.

```json
{
  "version": 1,
  "runtime": "python",
  "entrypoint": "main.py",
  "args": [],
  "env": { "MODE": "test" },
  "inputs": [
    {
      "path": "data/input.txt",
      "source": { "type": "inline", "data": "hello" }
    }
  ],
  "outputs": [{ "path": "data/output.txt", "required": true }],
  "resources": {
    "timeout_ms": 30000,
    "stdout_bytes": 1048576,
    "stderr_bytes": 1048576
  },
  "network": "network"
}
```

Execute, inspect, or plan the same specification without creating another
execution path:

```sh
compute run --workload workload.json
compute inspect --workload workload.json --json
compute run --workload workload.json --dry-run --json
```

Inline inputs contain their bytes directly. File-backed input source paths are
resolved relative to the workload JSON file; absolute and traversing paths are
rejected. All destinations are materialized under `/work` in stable order.

Declared outputs are collected from `/output` in stable order and returned
with their bytes and sizes. Missing optional outputs are recorded without
failing execution. A successful workload that omits a required output becomes
an explicit `output_contract` failure, distinct from runtime and exit failure.
Each execution uses a fresh workspace, so repeated runs share no input,
output, or temporary files.

Compute workload specifications are not `.app` files. `.app` remains an
AppBoundry application artifact and is intentionally outside Compute.

## Portable workload bundles

A Compute bundle is a deterministic, self-contained transport artifact for a
workload. It contains the canonical `WorkloadSpec`, the entrypoint, and every
file-backed declared input. Inline inputs remain in `workload.json` and are not
```text
WorkloadSpec + entrypoint + declared inputs
                    ↓
          deterministic WorkloadBundle
                    ↓
              verify identities
                    ↓
          existing Compute execution engine
```

Bundles are uncompressed deterministic tar archives with canonical entry
ordering and headers:

```text
entrypoint/...
inputs/...
manifest.json
workload.json
```

The manifest records the format version, workload identity, bundle identity,
and SHA-256 metadata for every bundled file. Verification rejects unsupported
versions, identity or metadata mismatch, missing or unexpected inputs,
duplicates, traversal, absolute paths, non-file archive entries, and
non-canonical archive bytes before a runtime is selected or started.

```sh
compute bundle create --workload workload.json --output workload.compute
compute bundle inspect workload.compute --json
compute bundle verify workload.compute --json
compute run --bundle workload.compute
compute run --bundle workload.compute --dry-run --json
```

`workload_id` hashes only canonical workload semantics. `bundle_id` hashes the
workload plus the bundled entrypoint and file input bytes. Consequently, the
same workload with different input bytes keeps its `workload_id` and receives
a different `bundle_id`. Both can be pinned with `--expected-workload-id` and
`--expected-bundle-id`.

A `.compute` bundle is a transportable execution artifact, not an application
package. `.app` remains an AppBoundry artifact; Compute bundles do not replace
or overlap with AppBoundry `.app`.

## AppPort capability

The package in `packages/compute-appport` exposes the portable model as two
versioned `@appport/core` capabilities:

| Capability | Effect | Authorization | Behavior |
| --- | --- | --- | --- |
| `compute.inspect@1` | observation | public | validates and returns the workload plan |
| `compute.run@1` | consequential | `compute.run` | executes through the existing Compute engine |

`WorkloadSpec` remains the portable, deterministic description of the work.
The AppPort `ExecutionRequest` wraps that specification with requested
execution boundaries, environment, and invocation metadata. Local source
paths are invocation metadata and never affect the workload identity. The
identity is `sha256:` plus the SHA-256 digest of normalized deterministic
WorkloadSpec JSON; execution IDs, providers, machines, and workspace paths are
excluded.

The local provider is initialized in process and delegates to the `compute`
binary. It compares the transported specification with the local workload
bundle and pins execution to the inspected workload ID, while the Rust engine
continues to own validation, capability enforcement, materialization,
isolation, runtime execution, and declared-output collection.
`compute.inspect@1` and `compute.run@1` accept either the existing workload
request or portable bundle bytes. Bundle authorization still occurs in
AppPort before verification and execution; verified workload and bundle IDs
are pinned again when the provider invokes the engine.

```ts
import { permissionAuthorizer } from "@appport/authorization";
import { createComputeApplication } from "@compute/appport";

const compute = createComputeApplication({
  computeBinary: "/path/to/compute",
  authorizer: permissionAuthorizer(),
});

const manifest = compute.manifest();
```

Inspection is a public observation and does not grant execution authority.
Execution is marked consequential and reaches the handler only after the
injected AppPort authorizer grants `compute.run`, allowing AuthBoundry-compatible
policy without placing principals or permissions in the workload.

```text
create WorkloadSpec
        ↓
compute.inspect (no runtime invocation)
        ↓
authorize compute.run
        ↓
local AppPort provider
        ↓
existing Compute engine
        ↓
isolated workspace → declared outputs
```

AppPort results preserve output bytes and metadata and distinguish invalid
workloads, invalid execution requests, capability denial, input
materialization, runtime failures, and `output_contract` failures. Unexpected
files never become declared outputs.

## Runtime capability matrix

These declarations describe enforcement implemented by each adapter. A dash
is an intentional backend limitation, not a relaxation of Compute semantics;
requesting an unsupported network or resource boundary fails explicitly.

| Capability | WASM | Deno | Python / Node / Bun / Ruby / PHP / JVM / .NET / Native / Shell |
| --- | :---: | :---: | :---: |
| stdin / stdout / stderr | ✓ | ✓ | ✓ |
| environment isolation | ✓ | ✓ | ✓ |
| filesystem isolation | ✓ | ✓ | — |
| artifacts | ✓ | ✓ | ✓ |
| timeout | ✓ | ✓ | ✓ |
| cancellation | — | — | — |
| network: none | ✓ | ✓ | — |
| network: localhost | — | ✓ | — |
| network: network | — | ✓ | ✓ |
| stdout / stderr limits | ✓ | ✓ | ✓ |
| memory limit | ✓ | — | — |
| CPU / process limits | — | — | — |

Process adapters stage inputs in a private workspace and clear the inherited
environment, but an ordinary host process can still address host paths; they
therefore do not claim filesystem isolation. Availability is reported
separately from capability support. In an official distribution every bundled
runtime is pinned; a missing or mismatched payload is reported unavailable
with remediation instead of being replaced by a host interpreter.

The `compute-runtime-conformance` crate owns the shared behavioral suite used
by adapter tests. Runtime fixtures provide executable test workloads, while
argument, stdin, stream, exit, environment, artifact, lifecycle, ID, limit,
and timeout assertions remain identical across adapters. JVM, .NET, and native
fixtures are compiled before the execution test; compilation is never part of
the runtime path.

Compute is an execution substrate, not an application model, package format,
authorization system, durable state store, workflow engine, orchestration
layer, or cloud control plane. AppPort supplies the capability boundary;
Compute continues to own execution semantics. Execution workspaces are
temporary and are cleaned up after the workload finishes.

## Development

PAX is the preferred project-tooling interface for this repository. From
the repository root, use it to inspect and validate the Rust workspace:

```sh
pax info
pax doctor
pax build
pax test
pax lint
pax typecheck
```

PAX is an external developer tool and is not vendored by Compute. Compute
remains a standalone Rust project; PAX detects the workspace and delegates
Rust operations to Cargo. Cargo remains authoritative for Rust semantics,
and PAX is not a Compute runtime dependency.

See [DEVELOPMENT.md](DEVELOPMENT.md) for the complete inspection workflow
and the direct Cargo escape hatch.
