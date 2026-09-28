# Compute applications and deployments

An application is a durable thing you run. A deployment is one immutable,
versioned release of it. You say what the application needs; Compute
decides where it runs.

An application is not a second deployment model. It is a name for the
canonical records every computer workload has
([architecture.md](architecture.md#applications-one-lifecycle-g-arch-2)):

```text
application <name>
  → environment `application-<name>` and its computer (placed, owned by you)
  → project <name>: the artifact's source, imported into the computer
  → a version of the project (published: commit, package digest, artifact)
  → a rollout of that version: the deployment (`rol_…`)
  → the durable target job that started its process, in the computer's session
  → the computer's endpoint for that process
  → the target's receipt for that job
```

```sh
compute init my-api
compute deploy my-api
curl "$(compute status my-api --json | jq -r .endpoint)"
compute status my-api
compute logs my-api --follow

# Change the source and deploy again. The endpoint stays the same.
compute deploy my-api
compute history my-api
compute rollback my-api 1
compute stop my-api
```

```text
Application: my-api
Version:     v1
Provider:    linux-worker
Runtime:     node 24.18.0
Endpoint:    http://10.0.0.20:20000
Status:      running
```

The same lifecycle is one resource under `compute application`, for people
and agents; every operation takes a directory, an artifact, or the
application's name, and `--json` returns structured results:

```sh
compute application pack my-api --output my-api.capp
compute application info my-api.capp
compute application deploy https://artifacts.example/my-api.capp --set API_KEY=…
compute application status my-api --json
compute application logs|history|stop my-api
compute application rollback my-api v1
```

```json
{
  "application_id": "sha256:…",
  "deployment_id": "rol_…",
  "version": 1,
  "provider": "linux-worker",
  "runtime": "node",
  "endpoint": "http://10.0.0.20:20000",
  "status": "running",
  "receipt": "sha256:…",
  "artifact": { "artifact_id": "sha256:…", "url": "https://…", "version": "1.2.0" },
  "environment": "application-my-api",
  "computer_id": "cmp_…",
  "version_id": "ver_…",
  "rollout_id": "rol_…",
  "target": "this-machine",
  "session_id": "ses_…",
  "job_id": "job_…",
  "execution_id": "exec_…"
}
```

Every ID is a canonical record's: `GET /environments/application-my-api/computer`,
`GET /software/my-api/versions/{label}`, `GET /rollouts/{rol_…}`, and
`GET /environments/application-my-api/jobs/{job_…}` return them.

## Where it runs

```text
compute deploy APP
  → artifact       a directory is packed into one (below)
  → requirements   its bundle: runtime and version, architecture,
                   resources, network, isolation, dependencies
  → discovery      every provider in the caller-owned pool
  → placement      compatible, admitted, and offering deployments
  → the selected provider's Compute daemon (which fetches a URL itself)
  → the application's computer on a target of that daemon's pool
  → version, rollout, health check, stable endpoint, receipt
```

A provider hosts deployments when it is a Compute daemon (`compute start`)
that offers them: `deployments` in the `execution` modes of its
`compute.remote@1` capabilities. Placement rejects a provider that does
not, such as a `compute serve` endpoint, with `deployment_unsupported`,
before anything is deployed (see
[providers.md](providers.md#execution-modes)). The pool is the same
`compute-pool.toml` that `compute run` uses:

```toml
[providers.local]            # this machine; its daemon is started on demand
kind = "local"

[providers.linux-worker]     # another node's Compute daemon
kind = "remote"
endpoint = "https://10.0.0.20:8787"
token_env = "LINUX_WORKER_TOKEN"
```

With no pool configured, the pool is `local` alone, and `compute deploy`
starts a local development daemon when none is running. `--provider auto`
(the default) lets placement choose; `--provider ID` or `provider:ID` names
one. When nothing can host the application, nothing is deployed and the
error says what the application requires and what each provider lacks.

The selected provider's daemon is authoritative for everything after
placement, and it runs nothing itself: the application gets a computer of
its own on a target in the daemon's pool (`compute serve`), placed by the
same computer placement as every environment, from the bundle's CPU,
memory, architecture, and network requirements. A daemon with no target
that can host it refuses the deployment (`has no computer to run on`, with
every target's reasons) and records nothing. The caller keeps the pool
placement and the result. There is no second deployment database.

**An application lives on one provider.** A new version, a rollback, `status`,
`logs`, `history`, and `stop` go to the provider that holds it, found by
asking the pool's providers. `compute deploy --provider OTHER` for an
application deployed elsewhere is refused: moving an application between
providers is not something deploy does silently. A provider in the pool
that cannot be reached makes these commands fail rather than guess.

Remote deployment uses the daemon's authenticated API: the pool's
`token_env` credential is sent with every request, and a daemon started
with `--require-token-env` (development) or `--production` refuses requests
without it. A provider's daemon names itself with `--public-url` and says
where its applications are reached with `--application-host`.

## Portable application artifacts

An application artifact (`compute.application-artifact@1`) is one file that
carries everything a provider needs: the canonical workload bundle and the
application manifest.

```text
application.json    identity, version, runtime, entrypoint, port,
                    requirements, environment contract, capabilities,
                    metadata, and the bundle's identities
workload.compute    the canonical workload bundle
```

`compute application pack DIR` writes it (`NAME.capp` by default). The
archive is deterministic, so the same source packs to the same bytes, and
its SHA-256 is the artifact's identity. Reading one verifies that the
manifest describes exactly the bundle it carries: a manifest cannot claim
a runtime, entrypoint, requirement, or default the bundle does not have.
`.capp` is a convention; Compute recognizes an artifact by its contents,
whatever the file is called. (`.app` is AppBoundry's format, which
Compute does not claim.)

`[application]` in `compute.toml` describes what the artifact declares:

```toml
[application]
name = "my-api"
port = 3000
version = "1.2.0"                 # the developer's label for this build
required_env = ["API_KEY"]        # the deployment's configuration must supply these
capabilities = ["http.orders"]    # what it offers, recorded and reported
[application.metadata]
team = "platform"
```

`compute deploy` and `compute application deploy` take:

| Given | Sent to the provider | Who reads it |
| --- | --- | --- |
| a directory | packed into an artifact, inline | the CLI |
| an artifact file | inline | the CLI |
| `file://…` or `http(s)://…` | a reference pinned to the digest placement evaluated | the provider fetches it and verifies the digest |

Placement evaluates the artifact's bundle; for a URL, the CLI fetches it to
place it, and the provider must fetch the same bytes. The provider refuses,
before recording anything, an artifact whose name is not the application's
or a release whose configuration lacks a required environment name
(`--set NAME=VALUE`, or the application's current configuration; values
built into the artifact count). A `file://` reference names a path on the
provider. Fetches are limited to 256 MiB.

## Identity and versions

```text
application → version (a rollout) → target job (execution) → receipt
   (+ artifact)   (+ the project's published version)
```

The application name is its identity (`compute.application@1`), wherever it
runs: `application_id` is the digest of `compute.application@1` and the name,
the same on every provider and after every restart.

A deploy imports the artifact's files (its entrypoint and inputs) into a
repository in the computer's workspace, through durable jobs in the
computer's authenticated target session, and commits them. The environment
then holds the repository at that commit, the project, and one process of
kind `application` that runs the entrypoint with the target's runtime (the
application is given its endpoint port as `PORT`). The project is published
as a version — its commit, the digest of its source package, and the
artifact it came from (`artifact_id`, URL, version, capabilities, runtime) —
and that version is deployed as a rollout. `v1`, `v2`, … number the
project's rollouts in the application's environment; `deployment_id` is the
rollout's ID.

A computer runs the target's own runtimes (Python, Node, Bun, Deno, Ruby,
PHP, shell, native). An application that needs Compute's pinned runtime
catalog (WASM, JVM, .NET) or carries a dependency capsule is refused with
that reason.

## Lifecycle and replacement

A deploy changes the computer in place: the new commit is checked out and
the process restarts on it; the rollout becomes active when the process
runs and its endpoint answers (its health check). A deploy that fails
before then leaves the rollout failed, with the step and the job that
failed. The endpoint is the computer's endpoint for the process's port on
its target, stable across versions. The process restarts, so a release is
not zero-downtime (G-DEP-1); that is the computer model's behavior for
every workload.

`compute history` shows each version's state:

| State | Meaning |
|---|---|
| `active` | the version the endpoint serves (its rollout is active) |
| `stopped` | the active version, with the application stopped |
| `superseded` | served once; a later version replaced it |
| `deploying` | its rollout is still being applied |
| `failed` | its rollout failed |

A version whose code is an earlier version's, after other code replaced it,
is marked `rollback to vN`.

## Rollback, stop, and recovery

`compute rollback APP VERSION_OR_DEPLOYMENT` is the canonical rollback of
the project in the application's environment to that version's published
version: a rollout of kind `rollback`, the same record a rollback through
`/software/{project}/rollback` makes. History is never edited: rolling `v3`
back to `v1` creates `v4`.

`compute stop` sets the process's desired state to stopped; the computer,
versions, endpoint, and evidence remain. Deploying again starts it.

Recovery is the computer's. A target that stops answering makes the
application `unreachable` (nothing is deployed to it meanwhile); when the
same machine answers again, the application runs on without a redeploy. A
target that no longer has the machine makes it `lost`: no deploy or stale
answer revives it, and replacing the computer provisions a new machine,
where the next deploy imports the source again. A control-plane restart
resumes every driver from FeltDB; the target keeps running the process
while the control plane is away.

## Evidence

The evidence of a version is the target's receipt (`compute.receipt@1`) for
the job that started its process. The rollout's "Restart applications" step
names that job, its execution, and the receipt's hash, and
`GET /applications/{name}/deployments/{version}/receipt` serves the
receipt's canonical bytes, fetched from the target, so a downloaded receipt
verifies offline:

```sh
curl -H "Authorization: Bearer $TOKEN" \
  "$NODE/applications/my-api/deployments/v3/receipt" > receipt.json
compute receipt verify receipt.json
```

No application receipt is stored beside it: the control plane keeps the
reference, the target keeps the receipt.

## The application API

A provider's daemon serves the application resource that the CLI uses and
an agent can use directly (`compute.api@1`, scopes in parentheses):

| Operation | Route |
|---|---|
| list | `GET /applications` (read) |
| status | `GET /applications/{name}` (read) |
| deploy | `POST /applications/{name}/deployments` with `artifact` (`{"inline": {"data": …}}` or `{"reference": {"url", "digest"}}`), or a bundle and port; `env`; and placement (deploy, owner) |
| history | `GET /applications/{name}/deployments` (read) |
| one version | `GET /applications/{name}/deployments/{v3 or rol_…}` (read) |
| receipt | `GET /applications/{name}/deployments/{v3 or rol_…}/receipt` (read): the target's receipt, canonical bytes |
| rollback | `POST /applications/{name}/rollback` `{"target": "v1"}` (deploy, owner) |
| stop | `POST /applications/{name}/stop` (operate, owner) |
| logs | `GET /applications/{name}/logs` (read, owner): the process's log, the last 1000 lines, read by a target job |

Every mutation, and the logs, belong to the owner of the application's
computer, exactly as for any computer: another operator is refused.
| provider discovery | `GET /compute/capabilities`, `GET /compute/health` (read) |

## AppPort

Agents operate applications through AppPort capabilities
(`@compute/appport`), in application terms and without Compute transport
details:

| Capability | Effect | Scope |
| --- | --- | --- |
| `compute.application.deploy@1` | consequential | `compute.application.deploy` |
| `compute.application.status@1` | observation | `compute.application.read` |
| `compute.application.logs@1` | observation | `compute.application.read` |
| `compute.application.history@1` | observation | `compute.application.read` |
| `compute.application.rollback@1` | consequential | `compute.application.deploy` |
| `compute.application.stop@1` | consequential | `compute.application.stop` |

`deploy` takes an application directory, an artifact URL, or the
artifact's bytes, with an optional provider and configuration, and
returns the release: `application_id`, `deployment_id`, `version`,
`provider`, `runtime`, `endpoint`, `status`, `receipt`, and `artifact`, and
the canonical records it is (`environment`, `computer_id`, `version_id`,
`rollout_id`, `job_id`, `execution_id`).
Placement, provider discovery, and the pool's credentials stay in Compute:
the capabilities operate the caller's pool as `compute application` does.

## `run` versus `deploy`

`compute run FILE_OR_DIR` executes a workload once, on a provider placement
chooses, and returns its result and receipt. `compute deploy APP` operates
an application: a process in a computer of its own, changed by versions and
rollouts like every computer workload. Application commands never fall back
to jobs, to the daemon's own node, or to another provider.

Logs are the process's; `--version` names a version and succeeds only for
the active one, since only the running process keeps output. Compute
reports this rather than keeping a second log store.

See [the runnable HTTP demo](../examples/compute-demo/),
[computers.md](computers.md), [providers.md](providers.md), and
[receipts.md](receipts.md). The convergence regression is
`crates/compute-environment/tests/applications.rs`: every canonical record
exists and the application API names it, failures of the target and the
control plane follow the computer, and nothing of the node model is
written. The provider-neutral acceptance is
`crates/compute-cli/tests/product.rs`: an application from source through
versions, rollback, provider restart, stop, and restart; an artifact
deployed by URL; and every provider type (local, a daemon, a
deployment-only daemon, a jobs-only server) crossed with run, job, and
deployment, where every unsupported combination is refused before
anything executes.
