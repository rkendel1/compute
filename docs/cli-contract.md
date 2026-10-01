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

(Here `COMPUTE_DAEMON` was unset and the default controller was down, so the
envelope also carries `recovery.command: "compute start"`.)

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
deterministic, and today that is a single case:

| Code | Recovery | Condition |
| --- | --- | --- |
| `controller_unavailable` | `compute start` | the endpoint in use is the default local controller (`http://127.0.0.1:8787`: no `--daemon`, no `$COMPUTE_DAEMON`) |

For a remote or custom endpoint, starting a local controller is not the fix, so
nothing is claimed. Every other error has several possible causes (an
unavailable runtime might be installed, selected differently, or fetched), so
no recovery is claimed for it. Recovery is computed by the CLI, not carried by
the error: `compute-core` knows nothing about CLI commands.

### Usage errors

A usage error (an unknown flag, a missing operand) keeps clap's message and its
exit status, `2`. With `--json` it also ends stderr with the envelope, code
`invalid_arguments`. `--help` and `--version` are not errors.

### Error codes

Codes are identifiers, never prose, and are never renamed. The CLI's own:

| Code | Meaning |
| --- | --- |
| `invalid_arguments` | a usage error (exit 2) |
| `doctor_checks_failed` | `compute doctor --strict` found a check that did not pass |
| `runner_failed` | `compute worker github-actions run`: the runner did not end cleanly (timeout, cancellation, non-zero exit, registration or checksum failure) |
| `job_failed` | the runner exited cleanly but reported a job result other than `Succeeded` |

From `ComputeError::code()`: `ambiguous_runtime`, `unsupported_capability`,
`unsupported_workload_version`, `unknown_runtime`, `runtime_unavailable`,
`runtime_version_mismatch`, `invalid_workload`, `invalid_isolation_profile`,
`isolation_unavailable`, `invalid_bundle`, `invalid_dependency_capsule`,
`invalid_receipt`, `invalid_mount_path`, `io_error`, `invalid_json`,
`runtime_error`. Controller errors keep their kind through `Coded`
(`not_found`, `conflict`, `controller_unavailable`, …). The GitHub worker's:
`invalid_repository`, `invalid_runner_spec`, `missing_credential`,
`repository_not_found`, `github_unauthorized`, `github_rejected`,
`github_unreachable`, `github_malformed_response`, `runner_execution_failed`.
A test (`contract::every_error_code_is_documented`) fails if a code exists that
this page does not name.

## `compute doctor`

What it checks: for each runtime on this host, whether its adapter reports it
available (`RuntimeAvailability`, the same source placement uses), and, unless
`--runtimes-only`, whether the controller at the endpoint answers `/health` and
accepts the credential on `/info`. It evaluates no requirement and makes no
admission decision; it only reports what the adapters and the controller say
about themselves. Output is deterministic for a given host: checks come in
runtime order, controller last.

```sh
compute doctor                 # report; exit 0 whatever it finds
compute doctor --strict        # exit 1 unless every check is `pass`
compute doctor --json          # the envelope, plus the original keys
compute doctor --runtimes-only # this host's runtimes only; no controller
```

| `status` | Meaning |
| --- | --- |
| `pass` | verified |
| `warn` | this host cannot do something, which matters only if you need it: an uninstalled runtime. Carries the adapter's `remediation` |
| `fail` | Compute cannot work here: the controller does not answer, or refuses the credential |

Exit status: `0`, unless `--strict` and at least one check is not `pass` (a
warning counts), then `1`. Without `--strict` the exit is always `0`, as it was
before the flag existed.

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
      { "id": "runtime:ruby", "status": "warn", "detail": null, "remediation": "install ruby …" },
      { "id": "controller", "status": "fail", "detail": "http://127.0.0.1:8787",
        "remediation": "start the controller with `compute start`" }
    ]
  },
  "runtimes": [ … ], "controller": { … }
}
```

`data.healthy` is true iff every check is `pass`. Without `--strict`, `ok` is
`true` (the command ran). With `--strict` and a non-`pass` check, `ok` is
`false`, `exit_code` is `1`, `error.code` is `doctor_checks_failed`, and `data`
is still printed, on **stdout** (it is doctor's result). Tests:
`contract.rs` unit tests for every combination of healthy/warning/failure ×
strict, and `crates/compute-cli/tests/contract.rs` for the real binary.
