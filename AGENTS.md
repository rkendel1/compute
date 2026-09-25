# Working on Compute

Compute is a Rust workspace; Cargo is authoritative (`cargo build`,
`cargo test --workspace`). See [DEVELOPMENT.md](DEVELOPMENT.md) and
[docs/architecture.md](docs/architecture.md). Compute owns execution;
FeltDB owns durable state. Neither becomes the other.

## Durable state: FeltDB or memory?

The full contract is [docs/feltdb.md](docs/feltdb.md).

**Use FeltDB (through `compute-state`) when state**

- must survive a restart;
- is authoritative (desired state, evidence);
- must be audited;
- takes part in a durable transaction (a change and its events commit
  together);
- must be agreed on by every controller and restart.

**Use Compute memory when data**

- is ephemeral, or can be rebuilt from FeltDB;
- is a working cache with an explicit invalidation rule and a stated
  maximum staleness (add it to the working-state table in
  `docs/feltdb.md`).

**Read with**

- an identity lookup (`get`, `get_many`) or an indexed equality, ordered
  and limited by FeltDB (`Query::…eq(…).descending(…).limit(n)`), when the
  controller needs one or a few records. Filter on a field `compute.flow`
  indexes; if none fits, add an index there (an additive model change);
- a snapshot (`ControlState::snapshot`) when it needs a coherent, bounded
  view across sources.

**Never**

- scan a collection to find a record, or read a scope to sort or truncate
  it in Compute;
- keep a second durable database, a local fallback, or a Compute backup
  format;
- serve stale state as current: label it (`stale`, with its as-of time) or
  fail with `state_unavailable`;
- add a Compute-specific API to FeltDB;
- make FeltDB availability a requirement for workloads that already run.

`compute-environment/tests/feltdb_consumer.rs` fails when a controller path
makes FeltDB scan anything but the listed shapes; run it (and
`compute-state-feltdb`'s ignored tests) with `FELTDB_SERVER_BIN` set when
you touch state access. A model change needs `compute.flow`, the
regenerated manifest (`npm run generate` in `packages/compute-state-model`),
and a `MODEL_GENERATION` bump.
