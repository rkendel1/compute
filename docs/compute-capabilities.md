# Compute capabilities and portability

**Status:** design, extending what exists. Nothing here is implemented.
Companion to [opencomputer-evaluation.md](opencomputer-evaluation.md).

> What does Compute promise everywhere, and what does it merely expose when a
> provider satisfies a capability?

## The three tiers

Every operation in this design belongs to exactly one tier. The tier decides
what may differ between providers.

| Tier | Meaning | Who guarantees it | May a provider say "unsupported"? |
| --- | --- | --- | --- |
| **1. Compute contract** | Semantics Compute defines and implements over a *prerequisite capability* | Compute, wherever the prerequisite holds | Only by lacking the prerequisite; the refusal names it |
| **2. Provider capability** | An optional feature a provider advertises for the environment it built | The provider, for that environment | **Yes**, explicitly, always |
| **3. Not offered** | Rejected as a portable promise | nobody | n/a |

Rules:

1. **Capabilities are the environment's, not the provider's name.** They are
   reported per environment (`SessionCapabilities`,
   `compute-core/src/sessions.rs:122`) and per target (`target_features`,
   `compute-core/src/computers.rs:129`), and placement records what it matched
   against (`capability_version`).
2. **A required capability the environment lacks is refused before anything is
   sent** (`SessionCapabilities::missing`, `sessions.rs`; `operation_unsupported`).
   An unknown capability name is an error, never "not required"
   (`SessionCapabilities::validate_names`).
3. **A provider feature never becomes implicit.** If it is not a named,
   advertised capability, portable contracts must not depend on it.
4. **Tier-1 contracts are implemented by Compute, not delegated.** A provider
   supplies *access* (a directory, a connection); Compute supplies the
   semantics, the identity, the verification, and the evidence.
