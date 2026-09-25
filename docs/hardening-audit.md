# Compute production substrate hardening: audit

Audited 2026-09-25 at Compute `f68ce7f` on branch `claude/quirky-keller-s208we`. This follows the platform audit ([platform-audit.md](platform-audit.md)), whose numbers are the baseline here. It records what this change set exercised and what it measured. It does not record what is intended.

The raw evidence is in [`hardening-evidence/`](hardening-evidence/). The harnesses that produced it are in [`scripts/platform-audit/`](../scripts/platform-audit/) and the named test files. The machine-readable summary is [`hardening-audit.json`](hardening-audit.json).

## How to read this

Every row carries exactly one status.

| Status | Meaning |
|---|---|
| **verified** | A test or drill exercised the behaviour and it held. The evidence is linked. |
| **measured** | A harness produced the number on the host below. |
| **supported** | Implemented and covered by unit or integration tests, but not exercised end to end in the setting where it matters (for example, under systemd, or on a remote host). |
| **unsupported** | Not implemented, or refused by design on this host. Stated as such. |
| **unknown** | Not determinable from the evidence available. |

**Host.** Every number comes from a single 4 vCPU, 16 GB Linux VM (Intel Xeon 2.1 GHz, kernel 6.18), running as root, with cgroup v1 and Landlock ABI 7. FeltDB is a `feltdb-server` on the same host.

**Builds.** Harness numbers use a release build. Test-suite numbers use a debug build, and each table says which.

**Production readiness.** Nothing here claims it. Every behaviour below was exercised on one host. None was exercised on a production deployment, against Managed FeltDB, or under systemd.

### Evidence

| File | Produced by | Contents |
|---|---|---|
| [`compute-measurements.json`](hardening-evidence/compute-measurements.json) | `measure_compute.py --only runtimes,capacity` (release, `f68ce7f`) | Runtime latency, 300-service capacity |
| [`feltdb-latency.json`](hardening-evidence/feltdb-latency.json) | `feltdb_latency.py` (release, `f68ce7f`) | Control-plane reads and mutations on a FeltDB backend |
| [`feltdb-latency-uncached.json`](hardening-evidence/feltdb-latency-uncached.json) | `feltdb_latency.py` (release, `8daf442`) | Reads with parallel loading and no read cache |
| [`baseline-feltdb-latency.json`](hardening-evidence/baseline-feltdb-latency.json) | `feltdb_latency.py` (release, `8daf442`) | The same operations before FeltDB access was optimized |
| [`control-plane-drills.json`](hardening-evidence/control-plane-drills.json) | `control_plane_drills.py` (release, `6bca1ec` plus the degraded-start change in `28c8231`) | FeltDB outage and recovery, controller SIGKILL, start without FeltDB, fresh-node restore, remote operation |
| Workspace tests | `cargo test --workspace --no-fail-fast`, then `-- --ignored` against a real `feltdb-server` (debug) | 57 test binaries: 238 passed, 0 failed; the 5 FeltDB-backed tests pass |
| [`test-evidence.txt`](hardening-evidence/test-evidence.txt) | `cargo test … -- --nocapture` (debug, `f68ce7f`) | Concurrency, security, availability, scale, upgrade, isolation and recovery tests, with their printed measurements |

---

## 1. Results against the requested measurements

