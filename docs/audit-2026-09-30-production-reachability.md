# Compute audit: production, reachability, URLs and the release path

Audited 2026-09-30. Code under audit: `9dfd467` (0.1.8, `main`) plus the
earlier audit doc on branch `claude/wizardly-mayer-61dtbt` (`e21c5ed`, docs
only). No production code was changed. One temporary test was added to
`crates/compute-environment/tests/applications.rs` to capture evidence, run,
and reverted (`git checkout`); its output is quoted in §6 and §9.

Status words are used strictly: **IMPLEMENTED** (code path exists and a test
or run exercised it), **PARTIAL**, **NOT IMPLEMENTED**, **UNKNOWN** (not
verified here). "Implemented" is separated from "productized" (reachable
from the CLI/API/UI a user touches) and "verified" (run by this audit).

This report **supersedes and corrects**
[audit-2026-09-30-reachability.md](audit-2026-09-30-reachability.md) (written
from a lighter read of the same commit). Corrections are listed in §14a.

---

## 1. Executive summary

**Can Compute give a user a stable, collision-free, clickable URL that stays
valid across deployment, promotion and rollback? No.**

- A deployed application gets `http://<target-host>:<host-port>` (for
  example `http://127.0.0.1:39327` in the test harness), built from the
  target's configured address and a port Compute allocated from a node-wide
  range (`daemon/computers.rs:1258`). It is a private address. It is not a
  hostname, not TLS, not routed, and not externally reachable unless the
  target host happens to be.
- The port *is* stable across v1 → v2 → rollback → controller restart
  (verified: `tests/applications.rs`, passing). So "stable `host:port`" is
  true. "Stable Compute-owned public URL" is false.
