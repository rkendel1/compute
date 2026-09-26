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

Start with an existing file—no workload JSON is required:

```sh
compute run script.py
compute run --runtime node app.js -- --port 8080
compute run script.py --explain
compute bundle create script.py --output script.compute
```

These commands generate the canonical `WorkloadSpec` and use the existing
engine. See [docs/getting-started.md](docs/getting-started.md) for project
configuration, inputs, outputs, environment, policy, dry runs, and receipts.

Or create an application and deploy it. Compute resolves its requirements,
places it on a provider that can satisfy them and host it (this machine, or
another node's Compute daemon in your pool), and returns a stable endpoint,
versions, and receipts:

```sh
compute init my-app
compute deploy my-app
compute status my-app
compute logs my-app --follow
compute deploy my-app          # v2, same endpoint
compute rollback my-app 1      # v3, v1's code
compute history my-app
compute stop my-app
```

An application also travels as one portable artifact, which a provider
fetches and verifies itself; agents do the same through AppPort
(`compute.application.*`):

```sh
compute application pack my-app --output my-app.capp
compute application deploy https://artifacts.example/my-app.capp --json
```

See [docs/applications.md](docs/applications.md) and the
[HTTP demo](examples/compute-demo/).

Resolved dependencies can be packaged separately as deterministic,
content-addressed `compute.deps@1` capsules and embedded in the same `.compute`
bundle. See [docs/dependencies.md](docs/dependencies.md).

Compute executes the same portable workload locally or remotely. The workload
does not know where it runs: a provider supplies transport, the Compute runtime
supplies execution, and the receipt proves what happened. See
[docs/providers.md](docs/providers.md) and
[docs/remote-execution.md](docs/remote-execution.md).

```sh
compute run script.py
compute remote run --provider dev script.py
compute remote submit --provider dev --bundle script.compute
compute remote status --provider dev job_...
compute remote wait --provider dev job_... --timeout 60s
```

Durable jobs are documented in [docs/jobs.md](docs/jobs.md).

Providers can be grouped into a caller-owned pool. Placement selects the
provider that can prove it satisfies the workload contract. Compatibility
comes first, then priority, then provider ID. It never substitutes runtimes,
distributions, dependencies, isolation, or network policy, and it never
retries on another provider. See [docs/provider-pools.md](docs/provider-pools.md)
and [docs/placement.md](docs/placement.md).

```sh
compute provider list
compute placement explain ./script.py
compute run ./script.py --provider auto
compute run ./script.py --provider provider:production
compute runtimes --provider production
compute pool submit --bundle script.compute --json
```

A provider may advertise a pinned runtime as `available` before it is
installed. Compute resolves, acquires, verifies, and prepares that exact
distribution before admission, then records its distribution ID, artifact
digest, payload identity, and executable identity in the receipt. Node 24 is
joined by catalog-managed Python 3.13, Deno, and Bun distributions on Linux
x86-64 and arm64. Workload version constraints resolve to exact catalog
versions; distributions and receipts never use aliases.

Capability asks "can it run?"; policy asks "may it run?". Compute executes
only when both answers are yes. Every execution is admitted by a
deterministic, fail-closed `compute.policy@1` evaluation. With nothing
configured, the evaluation uses the documented baseline policy. Receipts
record `policy_id`, `admission_id`, and `admission_status`. See
[docs/policy.md](docs/policy.md) and [docs/admission.md](docs/admission.md).

```sh
compute policy validate production.json
compute policy check ./script.py --policy production.json
compute explain ./script.py
compute serve --policy production.json
```

Compute also operates long-running software. A persistent daemon holds
**environments** (such as `staging` and `prod`), each containing
**projects**, each containing **workloads**: services that stay running and
tasks that run to completion. Environments are isolated from each other in
filesystem, configuration, policy, ports, and processes. Stopping a child
never stops its parent or its siblings. The CLI, AppPort, and the UI are
all clients of one Compute API, and the daemon serves a control-plane UI.
Desired state is durable: in a local file, or in Managed FeltDB as the
control-plane authority. The daemon reconciles toward it, and restores it
after a crash or on a new node. Releases have zero downtime: a new revision
starts beside the old one, traffic moves only once it is ready, and the old
one drains, with rollback and a resumable, durable state machine. Domains get
DNS records at their provider and ACME certificates. See
[docs/environments.md](docs/environments.md), [docs/daemon.md](docs/daemon.md),
[docs/control-plane.md](docs/control-plane.md), [docs/releases.md](docs/releases.md),
and [docs/networking.md](docs/networking.md).

Workloads run on the node's supervisor, so they keep serving while the
controller restarts, crashes, or is upgraded (`compute node upgrade`, with
automatic rollback). A controller whose FeltDB is unreachable keeps
everything running and refuses changes until it returns. The remote API
needs TLS and scoped operator credentials in production (`compute auth`),
and `compute doctor` diagnoses the whole node. See
[docs/architecture.md](docs/architecture.md) for the invariants, and
[docs/hardening-audit.md](docs/hardening-audit.md) for what was measured.

```sh
compute start --detach
compute environment apply compute.environment.toml
compute environment status staging
compute workload run authboundry/migrate --environment staging
compute project restart authboundry --environment staging
compute deploy authboundry --environment preprod --source ./authboundry --revision abc123 --wait
compute deploy authboundry --from preprod --to production --wait
compute deployment rollback dep_…
compute domain add app.example.com --environment production --project authboundry
```

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

The canonical builder resolves the current OS and architecture, downloads
only the exact locked artifacts, verifies their SHA-256 digests even when
they come from the local cache, launches each installed runtime, and emits a
deterministic manifest, inventory, identity, and tar archive:

```sh
cargo build --release --locked
target/release/compute distribution build --output dist/compute-distribution
target/release/compute distribution build --output dist/verified --verify
target/release/compute distribution build --output dist/offline --offline
compute distribution inspect dist/compute-distribution --json
compute distribution verify dist/compute-distribution --json
```

The catalog includes pinned `linux-x86_64` and `linux-aarch64` distributions
for Node, Python, Deno, and Bun; unsupported OS
or architecture combinations fail explicitly. See `distribution/README.md`
for cache, offline, reproducibility, certification, and Docker details.

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

## Compute certification

The validation layers answer different questions:

- `cargo test` tests the implementation in the source workspace.
- `compute doctor` reports whether the current runtime environment is healthy.
- `compute certify` tests the assembled artifact that users actually receive.

Certification refuses to run from `cargo target/` or a source checkout. It
loads the distribution's embedded runtime lock and fixture manifest, re-runs
itself with controlled `PATH`, `HOME`, `TMPDIR`, locale, and `COMPUTE_*`
variables, and places deliberately broken host-runtime names first on `PATH`.
Every locked runtime must then self-identify and execute the same semantic
input/output workload through a verified `.compute` bundle. Certification
also checks expected workload and bundle identities, stdin, arguments,
environment clearing, stdout/stderr, exit status, timeout enforcement, and
authorized/unauthorized `compute.run@1` behavior through AppPort.

```sh
compute certify
compute certify --json
COMPUTE_REQUIRE_ALL_RUNTIMES=1 compute certify --json
```

The last command is the release gate. It cannot produce a successful partial
report: a missing fixture, runtime, exact version, library, output, or AppPort
check makes certification fail. The reusable distribution-certification CI
workflow runs this gate against bare Linux and then against the Docker image
assembled from the same directory.

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

Directory workloads may declare the portable runtime and placement resources
in `compute.toml`:

```toml
[runtime]
name = "node"
version = ">=24"
architecture = "arm64"

[resources]
cpu = 2
memory = "2GiB"
disk = "5GiB"
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

## Verifiable execution

Every executed workload includes a `compute.receipt@1` evidence object in JSON
output. The same canonical receipt can be written as a portable artifact:

```sh
compute run --workload workload.json --receipt receipt.json
compute receipt inspect receipt.json
compute receipt verify receipt.json
```

A Compute receipt lets another system independently verify what workload ran,
under which exact Compute distribution and runtime, with which input and output
digests, and what happened. Bind verification to local evidence with
`--distribution <path>` and re-hash collected outputs with `--artifacts <dir>`.
Receipts contain environment names but never their values. See
[docs/receipts.md](docs/receipts.md) for the format and security model.

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

## Explicit isolation profiles

Compute resolves a requested isolation profile against the selected runtime's
actual filesystem, network, environment, timeout, and resource capabilities
before execution:

```sh
compute isolation
compute isolation --json
compute run --workload workload.json --isolation strict --dry-run --json
```

`process` is the backwards-compatible baseline and is **not a security
sandbox**. `sandboxed` requires runtime-enforced filesystem, network, and
environment boundaries. `strict` requires the strongest enforceable Compute
boundaries for the requested policy and fails closed when any requested limit
cannot be enforced; it is not a VM or container boundary. Results and receipts
record requested/effective profiles and concrete boundary states. See
[docs/isolation.md](docs/isolation.md).

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
