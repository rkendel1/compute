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

---

# Implementation (follow-up, same day)

The audit above describes the state **before** this change. Where it says the
integration is missing, this section records what was then built. Nothing here
copies AppPort Services into Compute.

## PROTOCOL

The authoritative contract is `AppPort/ui/1`, owned by `@appport/protocol`
(`rkendel1/appport`, `packages/protocol/src/ui.ts`; published in
`@appport/protocol@1.0.2`, which contains `dist/ui.js`). Read from source, not
docs:

* Discovery: `GET /v1/ui` (`UI_DISCOVERY_PATH`); the protocol server answers
  `404 NOT_FOUND` "No composable UI is advertised" when it has none
  (`packages/server/src/http.ts:199-207`).
* Document: `{ protocol: "AppPort/ui/1", product: {id, version}, surfaces:
  [{id, title, route, capabilities[], entities?, actions?}], navigation:
  [{id, label, group, order, surface}], composition: { requires:
  ("identity"|"tenant"|"application"|"environment")[] } }`, plus `capabilities[]`
  in the discovery form (`UiDiscoveryDocument`).
* Routes are same-application paths: they must start with `/`, not `//`, and
  not contain `://` (`validateUiContribution`). Capabilities are protocol names
  (`<namespace>.<operation>`, lowercase). It describes **all three** things:
  routes (surfaces), navigation, and required capabilities.
* There is **no JSON Schema** for it in `spec/schemas`; the TypeScript validator
  is the schema. The protocol repository owns it. A host composes contributions
  with `composeUi` (`packages/client/src/composition.ts`) and skips any whose
  `composition.requires` it cannot supply.
* Resolution of the audit's discrepancy: the old `APPPORT_UI_CONTRIBUTIONS`
  (`[{protocol, id, requiredCapabilities}]`, API keys only) was not an
  `AppPort/ui/1` document. It was **replaced** (not kept alongside) by a valid
  one built with the protocol's own validator.
* No embedding contract exists in the protocol: surfaces are routes, "not
  remote code". No iframe protocol was invented.

## APPPORT SERVICES

Branch `claude/determined-curie-07x0a9` of `rkendel1/appport-services` (not
merged; no PR opened).

* `@appport/services` now depends on `@appport/protocol@^1.0.2` and builds its
  contribution with `validateUiContribution`/`filterUiContribution`
  (`src/runtime/ui.ts`): one surface per page it actually mounts, with the
  capabilities the page drives (all checked against `SERVICE_CAPABILITY_MANIFEST`
  by a test), navigation grouped as "AppPort Services", `composition.requires: []`.
* `createManagementRouter` serves it at `GET /v1/ui` when it serves the pages
  (`includeUi`), and `404`s with the protocol's body otherwise.
* **Standalone `appport()` is unchanged**: it still serves the `/_appport/*`
  API and no pages, so it answers no `/v1/ui` and appears in Compute as
  "advertises no UI". Making it serve pages would change its behaviour for
  existing consumers and its pages cannot authenticate against Bearer-only
  standalone mode, so it was not done.
* **Corrected after review.** The first version served the full surface list
  anonymously and unfiltered, which did not match the protocol: `GET /v1/ui` is
  caller-contextual (`Server.uiDiscovery` filters by the caller's capabilities;
  there is no public mode; capability-free surfaces are visible to everyone).
  `@appport/services` cannot probe capabilities without side effects
  (`ServiceGateway.authorize` throws on denial and records refusal evidence), so
  it now returns the protocol's own filtered view for a caller with no asserted
  capabilities: the capability-free **overview** surface. The full contribution
  is still exported (`APPPORT_UI_CONTRIBUTIONS`) for hosts that know their
  callers' capabilities. Compute's operator token is never forwarded to obtain
  more.
* Tests: 239 pass, 0 fail (`npm test`), including `tests/ui-discovery.test.ts` and `tests/runtime-boundaries.test.ts`
  (valid document accepted by the protocol validator, every route served,
  capabilities real, invalid documents rejected, nothing mounted → none, no UI
  → 404, no secrets in the document).

## COMPUTE

* **Registry:** unchanged `ServiceRecord` (name, capabilities, endpoint). No new
  field, state, database or `.flow`.
* **Protocol-based detection:** a service declares the capability string
  `AppPort/ui/1` (the protocol id). No service name, path or page is known to
  Compute.
* **Discovery:** `compute-environment/src/service_ui.rs`; `Daemon::service_ui`;
  `GET /services/{service}/ui` (scope read; listed in `ROUTES` and
  `docs/audit.json`). Nothing is stored or cached.
* **UI:** the Services page gains a **Management** column
  (`crates/compute-environment/ui/app.js`, `serviceUiCell`): links to the
  service's own pages, or a status chip and message. Navigation is unchanged.
* **Embedding choice:** external links. See AUTH.
* **CLI bug found and fixed on the way:** `compute service register --endpoint`
  shared a clap id with the global daemon location, so registering a service
  with an endpoint redirected the CLI to that endpoint and failed. It had never
  been exercised (existing tests used the API directly). One-line fix
  (`id = "service_endpoint"`), with `tests/service_register.rs`.
* **UI bug found by the browser test:** appending an array rendered
  `[object HTMLDivElement]`; fixed (`append(...links)`).

## AUTH

