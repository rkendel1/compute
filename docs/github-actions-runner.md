# GitHub Actions runner worker

**Status:** implemented as an external worker adapter,
`crates/compute-worker-github`, composed by `compute worker github-actions run`.
It is **not** a runtime, a target, or a Compute special case: nothing in
`compute-core`, `compute-runtime*`, `compute-provider`, `compute-placement`, or
`compute-environment` mentions it (`crates/compute-cli/tests/architecture.rs`
enforces that).

> Compute owns the worker's lifecycle. GitHub only supplies the work.

```text
recipe (github-actions-runner)         environment: ephemeral, network on, no credential
        ↓
compute worker github-actions run      repository, runner version, checksum: workload inputs
        ↓  registration token obtained from GitHub (never persisted)
shell runtime execution                Compute stages a private workspace
        ↓  download → verify sha256 → extract → config.sh --ephemeral → run.sh (one job)
runner exits
        ↓
Compute receipt + runner report        collected, then the workspace is removed
```

## Use

```sh
export GITHUB_TOKEN=…   # may administer the repository's runners; read from the environment only

compute worker github-actions run \
  --repository rkendel1/compute \
  --runner-version 2.331.0 \
  --runner-sha256 <sha256 of actions-runner-linux-x64-2.331.0.tar.gz> \
  --label compute --timeout 2h \
  --recipe-file examples/recipes/github-actions-runner.json \
  --receipt runner.receipt.json --json
```

* `--runner-version` is pinned and `--runner-sha256` is required: an archive is
  never run unless it matches. Compute does not choose "latest" for you, because
  a receipt must say exactly what ran. (The release page lists each archive's
  digest.)
* `--token-env NAME` reads the credential from another variable.
* `--download-url` replaces the release URL (a mirror). The checksum still
  decides.
* `--install-dependencies` runs the runner's `installdependencies.sh`, which
  needs privileges; it is off by default.
* Ctrl-C / SIGTERM cancels: Compute ends the runner's whole process group and
  removes the workspace before the command returns.
* Exit status: `0` when the runner ended cleanly; otherwise the runner's exit
  status (or `1`). Failures use the `--json` envelope
  ([cli-contract.md](cli-contract.md)): `missing_credential`,
  `invalid_repository`, `repository_not_found`, `github_unauthorized`,
  `github_rejected`, `github_unreachable`, `invalid_runner_spec`,
  `runner_failed`.

## The recipe

[`examples/recipes/github-actions-runner.json`](../examples/recipes/github-actions-runner.json)
states an ephemeral environment that reaches the network and offers `exec`,
`filesystem`, `network` and `process_tree_termination`. It contains **no**
repository, token, or credential, and cannot: a recipe spec rejects unknown
fields, and a guard test scans every recipe for credential-shaped keys and
values. `--recipe-file` validates the file and records its digest in the
report; the recipe does not drive the run.

## What is guaranteed

| Property | How |
| --- | --- |
| The registration token is never persisted | It is requested after validation, held in a `Secret` (no `Display`, no `Serialize`, redacted `Debug`), and handed to the execution only as the process environment variable `ACTIONS_RUNNER_INPUT_TOKEN`. Compute's receipt records environment variable **names**, never values (`compute-core/src/receipt.rs`). It is not an argument, not written by the script, and `unset` before the runner starts |
| It never appears in output | stdout, stderr and error text are scrubbed of both the token and the GitHub credential before they are returned or printed; errors are scrubbed when constructed |
| One job, then gone | `config.sh --ephemeral --unattended --disableupdate`, then `run.sh` in the foreground |
| Compute owns the lifecycle | The runner is a child in the execution's process group. No background process, no fixed path (the workspace is the runtime's private temp directory) |
| Cleanup on every ending | success, runner failure, registration failure, checksum failure, timeout and cancellation all end the process group and remove the workspace (`crates/compute-worker-github/tests/runner.rs`, `crates/compute-runtime/tests/ephemeral.rs`) |
| Reproducibility | the report (below) plus Compute's execution receipt, which names the runtime, the policy, and seals `runner-metadata.json` as an output |

## The report

`--json` prints the adapter's report in `data.report`. The execution receipt
(`--receipt`) remains Compute's record; the report references it by
`receipt_hash` and adds what only the adapter knows:

```json
{
  "worker": "github-actions",
  "repository": "rkendel1/compute",
  "recipe": { "name": "github-actions-runner", "digest": "sha256:…" },
  "runner": { "version": "2.331.0", "os": "linux", "arch": "x64", "name": "compute-3f2a…", "ephemeral": true },
  "environment": "shell on Linux x86_64",
  "execution_id": "…", "receipt_hash": "sha256:…",
  "started_at": "…", "finished_at": "…",
  "status": "completed", "exit_code": 0, "stage": "done",
  "job": { "name": "build", "result": "Succeeded" },
  "cleanup": { "workspace_removed": true }
}
```

`stage` says where a failure happened (`download`, `verify`, `extract`,
`configure`, `run`). `job` is read from the runner's own log and is
best-effort: it is absent when the runner never took a job.

## Limits (stated, not hidden)

* **Not verified against a live GitHub runner.** The tests use a fake runner
  archive and a local stand-in for the API. Two facts are taken from the
  runner's public behaviour and should be confirmed on first real use: that
  `config.sh` reads its token from `ACTIONS_RUNNER_INPUT_TOKEN`, and the log
  lines the report's `job` fields are parsed from.
* **It runs through `compute run`'s execution layer, not a Computer.** A
  session command's `command` and `env` are recorded durably
  (`compute-core/src/sessions.rs`, `SessionExecution`/`SessionCommand`) and there
  is no secret-input channel, so the token must not travel that path. Running
  runners inside a persistent Computer is deferred until a secret input exists.
* A runner killed before it takes a job (timeout, cancel) can leave an offline
  registration on GitHub; GitHub removes unclaimed ephemeral runners itself. The
  adapter does not call the removal API.
* Linux and macOS hosts only (`RunnerOs`), `x64` and `arm64`.
* The runner needs `curl`, `tar`, and `sha256sum` or `shasum` on the host.
