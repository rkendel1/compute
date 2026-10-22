# The Compute API contract

> **One control plane talks to one Compute API.** A control plane (which
> lives outside this repository) reaches every Compute capability through
> the controller's API, `compute.api`, and nothing else. Each operation is
> declared once, with a stable operation ID and the scope it needs. The
> contract can be read from the running controller.

```text
external control plane ─┐
UI · CLI · AppPort ─────┤  compute.api           GET /api: the contract
                        ▼
            Compute controller (the authority: operators, scopes, audit, FeltDB)
                        │  compute.remote@1      /compute/*: the target protocol
                        ▼
            targets (compute serve): answer, never an authority
```

## The registry

`crates/compute-environment/src/routes.rs` (`routes::ROUTES`) is the single
source of the surface. Each entry has:

| Field | Meaning |
| --- | --- |
| `method`, `path` | The HTTP operation. Parameters are written `{name}`. An environment is named by its name or its `env_…` ID |
| `operation` | A stable ID (`environment.fork`, `software.deploy`), independent of the path |
| `scope` | What the caller's credential needs (`compute.read`, `.execute`, `.deploy`, `.operate`, `.admin`) |
| `surface` | `controller`, or `target` for `compute.remote@1` |

Authorization (`auth::required_scope`), audit operation names, the published
contract, and the UI/API parity tests all read this one table. The scope
isn't kept in a second list beside it.

- **Only declared routes are served.** An authenticated request that
  matches no entry is answered `no_route`, whatever the caller's scopes, so
  a handler without a declaration is unreachable. With the parity test
  (every declared route is served), the served surface is the published one.
- **Reads are reads.** A route declared `compute.read` is a read even when
  it is a `POST` because it takes a body (job output, recipe resolution).
  Reads aren't written to the audit trail, and like `GET`s they are served
  while control state is unavailable. Every other request is a change: it
  is audited, and it is refused while control state is unavailable.
- **Enforced as published.** `tests/security.rs` checks every route on a
  real TLS daemon. A credential holding exactly the declared scope gets past
  authorization; a credential holding any other single scope is refused.

`GET /api` (scope `compute.read`) returns the contract:

```json
{
  "api": "compute.api@1",
  "scopes": ["compute.read", "compute.execute", "compute.deploy", "compute.operate", "compute.admin"],
  "routes": [
    { "method": "POST", "path": "/environments/{environment}/fork",
      "scope": "compute.operate", "operation": "environment.fork", "surface": "controller" }
  ]
}
```

A control plane negotiates against `GET /api` (what it may call),
`GET /compute/capabilities` (what this node can execute) and the
environment's computer view (what this machine can do). It doesn't assume
an installation's shape.

## Two surfaces, not two APIs

Most of the apparent duplication in the old surface came from mixing two
layers:

| | Controller (`compute.api`) | Target (`compute.remote@1`, `/compute/*`) |
| --- | --- | --- |
| Who calls it | a control plane, the UI, the CLI, AppPort | a controller (or a pool) placing work on a node |
| Authority | yes: operators, scopes, ownership, audit, FeltDB | none: it answers, and the controller records |
| Session | a **work session** (`wks_…`), a way into an environment | a **machine session** (`ses_…`), what a computer is made of |
| Errors | `{ kind, message, request_id }` | `{ kind, message, admission? }` |

A control plane never calls `/compute/sessions`. It works with the
environment that owns a computer, and the controller drives the target.
The target routes are declared in the registry (`surface: "target"`) so
that the surface is complete and audited, not so a control plane will
call them.

## Machine capabilities through the controller

What a computer's backend can do (memory-preserving suspend, machine fork,
interactive terminals; see [session-backends.md](session-backends.md)) is
reached through the environment that owns it:

| Capability | Controller operation | Refused when |
| --- | --- | --- |
| Interactive command | `POST /environments/{e}/exec` with `terminal: {rows, cols}` or `interactive: true` | the backend lacks `terminal` |
| Its output, from an offset | `POST /environments/{e}/jobs/{job}/output` `{stream, offset, wait_ms}` | the job isn't this environment's |
| Input, resize, signal, end of input | `POST /environments/{e}/jobs/{job}/input` `{data, resize, signal, close}` | the job isn't running, or isn't interactive |
| Cancel | `POST /environments/{e}/jobs/{job}/cancel` | the job isn't this environment's |
| Suspend and resume | `POST /environments/{e}/stop`, `/start` | never; memory is kept only where the computer view's `memory_suspend` says so |
| Machine fork | `POST /environments/{e}/fork` with `state: "machine"` | the backend lacks `fork`; the source has configuration and `copy_config` is not set (its processes carry the values) |

Every one of these checks the environment's owner, carries the scope in the
registry, and is audited like any other controller operation.

## Overlaps, and how each is resolved

| Overlap | Resolution |
| --- | --- |
| `/sessions` vs `/compute/sessions` | Different layers (above). On the controller, "session" means a work session only. |
| `/environments/{e}/fork` vs `/compute/sessions/{id}/fork` | One controller operation with an explicit `state`: `workspace` (portable files: the existing fork) or `machine` (a running copy of memory and disk). Neither is ever substituted for the other. |
| `/environments/{e}/exec` vs `/compute/sessions/{id}/exec` | The controller route is the control-plane one. It accepts the same command, terminal fields included. |
| Three ways to deliver software: `/projects` + `/deployments` (environments without a computer), `/applications`, `/software` + `/rollouts` | `/software` and `/rollouts` are canonical. The other two are **legacy**: each entry names its successor, and they stay served until `compute.api@2`. |
| Rollback in three places | `POST /software/{p}/rollback` is canonical; the others are legacy aliases of it. |
| Two error vocabularies | One per surface. Controller routes that relay a target's refusal translate it into a controller `kind`. On the target surface a scope refusal is `403` with the protocol's single `unauthorized` kind (the message names the missing scope); the controller says `authorization_denied`. |

## Versioning and migration

- **Additive changes stay `compute.api@1`:** new routes, new optional
  fields, new capabilities. A client ignores what it doesn't know.
- **Legacy routes** keep working and say so on every response: a
  `Deprecation: true` header and `Link: <successor>; rel="successor-version"`.
  Their contract entry carries `"status": "legacy"` and `"successor"`.
- **`compute.api@2`** is published when the legacy routes are removed:
  it is v1 minus the legacy entries, with nothing renamed. A client that
  uses only non-legacy routes is already a v2 client.

## Status

| Piece | State |
| --- | --- |
| Registry, `GET /api`, `/compute/*` fully declared, scopes derived from it | implemented |
| Environment job output, input, cancel | implemented |
| Machine fork through environments | implemented |
| Legacy marking and deprecation headers | planned |
| Removing legacy routes (`compute.api@2`) | after client migration (UI, CLI, `@compute/appport`) |
