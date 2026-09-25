# Compute as a FeltDB 0.11.8 consumer: certification

Certified 2026-09-25 at Compute **`99afae5`** (branch `claude/funny-allen-96cx46`),
against **`@feltdb/core` 0.11.8**, Compute model **`compute.state@1`
generation 2**. Machine-readable: [feltdb-0.11.8-consumer-certification.json](feltdb-0.11.8-consumer-certification.json).
Evidence: [feltdb-0.11.8-evidence/](feltdb-0.11.8-evidence/). The contract
certified here is [feltdb.md](feltdb.md).

**Production readiness is not claimed.** Everything below ran on one host,
against a `feltdb-server` on the same host, never against Managed FeltDB over
a network.

## How to read this

| Status | Meaning |
| --- | --- |
| **PASS** | A test or check exercised it against the stated build and it held |
| **MEASURED** | A harness produced the number on the host below |
| **SUPPORTED** | Implemented and unit-tested, not exercised end to end here |
| **NOT_RUN** | Not exercised: the infrastructure was not available |
| **UNKNOWN** | Not determinable from the evidence |
| **FAIL** | Exercised, and it did not hold |

## Environment

| | |
| --- | --- |
| Compute | `99afae5` (tests and benchmark); before: `eac2be8` |
| FeltDB | `@feltdb/core` 0.11.8, resolved in the lockfile and `node_modules` (`scripts/feltdb/verify-version.mjs`: PASS) |
| `feltdb-server` | Built from the source shipped inside the 0.11.8 package (reports `0.2.0`); previous: built from 0.11.7 (also reports `0.2.0`) |
| Model | `compute.state@1`, generation 2 |
| OS | Linux 6.18.44 |
| CPU | Intel Xeon @ 2.10 GHz, 4 vCPUs |
| Memory | 16 GB |
| FeltDB transport | HTTP over loopback |
| Builds | Tests: debug (`CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0`). Benchmarks: release |

## Commands

```sh
# Build the servers from the packages (CI does the same from the resolved package):
npm pack @feltdb/core@0.11.8 && tar xzf feltdb-core-0.11.8.tgz
cargo build --release --manifest-path package/dist/create/server-source/Cargo.toml -p feltdb-server

# Everything below, with logs, into docs/feltdb-0.11.8-evidence:
FELTDB_SERVER_BIN=… FELTDB_PREVIOUS_SERVER_BIN=… COMPUTE_CERTIFICATION_OUT=docs/feltdb-0.11.8-evidence \
  scripts/feltdb/certify.sh
python3 scripts/feltdb/report.py 99afae5
```

`certify.sh` runs:

| Check | Command | Status |
| --- | --- | --- |
| Resolved version | `node scripts/feltdb/verify-version.mjs --json` | **PASS** |
| Model compiles | `npm run check && npm test` in `packages/compute-state-model` | **PASS** |
| Workspace | `cargo test --workspace --no-fail-fast`: 239 passed, 0 failed, 17 ignored (the real-server and benchmark tests, run below) | **PASS** |
| Adapter on a real server | `cargo test -p compute-state-feltdb -- --include-ignored --test-threads 1 …`: 12 tests | **PASS** |
| Previous server | `cargo test -p compute-state-feltdb --test consumer state_written_by_the_previous_server -- --ignored` | **PASS** |
| Controller on a real server | `cargo test -p compute-environment --test feltdb_consumer -- --ignored --test-threads 1 --skip benchmark` | **PASS** |
| CLI end to end | `cargo test -p compute-cli --test recovery managed_feltdb -- --ignored` | **PASS** |
| FeltDB request cost | `cargo test --release -p compute-state-feltdb --test consumer feltdb_request_cost -- --ignored` | **MEASURED** |
| Benchmark | `cargo test --release -p compute-environment --test feltdb_consumer benchmark -- --ignored` | **MEASURED** |
| Benchmark, before | The same benchmark on `eac2be8` ([before_bench.rs.txt](feltdb-0.11.8-evidence/before_bench.rs.txt)) | **MEASURED** |

## Results

### Version

| | Status |
| --- | --- |
| Compute resolves exactly `@feltdb/core` 0.11.8; no other version in any lockfile | **PASS** |
| CI verifies the resolved version, builds the server from the resolved package, and runs the real-server suites (`.github/workflows/feltdb-consumer.yml`) | **SUPPORTED** (the workflow was not run here) |
| Remaining `0.11.7` strings: `docs/platform-audit*` (a dated historical audit), and `v0.11.7`/`v0.11.8` in `deploy/hetzner/README.md` and `compute-ui-e2e` (revision labels of an example project named `feltdb`, not dependency versions) | Left as is |

### Access paths

The audit of every FeltDB access path, with its old and new pattern and its
test, is [access-inventory.json](feltdb-0.11.8-evidence/access-inventory.json)
(37 paths).

