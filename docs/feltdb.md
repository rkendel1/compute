# Compute on FeltDB

**Compute does not implement a second durable-state system. FeltDB is the
authoritative durable-state substrate. Compute may keep bounded, ephemeral
working state and caches, but they are never authority.**

**Compute reads targeted state through bounded, indexed FeltDB queries and
coherent multi-record views through FeltDB snapshots. Whole-collection
reads filtered in Compute are prohibited on production controller paths.**

```text
                 Compute
                    │
        ┌───────────┴───────────┐
        │                       │
 ephemeral working state   durable authority
        │                       │
     controller             FeltDB 0.11.8
        │                       │
        └───────────┬───────────┘
                    │
             bounded queries
                    +
             coherent snapshots
                    +
             durable transactions
```

Compute owns execution. FeltDB owns durable state.

This page is the contract between them. The certification that the
contract holds is [feltdb-0.11.8-consumer-certification.md](feltdb-0.11.8-consumer-certification.md).

## Version

Compute is certified against `@feltdb/core` **0.11.8**. The version is
declared once, as `CERTIFIED_FELTDB_VERSION` in
`crates/compute-state-feltdb/src/lib.rs`, and:

- `packages/compute-state-model` pins it exactly, and compiles
  `compute.flow` with it;
- `scripts/feltdb/verify-version.mjs` fails unless every lockfile and the
  installed `node_modules` resolve exactly that version;
- CI builds the `feltdb-server` the tests run from the source shipped inside
  the resolved package, so the compiler, the tests, and the authority under
  test are one release (`.github/workflows/feltdb-consumer.yml`);
- `/info` and `compute doctor` report it, with the server's own version.

## How Compute talks to FeltDB

`compute-state-feltdb` is a Rust client of FeltDB's application-scoped
Service API. It uses only public FeltDB endpoints, and adds none:

| Endpoint | Use |
| --- | --- |
| `GET /v1/application` | The Compute application's active revision |
| `POST /v1/query` | Bounded, ordered, paginated reads; the plan FeltDB reports is counted |
| `POST /v1/transactions` | Atomic, version-fenced writes (a change and its events are one transaction) |
| `GET /v1/state/version` | The committed revision: proves a snapshot coherent, a cache current |
| `GET /health` | FeltDB's own health and server version |
| `/api/applications/…` | Provisioning and upgrading the model (draft, validate, commit, promote) |
| `POST /admin/backups` | FeltDB's online backup, when an upgrade is asked to take one |

## Reading

### Rules

| The controller needs | It uses |
| --- | --- |
| One known record | An identity lookup: one index probe |
| A few records it can name | Identity lookups, issued concurrently (bounded concurrency) |
| The newest *n* of a scope | An indexed equality, ordered and limited by FeltDB |
| A coherent view across related records | A snapshot |
| Independent reads with no consistency requirement | Concurrent bounded reads |

Every query is ordered and limited by FeltDB. Compute never reads a scope
to sort it or keep its first few.

### Indexes

FeltDB 0.11.8 answers a query from an index only for the first equality of
a conjunction, and only on a field the model declares an index on. It does
not index `_id`, `In`, ranges, or orderings. So:

- **Identity.** Every Compute record carries its identity as the indexed
  field `record_id` (model generation 2). `get(id)` is an equality on
  `record_id`: one index probe instead of a scan of the collection. The
  adapter writes it, strips it on read, and `compute control-plane upgrade`
  gives it to records written before it existed.
- **Planning.** The adapter puts an indexed equality first, looks identity
  up through `record_id`, and turns `In` over an indexed field into one
  indexed equality per value, merged with the same ordering and limit.
- **Views.** `Execution.project_id`, `Receipt.project_id`,
  `Event.environment`, `Event.project`, and `Event.deployment_id` are indexed so that the views that filter by
  them are.

What FeltDB executed is counted: queries answered by index and by scan,
rows scanned and returned, and the *shape* (collection, fields, ordering;
never values) of every query answered by scanning. `compute doctor` shows
them. A controller test fails when a controller path scans anything but the
shapes listed below.

### What still scans, and why

| Shape | Why | Bound |
| --- | --- | --- |
| The whole-collection sources of the desired-state snapshot (`Environment`, `Project`, `EnvironmentProject`, `Workload`, `WorkloadInstance`, `TrafficAssignment`, `Domain`, `DnsRecord`, `Certificate`) | The controller converges every record of desired state; it needs them all | What the control plane runs, never history. Read only when the revision moved |
| `WorkloadStatus` (the observed-state snapshot) | One status per workload, all compared each cycle | Workloads the control plane has run. Read only when the revision moved |
| `OperatorCredential` | The node's credential cache | Operators. At start, after recovery, and every 30 s |
| `Event` newest by `sequence` | Continuing the event sequence at start and after an outage | FeltDB 0.11.8 has no ordered index access; FeltDB scans `Event` server-side and returns one record |
| Unscoped operator listings (`/deployments` with no environment or project, `/audit` with no operator, `/events` with no filter) | Operator views across everything | FeltDB orders and limits server-side; not on a controller path |

## Snapshots

