# Compute isolation profiles

Compute's version 1 isolation model makes execution boundaries explicit. A
profile is resolved from the workload requirement, any strengthening CLI
override, the selected runtime descriptor, and the requested network/resource
policy. An incompatible request is rejected before staging or runtime startup;
Compute never selects another runtime or silently weakens the profile.

## Profiles

### `process`

The compatibility baseline provides a private Compute workspace, a cleared and
explicit environment, managed standard streams, declared output collection,
and timeout handling where requested. **Process execution is not a security
sandbox.** Python, Node, Bun, Ruby, PHP, JVM, .NET, native Linux, and shell can
still address host paths because changing the working directory is not a
filesystem security boundary. Their evidence therefore reports filesystem
`unavailable`.

### `sandboxed`

Requires runtime-enforced filesystem, network, environment, and timeout
boundaries. WASM supports this through restricted WASI capabilities. Deno is
conditional: Compute derives `--allow-read`, `--allow-write`, `--allow-env`,
and `--allow-net` flags from the request, and rejects policies or resource
limits its adapter cannot enforce. Ordinary process adapters do not qualify.

### `strict`

`strict` represents the strongest isolation guarantees Compute can actually
enforce for the selected runtime and requested policy; it is not equivalent to
a VM or container boundary. It requires the sandboxed boundaries and rejects
every requested memory, CPU, process-count, timeout, or network limit that the
runtime cannot enforce. WASM is the reference implementation. Deno is
conditional because it cannot enforce every resource limit.

## Runtime profile matrix

The authoritative machine-readable matrix is generated from runtime
descriptors:

```sh
compute isolation --json
compute runtime wasm --json
compute runtime python --json
```

| Runtime | Process | Sandboxed | Strict |
| --- | :---: | :---: | :---: |
| WASM | yes | yes | yes |
| Deno | yes | conditional | conditional |
| Python, Node, Bun, Ruby, PHP, JVM, .NET, native Linux, shell | yes | no | no |

Conditional means compatibility depends on the requested network and resource
policy; the execution plan is authoritative.

## Workloads, bundles, and overrides

The optional WorkloadSpec field participates in workload and bundle identity:

```json
{
  "version": 1,
  "runtime": "wasm",
  "entrypoint": "module.wasm",
  "network": "none",
  "isolation": { "profile": "strict" }
}
```

Omission preserves compatibility and means `process`. A CLI override may
strengthen a workload or bundle requirement. For example, `process` to
`strict` is allowed if the runtime satisfies it, while a bundle declaring
`strict` cannot be run with `--isolation process`.

Dry-run returns a structured plan without executing:

```json
{
  "isolation": {
    "requested": "strict",
    "effective": null,
    "compatible": false,
    "reason": { "code": "filesystem_isolation_unavailable" }
  }
}
```

## Evidence

Successful `ExecutionResult` and `compute.receipt@1` objects contain identical
isolation evidence. Boundary states are `enforced`, `disabled`, `unavailable`,
or `not_requested`; ambiguous booleans are not used. Isolation evidence is part
of canonical receipt identity, so changing it changes the receipt hash.

WASM preopens only `/work`, `/tmp`, and `/output`, passes only declared
environment variables plus Compute workspace variables, provides no network
capability, applies epoch deadlines, and applies supported memory limits.
Conformance fixtures probe parent traversal, absolute host paths, undeclared
environment, workspace access, output access, timeouts, and resource limits.

Isolation is separate from authorization. AppPort authorizes the caller before
Compute resolves and enforces the technical isolation contract. No profile
grants authority, and Compute has no AuthBoundry dependency. Docker remains a
distribution environment, not an isolation runtime selected by Compute.
