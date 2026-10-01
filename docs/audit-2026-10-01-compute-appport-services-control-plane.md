# Audit: Compute and AppPort Services — control plane and UI

**Date:** 2026-10-01 · **Type:** audit only; no product code changed.

**Sources read** (shallow clones, current default branches): `rkendel1/compute`
at `8979e2d`; `rkendel1/appport-services` (`@appport/services` 0.4.6, which also
contains `packages/runtime` and `packages/create-appport`); `rkendel1/appport`
(the protocol repository). **Not read:** the AuthBoundry, AppBoundry, Attn or
FeltDB repositories, and any portal or studio outside these three. Nothing was
run; every statement below is from source and docs, with the file named.

## Executive answer

**Should you see API keys, webhooks, jobs, schedules, notifications, secrets and
files inside Compute today? No.** Compute's UI has no page for any of them, and
nothing in Compute starts, proxies, embeds or discovers AppPort Services.

Why, precisely:

1. **AppPort Services is a library, not a service Compute can start.** Compute's
   own platform inventory describes it as "Node library inside each
   application's process" (`scripts/platform/feltdb-consumers.json`, entry
   `appport-services`). It has no server entry point of its own; the only
   `bin` is an operator CLI (`package.json` → `appport-services`,
   `dist/src/cli.js`), and `@appport/runtime` is a wrapper that an
   *application* imports and starts.
2. **AppPort Services does have a UI, but it only exists inside a host
   application's process.** The pages are server-rendered HTML in
   `src/configuration/ui.ts` (`/services`, `/api-keys`, `/jobs`, `/schedules`,
   `/files`, `/secrets`, `/webhooks`, `/notifications`, `/configuration`),
   mounted by `createManagementRouter` (`src/runtime/management.ts`) into an
   Express app the host owns. The standalone runtime that `appport()` starts
   turns them **off**: `startHttpRuntime` passes `includeUi: false` and
   `includeConfiguration: false` (`src/runtime/platform.ts:46-52`). So running a
   plain `appport()` application gives you the JSON management API under
   `/_appport/*` and **no pages**.
3. **The composition mechanism that would let Compute show them is specified but
   not wired at either end.** `rkendel1/appport` defines `AppPort/ui/1`
   contributions and `GET /v1/ui` (`packages/protocol/src/ui.ts`,
   `packages/client/src/composition.ts`, `docs/composable-ui.md`), and
   `@appport/services` exports an `APPPORT_UI_CONTRIBUTIONS` constant — but it
   is a different, smaller shape (`{protocol, id, requiredCapabilities}`, API
   keys only; `management.ts:73-77`) and `@appport/services` does not serve
   `/v1/ui`. Compute does not consume `/v1/ui` anywhere (no reference in
   `crates/`, `packages/`, or `docs/`).
4. **Compute's "Services" page is something else.** `#/services`
   (`crates/compute-environment/ui/app.js`, `servicesView`) lists
   `GET /services`: shared services *registered by hand* with
   `compute service register NAME --capability … --endpoint …`. The docs call it
   "the model boundary only: Compute doesn't yet manage a service catalog"
   (`docs/control-plane.md`, "Shared services"). It stores a name, capability
   strings, provider and endpoint (`ServiceRecord`,
   `crates/compute-state/src/model.rs:941`); it has no AppPort-specific
   behaviour and no link to any AppPort page.

**Which of your statements is true: F, a combination.**

| | Verdict | Evidence |
| --- | --- | --- |
| A. Compute starts it, UI fails to show it | **False** | Nothing in Compute starts AppPort Services (below) |
| B. An integration mechanism exists but is not enabled | **Partly.** A generic, manual shared-service registry exists; it is not AppPort-aware | `ServiceRecord`, `docs/control-plane.md` |
| C. Runs separately, no Compute integration | **True** for runtime, UI, state and auth. The only links are a *package* in a stack and the registry record | `stacks/randy/stack.toml`, below |
| D. APIs but no UI | **False.** A UI exists, but only in embedded mode | `src/configuration/ui.ts`, `management.ts:129-143` |
| E. A different intended control-plane architecture exists | **True.** Composable UI (`AppPort/ui/1`, `GET /v1/ui`) is the intended direction, specified in `appport`, unimplemented in `appport-services` and not consumed by Compute | `docs/composable-ui.md` |

