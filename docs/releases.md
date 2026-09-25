# Zero-downtime releases

A release takes a project from the revision that serves in an environment to
a new one without refusing a connection. The new revision starts next to the
old one, traffic moves only once the new revision is ready, and the old one
finishes its open connections before it stops. A release that fails before
traffic moves leaves the old revision serving. A release that fails after
the move returns traffic to the old revision.

```sh
compute deploy feltdb --environment preprod --source ./feltdb --revision v0.11.8 --wait
compute deploy feltdb --from preprod --to production --wait     # promotion
compute deployment status dep_…
compute deployment rollback dep_…
```

## The state machine

A release is a `Deployment` record in control state. Its status is the
single source of truth for what happens next:

```text
pending → starting → ready → network_ready → switching → active → draining → complete
   ╰──────────╰─────────╰───────────╰──→ failed          (the old revision keeps serving)
                                switching ─→ rolled_back   (traffic returned to the old revision)
```

| Status | Durable evidence | What the controller does next |
| --- | --- | --- |
| `pending` | the release: revision, configuration and its digest, the deployment it replaces | Admits and places every workload, plans ports, and creates `starting` instances |
| `starting` | instances and their ports; admission and placement evidence | Starts the instances next to what serves, checks readiness, and marks each instance `ready` |
| `ready` | `readiness_result` | Verifies each endpoint can serve, and records the DNS and TLS state of its domains |
| `network_ready` | `network_result` | Switches traffic, in one transaction |
| `switching` | traffic assignments and `traffic_switch_result` | Verifies the data plane follows the assignments, through the endpoint |
| `active` | the verified switch | Starts draining what the release replaced |
| `draining` | `draining` instances | Waits until each has finished its connections or the drain timeout passes |
| `complete` | the deployment receipt | Nothing |

Each reconciliation cycle takes every release in flight one or more steps.
A step reads control state, inspects the node (processes, ports, endpoints),
and commits at most one transition. The controller keeps no plan in memory.
A daemon that crashes at any point is replaced by one that reloads the
release from control state, looks at the node, and continues from its
status. Certification kills the daemon with `SIGKILL` while a release is
starting, and the next daemon finishes it with exactly one switch.

While a release is in flight, the reconciler runs every 100 ms instead of
every `--reconcile-interval-ms`. One release per project and environment
runs at a time. A second one is refused with `409 conflict` until the first
ends or is rolled back.

## Instances and endpoints

A service runs as **instances**: the workload at one deployment's revision,
each on its own port from `--instance-port-range` (default `30000-39999`).
Clients never see an instance port. They reach the service's **endpoint**,
a host port from `--port-range` (default `20000-29999`) that stays the same
across releases. The daemon's data plane listens on each endpoint and
forwards every new connection to the instance its **traffic assignment**
names:

```text
client ──▶ endpoint :20006 ──▶ instance wi_… (v1) :30000     before
client ──▶ endpoint :20006 ──▶ instance wi_… (v2) :30001     after the switch
```

There is exactly one assignment per endpoint, so exactly one revision serves
it. The switch is one transaction: the assignments, the project's current
deployment and configuration, its workloads, and the instance states. The
data plane retargets right after the switch commits. The endpoint's listener
never closes, so a client never sees a refused connection. A connection
already open finishes on the instance it started on.

A service receives its instance port as `PORT` (with one port) and as
`COMPUTE_PORT_<NAME>`, and listens there. Endpoints listen on
`--endpoint-address` (default `127.0.0.1`). The public entry is
[ingress](networking.md).

## Readiness

Traffic moves only to an instance that proved it can take it. A service
declares how:

```toml
[[workload]]
name = "api"
kind = "service"
workload = "api/workload.json"
ports = [{ name = "http", port = 8000 }]
readiness = { check = "http", path = "/health", timeout_ms = 60000 }
```

| `check` | Ready when |
| --- | --- |
| `port` (the default with ports) | The port accepts TCP connections |
| `http` | `GET path` on the port answers 2xx or 3xx |
| `process` (the default without ports) | The process has stayed up for 500 ms |
| `task` | A task of the same revision (`task = "smoke"`) exits 0 |