| | Status |
| --- | --- |
| An identity lookup is one index probe: 1 row examined among 500 (before: every row) | **PASS** |
| A project's newest executions examine only that project's rows, ordered and limited by FeltDB | **PASS** |
| `get_many` never scans | **PASS** |
| A quiet reconcile cycle runs no queries: two revision reads | **PASS** |
| Controller paths make FeltDB scan only the documented shapes (the test fails on any other) | **PASS** |
| After its own change, the next cycle does not re-read desired state; a change by another writer (another process, or another client of the same store object) is read within one cycle | **PASS** |

### Snapshots

| | Status |
| --- | --- |
| Coherent under a concurrent writer: never half of a two-record transaction (memory, file, real FeltDB) | **PASS** |
| Bounded to its sources; reused while the revision is unchanged (one revision read); stale after any write | **PASS** |
| Deterministic identity: two handles over the same state derive the same `snap_…` | **PASS** |
| A key without `state:read` cannot build one | **PASS** |
| A failed build keeps the published snapshot | **PASS** |

### Authority, outage, and recovery

| | Status |
| --- | --- |
| healthy → outage → read (served, labelled `stale`) → mutation (`state_unavailable`) → recovery (snapshots rebuilt, `feltdb.recovered`) → mutation; nothing written during the outage | **PASS** |
| An execution this controller ran is served from its own evidence during an outage, labelled stale; an unknown one is not fabricated | **PASS** |
| The CLI controller: degraded start, refused changes, recovery, restore on a fresh node | **PASS** |
| `/info` and `compute doctor` report certified and server versions, model generation, authority state, last durable read and mutation, last recovery, cache generation and freshness, pending work, snapshots, and query plans | **PASS** (asserted through `AuthorityView`) |

### Concurrency

| | Status |
| --- | --- |
| 40 concurrent executions on FeltDB: every record, receipt, and event; unique event sequences | **PASS** |
| 1,000 concurrent executions lose no evidence (memory backend) | **PASS** |

### Model and upgrade

| | Status |
| --- | --- |
| A controller refuses to start on an older model, naming `compute control-plane upgrade` | **PASS** |
| `compute control-plane upgrade` from generation 1 with state: all 9 steps pass ([upgrade-report.json](feltdb-0.11.8-evidence/upgrade-report.json)); records written without `record_id` are found through the index afterwards; a second run is a no-op | **PASS** |
| An unverifiable backup stops the upgrade before anything changes | **PASS** |
| A newer model is never downgraded (upgrade and provision refuse) | **PASS** |
| New fields are additive (`record_id` optional); missing optional fields decode with defaults (a generation-1 `Deployment` without every later optional field) | **PASS** |
| State written by the 0.11.7 server opens unchanged on 0.11.8 and conforms; the 0.11.7 server opens what 0.11.8 wrote | **PASS** |
| Rolling back a Compute model upgrade (re-promoting the previous FeltDB revision) | **NOT_RUN**: no Compute command exists for it |

### Backup and restore

| | Status |
| --- | --- |
| FeltDB offline backup: create → verify → restore → serve; record-by-record equivalence of 51 records (30 executions, 20 events, 1 audit record); indexes answer after restore ([backup-restore.json](feltdb-0.11.8-evidence/backup-restore.json)) | **PASS** |
| FeltDB online backup (`POST /admin/backups`) of application state verifies | **FAIL**: `backup verify` reports "snapshot contains an invalid state row" (FeltDB 0.11.8) |
| Compute has no backup, export, or restore format of its own | **PASS** (audit: none found) |

## Performance

Same host, same `feltdb-server`, same dataset: 40 task projects, then 10
more, and 4,000 executions. Release builds, 30 samples (10 for the heavy
operations). "Before" is Compute `eac2be8` running the same operations. Raw:
[feltdb-consumer-benchmark.json](feltdb-0.11.8-evidence/feltdb-consumer-benchmark.json),
[feltdb-consumer-benchmark-before.json](feltdb-0.11.8-evidence/feltdb-consumer-benchmark-before.json).

**Scopes.** *FeltDB-only*: the adapter's requests to FeltDB and back.
*End-to-end*: a controller operation, Compute's work plus its FeltDB
requests. Compute-only time was not isolated.

### End to end (controller)