## Current topology

```text
 TODAY (extracted from source; nothing here is proposed)

 Browser ──▶ Compute UI /ui/ ─▶ Compute API (compute.api@1) ─▶ Compute daemon
             (127.0.0.1:8787)   operator token, scopes           │  control state:
                                compute.read … (auth.rs)         │  FeltDB (production) or a
                                                                 │  local file (development)
                                                                 ▼
                                              computers on targets (compute serve, :8788 locally)

 ── no connection ──

 YOUR APPLICATION PROCESS (Node)                      its own FeltDB
   imports @appport/runtime ──▶ @appport/services ──▶ namespace/application per app
   │  /_appport/* JSON management API (always)         (local: .appport/state;
   │  /api-keys /jobs /webhooks … HTML pages            server: $FELTDB_URL)
   │     only when the host mounts createManagementRouter
   │     (includeUi defaults true there; false in appport()'s own server)
   └─ authorization: AuthBoundry (ServiceAuthorizer); identity: the host's authenticate()
```

Components, extracted:

| Component | What it is | Start | Port / URL (from source) | Source |
| --- | --- | --- | --- | --- |
| Compute control plane + UI | daemon, UI served at `/ui/` | `compute` / `compute up` (spawns `compute serve` + `compute start`); or `compute start` | `127.0.0.1:8787` (UI at `/ui/`); computer host `127.0.0.1:8788` | `crates/compute-cli/src/launch_cmd.rs:1-10,217`; `compute-environment/src/client.rs:12` |
| AppPort Services | library inside an application | `npx create-appport my-app && npm run dev`, or your own `npm run dev`, which runs the app | configurable `[http] host/port`; `appport init` and `examples/services-demo/appport.toml` both write **`127.0.0.1:8787`** | `README.md`; `src/cli.ts:203`; `examples/services-demo/package.json` (`"dev": "npm run build && node dist/src/app.js"`) |
| AppPort Services CLI | operator commands against the app's state | `npx @appport/runtime api-key list --tenant …` etc. | none | `README.md` |
| AppPort Services UI | HTML pages in `ui.ts` | **no command**; exists only if the host app mounts `createManagementRouter` | host's port | `management.ts:96-146` |
| `@compute/appport` | Compute exposed *as* AppPort capabilities (`compute.run@1` …) | imported by an AppPort application | none | `packages/compute-appport/README.md` |

**Port collision to note:** Compute's default is `127.0.0.1:8787`, and AppPort
Services' generated and example configuration also use `127.0.0.1:8787`. Running
both locally with defaults makes the second one fail to bind. (Both are defaults
read from source; not exercised.)

No `vercel.json`, `fly.toml`, Dockerfile or workflow exists in
`appport-services`. Compute's only deployment material is `deploy/hetzner`,
which references nothing from AppPort. There is therefore **no deployment
configuration** for Vercel, Fly or any target that places AppPort Services next
to Compute; the claim "AppPort Services is the same process / same app / same
host as Compute" has no support in either repository.

## Compute: what it owns today

Verified in source: the CLI (`crates/compute-cli`), the daemon and API
(`crates/compute-environment`, `docs/daemon.md`), the UI (`ui/index.html`,
`app.js`, `app.css`, embedded by `include_str!` at `api.rs:240-242`), environments,
targets/providers/pools, recipes, applications (`daemon/applications.rs`),
sessions (`compute-core/src/sessions.rs`), executions and receipts, and
supervised service processes. UI navigation is a fixed list in `index.html`:
Home, Software, Computers, Environments, Bundles, Domains, Services, Events.
**There is no extension, plugin, module-registration or navigation-contribution
mechanism**: routes are a fixed `if (parts[0] === …)` table in `app.js`
(`~line 2187`), and API routes a fixed `ROUTES` list (`api.rs`). The word
"AppPort" in Compute's code means (a) `@compute/appport`, Compute exposed to
AppPort clients, and (b) a `SessionConnectionMode::Appport` enum value
(`sessions.rs:215`); neither integrates AppPort Services.

