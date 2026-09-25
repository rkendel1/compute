# The Compute daemon

`compute start` runs the Compute daemon (the controller). It serves the
**Compute API** and a web UI, and it reconciles durable desired state
against what runs on its node. Workloads and endpoints run on the node's
supervisor, not in the controller, so they keep serving while the
controller restarts, crashes, or is upgraded. See
[docs/architecture.md](architecture.md) for the parts and the invariants
between them. The CLI, the UI, and AppPort are all clients of the same API. There is
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
| `--config` | `$COMPUTE_CONFIG`, then `./compute.toml` | Configuration: `[state]`, `[network]`, `[release]` |
| `--policy` | none | Daemon-wide `compute.policy@1`, intersected into every admission |
| `--pool-config` | none | Provider pool for tasks. `local` is always the daemon itself |
| `--port-range` | `20000-29999` | Host ports for services' stable endpoints |
| `--instance-port-range` | `[release] instance_port_range`, then `30000-39999` | Host ports for service instances behind endpoints |
| `--drain-timeout-ms` | `[release] drain_timeout_ms`, then `30000` | How long a replaced instance may finish its connections |
| `--endpoint-address` | `[network] endpoint_address`, then `127.0.0.1` | Where endpoints listen |
| `--ingress-http`, `--ingress-https` | `[network]`, off otherwise | The public entry ([docs/networking.md](networking.md)) |
| `--reconcile-interval-ms` | `5000` | How often the reconciler rereads desired state |
| `--production` | off | Production mode: TLS required, and every request needs an operator credential |
| `--tls-cert`, `--tls-key` | `[api] tls_cert`, `tls_key` | The API's certificate and key (PEM). Reloaded within a second of changing, without a restart |
| `--insecure` | off | Development mode on any listener: plaintext, and requests without a credential are admitted |
| `--require-token-env NAME` | none | Development only: require `Authorization: Bearer $NAME` (a single shared token). Refused in production |
| `--data-plane` | `supervisor` | `supervisor`: workloads run on the node's supervisor and outlive the controller. `in-process`: they run inside the controller (development) |
| `--require-state-at-start` | off | Refuse to start while control state is unreachable, instead of starting in `degraded_control_plane` |
| `--detach` | off | Start in the background, log to `daemon.log` in the state directory, and return once the API answers |

Clients find the daemon through `--daemon`, then `$COMPUTE_DAEMON`, then
`http://127.0.0.1:8787`. They send the credential from
`$COMPUTE_DAEMON_TOKEN`, or from the variable named by `--token-env`. For
`https://` endpoints with a private CA, set `COMPUTE_CA_CERT` to its PEM.

### Security modes

The API fails closed. How it is secured depends on how it is started:

| Started with | Mode | Requests |
| --- | --- | --- |
| TLS (`--tls-cert`, `--tls-key`), or `--production` with TLS | production | Every request, reads included, needs an operator credential with the right scope |
| `--production` without TLS | refused | The controller does not start |
| A non-loopback `--listen` without TLS or `--insecure` | refused | The controller does not start |
| A loopback `--listen` without TLS | development | Admitted without a credential (or with the `--require-token-env` token when set) |
| `--insecure` | development | Admitted without a credential; stated in `/info` and at start |

`/info` and `compute doctor` report the mode and why.

## What the daemon keeps where

| What | Where | Durable? |
| --- | --- | --- |
| Desired state: environments, projects, revisions, memberships, releases, workloads, instances, traffic assignments, domains, DNS records, certificates (public facts), services, providers | Control state (file or Managed FeltDB) | Yes |
| Evidence: executions, receipt references, lifecycle events, observed workload status | Control state | Yes |
| Certificate keys and the ACME account key | The node's secret store (`<state-dir>/secrets`, 0700) | Node-local |
| Artifacts: workload bundles and receipt documents, by digest | The control state's artifact store (a directory for file state; chunks in FeltDB) | Yes |
| Operator credentials (SHA-256 verifiers, never the secrets) and the audit trail | Control state, with a 0600 snapshot of verifiers in `<state-dir>/credentials.json` and the audit trail mirrored to `<state-dir>/audit.log` | Yes |
| Running processes, endpoint listeners and their open connections, and the outcomes of units that ended | The supervisor, with its unit manifest in `<state-dir>/supervisor/` (no secrets) | Node-local, outlives the controller |
| Live health and output, restart backoff, the read cache | Controller memory | No |
| Artifact cache, service logs, the node lock, binaries kept for rollback, the last upgrade record | `--state-dir` | Node-local |

