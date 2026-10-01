# Service-contributed UI

**Status:** implemented. A registered service that contributes a management UI
is exposed on Compute's **Services** page as links to the service's own pages.
Compute implements none of that UI.

```text
compute service register NAME --capability AppPort/ui/1 --endpoint URL
        ↓
Compute asks   GET {URL}/v1/ui          (anonymous, 5 s, 256 KiB, no redirects)
        ↓
the service answers an AppPort/ui/1 document   (protocol: rkendel1/appport)
        ↓
Compute validates it as untrusted data         (service_ui.rs)
        ↓
Services page → Management: one link per navigation entry, to URL + route
        ↓
the browser goes to the service; the service authenticates its own user
```

## Using it

```sh
compute                                    # control plane + UI, 127.0.0.1:8787
# …in the AppPort application (a separate process; port 4100 by default):
npm run dev
compute service register invoices \
  --capability AppPort/ui/1 \
  --endpoint http://127.0.0.1:4100
```

Open `http://127.0.0.1:8787/ui/#/services`. The **Management** column shows the
service's pages: for `@appport/services` today, its **AppPort Services**
overview, which links to API Keys, Webhooks, Jobs, Schedules, Notifications,
Files, Configuration and Secrets according to what it mounts.
**Compute does not start the service.** It is a separate process, or a Compute
application you deploy yourself; registration only tells Compute where it is.

`AppPort/ui/1` is the protocol's own identifier, used as the capability that
says "this service contributes a UI". There is no service name or path in
Compute: any service that speaks the protocol works the same way, and a service
that does not declare it is an ordinary registered service and is never contacted.

## The contract

Owned by `@appport/protocol` (`packages/protocol/src/ui.ts`, repository
`rkendel1/appport`): protocol id `AppPort/ui/1`, discovery path `GET /v1/ui`,
and a document of `{protocol, product{id,version}, surfaces[{id,title,route,
capabilities}], navigation[{id,label,group,order,surface}],
composition{requires}}`. `validateUiContribution` is the validator; Compute's
`service_ui.rs` mirrors it (and is stricter about routes), and
`crates/compute-environment/tests/fixtures/appport-ui/appport-services.json` is
a document fetched from the **published** `@appport/services@0.4.10` the
configured stack installs, served over HTTP by its own `appport-services serve` at
`GET /v1/ui`. It is fetched from that artifact, not hand-written.

**What `GET /v1/ui` means.** In the protocol it is *caller-contextual*: a server
returns the contribution filtered by the capabilities the caller holds
(`filterUiContribution`), and there is no public mode; a surface that needs no
capability is visible to everyone. Compute is an anonymous caller that holds no
AppPort capabilities and supplies no identity, so it sees exactly the
capability-free surfaces. `@appport/services` cannot probe a caller's
capabilities without side effects, so its `/v1/ui` is that filtered view for
"no capabilities": one surface, the **AppPort Services overview** (`/services`),
whose page links to the rest; each of those pages authenticates its own user (an
anonymous request to `/api-keys` is `401`). Hosts with capability context can
serve the full contribution through the AppPort protocol server, and Compute
renders any number of links it is given (tested with the full nine-surface
document). Compute never forwards its operator token or any other identity to get
more.

**Which services publish it.** `@appport/services` serves it from
`createManagementRouter` (see its `docs/management.md`). The standalone
`appport()` runtime is an application runtime, not a management application: it
deliberately serves no pages and no `/v1/ui`, so Compute shows "advertises no
UI". A management UI therefore requires a host application that mounts the
router.

**What the configured distribution provides.** The configured stack pins
`@appport/services@0.4.10`, which ships the standalone management host
`appport-services serve --host <host> --port <port>`. `compute-configured` starts that
process, waits until `GET /v1/ui` answers, registers it as `appport-services` with the
`AppPort/ui/1` capability, discovers it through the generic mechanism described above,
and stops it on shutdown. Both fixtures under
`crates/compute-environment/tests/fixtures/appport-ui/` were fetched over HTTP from the
shipped `serve` process and accepted by the protocol's own `validateUiContribution`.

| Capability in 0.4.10 | Present |
| --- | --- |
| `AppPort/ui/1` discovery document at `GET /v1/ui` | yes |
| Packaged management pages (`/services`, `/api-keys`, `/webhooks`, `/jobs`, `/schedules`, `/notifications`, `/files`, `/configuration`, `/secrets`) | yes |
| Standalone runnable management host (`appport-services serve`) | yes |
| CLI `serve` with `--host` / `--port` | yes |