The one place Compute names `@appport/services` operationally is the `randy`
stack: a **`package` component** (`stacks/randy/stack.toml`), meaning a
dependency Compute can verify is installed on a Computer
(`docs/stacks.md`). A stack "never contains an application" and "contains no
commands". It does not start anything.

`docs/provider-pools.md:142` states the intended layering from Compute's side:
service discovery, a multi-tenant control plane and similar belong to "the layer
above Compute, such as Factory, AppPort Services, CI systems…".

## AppPort Services: what it owns today

From `README.md`, `docs/architecture.md`, `docs/management.md`, `appport.flow`:
tenant-scoped **API keys**, **webhooks** (outbound and inbound), **jobs and
schedules**, **secrets** metadata and scoped resolution, **configuration**,
**notifications**, **files** metadata — each with a service, a FeltDB store, a
management HTTP surface, and (embedded only) a page. Its own words: "AppPort
Services provides durable operational application capabilities that sit beside
AuthPort"; "@appport/services executes effects. It does not decide who is
allowed to cause them" (`README.md`).

Authority chain, as implemented: `ServiceGateway` asks the AuthBoundry-compatible
`ServiceAuthorizer` before every effect; without an authorizer, effects fail
closed (`src/runtime/appport.ts:58-59`; `docs/management.md`). The intended
shape `.flow authority → service API → CLI / UI` is what the code does: the
pages call `/_appport/*` and `/v1/configuration` with `fetch`
(`ui.ts`), the CLI opens FeltDB directly, and neither bypasses the gateway.

## UI

| Question | Answer |
| --- | --- |
| Does a UI exist? | **Yes**: nine server-rendered pages. Screenshots in `docs/screenshots/` |
| Where | `appport-services/src/configuration/ui.ts` (`createConfigurationUiRouter`); mounted by `createManagementRouter` |
| What starts it | Nothing by itself. A host Express app must call `createManagementRouter({services, authority, authenticate})`. `appport()`'s built-in server does not (`includeUi:false`) |
| Backend | `/_appport/*` and `/v1/configuration` on the same router, same process |
| Auth | Host `authenticate()` supplies identity only; AuthBoundry authorizes each capability (`apikeys.read` …). Standalone mode uses an AppPort Bearer API key |
| FeltDB | Not directly: through the services, which use FeltDB |
| Standalone or embedded? | Built to be **embedded** in a host that supplies identity; the standalone runtime deliberately omits it |
| Composable | Declared in `appport/docs/composable-ui.md` ("hosts compose those surfaces"); `@appport/services` exports a partial contribution constant but no `GET /v1/ui` |

`appport` (protocol repo) implements the composition side: `UiContribution`
types and validation (`protocol/src/ui.ts`), `composeUi` (`client/src/composition.ts`),
and `GET /v1/ui` on its HTTP server (`server/src/http.ts:199`). Its own docs
say "Surface routes are same-application routes, not remote code" and that a
host composes them with its own shell.

## State

| State | Owner | Where |
| --- | --- | --- |
| API keys, prefixes, audit | `appport.flow` (`ApiKeys`, `ApiKeyPrefixes`, `ApiKeyAuditEvents`) | the application's FeltDB |
| Webhook endpoints, deliveries, integrations, inbound events | `appport.flow` | same |
| Jobs, schedules | `appport.flow` (`Jobs`, `JobSchedules`, `JobAuditEvents`) | same |
| Secrets metadata and versions | `appport.flow` (`Secrets`, `SecretVersions`); **secret material stays with the provider and is never durable AppPort state** (`README.md`) | same |
| Configuration | `appport.flow` (`ConfigurationVariables/Secrets`) | same |
| Notifications and deliveries, files metadata, effect evidence | `appport.flow` | same |
| Compute's environments, computers, receipts, services registry | Compute's flow (`compute.flow`, `AGENTS.md`) | Compute's control state |

