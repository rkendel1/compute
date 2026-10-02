# Compute is API-first and headless

**Status:** implemented. Compute serves its complete platform capability through
one authenticated HTTP API and runs with no UI at all. The UI remains available
for operators and development, and is optional.

```text
Control plane
      │
      ▼
   AppPort
      │
      ▼
Compute API  (compute.api@1, the flat route table)
      │
      ▼
Compute execution ── AppPort Services (its own contracts, 127.0.0.1:4100)
```

## What this document is

The audit that preceded the change, and the record of what was decided from it.
Every row was read from the code, not from a design document.

## The audit

Compute already had one control-plane API. `crates/compute-environment/src/api.rs`
declares `ROUTES`: **148 operations**, authenticated to an operator and authorized
against the scope each route declares, with every mutation audited and every
execution it causes passing through admission. `tests/parity.rs` already held the
UI to that list: the UI performs no operation the API does not expose, and every
route in the table is really served.

The finding that shaped the work: **the requested surface already existed.** Not
as `/api/v1/*`, but as `compute.api@1`.

| Capability | Implementation | UI | CLI | Existing API | Required API | Headless-safe | Control-plane consumer |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Liveness | `daemon.health()` | yes | `compute doctor` | `GET /health` | same | yes | health probe |
| **Readiness** | **absent** | — | — | **absent** | **`GET /ready`** | **yes** | **rollout gate** |
| Capability discovery | provider catalog | yes | `compute capabilities` | `GET /compute/capabilities`, `GET /info` | same | yes | negotiation |
| Environments | `daemon/environments.rs` | yes | `compute environment` | `/environments…` (25) | same | yes | lifecycle |
| Projects / workspaces | `daemon/projects.rs` | yes | `compute project` | `/projects…`, `/environments/{e}/workspace/*` | same | yes | workspace |
| Execution | `compute-runtime*`, jobs | yes | `compute run`, `compute exec` | `POST /compute/execute`, `/compute/jobs`, `/environments/{e}/run` | same | yes | build, test, CI |
| Terminal / sessions | `compute-core/src/sessions.rs` | yes | `compute session` | `/sessions`, `/environments/{e}/connect` | same | yes | interactive |
| Releases | `daemon/release.rs` | yes | `compute version` | `/software/{p}/versions`, `/rollouts` | same | yes | build → release |
| Deployments | `daemon/deploy.rs` | yes | `compute deploy` | `/deployments`, `/applications/*/deployments` | same | yes | deploy, promote |
| **Rollback** | `daemon/release.rs` | yes | `compute rollback` | **`POST /deployments/{d}/rollback`**, `/software/{p}/rollback`, `/applications/{a}/rollback` | same | yes | rollback |
| Production | reconciler, drain/switch | yes | `compute status` | `/deployments/{d}`, `/status` | same | yes | observe |
| DNS / TLS | `compute-network` | yes | `compute network` | `/domains`, `/dns`, `/certificates` | same | yes | domain config |
| Services | `service_ui.rs` | yes | `compute service` | `/services`, `/services/{s}/ui` | same | yes | AppPort Services |
| Evidence | `compute-core/src/receipt.rs` | yes | `compute receipt` | `/receipts/{r}`, `/events`, `/audit` | same | yes | evidence |
| **API keys, secrets, webhooks, schedules, jobs, files, notifications** | **`@appport/services`** | links out | `appport-services` | **not Compute's to expose** | **the existing AppPort Services contracts** | yes | config |

### What the audit changed

| | Before | After |
| --- | --- | --- |
| UI | required to reach the platform by hand | optional; `ui: false` serves none of it |
| Readiness | no answer distinct from liveness | `GET /ready`, independent of the UI |
| Startup summary | — | reports API / AppPort / Execution / Services / UI |

## What was deliberately not built

The brief proposed a parallel `/api/v1` tree. **It was not built**, because it
would have violated the brief's own constraints:

- *Do not create a parallel bespoke protocol.* `/api/v1/executions` beside
  `/compute/execute` is two protocols for one capability.
