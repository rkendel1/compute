# The Compute daemon

`compute start` runs the Compute daemon. It serves the **Compute API** and a
web UI, and it reconciles durable desired state against what runs on its
node. The CLI, the UI, and AppPort are all clients of the same API. There is
one lifecycle implementation, not one per client.

The daemon doesn't need systemd, Docker, or Kubernetes. It can run under any
of them, or on its own with `--detach`.

```sh
compute start --detach --config /etc/compute/compute.toml \
  --state-dir /var/lib/compute \
  --policy /etc/compute/policy.json
compute status
compute stop
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--listen` | `127.0.0.1:8787` | API and UI address |
| `--state-dir` | `.compute/daemon` | Node-local data: artifact cache, logs, and the daemon lock (and, for the file backend, control state) |
| `--state` | `[state] backend`, then `file` | Control-state backend: `file`, `memory`, or `feltdb` ([docs/control-plane.md](control-plane.md)) |
| `--config` | `$COMPUTE_CONFIG`, then `./compute.toml` | Configuration with a `[state]` section |
| `--policy` | none | Daemon-wide `compute.policy@1`, intersected into every admission |
| `--pool-config` | none | Provider pool for tasks. `local` is always the daemon itself |
| `--port-range` | `20000-29999` | Host ports for logical port bindings |
| `--reconcile-interval-ms` | `5000` | How often the reconciler rereads desired state |
| `--require-token-env NAME` | none | Require `Authorization: Bearer $NAME` for every change |
| `--detach` | off | Start in the background, log to `daemon.log` in the state directory, and return once the API answers |

Clients find the daemon through `--daemon`, then `$COMPUTE_DAEMON`, then
`http://127.0.0.1:8787`. They send the token from `$COMPUTE_DAEMON_TOKEN`,
or from the variable named by `--token-env`.

## What the daemon keeps where

| What | Where | Durable? |
| --- | --- | --- |
| Desired state: environments, projects, revisions, memberships, deployments, workloads, services, providers | Control state (file or Managed FeltDB) | Yes |
| Evidence: executions, receipt references, lifecycle events, observed workload status | Control state | Yes |
| Artifacts: workload bundles and receipt documents, by digest | The control state's artifact store (a directory for file state; chunks in FeltDB) | Yes |
| Running processes, their live health and output, restart backoff | Daemon memory | No |
| Artifact cache, service logs, the daemon lock, the node's process records | `--state-dir` | Node-local |

The daemon's memory can disappear, and so can its node. The desired state
cannot. A daemon started on a fresh node against the same Managed FeltDB
restores every environment, bundles included.

## Reconciliation

Each cycle, the reconciler reads desired state from control state, and then
acts on it:

- **Missing:** a service that should run and doesn't is started.
- **Wrong revision:** a service running an old deployment is replaced.
- **Should not run:** a running service whose desired state (its own, its
  project's, or its environment's) is stopped is stopped.
- **Failed:** a service that fails or is killed restarts with backoff
  (1s doubling to 60s) unless its restart policy is `never`. A denied
  service is never retried.
- **Correct:** no change.

It then writes what it observed back to control state: workload status,
deployment progress, and events. Cycles run every `--reconcile-interval-ms`
and after every change made through the API. A change made directly in
Managed FeltDB, for example by another tool, takes effect within one cycle.

A cycle that can't read control state changes nothing. See
[Failing closed](control-plane.md#failing-closed).

## Crash recovery

A daemon that is killed leaves its services running. Its replacement on the
same node reaps them before reconciling, so a service never runs twice and
its host port is free. It acts only on a process group whose leader it can
verify, by boot ID and start time, as the process its predecessor recorded.
A reused PID is never signalled.

## The Compute API (`compute.api@1`)

Plain HTTP/1.1 with JSON bodies. Every response carries
`X-Compute-Api: compute.api@1`. `compute_environment::api::ROUTES` lists
every operation. The UI uses no other, and a test enforces it.

```text
GET  /status                                             POST /shutdown
GET  /environments                                       POST /environments
GET  /environments/:env        [/status]                 DELETE /environments/:env
POST /environments/:env/start | stop | restart
GET  /environments/:env/projects                         POST /environments/:env/projects
GET  /environments/:env/projects/:p [/status]            DELETE /environments/:env/projects/:p
POST /environments/:env/projects/:p/start | stop | restart
GET  /environments/:env/projects/:p/executions | receipts
GET  /environments/:env/projects/:p/workloads/:w         GET .../workloads/:w/logs
POST /environments/:env/projects/:p/workloads/:w/start | stop | restart | run
GET  /projects                GET /projects/:p [/status]
GET  /projects/:p/revisions   POST /projects/:p/revisions
GET  /deployments?environment=&project=&limit=           POST /deployments
POST /deployments/promote     GET /deployments/:id
GET  /executions/:id          GET /receipts/:receipt_id
GET  /events?after=&environment=&project=&deployment=&limit=
GET  /events/stream?after=    (server-sent events)
GET  /providers               GET /services   POST /services   DELETE /services/:name
GET  /ui/                     (the control-plane UI)
```

`:env` is an environment's name or its `env_…` ID.

Errors are `{ "kind": …, "message": … }`:

| `kind` | HTTP |
| --- | --- |
| `invalid` | 400 |
| `unauthorized` | 401 |
| `admission_denied` | 403 |
| `not_found`, `no_route` | 404 |
| `conflict` (including a lost race with another writer) | 409 |
| `state_unavailable` (control state unreachable) | 503 |
| `io` | 500 |

### Authorization

Every request passes through the provider authorization boundary that
`compute serve` also uses. Reads are `EnvironmentRead` and every other
method is `EnvironmentMutate`. With `--require-token-env`, reads are open and
changes need the bearer token. Without it, the API accepts every request,
so keep it bound to localhost. Authorization decides who may change the
daemon. Admission still decides whether each execution may run.

## The UI

`http://<listen>/ui/` is the control-plane UI. Environments are the first
screen. From there you can:

- open an environment's projects, and each project's overview, workloads,
  deployments, logs, resources, configuration, receipts, and events
- assign projects to environments
- deploy a registered revision, or promote the revision current in another
  environment
- start, stop, restart, or remove projects and workloads, each after a
  confirmation that states what it affects and what it doesn't

It updates live from `/events/stream`. When the daemon requires a token,
enter it under **Token**. It is kept for that browser tab only.

## AppPort

`@compute/appport` exposes the same operations as versioned capabilities,
backed by a client of this API. Each needs its scope. AppPort authorizes
the caller first, and the daemon then applies its own token check and
admission.

| Capability | Scope |
| --- | --- |
| `compute.environment.list@1`, `.inspect@1`, `.status@1` | `compute.environment.read` |
| `compute.environment.create@1`, `.start@1`, `.stop@1`, `.restart@1` | `compute.environment.<operation>` |
| `compute.environment.project.add@1`, `.remove@1` | same as the capability |
| `compute.project.list@1`, `.inspect@1` | `compute.project.read` |
| `compute.project.start@1`, `.stop@1`, `.restart@1` | same as the capability |
| `compute.deployment.inspect@1` | `compute.deployment.read` |
| `compute.deployment.create@1`, `.promote@1` | same as the capability |