`port` names which declared port to check, and defaults to the first.
`interval_ms` (default 250) spaces the checks. When `timeout_ms` (default
60000) passes without every instance ready, the release fails with what each
one was waiting for, and the old revision keeps serving. A candidate that
exits before it is ready fails the release at once when its restart policy
is `never`, or after three failures with `on_failure`. A service whose
desired state is `stopped` is not started, and so it is not checked.

`network_ready` then checks that each endpoint can listen and that each
instance still accepts connections. At `switching`, an `http` check is
repeated through the endpoint itself. If verification does not succeed
within `switch_timeout` (10 s), the release is **rolled back**: one
transaction returns every assignment, the project's current deployment, and
its workloads to the previous revision.

## Draining

After a verified switch, the replaced instances are `draining`. Each stops
once no connection through its endpoints is open, or when `--drain-timeout-ms`
(default 30000) passes, whichever is first. The release is `complete` when
nothing it replaced is still draining. A replaced instance stays up while the
release is `switching`, so a rollback returns to a running instance.

## Rollback

`compute deployment rollback dep_…` (and `POST /deployments/:id/rollback`)
does what the release's status allows:

| Status | Rollback |
| --- | --- |
| `pending` … `network_ready` | The release is abandoned (`failed`); its instances stop. Nothing served changes. |
| `switching`, `active`, `draining` | Traffic returns to the previous revision (`rolled_back`). |
| `complete`, and current | A new release of the revision it replaced, with that release's configuration |

## Revision identity

A revision is immutable. Its identity covers each workload's bundle, runtime,
runtime version, dependency capsule, required distribution, ports, restart
policy, and readiness. Configuration is not part of a revision. It belongs to
the environment. Each release records the configuration it runs with and a
digest of it (`config_digest`), and at the switch that configuration becomes
the project's configuration in the environment. Promotion releases the exact
revision (the same content digest) with the target environment's
configuration.

## Deployment receipts

Every release that ends (`complete`, `failed`, or `rolled_back`) writes a
receipt, `compute.deployment-receipt@1`, as an artifact. Its digest is the
release's `receipt` field. `compute deployment receipt dep_…` prints it.
The receipt records:

- the revision and its digest, and the revision it replaced
- the configuration digest
- admission and placement evidence for every workload
- the readiness, network, and traffic-switch results
- execution receipt IDs and the sequence of the release's events
- the failure or rollback reason

It never holds configuration values, keys, tokens, or credentials.
Certification checks that a secret in the release's configuration does not
appear in its receipt.

## Events

`deployment.started`, `deployment.admitted`, `deployment.placed`,
`instance.ready`, `deployment.ready`, `deployment.switched`,
`deployment.activated`, `network.route.switched`, `deployment.draining`,
`instance.stopped`, `deployment.completed`, `deployment.failed`,
`deployment.rolled_back`, and `deployment.promoted`. `compute events
--deployment dep_…` shows one release's events.

## Certification

`crates/compute-environment/tests/releases.rs` runs real services under
continuous client load:

- **A successful release.** No request fails, only v1 and v2 answer, the
  endpoint is unchanged, the events arrive in order, the replaced instance
  drains and stops, and the receipt holds no configuration values.
- **A failed release.** The candidate crashes. The release fails, and only
  v1 ever answers.
- **A readiness timeout.** The candidate never listens. The release fails
  after the timeout, and the candidate is stopped.
- **Draining.** A connection opened to v1 before the switch keeps the
  release in `draining`, and is answered by v1 after new connections reach
  v2.
- **Rollback after the switch.** The candidate passes readiness and then
  errors through the endpoint. Traffic returns to v1.
- **Operator rollback.** No request fails.
- **A restart mid-release.** The daemon stops while a release is starting.
  The next daemon finishes it with one switch.

`crates/compute-cli/tests/recovery.rs` repeats the restart with the real
binary and `SIGKILL`.

## Limitations

- **One node.** Instances run on the daemon's own node, one per service.
  Weighted, canary, and multi-region traffic are out of scope.
- **Endpoints are served by the daemon.** While the daemon is down, its
  endpoints do not accept connections. A daemon that restarts reaps the
  services its predecessor left, restores them, and serves the endpoints
  again. A release in flight continues.
- **Draining is per connection.** An idle keep-alive connection counts as
  open until it closes or the drain timeout passes.
