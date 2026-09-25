# The Compute control plane

Compute is the execution authority. Managed FeltDB is the durable authority
for Compute's control state. The UI and the CLI are clients of the Compute
API, and neither is an authority.

```text
UI / CLI / AppPort
        │  compute.api@1
        ▼
Compute daemon ── authorization + admission
        │
        ├── desired state ◀──────── Managed FeltDB (durable)
        │        │
        │   reconciliation
        │        ▼
        ├── runtime (processes, WASM, providers)
        │        │
        └── evidence: status, executions, receipts, events ──▶ Managed FeltDB
```

The execution engine (`compute-core` and the runtimes) has no FeltDB
dependency. The daemon depends on `compute-state`, a small interface for
durable state. Backends are separate crates:

| Backend | Crate | Use |
| --- | --- | --- |
| `feltdb` | `compute-state-feltdb` | Production: Managed FeltDB over HTTPS |
| `file` | `compute-state-file` | Local development: one JSON document, rewritten atomically |
| `memory` | `compute-state-memory` | Tests and throwaway daemons |

All three pass the same conformance suite (`compute_state::conformance`).
It covers:

- atomic batches
- create conflicts
- total replace
- merge update
- version fencing that refuses stale writes
- ordered and ranged queries
- chunked artifacts
- durability

`compute-state-feltdb` speaks FeltDB's wire protocol directly, using
`reqwest` over rustls. It has no dependency on `@feltdb/core` or on the
embedded FeltDB crate.

## The model: `compute.flow`

`crates/compute-state-feltdb/model/compute.flow` is the schema authority,
`compute.state@1`:

| Collection | Holds |
| --- | --- |
| `Project` | Software, independent of where it runs |
| `ProjectRevision` | Immutable content: a label, its workloads, and their bundle identities |
| `Environment` | A deployed, isolated place (`preprod`, `production`), with its configuration and policy |
| `EnvironmentProject` | A project's membership in an environment: desired state, configuration, current revision and deployment |
| `Deployment` | A release of a revision to an environment: its status, configuration digest, and the evidence of each step |
| `Workload` | A service or task of a project in an environment, with its stable endpoint ports |
| `WorkloadInstance` | One instance of a service at one deployment's revision, with its own ports and state |
| `TrafficAssignment` | Which instance an endpoint serves; one per endpoint |
| `Domain`, `DnsRecord`, `Certificate` | The network control plane ([docs/networking.md](networking.md)); certificates by reference only |
| `WorkloadStatus` | What the daemon last observed: actual state and health |
| `Execution` | One invocation, with its status, exit code, and evidence IDs |
| `Receipt` | A reference to receipt evidence; the receipt itself is an artifact |
| `Service` | A shared service and the capabilities it provides (the model boundary for a later service catalog) |
| `Provider` | A member of the daemon's provider pool |
| `Event` | A lifecycle event, sequenced |
| `Artifact`, `ArtifactChunk` | Bundles and receipt documents, by digest, in request-sized chunks |

Only desired state and evidence live there. Live process state does not,
and neither do secrets: certificate keys live in the node's secret store,
and DNS and FeltDB credentials come from environment variables.

`compute.manifest.json` beside it is `compute.flow` compiled by `@feltdb/core`.
Three tests keep the three descriptions from drifting apart:

- `packages/compute-state-model` checks that compiling `compute.flow`
  reproduces the checked-in manifest exactly.
- `crates/compute-state-feltdb/tests/model.rs` checks that every Rust
  record writes only declared fields, includes every required one, and
  never writes `null`.
- The conformance suite writes every collection against a real FeltDB.

A change to the state model therefore needs a `compute.flow` change.
`Deployment.status` keeps the values written before releases (`queued` …
`superseded`) so existing records stay valid. Compute reads them as
`pending` or `complete`.

## Configuration

```toml
# compute.toml
[state]
backend = "feltdb"

[state.feltdb]
url = "https://feltdb.example.com"
application = "app_..."          # from compute control-plane provision
environment = "production"       # the FeltDB environment holding this control plane
token_env = "COMPUTE_FELTDB_TOKEN"
# ca_file = "feltdb-ca.pem"      # a private CA, added to the public roots
```

Flags override the file: `--state`, `--feltdb-url`, `--feltdb-application`,
`--feltdb-environment`, `--feltdb-token-env`, and `--feltdb-ca-file`. The
FeltDB API key comes only from the environment variable. It is never
written to configuration or state.

There is no implicit fallback. If `backend = "feltdb"` and FeltDB can't be
reached, the daemon doesn't start, and it never creates local state
instead.

## Provisioning Managed FeltDB

```sh
export COMPUTE_FELTDB_TOKEN=fdb_live_...
compute control-plane provision --feltdb-url https://feltdb.example.com
compute control-plane upgrade   --config compute.toml   # after upgrading Compute
```

`provision` follows FeltDB's application lifecycle:

1. Create or reuse a tenant (default name `compute`).
2. Create or reuse the application named `compute`.
3. Create a draft holding the compiled Compute model.
4. Validate the draft.
5. Commit it as a revision.
6. Promote that revision to the FeltDB environment.

It prints the `[state]` configuration to use.

Both commands are idempotent. Running them again reuses the tenant and the
application, and when the model is already current it changes nothing.
`upgrade` installs a newer model on the existing application and bumps
FeltDB's `state_schema_version`, as FeltDB requires. Existing data stays in
place.

