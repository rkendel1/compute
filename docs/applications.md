# Compute applications and deployments

An application is a durable thing you run. A deployment is one immutable,
versioned release of it. You say what the application needs; Compute
decides where it runs.

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
  "deployment_id": "dep_…",
  "version": 1,
  "provider": "linux-worker",
  "runtime": "node",
  "endpoint": "http://10.0.0.20:20000",
  "status": "running",
  "receipt": "sha256:…",
  "artifact": { "artifact_id": "sha256:…", "url": "https://…", "version": "1.2.0" }
}
```

## Where it runs

```text
compute deploy APP
  → artifact       a directory is packed into one (below)
  → requirements   its bundle: runtime and version, architecture,
                   resources, network, isolation, dependencies
  → discovery      every provider in the caller-owned pool
  → placement      compatible, admitted, and offering deployments
  → the selected provider's Compute daemon (which fetches a URL itself)
  → revision, release, readiness, stable endpoint, receipts
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
placement: the revision, the release, the endpoint, instances, and
evidence. The caller keeps the requirements, the placement decision, and
the result. There is no second deployment database.

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

## Identity and immutable versions

```text
application → deployment version → execution → receipt
   (+ artifact)   (+ pool placement)
```

The application name is its identity (`compute.application@1`), wherever it
runs: `application_id` is the digest of `compute.application@1` and the name, the same
on every provider and after every restart. Each deployment has its own
`dep_…` identity and a monotonically increasing `v1`, `v2`, … version, and
records the provider that hosts it, the caller's pool placement that chose
that provider, and the artifact it released (`artifact_id`, the URL the
provider fetched, its version and capabilities; a rollback carries the
artifact of the version it restores). It references an immutable revision
holding the canonical workload bundle, runtime requirement, artifact
identity, resources, network contract, and readiness configuration.

## Lifecycle and replacement

```text
pending → starting → ready → network_ready → switching
        → active → draining → complete
        ↘ failed
```

The previous version keeps serving while the new one is admitted, started,
and checked. Compute switches the stable endpoint only after HTTP readiness
succeeds. A failure before the switch leaves the old version active.

The endpoint is a host port on the provider that keeps serving across
versions; each version's instance listens on its own port (the application
is given it as `PORT`), and the endpoint is retargeted. This is node-local
routing, not DNS, TLS, or load balancing.

`compute history` shows each version's state in product terms:

| State | Meaning |
|---|---|
| `active` | the version the endpoint serves |
| `stopped` | the active version, with the application stopped |
| `superseded` | served once; a later version replaced it |
| `deploying` | being released |
| `failed` | never served; what served before kept serving |
| `rolled_back` | served briefly, failed verification, and traffic went back |

A version whose code is an earlier version's, after other code replaced it,
is marked `rollback to vN`.

## Rollback and recovery

`compute rollback APP VERSION_OR_DEPLOYMENT` never edits history. It
deploys the selected version's revision and configuration again, on the
same provider, as the next version: rolling `v3` back to `v1` creates `v4`.

Transitions and the active version are committed durably on the provider.
After a controller restart, reconciliation resumes an in-flight release and
reconstructs the active service and endpoint; the supervisor keeps
serving while the controller is away.

## Evidence

Every execution receipt (`compute.receipt@1`) names its application and,
in its scope, its deployment. The deployment receipt
(`compute.deployment-receipt@1`) binds the application, version, revision,
configuration digest, this node's admission and placement, the caller's pool
placement, traffic switch, lifecycle events, and the receipts of its
executions. A service's execution ends after its release did, so the
deployment receipt is reissued when that execution's receipt is recorded.

Receipts are served exactly as stored (`GET /receipts/{id}`), so a
downloaded receipt verifies offline:

```sh
curl -H "Authorization: Bearer $TOKEN" "$NODE/receipts/sha256:…" > receipt.json
compute receipt verify receipt.json
```

## The application API

A provider's daemon serves the application resource that the CLI uses and
an agent can use directly (`compute.api@1`, scopes in parentheses):

| Operation | Route |
|---|---|
| list | `GET /applications` (read) |
| status | `GET /applications/{name}` (read) |
| deploy | `POST /applications/{name}/deployments` with `artifact` (`{"inline": {"data": …}}` or `{"reference": {"url", "digest"}}`), or a bundle and port; `env`; and placement (deploy) |
| history | `GET /applications/{name}/deployments` (read) |
| one version | `GET /applications/{name}/deployments/{v3 or dep_…}` (read) |
| rollback | `POST /applications/{name}/rollback` `{"target": "v1"}` (deploy) |
| stop | `POST /applications/{name}/stop` (operate) |
| logs | `GET /applications/{name}/logs` (read) |
| receipt | `GET /receipts/{id}`, `GET /deployments/{id}/receipt` (read) |
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
`provider`, `runtime`, `endpoint`, `status`, `receipt`, and `artifact`.
Placement, provider discovery, and the pool's credentials stay in Compute:
the capabilities operate the caller's pool as `compute application` does.

## `run` versus `deploy`

`compute run FILE_OR_DIR` executes a workload once, on a provider placement
chooses, and returns its result and receipt. `compute deploy APP` operates
an application. Both are the same execution on the same substrate: a
daemon runs a workload, a job, and an application's service through one
provider, one runtime store, one admission, and one receipt format; an
application is orchestration (revisions, releases, the endpoint) over
ordinary executions. Application commands never fall back to jobs.

Logs are the active version's; `--version` names a version and succeeds only
for the active one, since only its instance keeps output. Compute reports
this rather than keeping a second log store.

See [the runnable HTTP demo](../examples/compute-demo/),
[releases.md](releases.md), [providers.md](providers.md), and
[receipts.md](receipts.md). The provider-neutral acceptance is
`crates/compute-cli/tests/product.rs`: an application from source through
versions, rollback, provider restart, stop, and restart; an artifact
deployed by URL; and every provider type (local, a daemon, a
deployment-only daemon, a jobs-only server) crossed with run, job, and
deployment, where every unsupported combination is refused before
anything executes.

Screenshots of the UI showing an application this way are in
[audit-evidence/ui-after/](audit-evidence/ui-after/).