**These are two separate authoritative stores.** AppPort Services opens FeltDB
per application (`createFeltDbRuntime`: `$FELTDB_URL` with `applicationId =
config.application.name`, or local FeltDB at `.appport/state`;
`src/cli.ts:228-236`); Compute uses its own control state. Compute never reads
AppPort's collections and AppPort never reads Compute's. That is consistent with
"FeltDB owns durable state", with no shared writer.

Hidden persistence: none found. `grep` for SQLite, JSON-file writes and
`readFileSync(JSON)` in `src/` returned nothing. In-process `Map`s exist
(registered job/webhook handlers; `ApiKeyServiceImpl.knownPrefixes` and
`locallyCreatedKeys`, `src/api-keys/service.ts:50-51`); the handler maps are
registrations, not state, and the two API-key maps were not audited for
invalidation, so they are reported here and not judged. `docs/architecture.md`
also lists "JSON persistence", "SQLite persistence" and "an in-memory production
fallback" as deliberate non-goals, but note it still says webhooks and jobs are
not implemented, which is stale relative to `README.md` and `appport.flow`.

## Authentication

Two unrelated identity models today:

* **Compute:** operator credentials with scopes (`compute.read`,
  `compute.deploy`, …), minted by `compute auth create`; the UI keeps the token
  in the browser tab (`docs/daemon.md`, "The UI"; `crates/compute-environment/src/auth.rs`).
* **AppPort Services:** host-supplied principal (embedded) or Bearer API key
  (standalone), authorized by AuthBoundry capabilities, tenant-bound
  (`management.ts`, `platform.ts:120-126`).

Nothing maps a Compute operator to an AppPort principal or tenant. Putting
AppPort pages behind Compute's UI therefore needs a decision about which side
supplies identity; the source gives one answer on AppPort's side — the host
supplies identity, AuthBoundry decides permission, and "a second authorization
layer next to AuthBoundry is not permitted" (`docs/management.md`). I did **not**
inspect AuthBoundry, so whether Compute operators can be AuthBoundry principals
is unverified here.

## Can AppPort Services run on Compute today?

**As the application's library: yes. As a thing Compute manages: no.** An
application using `@appport/runtime` can be packaged and run as a Compute
application or computer process like any Node program (`compute application
deploy`, `docs/daemon.md`), and the `randy` stack can verify the package is
present. That runs the *host application*, which embeds AppPort Services. There
is no recipe, application definition, service manifest, health or readiness
check, or env contract for AppPort Services as such, and none is needed by its
design — it has no process of its own to define. What is missing for Compute to
*show* it is the UI/discovery wiring described below, not a deployment recipe.

## Startup: the exact current workflow

1. `compute` — control plane, UI at `http://127.0.0.1:8787/ui/`.
2. In a separate process, an application that embeds AppPort Services
   (`npx create-appport my-app && npm run dev`, or your own). Set its
   `[http] port` to something other than `8787`.
3. To see AppPort pages at all, that application must mount
   `createManagementRouter` (default `includeUi: true`); then browse to **that
   application's** `/services`. A plain `appport()` app only exposes
   `/_appport/*` JSON.
4. Optionally register it in Compute's catalog:
   `compute service register NAME --capability … --endpoint …` — a record shown
   under Compute's Services page; it does not link to or embed anything.

There is no command that starts "AppPort Services" or "the AppPort Services UI"
on its own.

## Integration gap

Exactly what is missing, in order of dependence:

1. `@appport/services` serves no `GET /v1/ui`, and its contribution constant is
   not an `AppPort/ui/1` document (different fields, API keys only).
