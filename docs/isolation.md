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

## Host profiles for process runtimes

Process runtimes (Python, Node, Bun, Ruby, PHP, JVM, .NET, native Linux,
shell) can additionally declare an operating-system boundary, enforced by
the kernel:

```json
{ "runtime": "python", "entrypoint": "main.py", "network": "none",
  "isolation": { "host": "isolated" } }
```

| Host profile | For | Filesystem | Network | Memory, CPU, processes |
| --- | --- | --- | --- | --- |
| `trusted` (default) | Operator-controlled code | Unrestricted | Unrestricted (`network` only) | Declared limits only |
| `restricted` | Normal application workloads | Reads the host, writes only its workspace (Landlock) | `none` or `localhost`: its own network namespace (loopback only for `localhost`), and for `none` TCP denied by Landlock too; `network`: the host's | Declared limits (cgroups, rlimits) |
| `isolated` | Untrusted workloads | Reads only its runtime, system libraries, certificates, and its workspace; writes only its workspace (Landlock) | As `restricted` | Always limited: 1 GiB memory, 100% of one CPU, 256 processes unless declared lower (cgroups) |

Both confined profiles also set `no_new_privs`, disable core dumps, and,
through Landlock, prevent the workload from inspecting processes outside
its domain (for example another process's environment).

**A profile is enforced or refused, never downgraded.** Compute detects
what the host can enforce (Landlock ABI, network namespaces, cgroup v1 or
v2 controllers, rlimits) and, for each profile, reports every dimension
as `enforced`, `restricted`, `unrestricted`, `not_requested`, or
`unsupported`:

```sh
compute isolation            # the table, with this host's capabilities
compute isolation --json     # .host.capabilities and .host.profiles
compute doctor               # the same, as the controller sees it
```

A workload that asks for a profile, or a dimension, this host cannot
enforce is refused before it starts, with the reason. `trusted` cannot
honour `network: none` or `localhost` and says so; it never runs such a
workload with the host's network. Receipts and execution results carry
the enforcement that applied under `isolation.host`, per dimension, with
the mechanisms used. A host profile does not make a runtime `sandboxed` or
`strict` in the model above: those remain runtime-enforced profiles, and
a process runtime is only ever labelled with what the kernel enforced.

Landlock and cgroups are Linux features. On other hosts, `restricted` and
`isolated` are reported `unsupported` and refused.

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