The host owns everything that belongs to AppPort Services: its durable state (a FeltDB
deployment under its own working directory), `createServices`, the `ServiceGateway`,
its authentication adapter, the management router, the pages, and `/v1/ui`.
`compute-configured` owns only distribution, process lifecycle, readiness, service
registration and shutdown. It does not reconstruct any of the above, and it holds no
AppPort Services state — only the registration it needs to find the endpoint.

Readiness deliberately means "`GET /v1/ui` answers", not "the management pages are
authorized for this caller": discovery and management authorization are separate, and a
management operation refused for want of an AppPort Services identity is correct
behaviour rather than an unhealthy process.

> **Note on 0.4.8.** 0.4.8 declared `serve` in its usage text but could not run it: its
> `appport-services` entry point compared `import.meta.url` against `process.argv[1]`,
> which never match when node runs through the `node_modules/.bin` symlink, so the CLI
> exited 0 without doing anything; and `[group, action, ...rest]` consumed the first
> `serve` flag as `action`, so `--host`/`--port` always failed with
> `Unexpected argument`. 0.4.10 fixes both.

## What Compute shows, and when

`GET /services/{service}/ui` (scope `read`, no stored state) returns
`{service, status, product?, links[], message?}`; `status` is one of:

| Status | Meaning |
| --- | --- |
| `none` | the service declares no UI. Normal; the service is not contacted |
| `available` | valid, with links |
| `empty` | declared but nothing advertised (`404`, or no navigation) |
| `unreachable` | refused, timed out, redirected, or answered with a server error |
| `unauthorized` | the service refused anonymous discovery (`401`/`403`) |
| `invalid` | not JSON, wrong protocol version, malformed route/capability/navigation, too large, or an endpoint Compute will not contact |
| `unsupported` | the contribution needs `identity`/`tenant`/`application`/`environment` from its host, which Compute does not supply, so a conforming host does not compose it |

Every call reads the current registration: changing a service's endpoint or
capabilities, or removing it, takes effect on the next view.

## Authentication

**There are two separate sign-ins, and Compute does not join them.** The browser
authenticates to Compute with its operator token; it authenticates to the
service however the service does (for `@appport/services`: its host's session
and AuthBoundry). The link is ordinary navigation (`target=_blank`,
`rel=noopener noreferrer`); Compute forwards no token, cookie or credential, and
the discovery request carries none (tested). Embedding (iframe or proxy) was
**not** done: AppPort pages assume their host's session, so embedding would need
a shared cookie or token forwarding, which would create a second identity path.
The protocol defines `composition.requires` for hosts that can supply identity;
Compute is not that host for an AppPort product.

## Security

The service is untrusted input.

* Compute contacts only the endpoint an operator registered: `http`/`https`, a
  host, no credentials, query or fragment. A read-scoped caller can trigger
  discovery of a registered endpoint but cannot choose a URL. The request is an
  anonymous `GET`, 5 s timeout, 256 KiB cap, redirects **not** followed, no proxy.
* The document is validated like the protocol's validator, plus: routes must be
  a path on the service (no scheme, authority, backslash, whitespace, `?`, `#`,
  or `.`/`..` segments); at most 64 links; text at most 512 characters.
* Compute **builds** each URL from the registered endpoint and the route and
  re-checks its origin; it never copies a URL from the document.
* The UI renders service text with `textContent` only; `app.js` contains no
  `innerHTML`/`eval`/`document.write` (a test enforces it), and links must also
  pass `safeHttpUrl`. A real-browser test registers services whose labels contain
  `<img onerror>` and `<script>`, a `javascript:` route and an off-origin route,
  and asserts nothing runs and no unsafe link exists.
* The contribution grants nothing: no Compute credential, session or
  filesystem access, and no navigation outside the service's own origin.

## Limits

* Discovery is anonymous, so Compute sees only what the service publishes to a
  caller with no capabilities (for `@appport/services`, the overview page).
  Compute does not show the per-page list unless the service publishes it to
  anonymous callers; each page authorizes its own user.
* Links, not embedding: opening one leaves Compute.
* The Services page does not poll; reload to rediscover.
* A service that serves its UI behind a path prefix must be registered with that
  prefix in its endpoint (routes are appended to the endpoint's path).
* Compute does not launch the service. `compute service register` only records it.