Unchanged and not unified. The browser signs in to Compute with an operator
token and to the service by the service's own means (`@appport/services`: host
session + AuthBoundry). Crossing the boundary is ordinary navigation to the
service's URL. Compute sends the service no credential, cookie or token, and the
discovery request is anonymous (a test asserts no `authorization`, `cookie`,
`proxy-authorization` or `x-compute*` header). No shared cookie, token
forwarding, credential proxy or AuthBoundry integration was added. Embedding was
rejected because it would require exactly those.

## STATE

Unchanged. AppPort Services' state stays in `appport.flow` in the application's
own FeltDB; Compute's service record stays in Compute's control state.
Discovery results are computed per request and stored nowhere. No new
database, `.flow` or cache.

## STARTUP

Nothing starts automatically. `compute` starts the control plane
(`127.0.0.1:8787`); the AppPort application is started separately
(`npm run dev`), and `compute service register … --capability AppPort/ui/1
--endpoint …` tells Compute where it is. Compute does not launch AppPort
Services and the docs say so (`docs/service-ui.md`). **Port collision:** real in
the default workflow (`appport init`/`create-appport` and `compute` both
defaulted to 8787). Compute's port was not changed; AppPort's *generated*
default is now `4100` (`appport-services` `src/cli.ts`, example, tests, docs).
An explicit port in an existing `appport.toml` is unchanged, and the parser's
own default when `[http] port` is omitted is still 8787, on purpose (changing it
would silently move existing apps). The two defaults are documented
(`docs/configuration.md`) and pinned by tests.

## TESTS

Compute, full `cargo test --workspace --no-fail-fast`: **673 passed, 1 failed,
19 ignored** (baseline `c50218a`: 581 / 0 / 19). The one failure,
`compute-cli/tests/recovery.rs::an_execution_that_ends_while_the_controller_is_down_is_recorded_after_recovery`,
is pre-existing: it fails identically on the untouched baseline commit
(verified in a clean worktree) and is unrelated to this change.

New and relevant, all passing:

| Suite | Result |
| --- | --- |
| `compute-environment/src/service_ui.rs` unit tests (validator) | 9 / 9 |
| `compute-environment/tests/service_ui.rs` (real daemon + stand-in HTTP service; discovery incl. the filtered and the full documents, non-AppPort service, 14 failure modes, timeout, endpoint change/removal, header check, UI source guard) | 9 / 9 |
| `compute-cli/tests/service_register.rs` (the `--endpoint` fix) | 1 / 1 |
| `compute-cli` `audit`, `contract`, `architecture`, `execution_paths` | all pass |
| `packages/compute-ui-e2e` (real Chromium, real daemon): the 3 existing tests plus `service-ui.test.mjs` | 4 / 4 |
| `appport-services` `npm test` | 239 / 239 (incl. `tests/ui-discovery.test.ts`, `tests/runtime-boundaries.test.ts`); `tsc --noEmit` clean. That repository configures no formatter or linter (no `fmt`/`clippy` equivalent exists); the strict TypeScript build is its check |

Also done by hand, once: a genuine `@appport/services` host
(`createManagementRouter` on `127.0.0.1:4100`) registered in a genuine Compute
daemon with `compute service register … --capability AppPort/ui/1`; the daemon
returned `available` with the eight links it mounts (before the correction
above; it now returns the one overview link), and the pages answered `401` to an
anonymous request (the service owns its authentication). Not
verified: the published npm package of `appport-services` (the branch is unmerged).

## SECURITY

Validation and isolation are described in `docs/service-ui.md`. Tested:
unavailable, timeout, malformed (not JSON, array, empty), wrong protocol
version, off-origin/scheme/`..` routes, oversized body, redirect to a metadata
address (not followed), 401/403, 404, 5xx, a contribution requiring identity, an
endpoint that is `file:`, `javascript:` or carries credentials, no endpoint,
service removed, endpoint changed, capability dropped; and in a real browser,
markup/script text, a `javascript:` route and an off-origin route (nothing runs,
no unsafe link). The UI source is guarded against markup-parsing APIs.

**Not added:** a Content-Security-Policy. The control-plane UI serves none today;
that is outside this change and is noted as a hardening follow-up.

## LIMITATIONS

* Compute links to a service's pages; it does not embed or proxy them, and the
  user signs in to the service separately.
* Only hosts that mount `createManagementRouter` publish `/v1/ui`; a standalone
  `appport()` app shows "advertises no UI".
* Compute is an anonymous caller, so it sees what the service publishes to a
  caller with no capabilities (for `@appport/services`, the overview page); the
  per-page list needs an identity Compute does not hold.
* The Rust validator mirrors, and is stricter than, the TypeScript one; they are
  kept aligned by a fixture generated from the real service, not by shared code.
* `@appport/protocol` on npm is 1.0.2 while the repository is at 1.0.3;
  `@appport/services` pins `^1.0.2` and is built and tested against the
  **published** 1.0.2 (it contains `dist/ui.js`; the validator matches the
  repository's). Publish order is protocol → services → consumers: the publish
  script refuses to publish services unless a published protocol satisfies the
  declared range (checked: `^1.0.2` passes, `^1.0.3` is refused), and a test fails
  if the installed protocol is out of range or lacks the UI exports. Publishing
  the protocol was not done (no credentials; and nothing here needs 1.0.3).
* The Services page does not poll. Not verified: behaviour with a service behind
  a TLS terminator with a private CA (the daemon uses the system roots).

## NEXT

One justified follow-up: publish `appport-services` with the new `/v1/ui` (the
branch is unmerged), then register a real embedded host in Compute and confirm
the page list matches. Embedding or per-user filtering needs an identity
decision that no source requires yet, so none is proposed.