- *Do not duplicate.* Of the twenty requested domains, **nineteen already exist**
  in `compute.api@1` under their real names. A second tree would have made two
  sources of truth for deployments, releases and rollback.
- *First expose what already exists.* The work was to make the existing surface
  genuinely headless and independently verifiable, which is what landed.

Five requested domains — `secrets`, `api-keys`, `webhooks`, `schedules`, `jobs` —
are **`@appport/services` capabilities, not Compute's**. Compute Configured
starts that process, registers it, and discovers its `AppPort/ui/1` contribution.
Reimplementing them in Compute's API is exactly the duplication the brief
forbids, so they stay behind their existing contracts.

## Operating headless

```sh
compute start --headless
compute-configured up          # the UI opens by default
```

In `compute.toml`:

```toml
[api]
ui = false
```

`--headless` wins over `[api] ui`, the same way `--production` wins over
`[api] mode`. The UI is on by default, so every existing installation is
unchanged.

With the UI off, `/` and `/ui`, `/ui/app.js` and `/ui/app.css` return `404` and
nothing else changes. Startup reports what is up:

```text
Compute-configured
  API:       ready
  AppPort:   ready
  Execution: ready
  Services:  ready
  UI:        disabled
```

### Health and readiness

| Route | Answers | Credential |
| --- | --- | --- |
| `GET /health` | is the process alive? | none |
| `GET /ready` | can it accept work? | none |

`/ready` reports `accepting_work`, `state_available`, the API id
(`compute.api@1`), and whether the UI is present. It never depends on the UI, so
a headless controller gates a rollout exactly as a normal one does.

### Capability discovery

`GET /compute/capabilities` describes what this node can execute;
`GET /info` describes the controller, its security mode, and the data plane.
A control plane negotiates against these rather than assuming an installation's
shape.

### Ports

The API port is whatever `--listen` (or the configured address) says. Headless
adds no port and removes none: there is no separate UI port today, because the
UI is served by the same listener. A control plane discovers the endpoint rather
than assuming `127.0.0.1:8787`.

### Authentication

Unchanged, and still Compute's own: `--production` requires TLS and an operator
credential on every request, reads included. **AuthBoundry is not a dependency of
this change** and is not introduced here; it belongs to the control plane's
authority model, not to Compute's platform API.

## Long-running operations

Nothing blocks on a build, test, or deploy. The API already returns `202` with an
identifier and a receipt, and the caller observes it:

```text
POST /compute/execute     → 202 {execution_id}
GET  /executions/{id}     → status, result, receipt_id
GET  /receipts/{id}       → the canonical receipt
GET  /events/stream?after=→ the same events, server-sent
POST /compute/jobs/{id}/cancel
```

Deployments are first-class: `POST /deployments`, `POST /deployments/promote`,
`GET /deployments/{id}`, `POST /deployments/{id}/rollback`, all backed by the one
deployment record in control state. There is no second deployment store.

## Invariants

Enforced by `crates/compute-environment/tests/headless.rs` and
`tests/parity.rs`:

| | Invariant | Enforced by |
| --- | --- | --- |
| 1 | Headless execution: the platform is fully operational without the UI | `headless_starts_and_serves_the_api_without_the_ui`, `headless_mode_is_the_only_ui_dependency` |
| 2 | API authority: operations are available through the API, not the UI | `every_ui_operation_is_an_api_operation`, `the_api_route_table_covers_every_domain_a_control_plane_needs` |
| 3 | Single control plane: Compute is not an application control plane | review; Compute exposes execution and deployment only |
| 4 | Execution boundary: execution happens in Compute | `daemon/` placement, admission and data plane |
| 5 | AppPort boundary: existing contracts are reused, not duplicated | no Compute route re-implements an AppPort Services capability |
| 6 | Optional UI: the UI is never a lifecycle dependency | `ui_assets_are_confined_to_their_own_routes`, `normal_startup_still_serves_the_ui` |
| 7 | No hidden state: API state is the authoritative state | every mutation writes control state and an audit record |