5. **Advertised is not verified.** A capability an environment reports is what
   the provider claimed; a Compute-run probe is what proves it (see
   [evaluation §8](opencomputer-evaluation.md#8-receipt--evidence-model)).

## Existing capabilities (verified in code)

`SessionCapabilities` has nine fields: `exec`, `terminal`, `filesystem`,
`network`, `public_endpoint`, `persistent_storage`, `suspend`, `resume`,
`claim` (`compute-core/src/sessions.rs:122-132`).

| Capability | Workspace provider | Container provider | Source |
| --- | --- | --- | --- |
| `exec` | yes | yes | `compute-provider/src/sessions.rs:414`; `containers.rs:146` |
| `filesystem` | yes | yes | same |
| `network` | yes | yes | same |
| `suspend`, `resume`, `claim` | yes | yes | same |
| `terminal` | **no** | **no** | same |
| `public_endpoint` | **no** | **no** | same |
| `persistent_storage` | **no** | **no** | same |

`persistent_storage` means "storage outlives the session"; `filesystem` +
`resume` is what keeps a workspace across stop/resume. The two are different
and this document does not conflate them.

## Capability matrix for the selected primitives

| Capability | Portable contract (tier 1) | Provider requirement | Unsupported allowed? | Verification |
| --- | --- | --- | --- | --- |
| **Persistent filesystem** across stop/resume of the same environment | The workspace tree is unchanged after `resume`. Promised wherever `filesystem` **and** `resume` hold | `filesystem`, `resume` (exist) | Yes: an environment without `resume` is not stoppable/resumable and says so | A probe job writes and reads a marker digest before/after (Phase 5) |
| **Checkpoint** (filesystem + declared contents) | `compute.checkpoint@1` archive identified by digest; verified on capture. Promised wherever `checkpoint` holds | `checkpoint`: the provider lets Compute read the workspace (new; default unsupported) | Yes; refused before any send | Archive re-hashed at capture; tree digest recomputed after seed |
| **Checkpoint of memory** | none | none | Not offered as a portable promise. A provider-scoped capability under a distinct name may be added later; never called `checkpoint` or `suspend` | n/a |
| **Restore** | Seeded replacement of the environment's computer (same requirements, new machine) | `checkpoint` (seed side: the provider lets Compute write an empty workspace) + placement finding a compatible target | Yes: placement-incompatible with reasons | Tree-digest verification job on the new machine |
| **Fork** | A new environment (own identity, computer, generation) seeded from a checkpoint, with durable lineage | as restore | Yes: same | as restore; independence proven by the conformance suite |
| **Hibernate** | No separate contract. `stop` keeps the disk; processes are re-derived | `suspend` (documented as disk-retaining) | Yes | Stop confirmed by `inspect`; not memory |
| **Resume** | The *same* machine, workspace intact; never a substitute machine | `resume` | Yes: `resume_unsupported`; stays `stopped` | Verification job after resume; else `unverified` |
| **Resize** | *Expressing* a requirements change is portable (`spec_generation`). Realizing it: in place if the environment advertises `resize`, else by explicit `replace` | `resize` (new; optional) | Yes: `operation_unsupported`, naming `replace` | Job reads the applied limits; requested vs observed recorded |
| **Interactive session** | Exec sessions (durable jobs) everywhere; an interactive terminal where `terminal` holds | `terminal` (exists, unimplemented) | Yes | Open/close events; no transcript unless requested |
| **Preview endpoint** | The *state model* of an endpoint (below); never a promise that a public URL exists | `public_endpoint` (exists, unimplemented) for public; ingress from `compute-network` for published | Yes: `unavailable` is a first-class state | Readiness probe from inside the computer (as process readiness) |
| **Lifetime ceiling** | A provider discloses `max_lifetime`; placement will not place a persistent environment on a ceilinged provider without an explicit accommodation | `max_lifetime` (new, numeric) | Absence means "no ceiling declared" | Observed against the provider's clock |

## Extending `SessionCapabilities` safely

`SessionCapabilities` is `Copy`, `deny_unknown_fields`, and its fields have no
`#[serde(default)]`; it travels on the wire (`compute.remote@1`) and is stored
in `ComputerRecord.capabilities`. Consequences for the plan:

- New boolean capabilities (`checkpoint`, `resize`) must be added with
  `#[serde(default)]`, added to `NAMES`, `get`, and `entries`, and rolled out
  **readers first**: a node that predates the field rejects the whole
  capabilities object it does not know.
- `max_lifetime` is a duration, not a boolean, so it does not belong in this
  `Copy` struct; it belongs on the target/provider descriptor
  (`ProviderCapabilities`, `compute-provider/src/lib.rs:331`) beside
  `target_features`, and is matched in `match_provider`
  (`compute-placement/src/matching.rs:185`) with a new `ReasonCode`.
- Machine features (`kvm`, `gpu`, …) are a closed list
  (`TARGET_FEATURES`); nothing in this design adds to it.

<a name="resize"></a>
## Resize

OpenComputer scales memory from inside the VM (`169.254.169.254/v1/scale`) on
one backend and returns `501` on the other (`docs/how-it-works.mdx`,
`docs/sandboxes/elasticity.mdx`). Compute's answer is shaped by two existing
facts: requirements are the only thing that provisions a machine, and
replacement is explicit (`docs/computers.md` "Replacement").

- **Where it belongs:** environment *requirements* (`ComputerRequirements`,
  `compute-core/src/computers.rs:155`) express the change; the *capability*
  belongs to the environment (`resize`); placement re-evaluates fit; capacity
  reservations are derived, not mutated (`docs/capacity.md`).
- **Portable:** a requirements change is always expressible and always
  evidenced (`computer.replacing`, or a new `computer.resized`).
- **Not portable:** in-place realization. `compute environment resize`
  succeeds in place only when the environment advertises `resize` and the new
  requirements fit the target's capacity; otherwise it fails with
  `operation_unsupported` and points to `compute environment replace` —
  which, once restore exists, can be made state-preserving
  (checkpoint → replace seeded → verify). It **never** replaces silently.
- **Evidence:** requested versus observed. A verification job reads the
  limits the machine actually has; a mismatch is recorded, not corrected in
  the record.
- **Accelerators and network capacity:** these are machine features and
  placement constraints (`target_features`, `network`), not resize targets;
  changing them is a replacement.
- **Provider placement** is never changed by resize; moving an environment is
  a replacement.

<a name="terminals"></a>
## Terminals

Compute already has the contract; it lacks an implementation. `terminal` is a
capability, `SessionConnectionMode::Terminal` a mode, and `connect` returns a
`ProviderConnection` (command, short-lived credentials, expiry) that is never
persisted (`compute-provider/src/sessions.rs:291`; `docs/sessions.md`
"Connection modes"). The container provider's `connect` today returns a
`compute session exec` command (`containers.rs:258`).

Decision: **do not add a new object.** A terminal is a *connection* to an
existing session, not a session. Implementing `terminal` means a provider can
open a PTY in the environment and a node route carries it over a
WebSocket-style upgrade, authorized as `SessionConnect` on every connection.

| Requirement in the brief | Existing, or to add |
| --- | --- |
| stdin/stdout/stderr | PTY stream (new transport); jobs already capture the three streams |
| working directory, environment variables | The session's; the PTY starts as the workspace user with the same `$HOME`/cwd rule as `exec` |
| process identity | Connection id (short-lived) recorded in an event; not a Compute identity |
| lifecycle | connected / closed events on the session |
| reconnect | Provider-side: the PTY is a supervised process the provider can re-attach; a session that has stopped cannot be reconnected |
| persistence | None by default: a connection is not durable. Optional transcript would be an artifact with a digest, recorded, never implicit |

Until a provider offers it, `terminal` stays `false` and the Work "terminal"
remains what `docs/environment-control-plane.md` states it is: durable jobs,
not a PTY.

<a name="endpoints"></a>
## Endpoints and previews

Existing: `SessionEndpointRequest {port, protocol, public}` and
`SessionEndpoint {id, protocol, address, port, public, expires_at}`
(`compute-core/src/sessions.rs:258-283`); each requested endpoint is an
authorization decision (`session_expose`); providers refuse endpoints today
(`sessions.rs:432`, `containers.rs:164`); deployed processes get endpoints
from the daemon (`docs/environments.md`), and `compute-network` provides
domains, ingress, and certificates.

A preview is **not provider-specific and not always available**. Model an
endpoint as a resource of an environment with an explicit reachability state:

| State | Meaning | Who can reach it |
| --- | --- | --- |
| `unavailable` | The environment cannot publish this port (no `public_endpoint`, no ingress) | nobody; the reason is recorded |
| `internal` | Reachable at the target's address and the process's port (today's behaviour for processes) | the control plane / same network |
| `published` | A Compute-owned ingress route with a domain and certificate (`compute-network`) | whoever the route's authorization admits |

- **Compute capability, provider-independent when ingress exists.** Publishing
  is done by Compute's network layer, not the provider; the provider need only
  make the port reachable from the node. That keeps "preview" portable across
  targets and keeps public exposure a *policy decision* (`session_expose` /
  `environment` authorization), never a default. OpenComputer's previews are
  public unless `previewAuth` is set at creation (`docs/sandboxes/preview-urls.mdx`);
  Compute's default is the opposite.
- **Application level:** what a service *means* at its endpoint is the
  application's (AppPort integration, `SessionConnectionMode::Appport`); Compute
  only routes and evidences reachability.
- **Evidence:** the same HTTP readiness contract processes already have
  (checked *inside the computer*), plus the ingress route's own events.
- **Do not assume a public URL exists.** `unavailable` and `internal` are
  normal, recorded outcomes.

<a name="max-lifetime"></a>
## Provider lifetime ceilings

OpenComputer's current backend destroys every sandbox after 8 hours of
running-plus-hibernated time and says so in the sandbox's `endAt`
(`docs/sandboxes/lifetime.mdx`). Compute must be able to represent a provider
like that without pretending a "persistent" environment is persistent there.
The design: a provider discloses `max_lifetime` in its descriptor; placement
refuses a `persistent` environment on such a provider with a reason unless the
environment opts into a documented accommodation (checkpoint-and-replace before
the deadline, once restore exists); and the deadline is shown in `reality`
rather than computed by clients ("`endAt` is the truth; `createdAt + 8h` is
not", in OpenComputer's own words).
