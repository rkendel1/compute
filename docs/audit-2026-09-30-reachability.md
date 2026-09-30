# Compute audit: reachability and the application lifecycle

Audited 2026-09-30 at `9dfd467` (0.1.8). The question: **does Compute own
an application's whole lifecycle, including the URL a person opens?** Every
claim cites the code or a doc that the code backs. Nothing was run for this
audit; it is a read of source and docs, so "works" below means "implemented
and documented with tests", not "reproduced today".

## Verdict

The maturity table that prompted this audit rates domains, DNS, TLS,
routing, promotion, rollback and zero-downtime as missing (🔴/🟠). **Most of
that exists, on one path.** The real gap is narrower and more structural:

> The network and release machinery is built for **node environments** (the
> daemon host). Applications, dev, preview and the `compute run` path, where
> new work is meant to go (invariant 21), have none of it. They get a
> `host:port` string, not a Compute-allocated, durable, routed URL.

## What the code already has (node-environment path)

| Claim in the prompting audit | Reality | Evidence |
| --- | --- | --- |
| Domains missing | Implemented: domain records, unique across the control plane, routed to one workload port, isolated to their environment | `docs/networking.md`; `daemon/network.rs` `add_domain` |
| DNS missing | Implemented: Hetzner, Cloudflare and file providers; drift detection and repair; records removed with the domain | `docs/networking.md`; `compute-network/src/dns.rs` |
| TLS missing | Implemented: ACME HTTP-01, SNI, renewal, keys kept out of control state | `docs/networking.md`; `compute-network/src/{acme,tls,secrets}.rs` |
| Routing missing | Implemented: ingress :80/:443 routes host → endpoint; unknown host → 404 | `compute-network/src/ingress.rs` |
| Stable URL missing | The *endpoint* is stable across releases; domains follow it through every release | `docs/releases.md`, `docs/networking.md` |
| Promotion missing | `compute deploy --from preprod --to production` releases the exact revision; promotion never rebuilds | `daemon/deploy.rs` `promote`; `DeploymentRecord.promoted_from`; `model.rs:253` |
| Zero-downtime missing | Implemented, with a durable state machine `pending → … → active → draining → complete` and crash-safe resume | `docs/releases.md`; `daemon/release.rs` |
| Rollback incomplete | Implemented: abandon, switch back, or re-release the replaced revision, traffic on the same endpoint | `docs/releases.md` |
| Readiness | Explicit checks gate the traffic switch | `docs/releases.md`, `docs/readiness.md`, invariant 24 |

So invariants 2 and 3 from the prompting audit (an environment URL
outlives deployments; promotion is the same immutable revision) are
**already true** for node environments. They are not stated as invariants.

## What is actually missing

1. **No Compute-allocated hostname.** Domains are operator-supplied
   (`compute domain add app.example.com`). Nothing generates
   `<id>.<compute-domain>`, and nothing reserves one transactionally. The
   only uniqueness guarantee is "a domain name already exists" at
   `add_domain`. There is no allocation record, no release policy, no
   preview/stable distinction, no canonical-versus-alias distinction.
2. **The application path has no reachability.** `compute deploy <dir>` and
   `compute application …` (`daemon/applications.rs`) return
   `endpoint: http://10.0.0.20:20000`: a host port on a target, from a
   process endpoint. `add_domain` requires a node-environment project
   membership (`desired.memberships`), so an application cannot get a
   domain, DNS or TLS. Ingress lives on the daemon host only.
3. **The two paths do not converge.** `docs/releases.md` marks the release
   model a legacy exception (G-ARCH-5, blocked). The zero-downtime, domain
   and promotion work lives on the exception; the path invariant 21 points
   new work at has a stable `host:port` and versions, but no traffic
   switching or routing.
4. **`compute run` returns no URL.** It is ephemeral by design (invariant
   22: no deployment, no endpoint). There is no `compute dev`, no
   `compute preview`, and the CLI has no command that ends with a clickable
   URL. `compute status` and `compute up` print the daemon's own URL, not an
   application's.
5. **Environment policies are not productized.** Recipes (`dev`, `preview`,
   `staging`, `production`) exist and invariant 30 keeps them execution-only.
   There is no environment-level policy for "preview is disposable, staging
   consumes a revision, production requires an active predecessor".
6. **Ports are allocated, but only as host ports.** Endpoint ports come
   from `--port-range`; instance ports from `--instance-port-range`.
   Applications do get a collision-free host port and receive it as `PORT`,
   but the public identity is the port, not a hostname.
7. **Target capability contract lacks ingress.** Placement matches runtime,
   architecture and resources. A target does not declare "can provide public
   ingress/TLS", so Compute cannot ask "can this target satisfy production?"
   as a capability question.
8. **Studio/UI has no application URL.** The 2026-09-25 audit found the UI
   has no application concept; this remains unverified as changed.

## Corrected maturity

| Area | Node environment path | Application / computer path |
| --- | --- | --- |
| Domains, DNS, TLS, routing | 🟢 operator-supplied names | 🔴 none |
| Generated/allocated hostname | 🔴 | 🔴 |
| Stable environment URL | 🟢 (endpoint + domain) | 🟡 stable `host:port` only |
| Zero-downtime release, drain | 🟢 | 🟠 process restarts on release (`applications.md:230`) |
| Promotion of the same revision | 🟢 | 🟡 `RolloutKind::Promote` exists for versions |
| Rollback on the same endpoint | 🟢 | 🟡 rollback-as-new-version, endpoint stable |
| Dev/preview URL on run | n/a | 🔴 |

## Recommended next change

Do not build "dev URLs". Build the missing layer once, on the path new
work uses:

1. **Endpoint allocation record** in `compute.flow` (additive model change,
   `MODEL_GENERATION` bump, regenerated manifest): `endpoint_id`,
   `hostname` (unique index), `application`/`environment`/`deployment`,
   `kind` (`canonical` | `preview` | `alias`), `status`, `created_at`,
   `released_at`. Allocate in the same transaction as the deployment or
   rollout record. Read by identity or indexed equality; never scan
   (AGENTS.md).
2. **Reuse, don't rebuild, the network stack.** Domains already give
   routing, DNS and TLS. A generated hostname is a `DomainRecord` the
   allocator creates under a configured Compute zone, so ingress, ACME and
   DNS reconcile it unchanged. Extend `add_domain`'s membership check to
   resolve an application's computer endpoint, so the application path can
   be routed.
3. **Ingress target for computer endpoints.** The route resolves
   host → environment → current deployment → the computer's endpoint.
   Ingress stays a node capability; add it to the target offer.
4. **Surface the URL.** `compute deploy`, `compute status` and `/applications`
   return `url` (canonical) and `aliases`; the CLI prints it last.
   Add `compute dev` and `compute preview` as thin callers of the same
   deploy path with environment policy, not new execution paths.
5. **Lifecycle on the application path.** Give a computer deployment the
   release states node environments already have (start beside, ready,
   switch, drain), so the URL survives replacement without a restart gap.

Suggested invariants to add to `docs/architecture.md` (next number is 31):

- Compute is the authority for reachability: applications declare
  listeners, never allocate or assert public endpoints; every reachable
  application has a durable, collision-free, Compute-allocated endpoint.
- A production URL identifies an environment, not a deployment;
  replacement, promotion and rollback never change it.
- Promotion deploys the revision validated in the preceding environment and
  never rebuilds source. (Already enforced for node environments; extend the
  test to applications.)

## Not assessed

Hetzner end-to-end, Apple Container, and Mac target status were not
re-verified here. The claim that Studio shows no application URL is carried
forward from 2026-09-25.