- Compute has a real production network stack (domains, DNS providers with
  drift repair, ACME, SNI TLS, ingress) and a real zero-downtime release
  machine with promotion and rollback. **Both work only for node
  environments** (bundle projects on the daemon host). The repository already
  knows this: `docs/gap-analysis.md` G-APP-1 ("no domains, TLS, or ingress for
  computer applications"), G-DEP-1 ("a release restarts processes"), and
  G-ARCH-5 pin the node model to the daemon host and forbid it from entering
  a computer environment (`tests/execution_paths.rs`).
- No component generates a public hostname. Domains are operator-supplied
  (`compute domain add`). The application does not choose its public
  hostname, but nothing else does either.
- **New finding, not in earlier audits: a failed replacement takes down the
  healthy version.** The application path restarts the same process in place.
  Measured: v1 answering → deploy a crashing v2 → the v1 URL stops answering
  at once, `status` reads `failed`, v2 stays `Deploying` for ~100 s, then
  `Failed`, while the history still shows v1 `Active` (§9).
- The product the prompt describes (dev → preview → staging → production)
  does not exist as a workflow. `compute dev` and `compute preview` do not
  exist. An application is exactly one environment (`application-<name>`).
  Promotion exists between computer environments, as a version move, not as
  an application lifecycle.

**The smallest gap** is narrower than "build endpoints": the network layer
routes only to node-environment traffic assignments, and ingress can only
forward to `127.0.0.1` (§7, §21).

---

## 2. Repository and version audited

| Item | Value |
| --- | --- |
| Commit | `9dfd467` (main); this report's branch head `e21c5ed` adds docs only |
| Branch | `claude/wizardly-mayer-61dtbt` |
| Version | `0.1.8` (`Cargo.toml` workspace) |
| Workspace | 16 crates: `compute-{core,runtime,runtime-process,runtime-wasm,runtime-conformance,provider,policy,placement,state,state-memory,state-file,state-feltdb,network,environment,project,cli}`; `packages/compute-appport`, `packages/compute-state-model` |
| Binary | `compute` (`compute-cli`): client, `serve` (target), `start` (daemon), `supervisor` |
| UI | `crates/compute-environment/ui` (`index.html`, `app.js`), served by the daemon |
| Targets in code | `ProviderKind::{Local, Remote}` only (`compute-placement/src/descriptor.rs:25`) |
| Durable state | `compute-state` (`memory`, `file`, `feltdb` backends); model `compute.flow`, `MODEL_GENERATION = 12` |
| Platform of this audit | Linux x86_64 container, 4 vCPU; debug build |

How it fits together: `compute up` starts a **target** (`compute serve`,
127.0.0.1:8788, hosts computers) and the **control plane** (`compute start`,
127.0.0.1:8787, the authority, the UI) — `launch_cmd.rs`. The CLI is a client
of the daemon's HTTP API. The daemon reconciles desired state from control
state (file or FeltDB) — `daemon/reconcile.rs`.

---

## 3. Commands and tests executed

| Command | Result |
| --- | --- |
| `cargo build -p compute-environment --tests`, `cargo build -p compute-cli` | built |
| `cargo test -p compute-environment --test applications` | **5 passed** (30 s) |
| `cargo test -p compute-environment --test network` | **2 passed** (domains → DNS reconcile; ACME issue/renew) |
| `cargo test -p compute-environment --test releases` | **7 passed** (switch under load, failed/never-ready/verify-fail rollback, restart resume, drain) |
| `cargo test -p compute-environment --test recipes` | **11 passed** |
| `cargo test -p compute-network -p compute-state -p compute-state-memory -p compute-state-file -p compute-placement -p compute-policy` | **all passed** (13 result lines, 0 failed) |
| Temporary probe test `audit_probe` (reverted) | passed; output in §6, §9 |
| `compute up --no-browser`, `compute init`, `compute deploy` (real CLI) | daemon up; **deploy stopped**: python runtime "available", not installed, no network to fetch: `no target can host this computer … this-machine: runtime_unavailable` |
| `cargo test -p compute-environment --test execution_paths --test lifecycle --test control_plane` | **6 + 4 + 7 passed** (incl. `the_node_model_never_enters_a_computer_environment`, `deploy_verify_and_promote_the_exact_revision`) |
| `cargo test -p compute-cli --test product` | **3 passed**: the real `compute` binary against a real daemon and targets (`an_application_moves_from_source_to_a_placed_versioned_verifiable_deployment`) |

**Things that could not run, and why.**

- Workspace-wide `cargo test` exhausted the sandbox disk once (ENOSPC at
  linking). Suites were re-run in batches after clearing build artifacts.
- Driving `compute up` + `compute deploy` by hand was not possible here:
  catalog runtimes are downloaded on demand and the network policy denies it
  (`runtime_unavailable`). The same flow **was** run by tests that start real
  processes: `tests/applications.rs` (in-process daemon, real `compute serve`
  target, real Python, URL read back over HTTP) and
  `compute-cli/tests/product.rs` (the real `compute` binary). Both pass.
  (`compute up` also needs a short `COMPUTE_HOME`: a long path overflows the
  supervisor's unix-socket `SUN_LEN`; this is a robustness finding, not a
  product gap.)
- `FELTDB_SERVER_BIN` is not set, so FeltDB-backed suites
  (`feltdb_consumer`, `compute-state-feltdb` ignored tests) did not run.
- macOS, Apple Container, Hetzner: not available; UNKNOWN.


---

## 4. The product model as implemented

```text
                     ┌─────────────────────── NODE ENVIRONMENT (daemon host) ───────────────────────┐
compute.project.toml │ Project → ProjectRevision (immutable) → Deployment (state machine) →         │
  bundle projects ──▶│ WorkloadInstance → TrafficAssignment (endpoint: host port)  ◀── DomainRecord  │
                     │    supervisor on the daemon host        ▲            │                        │
                     │                                         │            ▼                        │
                     │                          Ingress :80/:443 ◀─ DnsRecord / Certificate (ACME)   │
                     └───────────────────────────────────────────────────────────────────────────────┘

                     ┌────────── COMPUTER ENVIRONMENT (application / dev / preview) ─────────┐
compute deploy <dir> │ Environment `application-<name>` ─ ComputerRecord (placed on a target)│
compute application  │    ├ Project <name> (source imported by target jobs)                  │
                     │    ├ Version (immutable: commit, package digest, artifact)            │
                     │    ├ Rollout (deploy | promote | rollback; steps; receipt)            │
                     │    └ ProcessSpec{port} ─▶ "endpoint" = http://<target-host>:<port>    │
                     └── no DomainRecord, no ingress, no TLS, no hostname ───────────────────┘
```

| Concept | Node environment | Application / computer |
| --- | --- | --- |
| application | a *project* | an *environment* named `application-<name>` (`applications.rs:49`); the word is a view |
| environment | named, holds projects | one per application; also the unit of owner + computer |
| recipe | policy → `ComputerRequest` | same; consumed only at `environment create --recipe` |
| revision | `ProjectRevision` | `Version` (`compute-state/src/model.rs`) |
| deployment / release | `Deployment` (`pending→…→complete`) | `Rollout` (`deploy`/`promote`/`rollback`) |
| process | supervisor unit on daemon host | `ProcessSpec` run by a target job |
| endpoint | `TrafficAssignment.host_port` (data plane listener) | `ProcessEndpoint{port,url}`: view only; nothing durable of its own |
| port | `--port-range` endpoint + `--instance-port-range` per instance | `--port-range` (shared pool), chosen at deploy |
| domain / DNS / cert / ingress | `DomainRecord`, `DnsRecord`, `Certificate`, `Ingress` | none |
| target | the daemon host | a pool member (`compute serve`); chosen by placement |

---

## 5. The real deployment paths

| Command | Creates | Durable? | Runs where | Port | Readiness | Output |
| --- | --- | --- | --- | --- | --- | --- |
| `compute run <path>` (local) | nothing in control state (receipt only with `--receipt`) | no (invariant 22) | caller's machine | none | none | program output |
| `compute run` / `pool run` (remote provider) | one synchronous request | no | provider placement chose | none | none | result + receipt |
| `compute up` | target + control plane processes | state dir | this machine | 8787/8788 | n/a | `Compute is running: <url>` |
| `compute deploy <dir\|artifact\|url>` → `application::deploy` | Environment, Computer, Version, Rollout, target jobs | yes (control state) | the application's computer on a pool target | first free in `--port-range`, kept for the process's life | rollout "Health check": process running + endpoint TCP-reachable (`readiness: None` at `applications.rs:322`; HTTP readiness available on computer processes but unused by deploy) | `Application/Version/Provider/Runtime/Endpoint/Status` (`application.rs:1187`) |
| `compute deploy <project> --environment E` | node `Deployment` | yes | daemon host | endpoint + instance ports | `port`/`http`/`process`/`task` | deployment record |
| `compute deploy <p> --from A --to B` | node promotion | yes | daemon host | same endpoint | yes | deployment record |
| `compute promote <p> --from A --to B` | if target env has a computer: `Rollout{Promote}` of the active Version; else node promotion | yes | as above | as above | as above | rollout / deployment |
| `compute status` | none | n/a | n/a | n/a | n/a | `Endpoint: http://host:port` |

`compute deploy` picks the path by argument shape
(`main.rs:1662`: `is_deployable` → application, else node). The CLI help for
`compute deploy` ("with zero downtime") describes the node path only.

The deploy operation is **not** complete at start: `deploy_application`
returns after the rollout is created; the rollout goes `Deploying` →
`Active`/`Failed` by the controller. `compute deploy` follows it.

---

## 6. The URL question, answered by test

Probe output (temporary test, real `compute serve` target, real Python
servers; both apps declare `port = 3000` in their identity):

```text
PROBE app-a endpoint = Some("http://127.0.0.1:39327")
PROBE app-b endpoint = Some("http://127.0.0.1:39328")
PROBE add_domain(application env) = Err("not found: project app-a in
      application-app-a; a domain routes only within its environment")
```

| Property | Finding | Evidence |
| --- | --- | --- |
| Form | `http://<target-host>:<port>`; no hostname, no TLS | `computers.rs:1258` |
| Generated by Compute? | the port is; the host is the target's configured address | `applications.rs` (port pick), `target_host` `computers.rs:2490` |
| Supplied by the application? | no. The app's declared `port = 3000` is **ignored**; it receives `PORT=<allocated>` (`computers.rs:156`). An app-supplied `PORT` env in its bundle would override the listener port and break the endpoint (`process.env.extend`, same function) | code; not run |
| Durable | the port is in `EnvironmentRecord.contents.processes[].port` (control state); the URL is recomputed per request | `applications.rs:296-324` |
| Unique | per daemon, by a scan of the in-memory snapshot (`ports_in_use`, `software.rs:508`), **not** by a store constraint; two concurrent first-deploys can read the same free port | code; race not exercised |
| Stable across v1→v2→rollback | **yes** | `tests/applications.rs` passes |
| Stable across controller restart | yes | same test |
| Stable across delete + recreate | **no guarantee**: environment destroy frees the port; recreate takes the first free port | `lifecycle.rs:106`, `applications.rs:296` |
| Routable externally / TLS | no | no DomainRecord path |

**Same port test.** Two applications that both declare :3000 receive distinct
ports (39327, 39328) and both answer correctly. They never share an address,
because Compute gives each its own host port, not because each listens on
:3000 in a private namespace. Two applications cannot "both use :3000" in the
sense of the application seeing :3000.

---

## 7. Public identity: who owns it

There is **no durable endpoint concept** for computer applications:
`grep -ri "EndpointAllocation\|endpoint_id\|ApplicationEndpoint"` finds none.
The things that exist:

- `TrafficAssignmentRecord` (node environments): stable `host_port` per
  `environment/project/workload/port`, one record per endpoint
  (`model.rs:1115`). Stable across releases; node-shaped.
- `DomainRecord`: an operator-named hostname bound to
  `environment/project/workload/port` (`model.rs:1161`).
- `ProcessEndpoint` (computer): a computed view, `{process, port, url,
  serving}` (`computers.rs:1250`).

**Hostname uniqueness** for domains is enforced two ways in `add_domain`
(`daemon/network.rs:124`): an in-memory pre-check
(`desired.domains.contains_key`, line 134), then a `Write::Create` of a
document whose id is derived from the name (`ids::domain`, `model.rs:1522`;
`Change::create` = "insert a document that must not exist",
`compute-state/src/store.rs`). The second is store-backed and transactional
across the domain, DNS record, certificate and traffic update (one `Change`).
It is **not** delegated to DNS. Nothing allocates a hostname; the operator
supplies it.

The application does not influence a public hostname today, because it has
none.

---

## 8. Networking findings (traced, not inferred)

| Piece | Where | Who can use it |
| --- | --- | --- |
| Domain model, uniqueness | `daemon/network.rs` `add_domain` | node-environment projects only: requires `desired.memberships[(env, project)]`, a released `deployment_id`, and a service with ports from the node `ProjectRevision` (network.rs:144-196). An application's project is not an `EnvironmentProject`. **Verified by probe: refused.** |
| DNS providers | `compute-network/src/dns.rs` (Hetzner, Cloudflare, file) | any `DomainRecord`; created with it |
| DNS reconcile / drift repair | `reconcile_dns_records`, `compute dns reconcile` | any `DnsRecord` |
| ACME HTTP-01, TLS, SNI, secrets | `acme.rs`, `tls.rs`, `secrets.rs`, `reconcile_certificates` | any `Certificate` |
| Ingress routing | `ingress.rs`, `reconcile_network` (network.rs:611-632) | route = `IngressRoute{endpoint_port}` from a **TrafficAssignment** only |
| Ingress upstream | `connect_local(port)` → `127.0.0.1:port` (`ingress.rs:310,335`; `endpoints.rs:205`) | **loopback only**: cannot reach a remote target |
| Ports | `--port-range` (endpoints), `--instance-port-range` | `ports_in_use` unions both; one shared pool |

**Answer to the critical question.** The DNS, ACME, TLS and certificate code
is object-agnostic once a `DomainRecord` exists. The two node-specific seams
are (1) `add_domain` authorization/target resolution, and (2) ingress's route
type and upstream (`IngressRoute` has only a local port; `connect_local` is
hard-wired to 127.0.0.1). Everything else is reusable as is.

`docs/networking.md`: "A domain follows its endpoint through every release"
(true for node). For computer applications there is nothing for a domain to
follow.

---

## 9. Release, promotion, rollback

### Node environment (daemon host) — IMPLEMENTED, tested

State machine `pending → starting → ready → network_ready → switching →
active → draining → complete`, plus `failed` / `rolled_back`
(`daemon/release.rs`, `docs/releases.md`). Start beside, readiness
(`port`, `http`, `process`, `task`), switch in one transaction, data plane
retargets, old instance drains. 7/7 release tests pass, including: a release
moves traffic without dropping a request; a failed or never-ready release
leaves the current revision serving; failure after the switch rolls back;
the daemon restarts mid-release and finishes with exactly one switch.
Promotion releases the exact revision (`daemon/deploy.rs:565`,
`promoted_from`). Rollback keeps the endpoint.

### Application / computer — PARTIAL

Rollout steps are fixed: `Desired state, Checkout, Build, Restart
applications, Health check` (`software.rs:1450`). The **same process** is
restarted with the new commit. There is no second instance, no traffic
switch, no drain.

Measured (probe): with v1 serving, deploy a v2 whose `main.py` exits
immediately.

```text
PROBE broken deploy accepted: Deploying
PROBE t+5s   status=failed   old-url-answers=None ["v2:Deploying","v1:Active"]
PROBE t+10s  status=starting old-url-answers=None ...
PROBE t+100s status=failed   old-url-answers=None ["v2:Failed","v1:Active"]
PROBE after failed deploy, old URL answers? None
```

So: (a) **the healthy deployment's URL stops serving immediately** and never
recovers without a human deploying/rolling back; (b) the record still says
v1 is `Active`, which is false for the running system; (c) the failure is
declared only after `HEALTH_DEADLINE` (60 s) plus step latency (~100 s);
(d) no automatic rollback. The URL itself (`host:port`) is not stolen —
there is no other claimant — but it is invalidated. Answer to "can a failed
deployment invalidate the healthy application's URL": **yes**.

Promotion between computer environments exists: `promote_version`
(`software.rs:1216`) makes a `Rollout{Promote}` of the exact active
`Version`; `compute promote` and the UI ("Move test → production") use it
(`environment_cmd.rs:2082`). It does not rebuild (a Version is immutable;
`roll_out` checks `Published`). It has the same restart-in-place semantics
and, because each environment has its own port, **the promoted environment's
address is its own, unchanged port**. What is missing is the *application
lifecycle around it*: an application is one environment, so there is nothing
to promote *to* unless the user creates environments by hand.

Rollback (application): `rollback_version` makes a new `Rollout{Rollback}`
of an earlier Version in the same environment; endpoint unchanged (tested).

| Event | Node env | Application / computer |
| --- | --- | --- |
| successful deploy | switch, drain | restart in place (downtime) |
| failed startup | old keeps serving | **old stops serving**; new fails after ~100 s |
| failed readiness | old keeps serving (release `failed`) | same as above |
| controller restart mid-deploy | resumes from control state, one switch | rollout `Applying` survives in control state; process is the computer's (tested for restart, not mid-rollout) — UNKNOWN for mid-rollout |
| process crash | restart policy | bounded restart policy (invariant 25), tested |
| rollback | traffic returns | new rollout; restart in place |

---

## 10. Targets and providers

`SessionCapabilities` has `public_endpoint` (`compute-core/src/sessions.rs:127`).
**Both shipped providers set it to `false`** (`compute-provider/src/sessions.rs:424`,
`containers.rs:152`); `docs/compute-capabilities.md` and gap G-PLACE-1 say
the same. So the capability *exists as a requirement* (recipes `preview`,
`staging`, `production` request it) and **no target can satisfy it**. Those
three recipes are therefore **unsatisfiable today** (`recipe validate` →
`unsatisfied`; `environment create --recipe preview` refuses). The earlier
audit's claim that the target model has no ingress representation was wrong:
the flag exists; it is unimplemented.

Placement can answer "does this target have a public endpoint?" only as
`false` everywhere. It cannot yet answer "can ingress front this target?",
which is a *daemon* property (ingress configured), not a provider one.

| Capability | Local Linux | Remote Linux (`compute serve`) | macOS | Apple Container | Hetzner |
| --- | --- | --- | --- | --- | --- |
| Run process | IMPLEMENTED (verified) | IMPLEMENTED (verified, test target) | UNKNOWN | UNKNOWN | via remote Linux target |
| Persistent environment | PARTIAL (file/workspace; `persistent_storage` false) | PARTIAL | UNKNOWN | UNKNOWN | UNKNOWN |
| Stable endpoint | IMPLEMENTED (`host:port`) | IMPLEMENTED (`host:port`) | UNKNOWN | UNKNOWN | UNKNOWN |
| Public hostname | NOT IMPLEMENTED | NOT IMPLEMENTED | NOT IMPLEMENTED | NOT IMPLEMENTED | NOT IMPLEMENTED (node env only, operator-named) |
| DNS / TLS / ingress | node env only | node env only | node env only | node env only | node env only: DNS provider + single-host runbook (`deploy/hetzner`) |
| Deployment | IMPLEMENTED | IMPLEMENTED | UNKNOWN | UNKNOWN | node env on one host |
| Promotion / rollback | PARTIAL (version move / new rollout) | same | UNKNOWN | UNKNOWN | node env |
| Zero-downtime | NOT IMPLEMENTED (app) | NOT IMPLEMENTED (app) | — | — | node env only |
| URL returned | `http://host:port` | `http://host:port` | — | — | — |

There is no Hetzner *target*; "Hetzner" is a DNS provider kind and a
one-host runbook. macOS appears in distribution docs (Preview
distribution), not verified here.

---

## 11. Recipes and environments

Recipes are persisted, immutable, versioned documents that resolve to the
existing `ComputerRequest` plus policy (`docs/recipes.md`; invariant 30;
`tests/recipes.rs`, 11 pass). Starters in `recipes/starters/*.json`:

- `preview`: ephemeral, `ttl_seconds: 86400`, sandboxed, `public_endpoint`.
- `staging`: persistent, `persistent_storage`, `public_endpoint`.
- `production`: persistent, strict isolation, `persistent_storage`,
  `public_endpoint`.
- `dev`, `ci`, `agent-task`, `migration`.

What they establish: **lifetime and requirements only.** `preview` is
disposable (TTL); `production` is durable (never expired). They do **not**
establish promotion policy, "staging is where production revisions come
from", traffic policy, URL policy or release strategy; invariant 30 forbids
it ("recipes express lifecycle policy but do not implement execution"). There
is no environment policy object. A recipe does not know about
previous/next environment. Preview/staging/production are three names for
the same machinery, and two of the three need a capability no provider
offers.

---

## 12. Persistence and authority

| State | Where | Durable | Note |
| --- | --- | --- | --- |
| environment, computer, contents, process port | `Environment`, `Computer` | FeltDB/file | authority |
| Version, Rollout, steps | `Version`, `Rollout` | FeltDB/file | authority |
| node Deployment/Revision/Instance/TrafficAssignment | collections | FeltDB/file | authority |
| Domain, DnsRecord, Certificate | collections | FeltDB/file | authority; key material in node secret store only |
| application endpoint URL | **not stored**; derived per request from target config + process port | no | target host comes from pool config, not control state |
| target/provider config | pool TOML / daemon config | file config | not in control state |
| ingress routing table | daemon memory, rebuilt by `reconcile_network` | rebuilt | derived, correct per invariant 16/17 |
| hostname allocation | none | — | — |

An endpoint for an application would belong in FeltDB (must survive restart,
be agreed on by controllers). The pattern already exists: deterministic ids
+ `Write::Create` + `compute.flow` indexes + `MODEL_GENERATION` bump +
regenerated manifest (`packages/compute-state-model`; `npm ci` succeeded
here). The desired snapshot already loads all Domain, Environment and
Computer records (`desired_snapshot`, `daemon/mod.rs`), so route resolution
for computer endpoints needs no new query shapes.

---

## 13. CLI, API, Studio

| Surface | What the user gets |
| --- | --- |
| `compute deploy <dir>` | `Application / Version / Provider / Runtime / Endpoint: http://host:port / Status` — **no clickable public URL** |
| `compute status <app>` | `Endpoint: http://host:port` (+ `--json` `endpoint`) |
| `compute run` | program output; no URL, by design |
| `compute up` | control-plane URL only |
| `GET /applications/{name}` | `ApplicationView`: `active, application, computer, deployments, endpoint, environment, node, status` (keys observed) — `endpoint` is a string; no `hostname`, `url`, `endpoint_id` |
| Studio (Work) | Endpoints table with `endpoint.url` as a link (`ui/app.js:923-927`) — the `host:port` address; Manage page the same (581). No application view: the UI never calls `/applications` (grep). Domains pages exist for node-environment domains (`app.js:2057`) |
| Clickable result | the UI link works only when the browser can reach the target address (for `compute up`, 127.0.0.1) |

The user must: know the target's reachability, configure DNS and
certificates by hand, and — for a public hostname — use node environments.
The application does not tell Compute its URL.

---

## 14. Documentation versus implementation

| Documentation says | Implementation |
| --- | --- |
| `compute deploy --help`: "with zero downtime" | true for node projects; the application path restarts in place (`docs/applications.md:230`, G-DEP-1) |
| `README.md:188`: "Releases have zero downtime" | node environments only |
| `docs/applications.md`: "The endpoint stays the same" | true; it is a private `host:port` |
| Recipes `preview`/`staging`/`production` list `public_endpoint` | no provider offers it (G-PLACE-1) → unsatisfiable; docs mention it |
| `docs/networking.md`: domain "routes to one workload port of one project" | a *node-environment* project; an application's project is refused |
| Rollout list shows v1 `Active` after failed v2 | the running system is not v1 (the process was replaced) |
| Docs describe G-APP-1 / G-DEP-1 / G-PLACE-1 as open | **accurate**; these are the repo's own register of this audit's findings |

### 14a. Corrections to the earlier audit (`audit-2026-09-30-reachability.md`)

1. It said targets have no ingress/TLS capability representation.
   `public_endpoint` exists; unimplemented.
2. It said promotion on the application path was thin. `promote_version`
   exists and is wired to CLI and UI; it is not an *application* workflow.
3. It did not find that a failed application replacement takes the healthy
   version down (§9).
4. It proposed reusing the node release machine for applications. G-ARCH-5
   and `tests/execution_paths.rs` forbid the node model inside a computer
   environment; the route is to extend the **network layer**, and to add
   switching to the **computer** model, not to reuse `release.rs`.
5. It proposed `EndpointAllocation` as the obvious primitive; see §21 for
   the narrower finding.

---

## 15. Maturity matrix

| Capability | Node environment | Application/computer | Local | Remote | Production |
| --- | --- | --- | --- | --- | --- |
| Application deployment | IMPLEMENTED | IMPLEMENTED (verified) | IMPLEMENTED | IMPLEMENTED | PARTIAL |
| Durable environment | IMPLEMENTED | IMPLEMENTED (restart verified) | IMPLEMENTED | IMPLEMENTED | PARTIAL (`persistent_storage` unoffered) |
| Generated hostname | NOT IMPLEMENTED | NOT IMPLEMENTED | NOT IMPLEMENTED | NOT IMPLEMENTED | NOT IMPLEMENTED |
| Stable URL | PARTIAL (stable endpoint; hostname operator-named) | PARTIAL (stable `host:port`, private) | PARTIAL | PARTIAL | NOT IMPLEMENTED |
| Custom domain | IMPLEMENTED | NOT IMPLEMENTED (refused, verified) | IMPLEMENTED (node) | IMPLEMENTED (node) | IMPLEMENTED (node) |
| DNS | IMPLEMENTED | NOT IMPLEMENTED | — | — | IMPLEMENTED (node) |
| TLS | IMPLEMENTED (ACME issue/renew test passes) | NOT IMPLEMENTED | — | — | IMPLEMENTED (node) |
| Ingress | IMPLEMENTED (loopback upstream only) | NOT IMPLEMENTED | — | — | IMPLEMENTED (node, one host) |
| Readiness | IMPLEMENTED (4 kinds) | PARTIAL (HTTP exists on processes; deploy sets none; TCP-reachable check) | IMPLEMENTED | IMPLEMENTED | PARTIAL |
| Zero-downtime release | IMPLEMENTED | NOT IMPLEMENTED (verified downtime on failure) | — | — | node only |
| Promotion | IMPLEMENTED | PARTIAL (version move between hand-made envs) | — | — | PARTIAL |
| Rollback | IMPLEMENTED | PARTIAL (new rollout; restart) | — | — | PARTIAL |
| URL returned by CLI | PARTIAL (deployment record; no URL) | PARTIAL (`host:port`) | PARTIAL | PARTIAL | NOT IMPLEMENTED |
| URL surfaced by API | PARTIAL (`/domains`) | PARTIAL (`endpoint` string) | — | — | — |
| URL surfaced by Studio | PARTIAL (domains page) | PARTIAL (endpoint link, `host:port`) | — | — | — |
| dev/preview command | — | NOT IMPLEMENTED | — | — | — |
| Public-reachability target capability | n/a | NOT IMPLEMENTED (`public_endpoint` false everywhere) | — | — | — |

---

## 16. Confirmed gaps

1. No Compute-allocated hostname anywhere.
2. Domains/DNS/TLS/ingress unreachable from computer applications
   (`add_domain` membership check; `IngressRoute` local-port only;
   `connect_local`). (G-APP-1)
3. Application replacement takes the serving process down; failure is not
   contained and records misstate the live version. (G-DEP-1, extended)
4. `public_endpoint` offered by no target; three starter recipes
   unsatisfiable. (G-PLACE-1)
5. "Application" = one environment; no dev/preview/staging/production
   structure for an application; no `compute dev`/`compute preview`.
6. Port allocation is a snapshot scan, not a store constraint; no
   reservation beyond environment life.
7. `deploy` sets no readiness for applications; rollout health is
   process-running + TCP-reachable.
8. Endpoint URL is computed from pool config at request time; no durable
   record, no `url`/`hostname`/`endpoint_id` in APIs.
9. Node environments cannot run on a target (G-ARCH-5), so the only path with
   production networking is single-host.

---

## 17. Is `EndpointAllocation` the missing primitive? (§21 answer)

**Partly.** Reading the code:

- *Uniqueness* does not need a new record: `DomainRecord` ids derive from the
  hostname and are created with `Write::Create`; that already gives a
  store-backed, transactional uniqueness guarantee for any hostname Compute
  generates.
- What is missing is (a) a **route target** that a `DomainRecord` can point at
  besides a node `TrafficAssignment` (a computer process endpoint:
  environment, process, port, current target host); (b) an **ingress
  upstream** that is not loopback; (c) a **hostname allocator** with a
  configured zone and an opaque id; (d) a **stable identity** for "this
  application environment's reachability" that survives deployments and
  carries `canonical | alias | preview` and a release/tombstone policy.

(d) is the justification for an `EndpointAllocation`-like record, and only
(d). It is small: one canonical record per (environment, process),
`hostname` (opaque, allocated once), `kind`, `status`, `released_at`,
referencing the `DomainRecord` that realizes it. (a)–(c) are the real work.
A design that adds the record but leaves `add_domain` and ingress node-only
would change nothing for the user.

---

## 18. Recommended target architecture

```text
Application declares: listens on $PORT                      (never names a public host)
Environment           ── owns ──▶  Endpoint (canonical)      hostname allocated once, opaque, durable
Endpoint              ── realized by ──▶ DomainRecord(s)     canonical + aliases (existing model)
DomainRecord          ── target ──▶ RouteTarget              NodeEndpoint{…} | ComputerProcess{environment_id, process}
Ingress               ── resolves ──▶ (host, port) of the environment's CURRENT process
Deployment/Rollout    ── changes only ──▶ what the current process is
```

Keep distinct: **endpoint identity** (environment-scoped, durable),
**domain** (DNS/TLS realization of a name), **deployment** (which revision is
current), **process** (what listens), **port** (an execution detail,
allocated by Compute, never public identity).

Constraints from the code: extend `DomainRecord` (additive optional target
field), `IngressRoute` (optional upstream host), `add_domain` authorization
(owner-bound computer environment, like every computer change —
`owned_environment`), and `reconcile_network` (resolve computer routes from
the existing desired snapshot). Do **not** enter the node release state
machine.

---

## 19. Recommended PR sequence

**PR 1 — Endpoint authority for computer applications** (closes G-APP-1)
- Objective: a deployed application gets a Compute-allocated canonical
  hostname and URL, routed by ingress to its computer process, with DNS/TLS
  from the existing stack, returned by CLI/API/UI.
- Files: `compute-state/src/model.rs` (+`store.rs`, `compute.flow`,
  manifest, `MODEL_GENERATION` 13: `EndpointAllocation` collection;
  `DomainRecord.target`), `compute-network/src/ingress.rs`
  (`IngressRoute{host?,port}`, `connect(host,port)`),
  `daemon/network.rs` (allocator, `add_domain`/`reconcile_network` for
  computer targets, release on environment destroy), `daemon/applications.rs`
  (allocate in the `change_environment_with` batch that records the process),
  `daemon/mod.rs` (snapshot source), `model.rs`/`views` (`url`, `hostname`,
  `endpoint_id`), `compute-cli/src/application.rs` (final-line URL),
  `ui/app.js`, `daemon/config` (`network.generated_zone`).
- Data model: additive collection + optional fields; deterministic,
  non-destructive; existing `DomainRecord`s unchanged.
- API: `ApplicationView`/`ApplicationDeploymentView` gain `url`, `hostname`,
  `endpoint_id`, `environment`, `deployment`.
- Networking: no new DNS/ACME/TLS. If no zone/ingress is configured, return
  `internal` with the reason (capabilities doc's `internal|unavailable`
  states), not a fake URL.
- Tests: hostname collision; two apps, same declared port → distinct URLs
  that route; v1→v2→rollback keeps hostname; daemon restart recovers
  allocation; authorization (other operator refused); node-env domains and
  custom domains unchanged; ingress to a non-loopback upstream; no-ingress
  degrades honestly.
- Dependencies: none. Usable after: `compute deploy` ends with a stable URL
  on a host with ingress and a DNS zone.

**PR 2 — Safe replacement for applications** (G-DEP-1, G-DEP-3)
- Objective: a failed replacement never takes down the serving version; a
  successful one switches without a refused connection.
- Files: `compute-core/src/computers.rs` (process instances), computer
  controller (`daemon/computers.rs`), `software.rs` (`roll_out` steps),
  the route-target resolver from PR 1 (points at the current instance),
  `applications.rs` (set HTTP readiness from the artifact or a default).
- Data model: process instance records (additive); rollout step names.
- Tests: the probe above, inverted (v1 keeps answering while a crashing v2
  fails); crash/restart mid-rollout; drain.
- Depends on PR 1 (switch via route target). Usable after: zero-downtime and
  contained failure for applications.

**PR 3 — Application environments and promotion**
- Objective: an application has named environments; `compute deploy
  --environment staging`, `compute promote app --from staging --to
  production`, `compute status` per environment, each with its own canonical
  endpoint retained across promotion.
- Files: `applications.rs` (`application_environment(name, env)`), CLI
  `application.rs`/`environment_cmd.rs`, views.
- Data model: environment naming/ownership convention (no new collection if
  `application-<name>-<env>` is kept); migration for existing
  `application-<name>` = default environment.
- Tests: promote uses the exact Version (no build step); endpoint unchanged;
  rollback keeps URL.
- Depends on PR 1, preferably PR 2. Usable: staging→production.

**PR 4 — `compute dev` and `compute preview`**
- Objective: thin callers of deploy with an environment policy; preview =
  ephemeral environment + `preview` endpoint released (tombstoned) on TTL.
- Files: CLI, recipe resolution reuse, endpoint `kind = preview`.
- Depends on PR 1 (and 3 for per-revision previews). Usable: URL on `dev`.

**PR 5 — Target capability and production target**
- Objective: make `public_endpoint` true where a daemon with ingress can
  front the target, so placement refuses unsatisfiable production requests
  and `preview`/`staging`/`production` recipes resolve; Hetzner as a target
  (not just DNS), Apple Container/macOS certification.
- Files: `compute-provider/src/sessions.rs`, `compute-placement`,
  `deploy/hetzner`.
- Depends on PR 1. Usable: production on a real target end to end.

Studio deployment UX rides along in PR 1 (URL) and PR 3 (environments); a
redesign is not required.

---

## 20. Explicit non-goals and what to reuse

Do **not** build: a second DNS, ACME, TLS, ingress or release system; a
Compute-specific FeltDB API; a local fallback store; a new deployment state
machine duplicating `release.rs`; hostname derivation from app/project names.
Reuse: `DomainRecord`/`DnsRecordRecord`/`CertificateRecord`, `Ingress`,
ACME and DNS reconciliation, `Change`/`Write::Create` and the desired
snapshot, `owned_environment` authorization, `Rollout`/`Version` (immutable,
promotion without rebuild), and `promote_version`/`rollback_version`.

---

## 21. Open questions and UNKNOWNs

- Real CLI end-to-end (deploy → URL → replace → rollback): not run here
  (runtime download blocked); in-process equivalent run.
- macOS, Apple Container, Hetzner host: UNKNOWN.
- FeltDB-backed behavior of everything above: UNKNOWN (no server binary).
- Port-allocation race under concurrent first deploys: derived from code,
  not exercised.
- Mid-rollout controller restart on the application path: UNKNOWN.
- Application-supplied `PORT` override: derived from code, not run.
- Where ingress lives relative to targets in a multi-host deployment (the
  daemon must reach the target's address): a deployment-topology decision for
  PR 1.
- Whether a wildcard certificate (DNS-01) is wanted for generated hostnames;
  today HTTP-01 per hostname only (`docs/networking.md`, limits).

---

## 22. Final answers

**A. What URL does Compute give me today?** `http://<target-host>:<port>`,
e.g. `http://127.0.0.1:39327`, from `compute deploy`/`compute status`/the UI.

**B. Why no public URL?** Nothing creates a `DomainRecord` for an
application (`add_domain` requires a node-environment project membership,
refused in the probe), and ingress can only forward to loopback; no component
generates hostnames.

**C. Can two applications safely listen on the same internal port?** They
can *declare* it, but Compute ignores the declaration and gives each its own
host port (`PORT` env): distinct ports verified (39327/39328). The
allocation is a snapshot scan, not a store constraint.

**D. Who owns public hostname allocation?** Nobody for applications. For
node environments the operator names the domain; uniqueness is store-backed.

**E. Does DNS/TLS/ingress already solve most of production networking?**
Yes, for what it can see: DNS reconcile, ACME, SNI, ingress are implemented
and tested (network 2/2 pass). Their reach is the limit.

**F. For applications or only node environments?** Only node environments.

**G. Does application deployment use the production release machinery?**
No. Node: start-beside/switch/drain. Application: restart in place.

**H. Promotion without rebuild?** Yes for node revisions and for computer
Versions (`promote_version`); not wired into an application workflow.

**I. Roll back without changing the URL?** Yes for the application's
`host:port` and node endpoints; but the URL is not a public URL, and
application rollback restarts in place.

**J. Missing for create → deploy → URL → share → update → same URL →
rollback → same URL?** A Compute-owned public hostname and its routing
(PR 1); contained/zero-downtime replacement so "update" cannot break the URL
(PR 2); an application-level environment/promotion story (PR 3). DNS zone and
ingress configuration on the daemon host are prerequisites.

**K. What should the next PR implement?** PR 1: the endpoint authority for
computer applications — allocator + durable endpoint identity + computer
route target + non-loopback ingress upstream + URL in CLI/API/UI, reusing
the existing domain/DNS/TLS/ingress stack. Not a dev-URL shortcut.

---

## Test results appendix

Every suite listed in §3 passed; none failed. Suites not run: FeltDB-backed
ones (no server binary), and the rest of `compute-environment`'s tests
(`scale`, `security`, `availability`, `parity`, `computers`, `environments`,
`bootstrap`, `process_policy`, `provenance`, `readiness`, `executions`) and
`compute-cli`'s other tests, because a full `cargo test --workspace` ran the
sandbox out of disk. Their status is UNKNOWN for this audit, not failing.

