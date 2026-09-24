# Portable dependency capsules

A dependency capsule is an immutable, content-addressed archive containing a
pre-resolved dependency payload. Its format is `compute.deps@1` and its
identity is `sha256:<digest>` over the runtime and platform bindings,
provenance, inventory, file manifest, permissions, and file contents.

Compute consumes capsules; it does not resolve dependency graphs. Use pip, uv,
npm, pnpm, Bundler, Maven, NuGet, or another external tool to populate a clean
directory first. Compute never invokes those tools during workload execution.

## Create, inspect, and verify

Package an externally resolved directory and retain lockfile provenance:

```sh
compute deps create \
  --runtime python \
  --resolved .resolved/python \
  --lock requirements.lock \
  --package requests=2.32.5 \
  --output python.deps

compute deps inspect python.deps
compute deps verify python.deps
```

`--package NAME=VERSION` adds deterministic SBOM-style inventory metadata. It
does not install or resolve that package. `--runtime-version` and `--platform`
can add stricter bindings when constructing artifacts for another machine.

Archives have canonical ordering, zero timestamps and ownership, normalized
permissions, explicit SHA-256 metadata, and no absolute host paths. Identical
inputs produce byte-identical capsules. Verification rejects modified or
non-canonical archives, unsafe and case-colliding paths, symlinks, unsupported
versions, and runtime/platform mismatches.

## Execute and embed

Attach a capsule to direct execution:

```sh
compute run script.py --deps python.deps
compute run app.js --runtime node --deps node.deps
```

The generated `WorkloadSpec` contains only the immutable capsule identity:

```json
{
  "dependencies": {
    "capsule": "sha256:..."
  }
}
```

The path is never part of portable workload identity. A project may configure
the same convenience:

```toml
[run]
runtime = "python"
entrypoint = "src/main.py"

[dependencies]
capsule = "python.deps"
```

Embed the verified capsule in the ordinary `.compute` bundle:

```sh
compute bundle create script.py --deps python.deps --output script.compute
compute run --bundle script.compute --offline
```

The bundle remains the portable workload artifact. Its identity covers the
embedded capsule bytes, while the workload identity covers the capsule ID.
Offline execution requires no package manager, cache, registry, or network
lookup.

## Referenced capsules

An explicit workload may reference a capsule without embedding it. Place the
verified artifact in a separate content-addressed cache and point Compute at
that cache:

```text
$COMPUTE_DEPENDENCY_CACHE/<64-character-sha256-digest>.deps
```

If the exact artifact is absent, execution fails with `dependency_missing`.
Compute never falls back to a project directory, host site-packages,
`node_modules`, gem cache, Maven cache, or NuGet cache.

## Isolation and runtime environments

Capsules are verified before materialization and written only beneath the
fresh execution workspace. Python receives an isolated import path with user
and system site-package discovery disabled; Node receives a capsule-only
module path; Ruby receives capsule `GEM_HOME`/`GEM_PATH`; JVM receives a
capsule classpath; and .NET receives a capsule package root. The capsule does
not broaden filesystem or network policy.

Execution results and `compute.receipt@1` record the capsule identity and
verification status, binding dependencies to the workload, distribution,
runtime, isolation evidence, and execution outcome without copying the full
dependency inventory into every receipt.

Capsules are platform and runtime specific. Compute rejects mismatched OS,
architecture, runtime, and explicitly declared runtime versions; it does not
translate artifacts or download replacements.
