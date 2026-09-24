# Compute AppPort provider

This package publishes `compute.inspect@1` and `compute.run@1` through
`@appport/core`. The local provider delegates validation, planning, isolation,
runtime execution, and output collection to the existing `compute` binary.

```ts
import { permissionAuthorizer } from "@appport/authorization";
import { createComputeApplication } from "@compute/appport";

const compute = createComputeApplication({
  computeBinary: "/path/to/compute",
  authorizer: permissionAuthorizer(),
});

const manifest = compute.manifest();
```

`compute.inspect` is a public observation and never runs the workload.
`compute.run` is consequential and requires the `compute.run` authorization
scope. Passing an AuthBoundry-compatible AppPort `Authorizer` at provider
initialization keeps identity and policy outside `WorkloadSpec`.

Inspection plans expose structured isolation compatibility and stable rejection
codes. Authorized execution responses return the effective boundary evidence
both directly and in the execution receipt. Isolation remains a technical
runtime contract and does not change AppPort authorization semantics.

An operation input contains an `ExecutionRequest`: the portable
`WorkloadSpec`, requested execution boundaries, and invocation metadata. The
local provider uses `invocation.workload_path` to resolve the workload source file,
verifies that file against the transported specification, and pins execution
to its deterministic workload ID.

The same operation versions also accept a bundle request containing portable
`.compute` bytes and optional expected workload and bundle IDs. The provider
writes those bytes only to a private temporary adapter directory, delegates
verification and execution to the Rust engine, pins both verified identities,
and removes the adapter directory afterward.

## Provider pools and placement

`compute.provider.list@1`, `compute.provider.inspect@1`,
`compute.provider.capabilities@1`, and `compute.placement.inspect@1` are
public observations and never execute. `compute.pool.run@1` requires the
`compute.run` scope, and `compute.pool.submit@1` requires `compute.submit`.
Placement is evaluated by the Compute CLI against a caller-owned pool.
Configure the pool with the `poolConfig` and `capabilityCache` options:

```ts
const compute = createComputeApplication({
  computeBinary: "/path/to/compute",
  poolConfig: "compute-pool.toml",
});
```

Pool results carry the full placement report. Their receipts bind the
placement ID, provider ID, and selection mode. A failed placement returns a
`placement_failed` failure and executes nothing.
