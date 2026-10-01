# Execution capabilities: what exists, and what is not offered

This page states, for each execution-fabric capability, what Compute does
**today**, with a command you can run. It was written from the audit in
[audit-2026-09-30-celesto-capability-audit.md](audit-2026-09-30-celesto-capability-audit.md);
the audit holds the source references. Anything not implemented is listed under
"Not offered" and is not described elsewhere as if it were.

## Environment lifecycle

A persistent environment is created, started, stopped, and destroyed by Compute,
and an ephemeral one expires on its own ([lifecycle.md](lifecycle.md)).

```sh
compute environment create dev --recipe dev      # persistent: kept until destroyed
compute environment stop dev                     # state kept; every owned process confirmed gone
compute environment start dev                    # the same machine resumes
compute environment destroy dev                  # the record stays as evidence
```

`Ready` (up and idle) and `Running` (an execution is active) are distinct
session states; `stopped`, `failed`, `expired`, `unreachable`, `lost` are
reported, never guessed. A request never becomes a state until the target
confirms it (`destroying` → `destroyed`).

## Ephemeral execution

`compute run` is *execute → collect outputs and receipt → clean up* on every
ending: success, command failure, timeout, and cancellation. The workspace is
private and removed, and the whole process group is ended
(`crates/compute-runtime/tests/ephemeral.rs`).

```sh
compute run job.sh --timeout 5m --output result.txt --receipt run.receipt.json --json
```

For a temporary environment you run several commands in, use
`compute session open` (destroyed when closed or expired) or an ephemeral recipe
(`agent-task`, `ci`: a TTL, then torn down by Compute).

## Interactive and detached execution

Lifecycle is never inferred from a terminal: nothing in the CLI asks whether
stdin is a TTY (a guard test enforces it), so automation behaves the same
interactively and in CI.

| You want | Use |
| --- | --- |
| Run and wait, stdout/stderr/exit status | `compute run …` (`--json` for the result) |
| Standard input | `--stdin TEXT` (explicit; otherwise an immediate EOF, never the terminal) |
| Submit and return | `compute remote submit …` then `status` / `wait` / `result` |
| Run in a session without waiting | `compute session exec --detach SESSION -- cmd` |
| An interactive terminal | not offered ([below](#not-offered)) |

## Mounts

A host path enters an execution by **copy** into its private workspace.
Consequently host → guest is read-only by construction: nothing the guest does
can change the host. Results leave only as declared outputs.

```sh
compute run build.sh --mount ./src:work/src --output dist.tar
```

* The guest path is relative to the execution root (`work/…` is the working
  directory). `..` and drive/prefix components are refused
  (`invalid_mount_path`); symbolic links in a mounted tree are refused; a
  missing host path is an error.
* Each execution has its own copy; concurrent executions never see each other's
  changes. The copy is removed with the workspace.
* Tests: `crates/compute-runtime/tests/mounts.rs`.

## Network requirements

Network is a requirement of the workload, environment, and recipe:
`none` (the default for a workload), `localhost`, or `network`.

```sh
compute run job.sh --network none --isolation sandboxed
```
```json
{ "lifecycle": "ephemeral", "requirements": { "network": "none", "isolation": "sandboxed" } }
```

It fails closed. A profile that cannot be enforced on this host is refused
(`network_isolation_unavailable`), never run unrestricted: `none` and
`localhost` need network namespaces (Landlock/namespace enforcement under the
restricted and isolated host profiles, `compute isolation` shows what this host
can do), and a runtime that cannot confine the network says so in
`compute capabilities RUNTIME`. A domain or CIDR allow-list ("restricted") is
not offered.

## Checkpoint, restore, and fork

Filesystem state of an environment is captured as an immutable, content-addressed
checkpoint and restored into a *new* environment on a new machine
([checkpoint.md](checkpoint.md), [restore.md](restore.md), [fork.md](fork.md)).

```sh
compute environment checkpoint dev
compute environment restore CHECKPOINT dev-2
```

These are filesystem checkpoints, not VM or memory snapshots, and a restored
environment has fresh identity.

## Agent recipes

`agent-task` is the shipped agent recipe: ephemeral, 30 minutes, sandboxed,
network off.

```sh
compute recipe create review --from agent-task
```

A recipe states requirements only; it never holds credentials or software.
Per-agent recipes would be requirement-identical, so none are shipped.

## GitHub Actions runner

An ephemeral runner worker is available as an external adapter:
[github-actions-runner.md](github-actions-runner.md).

## Machine-readable output and `doctor`

[cli-contract.md](cli-contract.md): `compute doctor [--strict] [--json]`, the
`--json` failure envelope, stable error codes, and structured recovery.

<a name="not-offered"></a>
## Not offered

| Capability | Why not |
| --- | --- |
| Writable host mounts | They would bypass the staged-copy and declared-output model that makes an execution reproducible and its receipt verifiable |
| Restricted (allow-list) network | No runtime or target enforces a domain/CIDR allow-list, and Compute's network layer is ingress, not egress |
| Browser (Chromium/CDP) and desktop (display, keyboard, mouse, clipboard) | No target can advertise them, and capabilities are a closed, wire-versioned set; nothing is implemented |
| Interactive terminal (PTY) | The `terminal` capability is defined and `false` on every provider |
| VM or memory snapshots | Deliberately not a portable promise ([compute-capabilities.md](compute-capabilities.md)) |