| Operation | Before p50 / p95 (ms) | After p50 / p95 (ms) | FeltDB per call (after) | Status |
| --- | --- | --- | --- | --- |
| Reconciliation, nothing changed | 4,091 / 4,397 | **168 / 218** | 0 queries, 2 revision reads | **MEASURED** |
| Environment create (mutation + event) | 4,182 / 4,328 | **686 / 803** | 2 queries, 1 transaction | **MEASURED** |
| Project stop/start | 4,121 / 4,324 | **697 / 787** | 2 queries, 1 transaction | **MEASURED** |
| Executions view (a project's 20) | 3,997 / 4,113 | **201 / 222** | 1 query (100 rows examined) | **MEASURED** |
| Release (register, deploy, complete) | 5,911 / 6,391 | **3,289 / 3,624** | 15 queries, 5 transactions | **MEASURED** |
| Add project/service | 9,718 / 11,421 | **5,497 / 6,057** | 25 queries, 12 transactions | **MEASURED** |
| Environment view | 0.32 / 0.40 | 0.31 / 0.36 | 0 (working copy) | **MEASURED** |
| Project view | 0.01 / 0.01 | 0.01 / 0.01 | 0 | **MEASURED** |
| Execution lookup | 102 / 116 | 135 / 158 | 1 query (1 row) | **MEASURED**, slower; cause **UNKNOWN** |
| Release lookup | 195 / 206 | 270 / 295 | 2 queries | **MEASURED**, slower; cause **UNKNOWN** |
| Controller start, empty | 130 | 270 | – | **MEASURED** |
| Controller start, with this state | 14,316 | 22,599 | – | **MEASURED**, slower (see gaps) |

### FeltDB-only (access patterns)

| Operation | Pattern | p50 / p95 (ms) | Rows examined per call | Status |
| --- | --- | --- | --- | --- |
| One record by identity | before: `_id` filter | 153 / 182 | the collection | **MEASURED** |
| | after: indexed `record_id` | 135 / 168 | 1 | **MEASURED** |
| A project's 20 newest executions | before: whole collection, filtered in Compute | 842 / 868 | 16,000 (4 pages) | **MEASURED** |
| | after: indexed, ordered and limited by FeltDB | 137 / 151 | 100 | **MEASURED** |
| Desired-state load | before: independent whole reads and `_id` gets | 9,221 / 9,933 | – | **MEASURED** |
| | after: coherent snapshot, built | 5,524 / 5,855 | 203 | **MEASURED** |
| | after: coherent snapshot, unchanged (reused) | 72 / 101 | 0 (one revision read) | **MEASURED** |
| Small bounded snapshot (2 records) | built | 329 / 354 | 2 | **MEASURED** |
| Large bounded snapshot (4,000 executions) | built | 999 / 1,050 | 16,000 | **MEASURED** |
| One durable mutation | | 157 / 202 | – | **MEASURED** |

### FeltDB's own cost per request

[feltdb-request-cost.json](feltdb-0.11.8-evidence/feltdb-request-cost.json):
the same requests against the same server as its committed state grows
(FeltDB-only, p50 ms).

| Committed operations | `/health` | `/v1/state/version` | Index probe, no match | FeltDB-reported execution of that probe |
| --- | --- | --- | --- | --- |
| 0 | 0.51 | 1.18 | 1.53 | 0.05 |
| 1,000 | 0.49 | 9.19 | 18.2 | 7.0 |
| 2,000 | 0.56 | 18.9 | 40.4 | 14.9 |
| 4,000 | 0.65 | 41.8 | 81.0 | 32.3 |

A request that reads no record at all (`/v1/state/version`) grows linearly
with committed state, and so does an index probe that examines one row.
`/health` does not. This is inside FeltDB's request handling; Compute's lever
is how many requests it makes, which is where the wins above come from.
Absolute latencies in this report are dominated by it.

### Thresholds

The tests assert architectural invariants, not milliseconds: rows examined
per identity lookup (1), queries in a quiet cycle (0), revision reads in a
quiet cycle (2), no desired-state rebuild after the controller's own change,
and the allowed set of scanned query shapes.

## Remaining gaps

| Gap | Status | Attribution |
| --- | --- | --- |
| FeltDB's per-request cost grows linearly with committed state (table above) | **MEASURED** | FeltDB; exact cause inside its request handling not isolated |
| Controller start with state: 22.6 s vs 14.3 s | **MEASURED** | Partly attributed: start now runs the identity-backfill probe (21 requests, each scanning a collection for records without `record_id`) and builds the desired snapshot from ~68 identity probes, each paying FeltDB's per-request cost; the split was not measured |
| Execution lookup and release lookup slower with the same number of requests | **UNKNOWN** | Not isolated; both are FeltDB-request bound |
| Online backup of application state does not verify | **FAIL** | FeltDB 0.11.8 (`export_snapshot` emits rows `install_snapshot` rejects). The upgrade's `--backup` stops before changing anything; `--backup-archive` (offline) works |
| A restored archive's sidecars must be renamed (`applications.json` → `state.applications.json`, `platform.json` → `state.platform.json`) before serving | **MEASURED** | FeltDB 0.11.8 |
| 0.11.7 and 0.11.8 servers are indistinguishable on the wire (same version and capability fingerprint) | **UNKNOWN** | FeltDB |
| No ordered index access: the newest event and unscoped operator listings scan server-side (returning only the limit) | **SUPPORTED** | FeltDB planner |
| Placement never re-discovers expired provider capabilities, so a long-running controller eventually cannot place new workloads (also on `eac2be8`) | **FAIL** | Compute placement, pre-existing, outside this change; reported, not fixed |
| Managed FeltDB over a network; half-open partitions; more than one controller per control plane | **NOT_RUN** | – |
| Rolling back a model upgrade | **NOT_RUN** | No command |
| The CI workflow | **SUPPORTED** | Not run here |
