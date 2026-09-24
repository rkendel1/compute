# The Compute daemon

`compute start` runs the persistent Compute daemon. It stores the desired
state of every environment, project, and workload, reconciles it against
what is actually running, and serves the **Compute API**. The CLI uses this
API, AppPort uses it, and the Control Plane UI will use it. There is one
lifecycle implementation, not one per client.

The daemon doesn't need systemd, Docker, or Kubernetes. It can run under any
of them, or on its own with `--detach`.

```sh
compute start --detach \
  --state-dir /var/lib/compute \
  --policy /etc/compute/policy.json \
  --pool-config /etc/compute/pool.toml
compute status
compute stop
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--listen` | `127.0.0.1:8787` | API address |
| `--state-dir` | `.compute/daemon` | Environments, bundles, logs, receipts, and port bindings |
| `--policy` | none | Daemon-wide `compute.policy@1`, intersected into every admission |
| `--pool-config` | none | Provider pool for tasks. `local` is always the daemon itself |
| `--port-range` | `20000-29999` | Host ports for logical port bindings |
| `--require-token-env NAME` | none | Require `Authorization: Bearer $NAME` for every mutation |
| `--detach` | off | Start in the background, write `daemon.log` in the state directory, and return once the API answers |

Clients find the daemon through `--daemon`, then `$COMPUTE_DAEMON`, then
`http://127.0.0.1:8787`. They send the token from `$COMPUTE_DAEMON_TOKEN`,
or from the variable named by `--token-env`.

## Persistence

All definitions and desired state are stored in the state directory:

```text
<state>/
├── ports.json                              stable host port bindings
├── daemon.log                              with --detach
└── environments/<environment>/
    ├── environment.json                    definition, desired state, projects
    ├── receipts/<execution>.json           one receipt per execution
    └── projects/<project>/
        ├── bundles/<workload>.compute      the deployed bundles
        └── workloads/<workload>/logs/<n>/  stdout.log, stderr.log
```

Files are written atomically. `compute stop`, a crash, or a reboot stops
the services but never changes desired state. The next `compute start`
loads every environment and reconciles, so each service whose desired state
is `running` at every level starts again on the same host ports.

Use one daemon per state directory.

## Reconciliation

The daemon reconciles whenever desired state changes, a service ends, or it
starts:

- A service should run when the service, its project, and its environment
  all desire `running`, and it has not ended since its last explicit start.
- A service that should run and isn't running is started.
- A service that is running and shouldn't be is stopped. Its process group
  is signalled, and the daemon waits for it to exit.
- A service that fails restarts only with `restart = "on_failure"`. A
  denied service is never retried until it is started explicitly.

Completions from an older run of a workload are ignored, so restarts never
race.

## The Compute API (`compute.api@1`)

Plain HTTP/1.1 with JSON bodies. Every response carries
`X-Compute-Api: compute.api@1`.

```text
GET    /status                                   daemon status
POST   /shutdown
GET    /environments                             POST /environments
GET    /environments/:id                         DELETE /environments/:id
POST   /environments/:id/start | stop | restart
GET    /environments/:id/status
GET    /environments/:id/projects                POST /environments/:id/projects
GET    /environments/:id/projects/:project       DELETE /environments/:id/projects/:project
POST   /environments/:id/projects/:project/start | stop | restart
GET    /environments/:id/projects/:project/status
GET    /environments/:id/projects/:project/workloads/:workload
POST   /environments/:id/projects/:project/workloads/:workload/start | stop | restart | run
GET    /environments/:id/projects/:project/workloads/:workload/logs
GET    /executions/:execution_id
```

`:id` is an environment's name or its `env_…` ID. The body of
`POST /environments` is an environment definition: `name`, `desired_state`,
`env`, `policy`, and `provider`. The body of `POST …/projects` is a project
definition with its bundles: `name`, `revision`, `source`, `desired_state`,
`env`, and `workloads`. Each workload has `name`, `kind`, `bundle`, `ports`,
`restart`, and `desired_state`. `compute environment apply` and
`compute project add` build these bodies from the
[manifests](environments.md#manifests).

Errors are `{ "kind": …, "message": … }`:

| `kind` | HTTP |
| --- | --- |
| `invalid` | 400 |
| `unauthorized` | 401 |
| `admission_denied` | 403 |
| `not_found` | 404 |
| `conflict` | 409 |
| `io` | 500 |

### Authorization

Every request passes through the provider authorization boundary that
`compute serve` also uses. Reads are `EnvironmentRead` and every other
method is `EnvironmentMutate`. With `--require-token-env`, reads are open and
mutations need the bearer token. Without it, the API accepts every request,
so keep it bound to localhost. Authorization decides who may change the
daemon. Admission still decides whether each execution may run.

## AppPort

`@compute/appport` exposes the same operations as versioned capabilities,
backed by a client of this API:

| Capability | Scope |
| --- | --- |
| `compute.environment.list@1` | `compute.environment.read` |
| `compute.environment.inspect@1` | `compute.environment.read` |
| `compute.environment.status@1` | `compute.environment.read` |
| `compute.environment.create@1` | `compute.environment.create` |
| `compute.environment.start@1` | `compute.environment.start` |
| `compute.environment.stop@1` | `compute.environment.stop` |
| `compute.environment.restart@1` | `compute.environment.restart` |
| `compute.environment.project.add@1` | `compute.environment.project.add` |
| `compute.environment.project.remove@1` | `compute.environment.project.remove` |

AppPort authorizes the caller first. The daemon then applies its own token
check and admission. Daemon refusals keep distinct AppPort error codes:
`NOT_FOUND`, `CONFLICT`, `FORBIDDEN`, and `INVALID_INPUT`.

```ts
const app = createComputeApplication({
  daemon: "http://127.0.0.1:8787",
  daemonToken: process.env.COMPUTE_DAEMON_TOKEN,
  authorizer,
});
```
