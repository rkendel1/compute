# Compute applications and deployments

An application is a durable thing you run. A deployment is one immutable,
versioned release of that application.

```sh
compute init my-api
compute deploy my-api
compute status my-api
compute logs my-api --follow

# Change the source and deploy again. The endpoint stays the same.
compute deploy my-api
compute history my-api
compute rollback my-api 1
compute stop my-api
```

`compute deploy` starts the local Compute daemon when the default daemon is
not already running. Runtime resolution, placement, admission, readiness,
traffic switching, and receipt creation remain behind the application
interface.

## Identity and immutable versions

```text
application → deployment version → workload job/execution → receipt
```

The application name has stable product identity. Each deployment has its
own `dep_…` identity and a monotonically increasing `v1`, `v2`, … version
scoped to that application. It references an immutable registered revision
containing the canonical workload bundle, runtime requirement, artifact
identity, resources, network contract, and readiness configuration. Jobs and
execution IDs are never reused as deployment IDs or versions.

Deployment records, the active relation, revisions, traffic assignments, and
receipts use the existing `compute-state` authority. Managed installations
use FeltDB; local development uses the existing file backend. There is no
application-specific database.

## Lifecycle and replacement

```text
pending → starting → ready → network_ready → switching
        → active → draining → complete
        ↘ failed
```

The previous deployment continues serving while the new revision is
admitted, started, and checked. Compute switches the stable endpoint only
after HTTP readiness succeeds. A failure before the switch leaves the old
deployment active. After switching, the previous instance drains and stops.

The stable endpoint is a logical port owned by the application workload.
Each deployment receives a separate instance port and Compute atomically
retargets the logical endpoint. This is node-local routing, not DNS, TLS,
global ingress, or load balancing.

## Status, history, inspection, and receipts

`compute status APP` reports the active deployment and actual application
state. `compute history APP` lists immutable deployments newest first.

```sh
compute deployment dep_...
compute deployment inspect dep_...
compute deployment receipt dep_...
```

Deployment receipts use `compute.deployment-receipt@1` and bind application
identity, deployment identity and version, immutable revision and artifact
digests, configuration digest, placement/admission evidence, traffic switch,
execution receipts, and lifecycle events. Existing `compute.receipt@1`
execution receipts remain compatible and carry deployment scope.

## Rollback and recovery

`compute rollback APP VERSION_OR_DEPLOYMENT` never edits history. It deploys
the selected immutable revision again as the next version. Rolling `v3` back
to `v1`, for example, creates `v4`. The normal readiness and traffic-switch
path applies, so a rollback that cannot become ready preserves the working
deployment.

Transitions and the active relation are committed durably. After a controller
restart, reconciliation resumes an in-flight release and reconstructs the
active service and endpoint. If FeltDB is unavailable, existing workloads
continue serving while new mutations fail closed.

## `run` versus `deploy`

`compute run APP` remains the lightweight durable-job experience against a
caller-owned provider pool. `compute deploy APP` creates a versioned release
with readiness, stable endpoint replacement, history, and rollback.

Logs resolve to the active deployment. `--version` accepts a historical
version only while that version's instance and logs are retained; Compute
reports this limitation instead of creating a second log store.

See [the runnable HTTP demo](../examples/compute-demo/),
[releases.md](releases.md), and [receipts.md](receipts.md).
