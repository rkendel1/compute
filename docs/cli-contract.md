# The command-line contract

**Status:** implemented (`crates/compute-cli/src/contract.rs`; tests
`crates/compute-cli/tests/contract.rs`).

Every command that has `--json` keeps its own success output: existing
consumers depend on those shapes. What is uniform is the **failure**, and the
commands that adopted the full envelope.

## The envelope

```json
{
  "ok": true,
  "command": "compute doctor",
  "exit_code": 0,
  "data": {},
  "error": null
}
```

| Field | Meaning |
| --- | --- |
| `ok` | the command succeeded (exit status 0) |
| `command` | the subcommand path, never the operands |
| `exit_code` | the process exit status |
| `data` | the command's result, `null` on failure |
| `error` | `null`, or `{ "code", "message", "recovery"? }` |

### Failures

stdout belongs to a command's **result**, and existing consumers parse it (a
failed `--json` command prints nothing there, and one that reports a failed
result, like `compute certify`, prints exactly one JSON document). So a command
run with `--json` that fails with an error writes the envelope as the **last
line of stderr**, after the unchanged human message; the exit status is `1`.

```sh
$ compute environment inspect web --json     # no controller running
runtime error: controller unavailable: cannot reach the Compute daemon at 127.0.0.1:1 (…); start it with `compute start`
{"command":"compute environment inspect","data":null,"error":{"code":"controller_unavailable","message":"runtime error: controller unavailable: …","recovery":{"command":"compute start"}},"exit_code":1,"ok":false}
```

```sh
compute environment inspect web --json 2>&1 >/dev/null | tail -n 1 | jq .error.code
```

A result that *is* a failure (`doctor --strict`, `worker github-actions run`
when the runner fails) is the command's own output, so its envelope is on
stdout with `ok: false`.

`error.code` is a stable identifier: codes are added, never renamed.
Controller errors keep their kind (`not_found`, `conflict`, `admission_denied`,
`authentication_failed`, `authorization_denied`, `runtime_unavailable`,
`controller_unavailable`, `state_unavailable`, …); the rest come from
`ComputeError::code()` (`invalid_workload`, `invalid_mount_path`,
`isolation_unavailable`, `unknown_runtime`, …). A command that already reports
an execution result (`compute run --json`) keeps doing so; a non-zero exit
status there still reflects the workload.

`error.recovery.command` is present only where exactly one action is
deterministic:

| Code | Recovery |
| --- | --- |
| `controller_unavailable` | `compute start` |
| `runtime_unavailable`, `unknown_runtime` | `compute doctor` |

Nothing else claims a recovery.

## `compute doctor`

```sh
compute doctor                 # report; exit 0 whatever it finds
compute doctor --strict        # exit 1 unless every check passes
compute doctor --json          # the envelope, plus the original keys
compute doctor --runtimes-only # this host's runtimes only; no controller
```

`--json` keeps `runtimes` and `controller` exactly as before and adds the
envelope and a flat list of checks:

```json
{
  "ok": true, "command": "compute doctor", "exit_code": 0, "error": null,
  "data": {
    "strict": false,
    "healthy": false,
    "checks": [
      { "id": "runtime:python", "status": "pass", "detail": "3.12.3", "remediation": null },
      { "id": "runtime:ruby", "status": "fail", "detail": null, "remediation": "install ruby …" },
      { "id": "controller", "status": "fail", "detail": "http://127.0.0.1:8484",
        "remediation": "start the controller with `compute start`" }
    ]
  },
  "runtimes": [ … ], "controller": { … }
}
```

`status` is `pass` or `fail`; a failing check carries `remediation`. Without
`--strict`, `ok` stays `true` (the command ran) and `data.healthy` says whether
everything passed. With `--strict`, any failing check makes `ok` false, the exit
status `1`, and `error.code` `doctor_checks_failed`; `data` is still printed.
A runtime that is simply not installed counts as failing, so use
`--runtimes-only` on a host that needs only some of them and read `checks`.