| Area | Before (platform audit) | Now | Status |
|---|---|---|---|
| Concurrent execution of one task | 1 of 80 calls succeeded. 79 records and receipts were lost. | 80/80 records and receipts; 1,000/1,000 | **verified** |
| Restart availability | Endpoints died with the daemon; 4.7 s to serve again, on new processes | 0 failed of 398 requests across a controller SIGKILL (debug); in the drill, the endpoint answered throughout from the same process | **verified** |
| FeltDB outage | Services kept serving; mutations returned 503 | Unchanged. Reads now report freshness. | **verified** |
| FeltDB recovery | Automatic | Automatic, with `feltdb.recovered` and a full reconciliation | **verified** |
| Start while FeltDB is down | Refused (exit 1); a rebooted node served nothing | Starts in `degraded_control_plane` in 169 ms and converges when FeltDB returns. `--require-state-at-start` keeps the refusal. | **verified** |
| Upgrade | Did not exist | 5.4 s with 0 of 849 requests failed; workloads kept their processes (debug) | **verified** |
| Rollback | Did not exist | 3.4 s. A failing, hanging, or digest-mismatched build rolls back automatically. | **verified** |
| 300-service reconciliation | Environment view p95 103 ms; add about 0.9 s; shutdown > 30 s | Environment view p95 37.6 ms; add p50 476 ms including process start and readiness; shutdown 0.32 s | **measured** |
| WASM latency | p50 81.6 ms | p50 8.2 ms, p95 9.1 ms | **measured** |
| Remote API authentication | Reads open; one shared token for changes | Every request, reads included, needs a scoped operator credential in production | **verified** |
| TLS | None | Served from a certificate and key, and reloaded without a restart. Production refuses plaintext. | **verified** |
| Credential rotation | None | Create, expire, revoke, and rotate (with optional grace), all audited | **verified** |

## 2. Execution correctness

Evidence: `executions.rs` and the `daemon::execute` unit tests (`test-evidence.txt`).

| Behaviour | Result | Status |
|---|---|---|
| 8 clients × 10 runs of one task | 80 records and 80 distinct receipts | **verified** |
| 1,000 concurrent runs | 1,000 records and receipts, none lost | **verified** |
| Duplicate finish of one execution | The second is a no-op (bounded terminal log of 4,096 entries) | **verified** (unit) |
| Evidence persisted twice | Recorded once | **verified** (unit) |
| An execution that ends while the controller is down | Recorded, with its receipt, after the next controller reattaches | **verified** (`recovery.rs`) |
| Unknown execution | Recovered from durable state before `not_found` is returned | **supported** |
| A failing run | Recorded as the workload's failure (`failure: workload_failed`), with its receipt. Never `admission_denied`. | **verified** |

## 3. Secure remote API

Evidence: `security.rs` (5 tests), the `auth` unit tests, and the drills' `remote_operation` section.

| Behaviour | Status |
|---|---|
| Production mode requires TLS; without a certificate the controller refuses to start | **verified** |
| An unauthenticated read in production returns 401 `authentication_failed` (drill: 401) | **verified** |
| Invalid, expired, and revoked credentials are refused | **verified** |
| Insufficient scope returns 403 `authorization_denied`; a valid scoped operation is accepted | **verified** |
| Every route declares a scope, and unknown routes need `compute.admin` | **verified** (unit) |
| Rotation: the new token works; the old one stops working, immediately or after the grace period | **verified** |
| Every mutation, and every refused request, has an audit record with operator, credential, request ID, operation, and outcome | **verified** |
| Tokens never appear in the audit trail, events, errors, the credential snapshot, or later API responses; only SHA-256 verifiers are stored | **verified** (the tests search responses, events, audit, and the snapshot for the secret) |
| The certificate reloads without a restart | **verified** |
| Plaintext development mode is explicit (`--insecure`, or a loopback listener) and reported in `/info` | **verified** |
| A reachable listener without TLS refuses to start | **verified** (unit) |
| Compute works without AuthBoundry | **verified**: no test uses it |
| Mutual TLS, external identity providers | **unsupported** (not in scope) |
| `compute serve` (remote jobs) under the same credentials and TLS | **unsupported**: it still has its own, older token check |

## 4. Data-plane independence

Evidence: `recovery.rs` (6 tests, debug) and the drills (release).