The key needs these FeltDB scopes:

- `state:read`, `state:write`
- `application:read`, `application:write`
- `application:revision:read`, `application:revision:create`,
  `application:revision:promote`
- `application:environment:read`, `application:environment:write`

## Desired and actual state

What you ask for is written to control state first and reconciled second.
What the daemon observes is written back as `WorkloadStatus`, `Execution`,
`Receipt`, and `Event` records. Views such as `compute environment status`
and the UI join both:

```text
production / attn
  desired: revision abc123, running
  actual:  revision abc123, running, healthy
```

## Deployments

```sh
compute project push attn --source ./attn --revision abc123   # register an immutable revision
compute deploy attn --environment preprod --revision abc123 --wait
compute promote attn --from preprod --to production --set DATABASE_URL=... --wait
compute deployment list --project attn
```

A revision is immutable: a label always names the same content, and
reusing a label for different content is refused. A deployment is a
**release**, a durable state machine:

```text
pending → starting → ready → network_ready → switching → active → draining → complete
   ╰──────────╰─────────╰───────────╰──→ failed          (the old revision keeps serving)
                                switching ─→ rolled_back   (traffic returned to the old revision)
```

The new revision starts next to the one serving. Traffic moves in one
transaction once the new revision is ready, and the old revision drains
before it stops. Every step's evidence is in the release record, so a
daemon that restarts continues a release from its status. See
[docs/releases.md](releases.md).

`deploy` exits non-zero when the release fails or is rolled back, and
`--wait` follows it until it ends. No deployment is silent. Each one is
recorded with its evidence, its events, and a deployment receipt.

**Promotion** deploys the exact revision current in the source
environment, with the same revision ID and content digest, and records
`promoted_from`. Nothing is rebuilt. Configuration isn't copied:
production's configuration is production's, and `--set` sets it. By
default only a released (`active`, `draining`, or `complete`) source
deployment can be promoted. `compute deploy feltdb --from preprod --to
production` is the same operation.

## Failing closed

- **At startup:** a daemon that can't read its control state doesn't
  start.
- **While it runs:** every change returns `503 state_unavailable`, and the
  reconciler changes nothing. Running services keep running, because an
  unreachable store isn't an order to stop. `compute status` shows the
  state as unavailable.
- **Nothing is ever written locally in place of the configured state.**

## Recovery

Certification (`crates/compute-cli/tests/recovery.rs`) runs the real binary
through two failures:

1. **The daemon is killed on its node.** A new daemon reaps the orphaned
   services, restores them on the same host ports, and never runs two
   copies.
2. **The node is lost entirely.** A new daemon on a fresh node, against the
   same Managed FeltDB, restores every environment, deployment, and bundle.

## Events

Lifecycle events are durable, sequenced records:

- **Environment:** `environment.created`, `environment.destroyed`,
  `environment.started`, `environment.stopped`, `environment.restarted`
- **Project:** `project.registered`, `project.revision_created`,
  `project.added`, `project.removed`, `project.started`, `project.stopped`,
  `project.restarted`
- **Deployment:** `deployment.started`, `deployment.admitted`,
  `deployment.placed`, `deployment.ready`, `deployment.switched`,
  `deployment.activated`, `deployment.draining`, `deployment.completed`,
  `deployment.failed`, `deployment.rolled_back`, `deployment.promoted`
- **Instance:** `instance.ready`, `instance.stopped`
- **Network:** `domain.created`, `domain.removed`, `network.dns.applied`,
  `network.dns.drifted`, `network.dns.failed`,
  `network.certificate.issued`, `network.certificate.renewed`,
  `network.certificate.failed`, `network.route.switched`
- **Service:** `service.started`, `service.healthy`, `service.unhealthy`,
  `service.stopped`, `service.failed`, `service.denied`
- **Task:** `task.completed`, `task.failed`, `task.denied`
- **Daemon:** `daemon.started`, `daemon.stopped`

Read them with `compute events [--follow]`, `GET /events?after=N`, or the
`/events/stream` server-sent events. The UI uses the stream, and any other
consumer (such as Attn) can too.

## Receipts are evidence

Control state keeps a receipt's reference: its ID, the execution, the
workload, project, environment, and deployment IDs, the policy and
admission IDs, and the artifact digest. The receipt document itself is an
artifact, served by `GET /receipts/:id` and verifiable with
`compute receipt verify`. A release's own receipt,
`compute.deployment-receipt@1`, is an artifact too, served by
`GET /deployments/:id/receipt`.

## Shared services

`compute service register laya --capability llm.generate@1 --endpoint …`
records a shared service and the capabilities it provides. This is the model
boundary only: Compute doesn't yet manage a service catalog.

## Limitations

- **One daemon per control plane.** Two daemons against the same FeltDB
  application and environment would both reconcile it; nothing prevents
  that yet.
- **Endpoints are served by the daemon.** While the daemon itself is down,
  endpoints do not accept connections (see
  [releases](releases.md#limitations)).
- **Services run on the daemon's own node.** Tasks can be placed on pool
  providers.
- **Revisions come from the CLI.** The UI deploys revisions that are
  already registered, with `compute project push` or
  `compute deploy --source`.
- **CPU usage is not measured.**
