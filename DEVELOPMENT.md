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
```

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