| Behaviour | Result | Status |
|---|---|---|
| Controller SIGKILL under HTTP load | 398 requests, 0 failed, one serving process throughout; controller back in 2.6 s (debug) | **verified** |
| Controller SIGTERM (a service-manager restart) and `compute stop --keep-workloads` | Workloads keep serving and are reattached, not restarted | **verified** |
| Endpoint during a controller outage (drill, release) | Answered from the same process (`v1 pid=5805`) while the controller was down; controller back in 1.05 s | **verified** |
| Supervisor killed | A new supervisor starts, stops what the old one left (never a second copy), and restarts the lost workloads regardless of restart policy | **verified** |
| Orphan detection and cleanup | Verified by the lost-supervisor test | **verified** |
| Workload crash | Restarted with backoff by its restart policy | **verified** |
| The supervisor manifest holds no plaintext secrets | Units record identities, ports, and policy; the environment passes only to the child process | **supported** (by construction; no test scans the manifest) |
| Ingress (ports 80 and 443, domains) during a controller outage | Pauses: ingress still runs in the controller | **unsupported** (known gap) |
| Upgrading the supervisor itself without restarting workloads | Not possible; the supervisor protocol must match | **unsupported** |

## 5. FeltDB degraded mode

Evidence: `availability.rs` (3 tests), `recovery.rs::managed_feltdb_is_the_durable_authority` (real `feltdb-server`), and the drills.

| Behaviour | Result | Status |
|---|---|---|
| FeltDB killed while workloads run | The service kept answering; the controller stayed up | **verified** |
| Mutation during the outage | 503 `state_unavailable`; nothing written locally | **verified** |
| Reads during the outage | Served from the last snapshot with `X-Compute-State: stale` and `X-Compute-State-As-Of` | **verified** |
| Start during the outage | `degraded_control_plane`; changes refused with 503 | **verified** |
| Recovery | FeltDB was back in 84 ms; mutations resumed without operator action; production restored on the degraded node | **verified** |
| Reconciliation after recovery | Full cycle; `feltdb.recovered` recorded | **verified** |
| No duplicate durable records after recovery | Evidence written while down is flushed once, with renumbered events | **verified** (unit and availability tests) |
| A second durable database | None was added. The node keeps only caches and its local manifest. | **verified** by design review |
| A network partition with half-open sockets | Not tested | **unknown** |

## 6. FeltDB access