2. The standalone `appport()` server mounts no pages.
3. Compute's UI has fixed navigation and no mechanism to take contributed
   surfaces, and does not call `/v1/ui`.
4. No identity mapping between a Compute operator and an AppPort principal or
   tenant, and no way to tell Compute where an application's AppPort endpoint
   is, other than the free-text endpoint of a registered service.
5. A port default collision (8787).

## Which architecture the source supports

| Option | Supported by source? |
| --- | --- |
| 1. Separate control planes | **This is what exists.** Each has its own UI/API/state/auth |
| 2. Compute embeds AppPort Services | **Not supported.** AppPort Services is per-application library state; embedding it in Compute's daemon would make Compute a tenant of its own service and duplicate Compute's state |
| 3. Compute is the unified control plane | **Contradicted** by Compute's own scope (`docs/provider-pools.md:142`: a multi-tenant control plane and service discovery belong to the layer above) |
| 4. Compute provisions/runs AppPort Services, separate UI | **Partly consistent** for *running the host application*; there is no separate AppPort Services process to provision |
| Composable UI (`AppPort/ui/1`) | **The product direction written in source** (`appport/docs/composable-ui.md`): products expose surfaces, hosts compose them, no product owns the shell |

So the architecture the source points to is neither 1 nor 3, but **hosts
compose products' contributed surfaces over each product's own API**, with
Compute as one possible host. That keeps each authority where it is.

## Implementation options (none implemented)

Only those consistent with the above:

* **A. Link-out (smallest).** In Compute's Services page, show a link to a
  registered service's AppPort `/services` when its record says it speaks
  AppPort. Needs: an agreed capability name on the `ServiceRecord`, and the host
  app mounting `createManagementRouter`. No state, no auth change on Compute's
  side; the user authenticates to the app.
* **B. Contribution discovery.** Compute reads `GET /v1/ui` from registered
  AppPort-speaking services and adds navigation entries that open the
  contributed same-application routes. Needs: `@appport/services` to publish a
  real `AppPort/ui/1` document and serve `/v1/ui` (its repository), and an
  identity decision. This is the option the AppPort docs describe.
* **C. Reverse proxy or iframe.** Compute's daemon already serves endpoints and
  domains; fronting an application's AppPort pages is possible, but the pages
  assume the host's session cookie/authentication, so this inherits the identity
  decision and adds a second origin concern. Not recommended first.

Not consistent with the source: copying the pages or APIs into Compute,
a Compute-side webhook/job/key/secret store, or a new database.

## Risks

* **Duplicate auth.** Compute operator tokens and AppPort/AuthBoundry principals
  are unrelated; a naive embed creates a second identity path, which AppPort
  forbids by design (`docs/management.md`).
* **Duplicate state.** Two authoritative stores exist today and must stay that
  way; any Compute-side copy of keys, jobs or webhooks is a violation of both
  repositories' rules.
* **Duplicate service APIs / hidden persistence.** The pages call `/_appport/*`;
  re-implementing them in Compute would fork the contract.
* **Compute becoming monolithic.** Compute's own docs assign multi-tenant control
  and service discovery to the layer above it.
* **AppPort Services coupled to one execution provider.** The contribution
  protocol is deliberately provider-neutral; embedding Compute-specific routes
  or assumptions in `@appport/services` would break that.
* **Port 8787 collision** between defaults.
* **Doc drift:** `appport-services/docs/architecture.md` lists webhooks and jobs
  as "not implemented" while `README.md` and `appport.flow` implement them.
* **Unverified:** AuthBoundry's side of identity, anything in repositories not
  read, and runtime behaviour (nothing was started).

## Findings that are one-line, not fixed

Per the instruction to document before fixing: the 8787 default collision
(`appport-services/src/cli.ts:203`; `compute-environment/src/client.rs:12`) is a
one-line configuration choice on either side; and `docs/architecture.md` in
`appport-services` is stale. Neither was changed.
