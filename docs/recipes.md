# Recipes

> **Compute provides execution. Recipes describe how that execution should be
> used.**
>
> Recipes express lifecycle policy but do not implement execution. All Recipe
> execution must resolve into existing Compute Computer, Configured
> Environment, workload, process, provider, persistence, isolation, and
> lifecycle primitives.

**Status:** implemented as a policy layer: `compute recipe list | get |
validate | resolve | create | edit`, `compute environment create --recipe`,
`GET`/`POST /recipes`, `GET /recipes/{recipe}[/resolve]`,
`POST /recipes/resolve`. Code: `compute-core/src/recipes.rs` (the spec),
`compute-environment/src/recipe.rs` (the pure resolver),
`compute-environment/src/daemon/recipes.rs` (versions and the read-only
preview), `compute-cli/src/recipe_cmd.rs`. Tests:
`compute-environment/tests/recipes.rs`.

The core thesis is unchanged: *build software once; run it wherever Compute
can satisfy its requirements.* Compute has the primitives. A Recipe says how
they should be used, and nothing more.

```text
Recipe ──resolve (read only)──▶ ComputerRequest + Policy     what `POST /environments` takes
                                      │
                                      ▼
                    existing environment create ─▶ placement ─▶ Computer ─▶ target session
                                      │
                       records recipe name · version · digest on the environment
```

## What a Recipe is, and is not

A Recipe is a **durable, user-owned, versioned declaration of lifecycle
intent**, made only of Compute's existing vocabulary. Writing one changes what
a future `environment create --recipe` asks for. It runs nothing.

It is **not** a Computer, environment, workload, deployment, or CI system, and
Compute has no `RecipeComputer`, `RecipeWorkload`, `RecipeEnvironment`, or
`RecipeLifecycleEngine`. There is no recipe scheduler, state machine, VM
manager, cleanup daemon, or process supervisor. A recipe reaches Compute only
as the ordinary create request; that request is placed, owned, reconciled,
expired, and destroyed by the code that already does those things for every
caller. `recipes_express_policy_and_implement_no_execution` fails if a recipe
module gains a way to execute.

**CI is a Recipe. Production is a Recipe. Preview is a Recipe. Agent work is
a Recipe. Migration is a Recipe.** They are not separate Compute products.
Their recipes exist ([`examples/recipes`](../examples/recipes)); that does
*not* mean a CI, deployment, staging, preview, agent, or migration system is
implemented. Those are uses of Recipes, and they belong to their consumers.

## The audit

Derived before any field was written, from the code at this commit. Nothing in
the Recipe is invented; where an idea has no primitive, it is a gap (below).

