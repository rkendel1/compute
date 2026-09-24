# compute

`compute` is a runtime-neutral execution engine written in Rust.
It exposes a single CLI and library interface for inspecting runtimes
and running workloads across WASM and common process-backed runtimes.

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