> **Superseded for FeltDB consumption** by the FeltDB 0.11.8 consumer
> certification ([feltdb-0.11.8-consumer-certification.md](feltdb-0.11.8-consumer-certification.md)),
> summarized in [section 14](#14-feltdb-0118-consumption). The numbers below
> are this audit's, at `f68ce7f`, on a smaller dataset; they are kept as
> the historical baseline.

Evidence: `feltdb-latency.json`, with the baselines. Release build, 40 samples per operation, local `feltdb-server`.

| Operation | Before p50 / p95 (ms) | Now p50 / p95 (ms) | FeltDB requests per call | Target | Status |
|---|---|---|---|---|---|
| Environment list | 212 / 256 | 0.31 / 0.81 | 0.8 | read p95 ≤ 100 ms | **measured**, meets |
| Environment inspect | 207 / 227 | 0.48 / 0.86 | 0 | ≤ 100 ms | **measured**, meets |
| Project status | 215 / 232 | 0.29 / 0.47 | 0 | ≤ 100 ms | **measured**, meets |
| Projects list | 228 / 269 | 14.0 / 16.0 | 2 | ≤ 100 ms | **measured**, meets |
| Events | 14.6 / 20.9 | 14.9 / 21.2 | 2 | ≤ 100 ms | **measured**, meets |
| An uncached read (parallel load of all desired state) | – | about 85 / 91 | about 19 | ≤ 100 ms | **measured**, meets |
| Environment create | 797 / 951 | 212 / 258 | 44 | mutation p95 ≤ 1 s | **measured**, meets |
| Environment stop | 964 / 1,167 | 303 / 388 | 44 | ≤ 1 s | **measured**, meets |
| Environment start | 1,041 / 1,436 | 404 / 616 | 45 | ≤ 1 s | **measured**, meets |
| Project restart | 2,195 / 3,554 | 1,620 / 2,161 | 150 | ≤ 1 s | **measured**, **misses** |

Project restart includes stopping and starting a real process and waiting for it to be ready. The share spent in FeltDB was not separated from the process restart and is **unknown**. It still issues 150 FeltDB requests.

The read cache (1 s by default) is an optimization, never an authority:

- Every write this controller makes invalidates it.
- A cached read says `X-Compute-State: cached`.
- While FeltDB is unreachable, reads bypass it and are marked `stale`.

Managed FeltDB latency, over a real network, is **unknown**.

## 7. Reconciliation at 300 services

Evidence:

- `compute-measurements.json → capacity`: release build, real Python HTTP services on the supervisor, memory state.
- `scale.rs`: debug build, in-process, 300 projects.

| Measure | Before | Now | Target | Status |
|---|---|---|---|---|
| Environment view p95 at 300 services | 103 ms | 37.6 ms (p50 29.6) | ≤ 50 ms | **measured**, meets |
| Add a service at 300 (end to end: register, deploy, start a process, ready) | about 0.9 s | p50 476 ms, p95 569 ms | ≤ 100 ms | **measured**, misses (see below) |
| Add at 300 (in-process, debug, `scale.rs`) | – | p50 195 ms, p95 238 ms | – | **measured** |
| Full reconcile at 300 (debug, in-process) | – | p50 80–89 ms, dominated by the full refresh | – | **measured** |
| Full reconcile at 300 (release, capacity run, last cycle) | – | 66 ms: refresh 47 ms, observe 17 ms, the rest under 2 ms; 900 resources examined, 0 changed | – | **measured** |
| Graceful shutdown with 300 services | > 30 s | 0.32 s | ≤ 10 s | **measured**, meets |
| Controller RSS at 300 | 265 MB | 92 MB | – | **measured** |
| Controller idle CPU at 300 (1 s reconcile interval) | 9.6% (5 s interval) | 12.8% | – | **measured** |

Changes made through the API reconcile only the records they wrote (targeted reconciliation), and cycles report their duration, resources examined and changed, errors, and per-phase time (`/metrics`, `compute node reconcile`).

**Bottleneck.** The 100 ms add target is missed. In the in-process test, an add spends:

| Phase | Time |
|---|---|
| Deploy (admission, bundle, release creation) | about 145 ms |
| Project view | about 80 ms |
| Register | about 3 ms |
| Release | under 1 ms |

End to end, starting the Python process and waiting for readiness adds the rest. The full-refresh read of desired state (47 ms in release at 300 services) runs on every periodic cycle. It is not on the add path.

The same debug test at commit `3369020` gives the same numbers (194 ms add, 89 ms reconcile), so nothing after that commit regressed it.

## 8. Upgrade and rollback

Evidence: `upgrade.rs` (2 tests, debug builds, a Python service under constant HTTP load).

| Behaviour | Result | Status |
|---|---|---|
| `compute node upgrade` under traffic | 5.36 s; 849 requests, 0 failed; the service kept its PID; workloads reattached | **verified** |
| `compute node rollback` | 3.40 s to the previous build, kept under `<state-dir>/binaries/` | **verified** |
| A build that exits, hangs, or refuses (cannot reattach every workload) | Rolled back automatically (`upgrade.failed`, `upgrade.rolled_back`) | **verified** |
| An artifact whose digest does not match `--sha256` | Refused before hand-over | **verified** |
| An incompatible build (platform, API, supervisor protocol) | Refused before hand-over | **supported** (unit-level checks) |
| Upgrade receipts carry both build identities | `upgrade.json` and the events carry version, commit, build ID, and platform | **verified** |
| Upgrade under systemd | When the old process exits, systemd considers the unit stopped. Use `systemctl restart` (SIGTERM detaches) instead. | **unsupported** (not exercised) |
| Signature verification of artifacts | Digest pinning only | **unsupported** |
| A controller crash during an upgrade | The upgrade record survives in `upgrade.json`; the next controller resumes or rolls back | **supported** |
| A release that includes a control-model change | Run `compute control-plane upgrade` first. This build adds optional `OperatorCredential` and `Audit` collections and an `Execution.failure` field; the previous controller keeps working on the new model. | **supported** |

## 9. Binary identity and WASM

| Behaviour | Result | Status |
|---|---|---|
| `version --json`, `/info`, `compute doctor`, events, and upgrade records report version, git commit, build ID, platform, API, supervisor protocol, and runtimes | Present | **verified** |
| The executable is hashed once per process, with a per-user trusted cache keyed by device, inode, size, and mtime | Not per execution | **verified** (unit: an untrusted cache is ignored and a changed file is rehashed) |
| Receipts remain verifiable | `execution_receipt_is_canonical_verifiable_and_binds_artifacts` passes | **verified** |

`compute run` latency, CLI end to end, release, 20 runs each:

| Runtime | Before p50 (ms) | Now p50 / p95 (ms) | Status |
|---|---|---|---|
| wasm | 81.6 | 8.2 / 9.1 (target ≤ 20) | **measured**, meets |
| native | 80.5 | 9.0 / 10.1 | **measured** |
| shell | 19.0 | 16.6 / 17.8 | **measured** |
| python | 32.8 | 22.7 / 24.8 | **measured** |
| bun | 166.7 | 26.4 / 28.1 | **measured** |
| node | 237.3 | 49.5 / 55.3 | **measured** |
| ruby | 101.4 | 95.0 / 112.2 | **measured** |
| php | 145.0 | 126.5 / 138.7 | **measured** |
| jvm | 167.6 | 171.9 / 188.3 | **measured** |
| deno, dotnet | – | not installed | **unknown** |

Ruby, PHP, and the JVM are dominated by their own start-up.

## 10. Isolation

Evidence: `isolation.rs` (4 tests) and the `host` unit tests. `compute isolation --json` reports the matrix per host.

| Profile, on this host | Filesystem | Network | Memory | CPU | Process | Status |
|---|---|---|---|---|---|---|
| `trusted` | unrestricted | unrestricted | declared only | declared only | unrestricted | **verified** (reported) |
| `restricted` | restricted: writes only its workspace | enforced (network namespace; TCP denied by Landlock for `none`) | declared | declared | restricted (no_new_privs, rlimits) | **verified** |
| `isolated` | enforced: reads only runtime, system libraries, and workspace | enforced | enforced (1 GiB default, cgroup) | enforced (100%, cgroup) | enforced (256, cgroup pids) | **verified** |

Tests exercised:

- A restricted workload cannot write outside its workspace.
- An isolated workload cannot read a secret outside it, nor `/etc/hostname`.
- Neither can open a TCP connection with `network: none`.
- Neither can read `/proc/1/environ`.
- A 64 MiB memory limit stops a 200 MB allocation.
- `localhost` keeps loopback only.
- Enforcement evidence appears in the result and in the receipt.
- A host profile is refused on WASM, and `trusted` refuses `network: none` rather than running with the host's network.

| Item | Status |
|---|---|
| Hosts without Landlock, network namespaces, or the cgroup controllers | Their profiles are reported `unsupported` and refused (`profiles_never_downgrade`). Exercised in unit tests; no such host was available. **supported** |
| Non-Linux hosts | **unsupported** |
| cgroup v2 hosts | Implemented, but not exercised here (this host has v1). **supported** |
| Running Compute as a non-root user | Not exercised: cgroup creation needs delegation, and a host without it reports `isolated` as `unsupported`. **unknown** |
| Containers, VMs, seccomp syscall filtering | **unsupported** |

## 11. Operations

| Behaviour | Status |
|---|---|
| `compute doctor` reports controller version, data plane, control plane, FeltDB connectivity, workload and unhealthy counts, endpoints, TLS, authentication, runtimes, isolation, and upgrade state; an unreachable or unauthorized controller is reported, not an error | **verified** (manual run and JSON shape) |
| `compute node info \| health \| reconcile \| upgrade \| rollback \| upgrade-status` | **verified** (used by the tests) |
| `compute auth create \| list \| revoke \| rotate \| whoami \| audit` | **verified** |
| Structured events for the controller, workloads, reconciliation, upgrades, FeltDB, and authentication, with request, operator, and credential IDs | **verified** (the kinds asserted in tests; see [architecture.md](architecture.md#structured-events)) |
| Failure kinds `admission_denied`, `authentication_failed`, `authorization_denied`, `state_unavailable`, `runtime_unavailable`, `workload_failed`, `controller_unavailable`, `endpoint_unavailable`, `upgrade_failed` | **verified** for all except `endpoint_unavailable`, which is **supported** (emitted when an endpoint cannot listen; not triggered in a test) |
| Prometheus metrics at `/metrics` | **supported** |
| The Hetzner kit (production mode, TLS, credentials, supervisor, `KillMode=process`) | **supported**: not deployed during this audit |

## 12. Architectural invariants

The 15 invariants and their regression tests are in [architecture.md](architecture.md#invariants).

- **Invariants 1–12:** each has at least one test above that passes at `f68ce7f`.
- **Invariants 13–15:** hold by construction and review (runtime neutrality, no AuthBoundry dependency, no application-specific framework).

## 13. Known gaps

1. **Ingress in the controller.** Ingress for domains on ports 80 and 443 still runs in the controller, so it pauses while the controller restarts. Direct endpoints do not.
2. **`compute serve` is not hardened.** Remote jobs do not use operator credentials or TLS.
3. **Project restart on FeltDB** has p95 2.2 s including the process restart, against a 1 s target. Its share spent in FeltDB is unmeasured.
4. **Add at 300 services** is 476 ms end to end, against a 100 ms target. The dominant costs are deployment admission and the post-add project view.
5. **Upgrades under systemd** (`compute node upgrade`) have not been verified. Restart with systemd instead.
6. **Artifact signatures are not verified.** Only the digest is pinned.
7. **The supervisor cannot be upgraded** without restarting workloads.
8. **Not measured:** Managed FeltDB over a network, and half-open network partitions.
9. **Isolation coverage.** Host isolation was exercised on cgroup v1 as root only.

## 14. FeltDB 0.11.8 consumption

A follow-up change moved Compute onto `@feltdb/core` 0.11.8 and removed the
access patterns this audit's section 6 measured around. The full report,
with every status and number, is
[feltdb-0.11.8-consumer-certification.md](feltdb-0.11.8-consumer-certification.md)
(machine-readable: [`.json`](feltdb-0.11.8-consumer-certification.json));
the contract is [feltdb.md](feltdb.md). In short:

| Area | This audit | Now |
| --- | --- | --- |
| FeltDB version | 0.11.7, manifest only | 0.11.8, verified as resolved in CI; the tests' server is built from the resolved package |
| Identity lookups | `_id` filters: FeltDB scanned the collection | Indexed `record_id`: one row examined |
| Views (executions, receipts, revisions, deployments, audit) | Scope loaded, sorted, truncated in Compute; receipts had no index | Ordered and limited by FeltDB, within an index |
| Desired-state load | Ten independent whole-collection reads plus per-record scans, every cycle; not coherent | A coherent, bounded snapshot; a cycle with no other writer reads one revision |
| After a mutation | The next read reloaded all desired state | The write is read back by identity; the controller's own commits carry its working copy forward |
| Recovery | Refresh, then record | Ordered: rebuild from FeltDB, continue the sequence, reload credentials, flush held evidence and audit, record, reconcile — before a mutation is accepted |
| Model changes | `upgrade` replaced the model | `compute control-plane upgrade`: inspect, refuse a downgrade, verified FeltDB backup, apply, verify, backfill, smoke test; controllers refuse an older model |
| Diagnosis | Mode and error | `compute doctor`: versions, model generation, authority state, last durable read and mutation, last recovery, cache, pending work, snapshots, query plans |

Two FeltDB 0.11.8 findings bound what Compute alone can improve: FeltDB's
cost per request grows linearly with its committed state (measured from
`/v1/state/version` and an index probe, while `/health` stays flat), and
`backup verify` rejects FeltDB's own online backups of application state.
Both are in the certification's remaining gaps.