`db.snapshot({ name, sources })` is FeltDB 0.11.8's runtime-snapshot
contract. It is a JavaScript API; Compute is a Rust client of the Service
API, so `compute_state::snapshot` implements the same contract over the same
public primitives:

- **Explicit and bounded.** A definition names its sources (a collection
  and optional filters) and its references (records named by a field of a
  source, read by identity). Nothing else is read.
- **Coherent.** The revision is read before and after the materialization.
  Unchanged: the snapshot is one point in time (`coherence: proven`).
  Moved: the work is discarded and retried (three attempts).
- **Reused.** When the revision still equals the active snapshot's, a
  refresh reads nothing but the revision.
- **Immutable, replaced atomically**, and never written to.
- **Identity.** `snap_` + SHA-256 over the definition digest, the authority
  it was read under, and the revision (or, without one, the content). The
  same definition under the same name resolves the same handle.
- **Authorized.** It reads with Compute's own FeltDB key; a key without
  `state:read` cannot build one.

The controller keeps two:

| Snapshot | Sources | When coherence cannot be proven |
| --- | --- | --- |
| `compute.controller.desired` | the desired-state collections above; releases in flight (an indexed equality per status); deployments and revisions they name, by identity (revisions are immutable, so known ones are reused) | Used, marked `unproven`, never reused; the next cycle reads again |
| `compute.controller.observed` | `WorkloadStatus` | Same |

A cycle in which nothing was written reads two revisions and nothing else.

### Carrying the working copy forward

The controller tracks the revision its working copy of desired state
provably represents. FeltDB states, for every committed transaction, the
committed revision immediately before and after it (read inside its commit
critical section), and the state store records that, with the collections
the commit wrote, for every commit made through it.

Before each refresh the controller reads back, by identity, the desired
state it wrote itself, then walks the recorded commits from its working
copy's revision:

- a commit that begins exactly where the working copy stands, and either
  wrote no desired-state collection (evidence, events, artifacts, statuses)
  or is one of the controller's own changes (whose writes were just read
  back), moves the working copy to the revision it ended at;
- anything else (a gap: a commit this process never saw; or a commit to
  desired state that was not the controller's own, even through the same
  store object) makes the working copy's revision unknown.

A refresh then reads the revision once: equal to the working copy's means
no other writer changed anything, and nothing else is read; otherwise the
snapshot is rebuilt. A commit and the queueing of its writes for read-back
happen under one gate, so a commit is never chained before its writes are
read back. `compute doctor` reports the working copy's revision, how often
a refresh was answered by it, and how many commits were carried forward.

## Controller working state

The controller keeps working state in memory. None of it is authority.

| State | Source of truth | Purpose | Invalidated by | Maximum staleness | After an outage |
| --- | --- | --- | --- | --- | --- |
| Desired state (`Desired`) | The desired-state snapshot, carried forward by this controller's own commits | Converging the node | Another writer's commit (the revision no longer chains; next periodic refresh), and this controller's own writes (read back by identity) | One reconcile interval (5 s default) for another writer's change; none for this controller's | Rebuilt from FeltDB before anything acts on it |
| The read cache | `Desired` | Serving reads without a round trip | Every write this controller makes (the cache generation), and age (`read_cache`, 1 s) | 1 s, and `X-Compute-State: cached` says so | Reads say `stale`, with the time of the last durable read |
| Observed statuses | The observed-state snapshot | Writing back only what changed | A revision change | One cycle | Rebuilt |
| Recently finished executions (4,096) | `Execution` records | Idempotent terminalization; serving an execution this controller just ran | Capacity | — (its own evidence) | Served marked `stale` when durable state cannot confirm it |
| Evidence and audit held during an outage | Written to FeltDB once it answers | Nothing is lost while FeltDB is down | Written, in order, before mutations resume | Until FeltDB answers | Flushed during recovery |
| Operator credential verifiers | `OperatorCredential` | Authenticating without a round trip | Reload every 30 s and after recovery | 30 s | Reloaded |

Rules:

- The controller never acts on a cache it cannot confirm: a cycle that
  cannot read durable state changes nothing.
- A read served without FeltDB says so (`X-Compute-State: stale`,
  `X-Compute-State-As-Of`). A read with no earlier durable read fails with
  `state_unavailable` rather than inventing state.
- Nothing in memory is written to local disk as a substitute for FeltDB.

### Targeted refresh and periodic reconciliation

