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

Compute is an execution substrate, not an application model, package format,
capability or authorization system, durable state store, workflow engine,
orchestration layer, or cloud control plane. Execution workspaces are
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
