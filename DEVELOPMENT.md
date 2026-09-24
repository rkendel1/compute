# Development

Compute is an independent Rust workspace. PAX is the repository's external
project-tooling interface: it observes the project and plans or delegates
development operations, while Cargo remains authoritative for Rust execution.

## Install PAX

Install the current `pax` executable using the instructions in the
[PAX repository](https://github.com/rkendel1/pax). Do not vendor the binary
or add PAX as a Cargo dependency. No Compute-specific PAX configuration is
required.

## Project inspection

Run these commands from the repository root:

```sh
pax info
pax doctor
pax deps
pax workspaces
pax graph
pax reality
pax drift
```

For machine-readable inspection output:

```sh
pax info --json
pax workspaces --json
pax graph --json
pax --json info
pax --json graph
pax --json reality
pax --json drift
```

These commands inspect and describe the project. They should not require a
running Compute service or runtime.

## Validation

PAX delegates the following operations to the native Cargo commands:

| PAX command | Native command |
| --- | --- |
| `pax build` | `cargo build` |
| `pax test` | `cargo test` |
| `pax lint` | `cargo clippy` |
| `pax typecheck` | `cargo check` |

Inspect a plan without executing Cargo by adding `--dry-run`:

```sh
pax build --dry-run
pax test --dry-run
pax lint --dry-run
pax typecheck --dry-run
pax build --dry-run --json
pax test --dry-run --json
pax lint --dry-run --json
pax typecheck --dry-run --json
```

The complete validation workflow is:

```sh
pax doctor
pax build
pax test
pax lint
pax typecheck
pax drift
```

Execution contract checks can also be exercised directly:

```sh
cargo test --workspace
compute doctor --json
cd packages/compute-appport && npm test
```

The shared adapter contract lives in `compute-runtime-conformance`. Its
failures include the runtime, test case, expected and actual behavior,
execution configuration, and the adapter's capability declaration. Process
runtimes are tested when their executable is installed. The suite builds JVM,
.NET, and native fixture artifacts before execution; Compute only executes the
resulting `.jar`, `.dll`, or Linux binary.

## Runtime distribution

Pinned runtime metadata lives only in `distribution/runtime-lock.json`.
`distribution/assemble.sh` validates a prepared payload tree, emits the
machine-readable distribution manifest, normalizes archive metadata, and
produces the input consumed unchanged by `distribution/Dockerfile`. See
`distribution/README.md` for the payload layout. Set `COMPUTE_HOME` to an
assembled root to exercise official fail-closed resolution locally; leaving
it unset permits explicitly labeled host-development discovery.

Portable workload checks cover versioned JSON parsing, deterministic
normalization, relative-path and symlink containment, input materialization,
capability planning, dry-run behavior, and execution through the same adapter
contract used by direct `compute run PATH` calls.

Bundle checks cover byte-for-byte reproducibility, distinct workload and
bundle identities, canonical archive headers and ordering, independent
verification, input and manifest tampering, duplicate/missing/unexpected
entries, archive traversal, expected identity enforcement, self-contained
execution after source deletion, and CLI create/inspect/verify/dry-run/run.

The AppPort package adds capability-level conformance for manifest discovery,
inspect/dry-run non-execution, authorization denial, deterministic workload
identity, inline and file-backed inputs, declared output bytes, output-contract
failure, unsafe paths, symlinks, destination conflicts, fresh workspaces, and
WASM/Node/Python/Bun workload and bundle execution when each runtime is
installed.

The JSON execution contract includes `execution_id`, `runtime`, `network`,
`lifecycle`, terminal `status`, exit information, output, artifacts, and
structured errors. Backend capability reports are authoritative: a
capability marked unsupported causes a request requiring it to fail.

## Cargo escape hatch

When direct native behavior is needed for debugging or contributor-specific
work, Cargo remains available:

```sh
cargo build
cargo test
cargo clippy
cargo check
```

Using Cargo directly does not change the PAX boundary.

## Independence invariant

The Compute workspace consists of the crates listed in the root
`Cargo.toml`. None of those crates may depend on, import, link against,
invoke, or otherwise require `pax` at build time or runtime. PAX may inspect
Compute, but Compute must remain usable with the native Rust toolchain alone.
