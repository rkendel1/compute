# GitHub Actions runner worker

**Status:** implemented as an external worker adapter,
`crates/compute-worker-github`, composed by `compute worker github-actions run`.
It is **not** a runtime, a target, or a Compute special case: no core crate
mentions it (`crates/compute-cli/tests/architecture.rs` enforces that).

> Compute owns the worker's lifecycle. GitHub only supplies the work.

## Verification status (read this first)

| Claim | Verified how | Status |
| --- | --- | --- |
| `config.sh` reads the registration token from `ACTIONS_RUNNER_INPUT_TOKEN` | Source: `actions/runner` `CommandSettings.cs` reads every `ACTIONS_RUNNER_INPUT_*` variable, masks secret ones, and removes them from its environment. **Run against the real v2.337.0 runner:** with no token, `config.sh --unattended …` stops with "Invalid configuration provided for token"; with the variable set to a fake value it accepts every flag the worker passes (`--unattended --ephemeral --disableupdate --url --name --labels --work`) and calls `POST https://api.github.com/actions/runner-registration`. **Through the worker**, as an unprivileged user, with the real archive (checksum verified, extracted by the script) and a local stand-in for the token API, the real runner reached real GitHub, which answered `404 Not Found` for the fake token; the worker reported stage `configure`, exit 75, workspace removed, no leftover files, no secret in any output | **verified** up to GitHub's acceptance of the token |
| Job name and result are readable from the runner's output | Source: `JobDispatcher.cs` prints `{time}: Running job: {name}` and `{time}: Job {name} completed with result: {result}` with `term.WriteLine`: to the terminal, **not** `_diag`. The first version read `_diag` and was wrong; the worker now reads the runner's captured output | **corrected from source; not observed from a live job** |
| A runner registers, takes one workflow job, and exits | needs a real repository, an admin token and an API path this environment does not allow (see below) | **not verified** |
| Compute captures the receipt and cleans up around a runner process | fake runner archives through the real shell runtime and the real binary (`crates/compute-worker-github/tests`, `crates/compute-cli/tests/worker.rs`) | verified with a **fake runner** |

**Real end-to-end: no.** This session cannot register a runner with GitHub:
its network proxy refuses `api.github.com/actions/runner-registration` and any
GitHub API path outside the session's repository scope, the `GITHUB_TOKEN` in
the environment is a placeholder, and GitHub API tools reach only
`rkendel1/compute`. Nothing in this repository claims otherwise. To verify it
yourself, run the command below against a disposable repository with a token
that may administer its runners, trigger a workflow with `runs-on: compute`, and
check that the report's `job.name`/`job.result` are filled in.

## Use

```sh
export GITHUB_TOKEN=…   # may administer the repository's runners; read from the environment only

compute worker github-actions run \
  --repository OWNER/NAME \
  --runner-version 2.337.0 \
  --runner-sha256 <sha256 of actions-runner-linux-x64-2.337.0.tar.gz> \
  --label compute --timeout 2h \
  --recipe-file examples/recipes/github-actions-runner.json \
  --receipt runner.receipt.json --json
```

* `--runner-version` is pinned and `--runner-sha256` is required: an archive is
  never run unless it matches. Compute does not choose "latest", because a
  receipt must say exactly what ran.
* The default source is the `actions/runner` release URL on `github.com`.
  `--download-url` may name a mirror, **https only** (the script passes
  `--proto =https --proto-redir =https --tlsv1.2` to curl, so redirects cannot
  downgrade); `file://`, `http://` and other schemes are refused. The checksum,
  not the URL, decides what runs.
* `--archive-file /abs/path.tar.gz` uses an archive you already have (an
  air-gapped host), verified the same way.
* `--token-env NAME` reads the credential from another variable.
* `--install-dependencies` runs the runner's `installdependencies.sh`, which
  needs privileges; off by default.
* Ctrl-C / SIGTERM cancels: Compute ends the runner's whole process group and
  removes the workspace before the command returns.
* Exit status: `0` when the runner ended cleanly **and** any job result it
  reported is `Succeeded`. A runner that exits cleanly after a job whose result
  was not `Succeeded` exits `1` with `error.code: job_failed` (an ephemeral
  runner exits `0` whatever its job did). Otherwise the runner's exit status, or
  `1`. When the runner printed no job result, nothing is claimed about the job.
* Failures use the `--json` envelope; codes are in [cli-contract.md](cli-contract.md).

## The recipe

[`examples/recipes/github-actions-runner.json`](../examples/recipes/github-actions-runner.json)
is an ordinary recipe: it parses as `RecipeSpec` (`deny_unknown_fields`), passes
`RecipeSpec::problems()`, and contains no repository, token, user identity, or
credential (a guard test scans every shipped recipe). It states an ephemeral
environment that reaches the network and offers `exec`, `filesystem`,
`network` and `process_tree_termination`.

`--recipe-file` **only records** the recipe's name and digest in the report as
`declared_recipe`. The recipe does not drive the run, and nothing evaluates
this host against its requirements: this command does not create an environment
and does not use placement. Treat `declared_recipe` as provenance the caller
asserted, not as proof the host satisfied it.

## Lifecycle: which Compute path it uses