| | Reads | For |
| --- | --- | --- |
| **Targeted refresh** | Exactly the records this controller just wrote, by identity, and the deployments and revisions they now name | The controller's own writes: read-your-writes without reloading the universe |
| **Periodic reconciliation** | One revision read for desired state (compared with the working copy's carried-forward revision) and one for observed state; a rebuild only when another writer moved it | Other writers and drift: a change made directly in FeltDB takes effect within one cycle |

## Writing

- A change and the events that record it commit in one FeltDB transaction,
  fenced on the versions read. A lost race is a `conflict`, never a silent
  overwrite.
- A replace is a fenced delete and an insert in one transaction.
- Compute reports success only after FeltDB committed.
- A mutation is never followed by a full reload: the targeted refresh reads
  back what it wrote.

## Availability

| State (`/info` → `control_plane.authority.state`) | Meaning | Reads | Mutations |
| --- | --- | --- | --- |
| `healthy` | FeltDB answers | Live or cached (labelled) | Accepted |
| `degraded_control_plane` | FeltDB is unreachable; the controller read it before | The last snapshot, labelled `stale` | `503 state_unavailable` |
| `state_unavailable` | FeltDB is unreachable and the controller never read it (started during the outage) | `503 state_unavailable` | `503 state_unavailable` |
| `recovered` | FeltDB answers again; the recovery sequence has not finished | — | Wait for recovery (it runs first) |

Workloads keep running and serving in every state: FeltDB availability is
never a requirement for workloads that already run.

Recovery, in order, before a mutation is accepted:

1. reconnect;
2. re-establish current durable state (the snapshots are rebuilt: nothing
   derived before the outage is reused, whatever its revision);
3. drop derived and cached state;
4. continue the event sequence from durable state and reload credentials;
5. write the evidence and audit held during the outage;
6. record `feltdb.recovered`;
7. reconcile (in the same cycle, or the one a mutation wakes).

## The model and upgrades

The model is `compute.flow` (`compute.state@1`), at **generation 2**
(`compute_state::MODEL_GENERATION`). Each generation only adds collections,
optional fields, and indexes.

| Active model vs. this build | Controller start | `compute control-plane upgrade` |
| --- | --- | --- |
| Current | Starts | No change (verified and smoke-tested) |
| Older | **Refuses**: FeltDB rejects fields it does not know, so this build cannot write to it | Upgrades |
| Newer (additive) | Starts: its writes stay valid | **Refuses**: a downgrade would discard what the newer model holds |
| Divergent | Refuses | Refuses |

```sh
compute control-plane inspect --config compute.toml            # read-only
compute control-plane upgrade --config compute.toml --dry-run
# Back up with FeltDB, then upgrade: the backup is verified again first.
feltdb-server backup create /backups/compute-$(date +%F) --data … --keys … --audit …
compute control-plane upgrade --config compute.toml \
  --backup-archive /backups/compute-2026-09-25 --feltdb-server-bin /usr/local/bin/feltdb-server
```

The upgrade:

1. inspects the active model;
2. inspects the model this build requires;
3. refuses an unsafe downgrade (a newer or divergent model);
4. secures a backup through FeltDB: an archive from `feltdb-server backup
   create` (`--backup-archive`), or an online backup (`--backup`, with a
   `cluster:write` key in `COMPUTE_FELTDB_BACKUP_TOKEN`), and verifies it
   with `feltdb-server backup verify`. An unverifiable backup stops the
   upgrade. `--skip-backup REASON` is recorded as skipped, never as passed;
5. applies the model (draft, validate, commit, promote);
6. verifies the active model is now this build's, and gives older records
   their indexed identity;
7. smoke-tests: every collection reads and decodes, the revision reads, and
   a transaction commits and is undone;
8. reports every step (`--json` for the machine-readable report).

Rolling a controller upgrade across a model change: run
`compute control-plane upgrade` first, then upgrade every controller of
the control plane. A controller of the previous generation keeps working on
the new model while it runs; the new controller gives any record it wrote
without `record_id` its identity when it starts.

## Backup and restore

FeltDB owns backup and restore. Compute has no export or backup format of
its own, and no emergency copy of state: `compute control-plane upgrade`
only orchestrates FeltDB's backup and refuses to continue without a
verified one. To restore, restore FeltDB (`feltdb-server backup restore`)
and point Compute at it; the certification checks record-by-record
equivalence of executions, events, and the audit trail, and that indexes
answer after a restore.

## FeltDB 0.11.8 behaviour Compute depends on or works around

| Behaviour | Effect on Compute |
| --- | --- |
| Each request costs time proportional to the committed state, even one that reads no record (`/v1/state/version`) | Compute minimizes requests: a quiet cycle makes two; its own writes are carried forward instead of re-read ([certification](feltdb-0.11.8-consumer-certification.md#feltdbs-own-cost-per-request)) |
| Only the first equality on a declared field uses an index | The indexed `record_id`, the adapter's planning, the view indexes |
| No ordered index access | "Newest event" and unscoped operator listings scan server-side (the table above) |
| `datetime` values compare as strings | Fields Compute orders by are written with a fixed nine-digit fraction |
| Unknown fields are rejected | A controller refuses an older model; `compute control-plane upgrade` first |
| `backup verify` rejects an online backup of application state ("snapshot contains an invalid state row") | Use an offline archive (`--backup-archive`); an online backup stops the upgrade before it changes anything |
| A restored archive names its sidecars `applications.json` and `platform.json`, while a server on `files/state.log` reads `state.applications.json` and `state.platform.json` | Rename them before serving a restore |
| `/health` reports the server crate version (`0.2.0`) and a capability fingerprint, both identical for the 0.11.7 and 0.11.8 servers | Compute cannot tell from the wire which release a server is; `compute doctor` reports the server version FeltDB gives and the certified client version, not a verified server release |