**1. Lifecycle primitives that exist.** Environment create (place, own,
provision a session); computer `pending → provisioning → running`; `stop`/
`start` (workspace kept); `replace` (new machine, same environment); `fork`,
`checkpoint`, `restore` (state moves; [docs/architecture.md](architecture.md#how-state-moves));
`destroy` (machine goes, record stays); TTL expiry of ephemeral computers
(`expired`); `lost`/`unreachable` observed reality
([docs/computers.md](computers.md)). Work sessions (`attached`, or `ephemeral`
owning a temporary environment that closes with them:
`compute-environment/src/daemon/work.rs`).

**2. Environment, computer, workload contracts.** `EnvironmentRecord` (config,
policy, owner, `ComputerSpec`, `EnvironmentContents`); `ComputerSpec =
lifecycle + ComputerRequirements + target + ttl + generation`
(`compute-core/src/computers.rs`); the request that creates them is
`ComputerRequest` inside `ComputerEnvironmentDefinition`
(`compute-environment/src/model.rs`). A workload runs as a durable job in the
computer's target session.

**3. Persistence semantics.** Four different things, kept apart:
*computer lifetime* (`ComputerLifecycle`: persistent until destroyed, or
ephemeral until TTL), *environment lifetime* (the record outlives its
computer as evidence), *workload lifetime* (a process's `desired`
running/stopped and restart policy: per process, inside `contents`), and *data
persistence* (the session capability `persistent_storage`; a persistent
computer also needs `claim`, added by `PlacementRequirements::for_computer`).

**4. Isolation.** `IsolationProfile` (`process`, `sandboxed`, `strict`) and
`NetworkPolicy` (`none`, `localhost`, `network`), both inside
`ComputerRequirements`; a target that cannot enforce the profile is refused
with `isolation_unsupported` ([docs/isolation.md](isolation.md)). An
environment `policy` (`compute.policy@1`) can only restrict admission.

**5. Provider and target.** A target is a pool member hosting sessions;
`ComputerRequirements` deliberately carries no provider identity; placement
chooses. `--target` constrains the choice for one create.

**6. Release and expiry.** Explicit `destroy`; automatic TTL expiry
(`expires_at` on an ephemeral spec, enforced by the computer controller); a
work session that owns a temporary environment destroys it on close. Nothing
releases a computer on idle, on job success, or on failure.

**7. Policy versus machinery in CI.** The Foundry CI implementation is not in
this repository (its scope is Compute only), so it was **not audited
directly**. What CI needs was taken from the two documents in this repository
that describe the consumer ([factory-control-plane.md](factory-control-plane.md),
[local-ci-audit.md](local-ci-audit.md)) and from the contracts above, and
should be confirmed against Foundry before Foundry adopts a recipe:

| CI behavior | Policy (Recipe) or machinery | Where it lives |
| --- | --- | --- |
| Ephemeral computer, disposable | **Policy** | `lifecycle: ephemeral` |
| Bounded lifetime / TTL recovery | **Policy** (the number); machinery (enforcing it) | `ttl_seconds`; the computer controller expires it |
| Isolated, no inbound reach; stronger for untrusted workflows | **Policy** | `requirements.isolation`, `network`; Foundry may only strengthen |
| Enough CPU/memory, architecture, features | **Policy** | `requirements` |
| Where it runs | Machinery | placement, never the recipe |
| Check out the committed revision | Project reality | `contents` (repositories): Foundry/PAX, not the recipe |
| Configured environment: install, build, test commands | Project reality | PAX decides; `contents` carries it |
| Secrets and configuration | Environment configuration | `environment config`; never a recipe |
| Execute operations, capture evidence | Machinery | target jobs, receipts, events |
| Release / destroy | Machinery (the operation); policy is only *which* release the lifecycle implies | `destroy`, TTL expiry |
| Hold a failed machine for inspection | Not expressible | gap: `claim` is a per-session operation, no policy |

**8. Proposed recipe fields that map directly.** `lifecycle` →
`ComputerLifecycle`; `ttl_seconds` → `ComputerSpec.ttl_seconds`; `requirements`
→ `ComputerRequirements` (which already contains resources, architecture,
network, isolation, session capabilities, target features, runtimes); `policy`
→ `EnvironmentRecord.policy`.

**9. Gaps kept explicit** are in [Gaps](#gaps).

## Structure

`compute.recipe@1` — a spec, written as JSON:

```json
{
  "description": "A disposable, isolated computer.",
  "lifecycle": "ephemeral",
  "ttl_seconds": 3600,
  "requirements": { "network": "network", "isolation": "sandboxed", "cpu_count": 2 },
  "policy": null
}
```

| Concept | Field | Existing primitive |
| --- | --- | --- |
| identity | the recipe's `name` (a name like an environment's), its version, its `digest` | `RecipeRecord` in control state |
| requirements | `requirements` | `ComputerRequirements`, unchanged: no `RecipeRequirements` |
| environment | `policy` | `EnvironmentRecord.policy` (`compute.policy@1`; restrict-only). Contents and configuration are not recipe fields |
| lifecycle | `lifecycle`, `ttl_seconds` | `ComputerLifecycle`, `ComputerSpec.ttl_seconds` |
| persistence | `lifecycle`; `requirements.capabilities` (`persistent_storage`) | computer lifetime; the session capability. See below |
| isolation | `requirements.isolation` | `IsolationProfile` |
| networking | `requirements.network`, `requirements.capabilities` (`public_endpoint`) | `NetworkPolicy`, session capabilities |
| observability | none: derived | the version recorded on the environment, placement evidence, events, receipts |
| release | none: derived from `lifecycle` | `destroy`, TTL expiry |

Unknown fields are rejected. A recipe cannot name a provider or target,
contain repositories, commands, or source, or carry configuration values.

### Lifecycle, precisely

* **`ephemeral`** resolves to `ComputerLifecycle::Ephemeral` with a TTL
  (`ttl_seconds`, default one hour). Compute itself tears the computer down
  when the TTL passes (`expired`); it is not "the caller intends to delete it".
  A caller may also `destroy` earlier.
* **`persistent`** resolves to `ComputerLifecycle::Persistent`: never expired,
  released only by `destroy`. Placement adds the session capability `claim`
  (shown as `implied_capabilities`). It does **not** promise the data survives
  the machine; ask for that by requiring `persistent_storage`, and
  `compute recipe resolve` says so when a persistent recipe does not.
* What stop, destroy, and cancel guarantee is Compute's ([lifecycle.md](lifecycle.md)), not the recipe's; a recipe that needs the guarantee requires the capability `process_tree_termination`.
* A persistent recipe with a TTL, or a zero TTL, is *invalid*: the same rule
  `environment create` enforces.
* `interactive` is the `terminal` capability. "Bounded" is an ephemeral TTL.
  "Long-lived" is persistent. Stop, restart, and recovery are operations and
  process restart policy, not recipe fields (gaps).

### Persistence

A recipe distinguishes only what Compute distinguishes. Computer lifetime is
`lifecycle`/`ttl_seconds`. Data persistence is `persistent_storage`. The
environment record always outlives its computer. Workload lifetime is a
per-process property of environment contents, so it is not a recipe field.

## Validation

Three answers, never collapsed:

| Verdict / failure | Meaning | Where you see it |
| --- | --- | --- |
| **Invalid policy** | The recipe is malformed or cannot resolve: an unknown feature or capability, a persistent recipe with a TTL, an invalid policy. Every problem is listed | `verdict: invalid`; writing it is refused (`invalid`); `compute recipe validate` exits 3 |
| **Unsatisfied requirements** | Valid, and no current target satisfies it; placement says why for every target (`isolation_unsupported`, `target_feature_unsupported`, …) | `verdict: unsatisfied`; exit 2, the status a failed placement exits with elsewhere; `environment create --recipe` refuses before recording anything |
| **Runtime failure** | It resolved and was created, and execution then failed | Not a recipe verdict. The computer's own `failed` status and `failure` (phase, code), and the job's receipt |

## Resolution: what will this Recipe cause Compute to do?

`compute recipe resolve NAME` (or `--file draft.json`, before creating it) is
read only and acquires nothing. It returns:

* the recipe version and digest;
* the **verdict** and any problems;
* what it resolves to: the `ComputerRequest` and policy that
  `POST /environments` takes, byte for byte;
* the lifecycle in Compute's words and the existing operations that carry it
  out;
* placement's own report: every target, compatible or not, with reasons, and
  which it would select.

It uses the placement evaluation `environment create` uses
(`Daemon::evaluate_placement`), extracted rather than copied. Resolution is
deterministic: the same spec resolves to the same request. `--target` (a
caller constraint, never a recipe field) narrows placement.

## Using a recipe

```sh
compute recipe create review --file examples/recipes/preview.json
compute recipe resolve review                 # what will happen, before it does
compute recipe validate review                # exit 0 / 2 unsatisfied / 3 invalid
compute environment create pr-42 --recipe review --contents contents.json
compute environment destroy pr-42             # release: the existing operation
```

`--recipe` takes the place of the requirement flags (they conflict), and
combines with `--target` and `--contents`. The CLI resolves the recipe, then
sends the ordinary create request with the version as evidence. The control
plane **verifies the evidence**: the version must exist, its digest must
match, and the computer and policy requested must be what that version
resolves to (only `target` may differ). An environment therefore cannot claim
a recipe it was not made from. The recipe is checked, not trusted.

The resolved `ComputerRequest` is also what work sessions
(`OpenSessionRequest.computer`) take; recording the recipe there is not built
(gaps).

## Versions and reproducibility

Recipes are versioned, because their history must be explainable and a run
must not depend on whatever the recipe says now:

* Every write is a new immutable version (`Recipe` collection, `rcp_` + name
  and version). Editing supersedes the previous one in the same transaction,
  fenced by the version the editor read (`expected_version`; `conflict`
  otherwise). An edit that changes nothing is not a new version.
* An environment made from a recipe records `{name, version, digest}`
  (`Environment.recipe`, shown in `environment inspect`, and in the
  `environment.created` event, never the spec).
* `compute recipe get NAME --version N` and `resolve --version N` answer for
  any past version, so what an environment was made from can be re-resolved
  exactly.

`recipe` on the environment records **provenance at creation**. If the
environment's computer is later changed (`replace`), the environment's
`computer` remains the authority and the recipe record is not updated; fork,
restore, and replace do not carry it.

## User-defined recipes

A recipe is data. `customer-demo`, `nightly-data`, `review-environment`,
`gpu-training`, and `migration-window` need no Compute change: no new Rust
type, Computer, lifecycle, or provider, and nothing in the daemon knows any
name. `any_recipe_a_user_writes_resolves_through_the_same_mechanism` proves it.

Recipes are shared, versioned policy: any operator with `compute.operate` may
write one, and each version records its author. They are not owner-bound like
environments. Edit with `recipe get NAME --json`, change the spec, and `recipe
edit NAME --file spec.json`.

## Starter recipes

[`examples/recipes`](../examples/recipes) holds ordinary documents, not
special cases. What each expresses, and what it does not:

| Recipe | Expresses | Does not (and why) |
| --- | --- | --- |
| `dev` | persistent, interactive (`terminal`) | "Developer-owned" is environment ownership, which every environment has |
| `ci` | ephemeral, sandboxed isolation, TTL | Checkout, install, run, evidence, and release are contents, jobs, and `destroy`; hold-on-failure has no primitive |
| `staging` | persistent, `persistent_storage`, `public_endpoint` | "Deployable" is a rollout ([releases.md](releases.md)), not a lifetime |
| `production` | persistent, strict isolation, durable storage, public endpoint | Durability is only as strong as the target's `persistent_storage`; Compute promises no backup or replication. "Hetzner" is a target, never here |
| `preview` | ephemeral, sandboxed, public endpoint, TTL | Creating one per pull request is the consumer's |
| `agent-task` | bounded (TTL), sandboxed, no network | "Workload-scoped" (a computer that ends with one workload) has no primitive: the bound is time. An agent is an ordinary workload |
| `migration` | a temporary computer | "Temporary workload against a persistent environment" is not expressible (gaps) |

`sandboxed` needs a target that can enforce it with the network policy
requested; on a host that cannot, `resolve` says `unsatisfied` with
`isolation_unsupported`. That is the semantics, not a recipe error.

## Boundaries

* **Foundry.** Compute knows nothing of Foundry projects, sessions, agents, UI,
  or approvals. A future Foundry CI intent selects a recipe by name; this
  change does not touch Foundry, and Foundry CI continues to use the existing
  primitives directly.
* **PAX.** Recipes detect nothing: not package managers, lockfiles, commands,
  or drift. Project reality stays PAX's and reaches Compute as environment
  contents.
* **Agents.** No agent Computer. An agent workload is a process like any other
  and may run in a computer a recipe made.
* **Migration.** No migration engine.
* **Providers.** No recipe names a provider or target.

## Gaps

Kept explicit; each names the missing primitive rather than a workaround.

| Gap | Missing primitive |
| --- | --- |
| Hold-on-failure (keep a failed CI machine) | A policy on when to `claim`/keep a session. `claim` exists per session |
| Release on completion or idle | No release-on-idle or on-exit trigger; only explicit `destroy` and TTL. A recipe would have to invent a supervisor |
| Workload-scoped lifetime | No computer bound to one workload's lifetime |
| Migration: temporary workload against a persistent environment | Work sessions attach to an environment, and `exec` runs jobs, but a recipe cannot name "an existing environment" without becoming environment-specific. No workload TTL |
| Recovery policy (`lost` → replace) | `replace` is explicit; nothing automatic |
| Stop/restart policy for a computer | Operations, not policy. Process restart is per-process contents |
| Checkpoint/fork policy | Operations, not lifetimes |
| Environment contents, configuration in a recipe | Deliberately absent: project reality is PAX's; values are secrets |
| Recipes for raw target sessions (`compute session create`) and work sessions | The resolved `ComputerRequest` fits `OpenSessionRequest`, but recording the recipe there is not built |
| Recipe drift after `replace`/`fork` | The recorded version is provenance at creation, not a live comparison |
| Deleting or archiving a recipe | Not built: versions are immutable evidence; only the current one lists |
| Authorization beyond `compute.operate` | Recipes are shared; no per-recipe owner |
| Foundry CI's implementation | Not in this repository; its policy is derived, not observed |