```text
compute worker github-actions run
        ↓ registration token (never persisted)
Compute::run_controlled   ← the same execution path as `compute run`
        ↓ shell runtime: private workspace, process group, wall-time limit
runner.sh: fetch/verify → extract → config.sh --ephemeral → run.sh
        ↓
declared output runner-metadata.json collected → receipt sealed → workspace removed
```

It uses the **execution, receipt and cleanup** path. It does **not** use an
environment or a session: a session command's `command` and `env` are stored in
the durable session record and there is no secret-input channel
(`compute-core/src/sessions.rs`, `SessionExecution`), so a registration token
must not travel that way.

Process ownership: the runner runs in the foreground inside the workload's own
process group (the runtime sets `process_group(0)`). On timeout and
cancellation the runtime ends the whole group. When the runner exits on its own
the script ends the group itself before returning, so nothing the runner (a
listener, a worker, a job step) started survives a successful run
(`a_runner_that_exits_leaves_no_descendant_behind`, and the CLI timeout /
signal tests). A descendant that ignores `SIGTERM` is not force-killed on the
normal-exit path; the wall-time limit bounds it. This is done inside the worker
script; the process runtime's behaviour for session jobs is unchanged.

## What is guaranteed (each is tested)

| Property | How |
| --- | --- |
| The registration token is never persisted | It is requested after validation, held in a `Secret` (no `Display`, no `Serialize`, redacted `Debug`), and handed to the execution only as the process environment variable `ACTIONS_RUNNER_INPUT_TOKEN`. Not an argument; not written by the script; `unset` before the runner starts. The CLI test runs success, runner failure and job failure with `HOME`, `TMPDIR`, working directory and runtime store under one root and asserts no file there contains the token or the GitHub credential and that `TMPDIR` is empty afterwards |
| It never appears in output | stdout, stderr and error text are scrubbed of the token and the credential before they are returned or printed; errors are scrubbed when constructed; tests use runners that print both |
| It is not in the receipt | Compute's receipt records environment variable **names**, never values (`receipt.rs:910`). **Caveat:** the receipt's `workload` identity is a SHA-256 of the serialized request (`receipt.rs:943`), which includes the token's value, so a receipt contains a digest of a document that contains the token. The token is high-entropy and single-use, so this is not a disclosure, but it means two runs never share a workload identity |
| One job, then gone | `config.sh --ephemeral --unattended --disableupdate`, then `run.sh` in the foreground |
| Cleanup on every ending | success, runner failure, job failure, registration failure, checksum failure, timeout and cancellation end the process group and remove the workspace |

## Receipt and report

The **receipt is Compute's own** (`--receipt`): runtime, policy, environment
variable names, timings, exit status, isolation evidence, and the digest of the
declared output `runner-metadata.json`. That output binds the things the
receipt has no field for: `repository_url`, `runner_name`, `runner_version`,
`stage`, `job_name`, `job_result`, `host`, `started_at`/`finished_at`.

`--json` prints the adapter's report in `data.report`, which references the
receipt by `receipt_hash`:

```json
{
  "worker": "github-actions",
  "repository": "OWNER/NAME",
  "declared_recipe": { "name": "github-actions-runner", "digest": "sha256:…" },
  "runner": { "version": "2.337.0", "os": "linux", "arch": "x64", "name": "compute-3f2a…", "ephemeral": true },
  "environment": "shell on Linux x86_64",
  "execution_id": "…", "receipt_hash": "sha256:…",
  "started_at": "…", "finished_at": "…",
  "status": "completed", "exit_code": 0, "stage": "done",
  "job": { "name": "build", "result": "Succeeded" },
  "cleanup": { "workspace_removed": true }
}
```

`stage` says where a failure happened (`download`, `verify`, `extract`,
`configure`, `run`). `job` comes from the runner's printed lines and is absent
when the runner never took a job or printed none: GitHub does not expose the
job to the registration flow, so it is not fabricated. The recipe, repository
and job are **not** fields of Compute's receipt; they are in the report and in
the sealed metadata output.

## Limitations (each observed, not assumed)

* **Not a sandbox.** The runner executes workflow code as the invoking user
  under the `process` isolation profile; the receipt records
  `filesystem: unavailable` (`copy_in.rs::process_isolation_does_not_confine_the_workload`).
  Run it on a disposable host or container.
* **Not as root.** The real `config.sh` refuses to run as root unless
  `RUNNER_ALLOW_RUNASROOT` is set, and the process runtime clears the
  environment, so the worker cannot pass it: on a root host registration fails
  at `configure`. Run it as an unprivileged user.
* **No proxy settings reach the runner.** The runtime passes only the workload's
  own environment, so `HTTPS_PROXY`, `NO_PROXY` and CA-bundle variables are not
  forwarded. (Separately, the real runner v2.337.0 crashes at start with a
  `NullReferenceException` in `RunnerWebProxy` when `https_proxy` is set and
  `NO_PROXY` is not, so forwarding them blindly would not be safe either.)
* The token is visible to the same user in `/proc/PID/environ` while the
  runner starts.
* Output is shown when the runner ends, not live (it is captured to a file so a
  lingering descendant cannot hold the script open).
* A runner killed before it takes a job can leave an offline registration on
  GitHub; GitHub removes unclaimed ephemeral runners itself. The adapter does
  not call the removal API.
* Linux and macOS hosts only, `x64` and `arm64`; the host needs `curl`
  (unless `--archive-file`), `tar`, and `sha256sum` or `shasum`. Only Linux
  was exercised.