The daemon's memory can disappear, and so can its node. The desired state
cannot. A daemon started on a fresh node against the same Managed FeltDB
restores every environment, bundles included.

## Reconciliation

Each cycle, the reconciler reads desired state from control state, advances
every release in flight ([docs/releases.md](releases.md)), and then acts on
it:

- **Missing:** an instance that should run and doesn't is started.
- **Replaced:** an instance a release replaced is stopped once it has
  drained.
- **Should not run:** a running instance whose desired state (its
  workload's, its project's, or its environment's) is stopped is stopped.
- **Failed:** a service that fails or is killed restarts with backoff
  (1s doubling to 60s) unless its restart policy is `never`. A denied
  service is never retried.
- **Correct:** no change.

Endpoints are pointed at the instances their traffic assignments name.
Ingress routes, DNS records, and certificates are reconciled
([docs/networking.md](networking.md)). Then the reconciler writes what it
observed back to control state: workload status and events. Cycles run every `--reconcile-interval-ms`
and after every change made through the API. A change made directly in
Managed FeltDB, for example by another tool, takes effect within one cycle.

A cycle that can't read control state changes nothing. See
[Failing closed](control-plane.md#failing-closed).

## Crash recovery

The controller can stop in four ways, and none of them stops a workload
that runs on the supervisor:

| How | Workloads |
| --- | --- |
| Killed (SIGKILL, a crash) | Keep serving. The next controller reattaches to them. |
| SIGTERM (a service manager restarting it) | Keep serving; the controller detaches. |
| `compute stop --keep-workloads` | Keep serving; the controller detaches. |
| `compute stop` or SIGINT | Stopped, and the supervisor with them. |

A starting controller asks the supervisor what runs, reattaches every unit
it recognizes (`workload.reattached`), records the evidence of units that
ended while no controller ran, and treats units it cannot account for as
orphans. If the supervisor itself died, the controller starts a new one,
which stops anything its predecessor left running (never a second copy),
and the lost workloads restart (`workload.orphaned`, then
`workload.restarted`) whatever their restart policy: Compute lost them;
they did not fail.

With `--data-plane in-process`, a replacement controller reaps what its
predecessor left before reconciling. It acts only on a process group whose
leader it can verify, by boot ID and start time, as the process its
predecessor recorded. A reused PID is never signalled.

## Degraded control plane

When control state is unreachable, the controller keeps running what it
last knew:

- workloads and endpoints keep running and serving;
- reads are served from the last snapshot, with `X-Compute-State: stale`
  and `X-Compute-State-As-Of`; fresh reads say `live`, and reads served
  from the short read cache say `cached`;
- every change returns `503 state_unavailable`; nothing is written
  locally instead;
- `/info` reports `control_plane.mode: degraded_control_plane`, and
  `feltdb.unavailable` is recorded once it is back, followed by
  `feltdb.recovered` and a full reconciliation.

A controller that starts while control state is unreachable starts in this
mode (unless `--require-state-at-start`), and converges when it returns.

## The Compute API (`compute.api@1`)

Plain HTTP/1.1 with JSON bodies. Every response carries
`X-Compute-Api: compute.api@1`. `compute_environment::api::ROUTES` lists
every operation. The UI uses no other, and a test enforces it.

```text
GET  /health                  (no credential; for probes)
GET  /info                    GET /metrics   (Prometheus text)
GET  /auth/whoami             GET /audit?limit=
GET  /auth/credentials        POST /auth/credentials
POST /auth/credentials/:id/revoke | rotate
POST /node/reconcile          GET /node/upgrade   POST /node/upgrade   POST /node/rollback
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
GET  /deployments/:id/receipt POST /deployments/:id/rollback
GET  /domains                 POST /domains
GET  /domains/:domain         DELETE /domains/:domain
GET  /dns                     POST /dns/reconcile
GET  /certificates            POST /certificates/:domain/renew
GET  /network
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
| `authentication_failed` | 401 |
| `admission_denied`, `authorization_denied` | 403 |
| `not_found`, `no_route` | 404 |
| `conflict` (including a lost race with another writer), `cancelled` | 409 |
| `state_unavailable` (control state unreachable) | 503 |
| `runtime_unavailable`, `controller_unavailable` | 503 |
| `upgrade_failed`, `io` | 500 |

Every response carries `X-Request-Id`; errors carry it as `request_id`
too. [docs/architecture.md](architecture.md#failure-kinds) says what each
kind means.

### Authentication and authorization

Every request resolves to an operator, a credential, its scopes, and a
request ID. Credentials are Compute's own; Compute needs no external
identity provider.

| Scope | Allows |
| --- | --- |
| `compute.read` | Every `GET` except credentials and the audit trail |
| `compute.execute` | Running a task (`POST …/workloads/:w/run`) |
| `compute.deploy` | Registering revisions, deploying, promoting, rolling back, adding and removing projects |
| `compute.operate` | Starting, stopping, and restarting; environments; domains, DNS, and certificates; services; `node reconcile` |
| `compute.admin` | Everything, including credentials, the audit trail, upgrades, and shutdown. Any route not listed needs it |

```sh
compute auth create --operator alice --scope compute.read --scope compute.deploy --expires-in 90d
compute auth list
compute auth rotate <credential-id>      # a new secret; the old one stops working (or after --grace)
compute auth revoke <credential-id>
compute auth whoami
```

A token (`cmpt_<credential>_<secret>`) is shown once, when it is created
or rotated. Compute stores only its SHA-256 verifier and compares it in
constant time. Tokens never appear in logs, receipts, events, errors, or
later API responses. A production controller with no credentials writes a
bootstrap admin token to `<state-dir>/bootstrap-admin.token` (0600) at
start; create operator credentials with it, then revoke it.

Every change, and every refused request, is audited with its operator,
credential, request ID, operation, resource, and outcome (`compute auth audit`,
`GET /audit`). Authorization decides who may operate the node. Admission
still decides whether each execution may run.

## The UI

`http://<listen>/ui/` is the control-plane UI. Environments are the first
screen. From there you can:

- open an environment's projects, and each project's overview, workloads,
  deployments, logs, resources, configuration, receipts, and events
- assign projects to environments
- deploy a registered revision, or promote the revision current in another
  environment, and follow the release step by step on its own page, with
  its instances, readiness, traffic switch, events, and receipt
- roll a release back
- add, inspect, and remove domains, and see their DNS, TLS, and routing,
  reconcile DNS, and renew certificates
- start, stop, restart, or remove projects and workloads, each after a
  confirmation that states what it affects and what it doesn't

It updates live from `/events/stream`. When the daemon requires a
credential, enter an operator token under **Token**. It is kept for that
browser tab only and sent as a header, never in a URL.

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
| `compute.deployment.create@1`, `.promote@1`, `.rollback@1` | same as the capability |
| `compute.domain.list@1`, `.inspect@1` | `compute.domain.read` |
| `compute.domain.create@1`, `.remove@1` | same as the capability |
| `compute.dns.inspect@1` | `compute.dns.read` |
| `compute.dns.reconcile@1` | same as the capability |
| `compute.certificate.inspect@1` | `compute.certificate.read` |
| `compute.certificate.renew@1` | same as the capability |
