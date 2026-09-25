# Platform audit — stack, runtime and production readiness

Audited 2026-09-25 at Compute `ab61430`. This document describes what exists and what was proven. It does not describe what is intended. The machine-readable twin is [`platform-audit.json`](platform-audit.json). Raw evidence is in [`platform-audit-evidence/`](platform-audit-evidence/), and the harnesses that produced it are in [`scripts/platform-audit/`](../scripts/platform-audit/).

## How to read this

Every capability carries exactly one status.

| Status | Meaning |
|---|---|
| **verified** | Exercised during this audit by an executable test, drill or measurement. The evidence is linked. |
| **implemented** | The code exists and its own tests exist, but this audit did not exercise it end to end. |
| **partial** | Some of the capability works, but a material piece is missing or development-grade by its own documentation. |
| **prototype** | Demonstrable, but not intended to carry production traffic in its current form. |
| **missing** | No implementation found. |
| **blocked** | Exists, but could not be exercised from the audit environment. The reason is stated. |
| **unknown** | Not determinable from the evidence available. It stays unknown. |

"Verified" never means production-ready. Every Compute measurement was taken on a single 4 vCPU, 16 GB Linux VM (Intel Xeon 2.1 GHz, kernel 6.18) using a release build. Hosted deployments on Fly.io and Vercel could not be reached: the audit environment's egress policy denies `*.fly.dev`. Their live state is therefore **unknown**, not assumed.

### Evidence sources

| Evidence | Produced by | Contents |
|---|---|---|
| [`compute-measurements.json`](platform-audit-evidence/compute-measurements.json) | `scripts/platform-audit/measure_compute.py` | Runtimes, throughput, daemon, API, releases, remote jobs, capacity |
| [`control-plane-drills.json`](platform-audit-evidence/control-plane-drills.json) | `scripts/platform-audit/control_plane_drills.py` | Remote operation, FeltDB outage and restart, Compute SIGKILL, fresh-node restore |
| [`feltdb-backup-drill.txt`](platform-audit-evidence/feltdb-backup-drill.txt) | `scripts/platform-audit/feltdb_backup_drill.sh` | 500 writes, backup, verify, restore |
| [`feltdb-upgrade-workflow.json`](platform-audit-evidence/feltdb-upgrade-workflow.json) | `scripts/platform/feltdb.mjs` | Consumer status, and a real upgrade / verify / rollback |
| Repository inspection | Clones of 15 repositories at the heads listed in §2 | Manifests, docs, deployment files, CI, tests |
| Compute workspace tests | `cargo test --release --workspace` | 20 test binaries, all passing (the largest: 33 control-plane tests, 30 unit tests) |

---

## 1. Platform tree

```
Platform
├── Developer tooling
│   ├── PAX .................... repo lint / typecheck / drift tooling (rkendel1/pax)
│   ├── create-appport ......... app scaffolder (appport-services/packages/create-appport)
│   └── Factory ................ software-factory runner (rkendel1/factory)
│
├── Application framework (in-process libraries)
│   ├── AppPort ................ capability protocol, schema, transports, SDK, MCP (rkendel1/appport)
│   ├── AppPort Services ....... API keys, jobs, schedules, secrets, webhooks, files, notifications (rkendel1/appport-services)
│   └── AppPort runtime ........ HTTP host for an AppPort app (@appport/runtime)
│
├── Authority
│   └── AuthBoundry ............ identity, authority decisions, audit (rkendel1/authboundry)
│
├── Product / portal
│   └── AppBoundry ............. developer portal + a fork of the AppPort monorepo (rkendel1/appboundry)
│
├── Execution & network control plane
│   └── Compute ................ runtimes, environments, releases, endpoints, ingress, DNS, ACME, remote jobs (rkendel1/compute)
│       └── compute-network .... TCP endpoint forwarders, ingress, DNS providers, ACME
│
├── State
│   ├── FeltDB ................. schema-first database; feltdb-server; @feltdb/core compiler (rkendel1/flow_db)
│   └── Managed FeltDB ......... hosted control plane + managed instances (flow_db/deploy/{managed,control-plane,intent-engine})
│
├── Intelligence
│   ├── rust-ml-runtime ........ ONNX inference runtime + Node binding + HTTP server (rkendel1/rust-ml-runtime)
│   ├── Laya ................... a local model served by rust-ml-runtime (not a separate service)
│   ├── Jev .................... AI SDK / agent surfaces (simple-jev, open-jev, jev-ui, @feltdb/jev)
│   └── llm .................... model-routing library @easy-llm/llm (rkendel1/llm)
│
├── Human attention
│   └── Attn ................... native macOS assistant embedding cline (rkendel1/attn)
│
└── Example / reference applications
    └── web-monitor ............ AppPort Services consumer (rkendel1/web-monitor)
```

The platform has **one long-running control-plane service, Compute**. It also has **one database server, feltdb-server**. The hosted services (Managed FeltDB, AuthBoundry API, AppBoundry portal, AppPort site, Factory) are separate Fly.io applications. Everything else is a library linked into an application's own process, or a developer tool.

## 2. Component inventory

Repository heads audited:

| Repository | Head |
|---|---|
| compute | `ab61430` |
| flow_db | `d751b58` |
| authboundry | `44f6059` |
| appboundry | `a160d57` |
| appport | `1b2132c` |
| appport-services | `f3a80fe` |
| factory | `db101e9` |
| attn | `6b6b227` |
| pax | `6a53d3e` |
| llm | `9ca9e9e` |
| rust-ml-runtime | `ebb9a95` |
| jev-ui | `b6ebc71` |
| simple-jev | `3223591` |
| open-jev | `b4782a6` |
| web-monitor | `ebd5d5d` |

### Compute — execution and network control plane
| Field | Value |
|---|---|
| Repo / package | `rkendel1/compute`: 15 Rust crates, the `compute` binary, and private npm packages `@compute/{state-model,appport,ui-e2e}` |
| Purpose | Run tasks and services across 9 runtimes (wasm, native, node, bun, deno, python, ruby, php, jvm, dotnet, shell) with policy admission and receipts. Also runs environments, projects, revisions, releases, stable endpoints, ingress, DNS, ACME and remote jobs. |
| Runtime | A single Rust binary: `compute start` runs the daemon, and `compute serve` runs the remote job server |
| Deployment location | Local, or a single node through `deploy/hetzner` (a systemd unit plus `compute.toml`). **No known production node.** |
| Dependencies | feltdb-server (optional state backend: memory, file or FeltDB); host language runtimes; an ACME CA; a DNS provider API (Hetzner or Cloudflare) |
| Consumers | Operators via CLI, UI and API. No other platform component calls Compute (see §3). |
| Providers | Runtime adapters, state backends, DNS providers, the node `SecretStore` |
| API / CLI | HTTP API at `127.0.0.1:8787` (the environment API, SSE events, and the UI served by the daemon); `compute run / start / environment / project / deploy / release / domain / certificate / dns / serve / submit / bundle / deps / certify` |
| AppPort capabilities | `@compute/appport` exposes control-plane capabilities (environments, projects, deployments, releases, domains) |
| Persistent state | Control state in memory, a file, or FeltDB (`compute.flow`). The node secret store holds secret material; state holds only `node:<id>/…` references. Workspaces and receipts live on disk. |
| Authority | A single shared bearer token for mutations. **Reads are unauthenticated.** No TLS on the API. |
| Shared / private | Shared: one daemon per node serves every environment |
| Production status | **partial**: verified on one node; no process sandbox; no HA; no production deployment |
| Tests | 20 test binaries passing, including release, network (with Pebble ACME), recovery and conformance. Certification runs through `compute certify` and `distribution/certify-distribution.sh`. CI runs **only on tags and manual dispatch**. |
| Operational status | Verified locally by this audit (§6, §8). |

### FeltDB — state
| Field | Value |
|---|---|
| Repo / package | `rkendel1/flow_db`: `@feltdb/core` 0.11.7 (published; the compiler, `feltdb` CLI and runtime), `create-feltdb` 0.11.7, `@feltdb/webllm` 0.11.7, and the Rust crates `feltdb` and `feltdb-server`. `@feltdb/jev` 0.1.0 is private and experimental. |
| Purpose | Schema-first database. A `.flow` model compiles to a manifest, served by `feltdb-server` over the `FeltDB-Protocol: 1` HTTP wire protocol or embedded through `@feltdb/core`. |
| Runtime | Rust server; Node library |
| Deployment location | Local or embedded; Managed FeltDB on Fly (`feltdb-managed`, `feltdb-control-plane`, `feltdb-intent-engine`); site on Vercel |
| Dependencies | None at the platform level |
| Consumers | compute, authboundry, attn, factory, appport-services, jev-ui, appboundry, and web-monitor (transitively). See §11. |
| API / CLI | `feltdb validate / compile / backup create / verify / restore`; HTTP `/v1/...` |
| Persistent state | On-disk store; backups |
| Authority | API keys (hashed); a managed control plane |
| Shared / private | The server is shared per deployment; the library is private per app |
| Production status | **partial**. Its own `docs/production/PRODUCTION-READINESS.md` says **"NOT PRODUCTION READY"**: single-node durability and transactions are proven, but distribution, endurance, performance, upgrade and operations are not. |
| Tests | 284 test files; CI includes `proof-qualification`, `architecture-conformance` and a monthly `disaster-recovery` drill |
| Operational status | The local server is **verified** (§8, and the backup drill). Managed FeltDB is **blocked/unknown**: it runs on one Fly machine with 1 shared CPU, 2 GB and one 10 GB volume with 7-day snapshots, and was unreachable from the audit. |

### AuthBoundry — authority
| Field | Value |
|---|---|
| Repo / package | `rkendel1/authboundry`: `@authboundry/core` 1.15.3 (native, 4 platforms), the `authboundry` CLI 2.15.3, and `@authboundry/bridge` 0.1.3 |
| Purpose | Identity, sessions, keys, authority decisions and audit |
| Runtime | Rust server that spawns a Node bridge (`bridge/feltdb-store.cjs`) for FeltDB storage |
| Deployment location | Fly `authboundry-api` (`FELTDB_DEPLOYMENT=local` on a volume); Vercel web |
| Dependencies | FeltDB 0.11.7, `@appport/core`, `@appport/services` |
| Consumers | factory (hard dependency), jev-ui, and apps that choose it as `APPPORT_AUTHORITY` |
| Persistent state | Embedded FeltDB on the Fly volume |
| Production status | **partial**. Its README says the local account and in-memory storage mechanisms "are development references" and the signature mechanisms "are for development". |
| Tests | 71 test files; CI covers architecture invariants, CLI smoke, deploy and release |
| Operational status | **unknown**: the hosted API was unreachable |

### AppPort — capability protocol and SDK
| Field | Value |
|---|---|
| Repo / package | `rkendel1/appport`: `@appport/{schema,protocol,server,client,transport-*,generators,mcp,…}` 1.0.1, and `@appport/core` 1.0.2. Published CLI `appport` 1.1.20 and `@appport/sdk` 1.1.21. |
| Purpose | Declare application capabilities once, then serve them over HTTP, WebSocket, in-process, Electron, Tauri and MCP |
| Runtime | Library in the app process |
| Consumers | compute (`@compute/appport`), authboundry, factory, jev-ui, attn, appport-services |
| Production status | **implemented** (published; 46 test files; this audit did not run its suite) |
| Finding | `rkendel1/appboundry` contains a fork of the same monorepo that publishes `@appport/*` **1.0.0**, while `rkendel1/appport` publishes **1.0.1**. There are two sources of truth for the same package names. |

### AppPort Services and runtime
| Field | Value |
|---|---|
| Repo / package | `rkendel1/appport-services`: `@appport/services` 0.4.5, `@appport/runtime` 0.4.5, `create-appport` 0.1.8 |
| Purpose | Application building blocks: API keys, jobs, schedules, secrets, webhooks, files, notifications. A runtime HTTP host and scaffolder are included. |
| Runtime | **A library inside each application's process, not a hosted service.** State lives in that app's FeltDB. |
| Dependencies | `@feltdb/core` 0.11.6, and an `APPPORT_AUTHORITY` module. Its docs say "AuthBoundry is the only authority", but §3 shows it is replaceable. |
| Production status | **verified** as a library: its full npm test suite passed (178 s) during the upgrade drill. **partial** as a platform service, because nothing is shared across apps. |
| Operational status | Per app; no shared deployment |

### AppBoundry — portal
| Field | Value |
|---|---|
| Repo / package | `rkendel1/appboundry`: `@appport/appboundry` 1.1.1, plus a fork of the AppPort packages (see above) |
| Purpose | Developer portal and "managed" application hosting story |
| Deployment location | Fly `appboundry` (`deploy/platform`); `apps/cloud-portal` |
| Dependencies | `@feltdb/core` 0.11.0 (integration package); `appport` from a vendored `file:./appport-1.1.18.tgz` in `deploy/platform` |
| Production status | **prototype**. The portal says "Managed: AppBoundry provides compute", but **no code in AppBoundry calls Compute and nothing in Compute knows about AppBoundry**. Its own proof document records 20 placeholder `expect(true)` tests. |
| Tests | 202 test files (including the placeholders) |
| Operational status | **unknown** (Fly unreachable) |

### Factory
| Field | Value |
|---|---|
| Repo / package | `rkendel1/factory`: `software-factory` 1.0.0 (not published) |
| Purpose | Durable software-factory runs and evidence |
| Deployment location | Fly `factory-idvhpa` |
| Dependencies | `@authboundry/core` (hard), `@appport/*`, `@appport/services`, `@feltdb/core` ^0.11.7 |
| Production status | **partial**. It cannot run without AuthBoundry. |
| Operational status | **unknown** |

### rust-ml-runtime, and Laya
| Field | Value |
|---|---|
| Repo / package | `rkendel1/rust-ml-runtime`: 19 crates; `@rust-ml-runtime/node` 0.2.3 (5 platforms) |
| Purpose | ONNX inference in-process (Node binding) or over HTTP (`ml-runtime serve` exposes `/v1/infer` and `/v1/infer/stream`) |
| Laya | A local ONNX model downloaded from HuggingFace and run in-process by attn and jev-ui. **Laya is not a service.** |
| Authority | **The HTTP server has no authentication** (`providers/server/src/lib.rs:123`) |
| Production status | **implemented** (library); **prototype** (HTTP server, as a shared service) |

### Jev
| Field | Value |
|---|---|
| Repos | `simple-jev` (a featherless fork, 0.1.0), `open-jev`, `jev-ui` (the Next.js Shapeshift demo), `@feltdb/jev` (private, experimental). The hosted TypeSafe AI SDK is `@typesafe-ai/sdk`. |
| Production status | **prototype**. No shared Jev service exists. jev-ui pins `@feltdb/core` 0.11.6 and requires `@authboundry/core`. |

### Attn
| Field | Value |
|---|---|
| Repo | `rkendel1/attn`: a native macOS app (`id8-macos`) embedding cline (`@cline/*`); 473 test files; the only CI is `id8-macos.yml` |
| State | Local FeltDB 0.11.7 (it also vendors `feltdb-core-0.11.7.tgz`); Laya in-process |
| Production status | **prototype**, and **blocked** for this audit: macOS only, and the audit ran on Linux |

### PAX, llm, web-monitor
| Component | Purpose | Status |
|---|---|---|
| PAX | Dev tooling for lint, typecheck and drift | **verified**: on Compute, `pax drift` reports no drift and `pax doctor` is OK |
| llm (`@easy-llm/llm` 0.10.1) | Model routing library | **implemented**; not exercised |
| web-monitor | Reference consumer of `@appport/services` 0.4.4 | **implemented**; its FeltDB 0.11.2 arrives transitively |

### Other control-plane and networking services
No separate networking services exist. Endpoints, ingress, DNS and ACME all live inside the Compute daemon (`crates/compute-network`). The Managed FeltDB control plane and intent engine are the only other control-plane processes.

## 3. Application development path

**Intended path:** source → AppBoundry → Compute → application.

**Actual path:** AppBoundry and Compute are not connected. The paths that exist today are:

```
source ──create-appport──▶ AppPort app (Node, @appport/runtime + services, embedded FeltDB)
   │                              │
   │                              └─▶ run locally (node server.js)                         verified
   │
   └──compute bundle / deps──▶ Compute revision ──release──▶ stable endpoint             verified (with workarounds)

AppBoundry portal ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─▶ Compute                  missing
```

A clean-environment walk-through is in §17.

**Can someone build and run an application without adopting AuthBoundry?**

- **On Compute: yes.** Compute has no AuthBoundry dependency. Any workload runs.
- **With AppPort Services: yes, but some authority module is still required.** The runtime refuses to serve without `APPPORT_AUTHORITY`. A 20-line allow-all authority module, written for this audit and not using AuthBoundry, was accepted. With it, API-key creation worked. The authority contract is undocumented: identities need `principalId`, `principalType` and `tenantId`.
- **Factory and jev-ui: no.** Both hard-require `@authboundry/core`.

## 4. Shared-infrastructure matrix

Column meanings:

- **Exists:** there is an implementation anywhere.
- **Shared:** one instance serves many applications.
- **Durable:** the capability survives a process restart.
- **Remote:** it is usable over the network.
- **Prod-verified:** it has been proven in a production deployment.

"Per-app" means the capability lives inside each application's process (AppPort Services).

| Capability | Exists | Shared | Durable | Remote | Prod-verified | Where |
|---|---|---|---|---|---|---|
| State | yes | yes | yes | yes | no | feltdb-server; Managed FeltDB (unknown) |
| Execution | yes | yes | yes | yes | no | Compute tasks, services and remote jobs |
| Networking | yes | yes | yes | yes | no | Compute endpoints and ingress |
| DNS | yes | yes | yes | yes | no | Compute DNS providers: Hetzner and Cloudflare (tested against mocks); file |
| TLS | yes | yes | yes | yes | no | Compute ACME, verified against Pebble; renewal implemented; never run against a real CA in this audit |
| Secrets | partial | no | yes | no | no | Compute node `SecretStore` (node-local files); AppPort Services secrets (per app). There is no shared secret service. |
| Webhooks | yes | per-app | yes | yes | no | AppPort Services |
| Notifications | yes | per-app | yes | no | no | AppPort Services (records only; no delivery provider verified) |
| Jobs | yes | both | yes | yes | no | Compute remote jobs (shared); AppPort Services jobs (per app) |
| Scheduling | partial | per-app | yes | no | no | AppPort Services schedules. Compute has no scheduler (no cron-style workloads). |
| Files | yes | per-app | yes | no | no | AppPort Services files, stored in the app's FeltDB |
| Inference | yes | no | n/a | partial | no | rust-ml-runtime, in-process; its HTTP server is unauthenticated |
| Decisions | partial | no | no | no | no | Compute policy admission; AuthBoundry decisions. There is no general decision service. |
| Identity | partial | yes | yes | yes | no | AuthBoundry (development-grade identity) |
| Authority | partial | yes | yes | yes | no | AuthBoundry; Compute uses its own bearer token |
| Observability | partial | no | partial | partial | no | Compute events (SSE), receipts, logs. No cross-component tracing. |
| Logging | partial | no | partial | yes | no | Compute per-workload logs through the API. Nothing is aggregated. |
| Metrics | missing | – | – | – | – | No metrics endpoint in Compute, FeltDB or the libraries |
| Alerts | missing | – | – | – | – | None |
| Human attention | prototype | no | local | no | no | Attn (macOS desktop) |

## 5. Missing capabilities

| Capability | Why it matters | Current substitute | Maturity | Blocks | Priority |
|---|---|---|---|---|---|
| Process sandbox for non-WASM runtimes | Node, Python and other workloads run as the daemon's user, with no memory, CPU, filesystem or network enforcement (measured, §6) | Trust the workload; WASM for untrusted code | missing | Any multi-tenant or untrusted workload | P0 |
| AppBoundry → Compute integration | The product promises "AppBoundry provides compute" | Manual `compute bundle` / `release` | missing | AppBoundry-hosted apps | P1 |
| Metrics and alerts | No way to know when anything is failing | Poll `/status`, read logs | missing | Operating any hosted component | P1 |
| Shared secret service | Secrets are per node or per app, with no rotation or audit trail | Compute `SecretStore` files; AppPort Services secrets | partial | Multi-node Compute; secrets shared across apps | P2 |
| Compute HA and data-plane survival | Endpoints stop answering while the daemon is down (§8) | Restart quickly (about 4.7 s) | missing | Availability SLOs | P1 |
| Authenticated reads and TLS on the Compute API | Anyone who reaches the port can read environments, logs and configuration | Bind to localhost; SSH tunnel | missing | Remote operation over any untrusted network | P0 |
| Scheduled workloads in Compute | Only AppPort Services apps have schedules | External cron plus `compute submit` | missing | Platform-level jobs | P3 |
| Shared inference service with authority | Laya and rust-ml-runtime are in-process only; the HTTP server is open | Embed per app | prototype | Shared-model hosting | P2 |
| Documented authority contract | AppPort apps need an authority module whose shape is undocumented | Read source; use AuthBoundry | partial | Non-AuthBoundry adopters | P2 |
| Managed FeltDB redundancy | One machine, one volume | 7-day volume snapshots | partial | Any hosted app on Managed FeltDB | P1 |

## 6. Compute audit

### Runtime coverage
Evidence: `compute-measurements.json → runtimes`. There were 20 runs per runtime; the CLI end-to-end figures include process start.

| Runtime | Available | Source | CLI p50 / p95 / p99 (ms) | Engine p50 (ms) | Memory limit | CPU limit | FS boundary | Net boundary | Receipt | Status |
|---|---|---|---|---|---|---|---|---|---|---|
| wasm | yes | embedded | 81.6 / 87.0 / 98.4 | 0 | enforced | no | enforced | enforced | yes | verified |
| native | yes | embedded | 80.5 / 86.5 / 90.2 | 1 | no | no | no | no | yes | verified |
| shell | yes | host | 19.0 / 19.7 / 20.8 | 1 | no | no | no | no | yes | verified |
| python | yes | host | 32.8 / 40.2 / 46.8 | 12 | no | no | no | no | yes | verified |
| node | yes | host | 237.3 / 254.0 / 255.1 | 33 | no | no | no | no | yes | verified |
| bun | yes | host | 166.7 / 176.5 / 177.3 | 12 | no | no | no | no | yes | verified |
| ruby | yes | host | 101.4 / 105.9 / 106.9 | 61 | no | no | no | no | yes | verified |
| php | yes | host | 145.0 / 155.0 / 164.5 | 31 | no | no | no | no | yes | verified |
| jvm | yes | host | 167.6 / 176.9 / 177.3 | 42 | no | no | no | no | yes | verified |
| deno | no | – | – | – | – | – | – | – | – | blocked (not installed) |
| dotnet | no | – | – | – | – | – | – | – | – | blocked (not installed) |

Findings:

- **Sandboxing.** Only WASM enforces a boundary. Process runtimes are *declared* with network and filesystem requirements: a workload must declare network access, because the default `network: none` is rejected rather than enforced. Nothing enforces those declarations at the OS level. See `docs/isolation.md`.
- **Fixed overhead.** About 75 ms of every WASM and native invocation is Compute hashing its own 49 MB executable to build the receipt's `executable_identity` (`crates/compute-runtime/src/lib.rs:856`, observed with strace). The engine itself takes 0–1 ms.

### Execution model
- **Tasks:** one-shot, admitted by policy, run through a runtime adapter, with a receipt written per run.
- **Services:** long-running instances supervised by the daemon behind stable endpoints.
- **Remote jobs:** `compute serve` / `compute submit`, with a durable queue.
- **Scheduling:** one node with port ranges; placement over provider pools.

### Throughput
Evidence: `throughput`. Each cell is a 10 s window; 0 failures across all runs.

| Runtime | ×1 | ×4 | ×8 | Bottleneck |
|---|---|---|---|---|
| shell | 49.9/s | 190.6/s | 206.6/s | 4 CPUs |
| python | 29.2/s | 112.4/s | 122.2/s | 4 CPUs |
| wasm | 12.1/s | 46.4/s | 48.5/s | Self-hash (about 75 ms of CPU per run) |
| node | 4.1/s | 16.2/s | 16.6/s | Node start-up |

### Daemon and API
Evidence: `daemon`, `api`.

| Measure | Value |
|---|---|
| Daemon start to API (memory state) | p50 6.2 ms, p95 9.5 ms |
| Daemon start to API (file state, 20 environments) | p50 6.8 ms, p95 9.7 ms |
| Idle RSS | 14 MB |
| Environment create | p50 0.5 ms, p99 0.8 ms |
| Environment list (30 environments) | p50 0.7 ms |
| API task run, wasm | p50 80.4 ms, p99 88.4 ms; 40/40 receipts |
| API task run, python | p50 35.4 ms, p99 41.7 ms; 40/40 receipts |
| Daemon start on a FeltDB backend | 230 ms |

**Defect: concurrent runs of the same task.** With 8 clients, 79 of 80 calls returned `403 admission_denied: the task did not execute`. A generation counter supersedes the earlier in-flight calls. `finish()` then finds no record for them and returns the error (`crates/compute-environment/src/daemon/execute.rs:697-705`), even though those tasks *did* run. Their ExecutionRecord and receipt are lost, so the evidence guarantee is broken under concurrency.

### Releases (Python HTTP service)
Evidence: `release`.

| Measure | Value |
|---|---|
| First release | 431 ms (placed → ready 327 ms; ready → switched 0.4 ms) |
| Release total, 5 under load | p50 445 ms, p95/p99 961 ms (one drain took 608 ms) |
| Traffic switch | p50 1.3 ms, max 2.0 ms |
| Requests during 5 releases (4 clients) | 23,261 OK, **0 failed**; p50 1.5 ms, p95 2.3 ms, p99 3.4 ms, max 1,029 ms |
| Operator rollback of a completed release | 381 ms |
| Crash-on-start detected | 3.27 s (the default `on_failure` retries 3 times) |
| Readiness timeout (3 s) | failed at 3.03 s |

The release is zero-downtime by this measure: no request failed. The 1,029 ms maximum is a tail request that straddled a switch. The p99 is 3.4 ms.

### Remote jobs
Evidence: `remote`.

| Measure | Value |
|---|---|
| Remote run (python) | p50 37.3 ms, p99 42.8 ms |
| Submit | p50 23.6 ms, p99 32.7 ms |
| 40 queued jobs | drained 0.23 s after the last submit |

### Capacity
Evidence: `capacity`. Workload: identical Python HTTP services, each behind its own stable endpoint, in memory state, on the host described above. The test ran for 235 s. Confidence is **medium**: one host, one run, and a trivial workload.

| Services | Running / answering | Service RSS (sum) | Daemon RSS | Environment view p50 | Time to add step | Daemon CPU during step | MemAvailable |
|---|---|---|---|---|---|---|---|
| 10 | 10 / 10 | 179 MB | 44 MB | 3.2 ms | 3.3 s | 0.4 s | 15.0 GB |
| 50 | 50 / 50 | 896 MB | 80 MB | 13.6 ms | 13.5 s | 4.6 s | 14.8 GB |
| 100 | 100 / 100 | 1.8 GB | 117 MB | 29.3 ms | 22.2 s | 13.7 s | 14.3 GB |
| 200 | 200 / 200 | 3.6 GB | 193 MB | 62.2 ms | 63.9 s | 55.2 s | 13.3 GB |
| 300 | 300 / 300 | 5.4 GB | 265 MB | 95.4 ms | 90.3 s | 85.4 s | 12.3 GB |

The service RSS sum overcounts shared pages, which is why MemAvailable fell by only 2.7 GB.

- **Observed ceiling:** at least 300 services answering on one node. The ceiling was not reached; the test was bounded by time.
- **Bottleneck:** the daemon, not memory.
  - Environment views grow linearly, at about 0.32 ms per service.
  - Adding a service costs about 0.9 s of wall time at 300 services, because reconcile work is O(n) per change.
  - Daemon CPU at idle is 9.6% with 300 services, spent on status probes.
- **Shutdown:** services stop sequentially. At 300 services shutdown exceeded the harness's 30 s cap and the harness killed it.

## 7. Performance targets

Current measurement comes first. The targets are *proposed* operator expectations, each with a reason. None was chosen to make current numbers pass; several fail.

| Path | Current p50 / p95 / p99 | Proposed target | Gap | Basis for target |
|---|---|---|---|---|
| WASM task, CLI end to end | 81.6 / 87.0 / 98.4 ms | p50 ≤ 20 ms | **−62 ms** | The engine takes 0 ms; 75 ms is the self-hash |
| Python task via API | 35.4 / 38.7 / 41.7 ms | p95 ≤ 50 ms | meets | Interpreter start dominates |
| Same-task concurrent API runs | 1/80 succeed | 80/80 with receipts | **defect** | The evidence guarantee |
| Release switch | 1.3 / 2.0 / 2.0 ms | p99 ≤ 10 ms | meets | – |
| Request latency during a release | 1.5 / 2.3 / 3.4 ms (max 1,029) | p99 ≤ 10 ms, 0 failures | meets | – |
| Release total (trivial service) | 445 / 961 / 961 ms | p95 ≤ 2 s | meets | Readiness probe interval dominates |
| Control-plane read (FeltDB backend: status, logs) | 390–460 ms | p95 ≤ 100 ms | **−300 ms or more** | Each request re-reads the full desired state (about 19 collection lists plus 7 status queries) |
| Control-plane mutation (FeltDB backend: deploy, restart) | 1.9–4.9 s | p95 ≤ 1 s, excluding readiness | **−1 to −4 s** | Same cause |
| Control-plane read (memory state) | 0.5–0.7 ms | – | – | For comparison |
| Daemon restart to serving endpoints | 4.7 s | ≤ 1 s, or no data-plane outage | **−3.7 s** | Endpoints die with the daemon |
| Add a service at 300 services | about 0.9 s | ≤ 100 ms | **−0.8 s** | O(n) reconcile |
| Environment view at 300 services | 95 / 103 / 103 ms | p95 ≤ 50 ms | **−53 ms** | Per-service probes in the view |
| Graceful shutdown, 300 services | > 30 s | ≤ 10 s | **> −20 s** | Sequential stop |
| FeltDB restart to serving | 99 ms | – | – | – |
| FeltDB backup create / restore (800 KB) | 29 / 50 ms | – | – | – |

Not measured, and marked **unknown**:

- Managed FeltDB latency
- AuthBoundry decision latency
- AppPort Services request latency under load
- Inference latency

## 8. Control-plane availability

Evidence: `control-plane-drills.json`, and `crates/compute-cli/tests/recovery.rs` (a SIGKILL during a release resumes correctly).

| Drill | Expected invariant | Observed | Holds? |
|---|---|---|---|
| Compute daemon restart (SIGKILL) | Desired state is reconstructed and services restored | Back at the API in 4.7 s; services reaped and restarted on new PIDs; same revision | **yes** |
| Compute down: data plane | Existing traffic keeps flowing | **Endpoints stop answering.** Forwarders run inside the daemon. | **no** |
| SIGKILL mid-release | The release resumes or rolls back without duplicating instances | `recovery.rs` test passes | **yes** |
| FeltDB killed while Compute runs | Services keep serving; mutations fail cleanly | Services answered; mutations returned 503 `state_unavailable`; status reported the error; the daemon stayed up | **yes** |
| FeltDB restart | Automatic recovery | Up in 99 ms; mutations recovered with no operator action | **yes** |
| Network interruption to FeltDB | Same as FeltDB killed | Same code path (a connection error). No partition with a half-open socket was tested. | **partial** |
| Compute starts while FeltDB is down | Starts degraded, or waits | **Refuses to start** (exit 1) | **no**. A node reboot during a FeltDB outage leaves the node serving nothing. |
| Fresh node against an existing FeltDB | Restores environments and running projects | 3 environments and a running project restored in 1.2 s | **yes** |
| Crash of a service instance | Restarted by `on_failure` policy | Crash-on-start retries 3 times, then the release fails in 3.27 s | **yes** |

## 9. Remote control

"Exists" means the operation works over the network. "Production-safe" means it could be exposed beyond a trusted network as it stands today.

| Operation | Exists | Evidence | Production-safe |
|---|---|---|---|
| Create environment; repeat returns 409 | yes | drill | no (see below) |
| Register revision; idempotent on identical content | yes | drill | no |
| Deploy, promote, rollback | yes | drill | no |
| Restart / stop / start workload | yes | drill | no |
| Logs, events, receipts, inspect | yes | drill | no: **readable without a token** |
| Domains, certificates, DNS status | yes | drill | no |
| Remote jobs (`serve` / `submit`) | yes | measurements | no |

All 25 remote operations succeeded. None is production-safe, for these reasons:

1. Reads need no authentication (`crates/compute-cli/src/environment_cmd.rs:136-160`).
2. Mutations use one shared bearer token, with no per-operator identity, no scopes and no rotation.
3. There is no TLS on the API.
4. There is no audit record of *who* did something. Events record *what* happened.

## 10. Deployability lifecycle

| Stage | Compute | FeltDB server | Managed FeltDB | AuthBoundry | AppBoundry | AppPort app |
|---|---|---|---|---|---|---|
| Build | verified (release build) | verified | unknown | implemented (CI release) | implemented | verified (create-appport) |
| Package | implemented (`distribution/assemble.sh`, Dockerfile) | implemented (Dockerfile) | implemented (Fly) | implemented (Fly, Docker) | implemented (Fly) | verified (`compute bundle` and `deps`, with workarounds) |
| Deploy | implemented (`deploy/hetzner` systemd); not exercised | implemented | unknown | unknown | unknown | verified on Compute |
| Configure | implemented (`compute.toml`; secrets from the environment) | implemented | unknown | unknown | unknown | **partial**: fixed port 8787, `PORT` ignored |
| Upgrade | **missing**: no in-place upgrade procedure for the daemon; restart only | partial (§11) | unknown | unknown | unknown | verified (a release) |
| Roll back | missing for the binary; verified for workloads | verified (backup restore) | unknown | unknown | unknown | verified |
| Observe | partial (events, logs) | partial | unknown | unknown | unknown | partial |
| Retire | implemented (`environment delete`); sequential shutdown | implemented | unknown | unknown | unknown | implemented |

## 11. FeltDB upgradeability

### Consumer inventory
Registry: [`scripts/platform/feltdb-consumers.json`](../scripts/platform/feltdb-consumers.json). Resolved from lockfiles on 2026-09-25. The latest published version is 0.11.7.

| Consumer | Resolved `@feltdb/core` | Manager | Schema (`.flow`) | Runtime use |
|---|---|---|---|---|
| compute | 0.11.7 | npm | `compute.flow` | Model compile only; the daemon speaks `FeltDB-Protocol: 1` |
| authboundry | 0.11.7 (with overrides) | npm | `authboundry.flow` | Node bridge at runtime |
| attn | 0.11.7 (also vendors the .tgz) | bun | `cline.flow` | Bundled helper binary |
| factory | ^0.11.7 (override `$@feltdb/core`) | npm | – | Node service |
| appport-services | **0.11.6** | npm | `appport.flow` | Library in each app |
| jev-ui | **0.11.6** | bun | `feltdb.flow` | Next.js app |
| web-monitor | **0.11.2** (through `@appport/services` 0.4.4) | npm | `feltdb.flow` | Node service |
| appboundry | **0.11.0** | pnpm | two `feltdb.flow` files | Library |

Four distinct versions are in use, so **the consumers are not aligned**. Three package managers are in use.

### Workflow
No second package manager is introduced. The script edits each consumer's own manifest and regenerates that consumer's own lockfile with its own tool.

```
node scripts/platform/feltdb.mjs status   [--root DIR] [--json]
node scripts/platform/feltdb.mjs upgrade  --consumer NAME --to VERSION [--apply]
node scripts/platform/feltdb.mjs verify   --consumer NAME [--install]
node scripts/platform/feltdb.mjs rollback --consumer NAME [--state FILE]
```

- **`upgrade`** is a dry run unless `--apply` is given. `--apply` snapshots the manifests and lockfile, rewrites specifiers (keeping `^`, `~` or exact), and regenerates the lockfile. The commands are `npm install --package-lock-only`, `pnpm install --lockfile-only` and `bun install --lockfile-only`.
- **`verify`** installs from the lockfile (`npm ci` or frozen) and runs `feltdb validate` on each `.flow` with the *installed* version. It then runs the consumer's own verify commands from the registry.
- **`rollback`** restores the snapshot byte for byte.

### Proven
Evidence: `feltdb-upgrade-workflow.json`.

- **appport-services, 0.11.6 → 0.11.7.**
  - Apply changed `package.json` and `package-lock.json`.
  - Verify ran a clean install, validated `appport.flow` against 0.11.7, and passed the full npm test suite (178 s).
  - Rollback restored both files byte for byte, identical to git HEAD. **verified**
- **compute verify.** `compute.flow` validates, the compiled manifest shows no drift, and the live conformance suite passes against `feltdb-server` (32 s). **verified**

### Not yet exercised
- The pnpm (appboundry) and bun (attn, jev-ui) apply paths
- Verify for authboundry, attn, factory, jev-ui, appboundry and web-monitor
- Upgrading **feltdb-server data** across on-disk format versions. Backup and restore are proven within one version only.

Status: **partial**.

## 12. Platform upgrade matrix

| Component | Versioned artifact | Upgrade mechanism | Data migration | Rollback | Status |
|---|---|---|---|---|---|
| Compute binary | Git tag, distribution bundle | Replace binary, restart (a data-plane outage, §8) | Control-state schema via `compute.flow`; no migration tool | Reinstall previous binary. Not tested against newer state. | partial |
| Compute workloads | Revisions | Release | – | `release rollback` (verified) | verified |
| FeltDB library | npm | `scripts/platform/feltdb.mjs` | `.flow` validation | Snapshot restore (verified) | partial |
| feltdb-server | Crate / Docker | Replace binary | Format versioned (backup `format 1`); no cross-version test here | Backup restore (verified, same version) | partial |
| Managed FeltDB | Fly image | `fly-deploy.yml` | Unknown | Fly release rollback; 7-day volume snapshots | unknown |
| AuthBoundry | npm plus a Fly image | Release workflow | Embedded FeltDB | Unknown | unknown |
| AppPort packages | npm | Semver | – | Pin | implemented; split sources (1.0.0 vs 1.0.1) |
| AppPort Services | npm | Semver | `appport.flow` | Pin | implemented |
| rust-ml-runtime | npm, crates | Semver | Model files re-downloaded from HuggingFace | Pin | implemented |

## 13. Shared-service readiness

| Service | What it is today | Shared? | Authn / authz | Durable state | Deployable | Readiness |
|---|---|---|---|---|---|---|
| **Laya** | An ONNX model loaded in-process by attn and jev-ui through `@rust-ml-runtime/node`; downloaded from HuggingFace at first use | no | none: in-process, or an unauthenticated `/v1/infer` if served over HTTP | none (model cache) | No service packaging; could run as a Compute service | **prototype** |
| **Jev** | SDKs and demos (simple-jev fork, open-jev, jev-ui), an experimental `@feltdb/jev`, and the external `@typesafe-ai/sdk` | no | via AuthBoundry in jev-ui | per app | Not packaged as a service | **prototype** |
| **AppPort Services** | A library in each app's process | no, per app | `APPPORT_AUTHORITY` module plus API keys | the app's FeltDB | verified per app; §3 and §17 | **partial**: it works as a library; there is no shared service |

To become shared services, Laya and Jev would need a service boundary, authority on each call, capacity figures, and a deployment. None of those exists.

## 14. Dependency graph

Edge types:

- `build`: package dependency
- `runtime-call`: network call at runtime
- `in-process`: library linked into the process
- `state`: persists state in
- `authority`: delegates authorization to
- `deploys-to`: hosted on
- `claims` (dashed): documented but not implemented

```mermaid
graph LR
  Compute -- state --> FeltDBServer[feltdb-server]
  Compute -- build --> FeltDBCore[@feltdb/core]
  Compute -- build --> AppPort
  Compute -- runtime-call --> ACME[ACME CA]
  Compute -- runtime-call --> DNS[Hetzner / Cloudflare DNS]
  AuthBoundry -- in-process --> FeltDBCore
  AuthBoundry -- build --> AppPort
  AuthBoundry -- build --> AppPortServices[AppPort Services]
  AuthBoundry -- deploys-to --> Fly
  AppPortServices -- in-process --> FeltDBCore
  AppPortServices -. authority .-> AuthBoundry
  AppApp[AppPort app] -- in-process --> AppPortServices
  AppApp -- authority --> AuthorityModule[APPPORT_AUTHORITY module]
  AppApp -- state --> FeltDBServer
  AppApp -- deploys-to --> Compute
  Factory -- authority --> AuthBoundry
  Factory -- in-process --> AppPortServices
  Factory -- in-process --> FeltDBCore
  Factory -- deploys-to --> Fly
  AppBoundry -- build --> AppPortFork[@appport/* 1.0.0 fork]
  AppBoundry -- in-process --> FeltDBCore
  AppBoundry -. claims .-> Compute
  AppBoundry -- deploys-to --> Fly
  ManagedFeltDB[Managed FeltDB] -- deploys-to --> Fly
  JevUI[jev-ui] -- authority --> AuthBoundry
  JevUI -- in-process --> Laya
  JevUI -- in-process --> FeltDBCore
  Attn -- in-process --> Laya
  Attn -- in-process --> FeltDBCore
  Laya -- in-process --> RustML[rust-ml-runtime]
  WebMonitor[web-monitor] -- in-process --> AppPortServices
```

Critical paths:

- **FeltDB** is under everything that holds state.
- **AuthBoundry** is a hard dependency of Factory and jev-ui only.
- **Compute** depends on nothing hosted except, optionally, feltdb-server.

## 15. Failure and recovery audit

| Failure | Detection | Blast radius | Recovery | Evidence | Status |
|---|---|---|---|---|---|
| Compute daemon crash | None built in; systemd `Restart=` in the Hetzner unit | **All endpoints on the node** go down until restart | Restart; about 4.7 s to restore | drill | verified; the data-plane gap remains |
| Compute node loss | None | Everything on the node | Fresh node plus the same FeltDB: 1.2 s restore | drill | verified (state); no failover |
| FeltDB outage | Status reports `state_available: false` | Mutations only | Automatic reconnect | drill | verified |
| FeltDB outage at Compute boot | Exit 1 | The node serves nothing | Manual restart after FeltDB returns | drill | **gap** |
| FeltDB data loss | – | All state | `backup restore`: 50 ms for 800 KB; API keys survive | backup drill | verified |
| Backup to a bare relative path | Exit 1 even though the output is complete | Operator scripts treat a good backup as failed | Use `./path` or an absolute path | backup drill | **defect** |
| Release readiness failure | Readiness check | That release only | Automatic failure; traffic stays on the old instance | measurements | verified |
| Release interrupted (SIGKILL) | On restart | That release | Resumes from durable state | `recovery.rs` | verified |
| Concurrent same-task runs | 403 returned | Receipts lost for tasks that ran | None | measurements | **defect** |
| Managed FeltDB machine loss | Fly | Every managed tenant | Restore a volume snapshot (up to 7 days old) | config only | unknown |
| Hosted AuthBoundry outage | – | Factory and jev-ui logins; apps using it as authority | – | – | unknown |
| Test-daemon leak | – | Detached daemons outlived failed tests | Fixed in this PR: `impl Drop for Cli` in `recovery.rs` | – | fixed |

## 16. Security and authority audit

| # | Location | Current behavior | Intended boundary | Risk | Follow-up |
|---|---|---|---|---|---|
| S1 | Compute process runtimes (`docs/isolation.md`) | node, python, ruby, php, jvm, bun, shell and native run as the daemon user, with no enforced memory, CPU, filesystem or network limits | The workload runs within its declared boundary | **High**: any workload can read node secrets, other workspaces and the daemon's files | P0: an OS sandbox (namespaces, cgroups, seccomp), or restrict process runtimes to trusted operators |
| S2 | `crates/compute-cli/src/environment_cmd.rs:136` | Environment reads are allowed without a token | Reads are authorized | **High** if the port is exposed: configuration, logs and domains leak | P0: authenticate reads |
| S3 | Compute API | One shared bearer token; plain HTTP | Per-operator identity, scopes, TLS | **High** off localhost | P0: TLS plus scoped tokens (AuthBoundry is an option; it must not be required) |
| S4 | Compute events | Record *what* happened, not *who* | An attributable audit trail | Medium | P2 |
| S5 | Compute secrets | DNS credentials and ACME keys go through the node `SecretStore`; state and receipts hold only `node:<id>/…` references (verified in the release PR tests) | As intended | Low: the store is node-local files | P2: a shared secret backend |
| S6 | `rust-ml-runtime/providers/server/src/lib.rs:123` | `/v1/infer` has no authentication | Authorized inference | High if exposed; compute exhaustion | P2 |
| S7 | AuthBoundry README | Account storage and signature mechanisms are "development references" / "for development" | Production identity | **High** for any hosted use | P1: owner decision; out of scope here |
| S8 | AppPort Services authority | Any module passed in `APPPORT_AUTHORITY` is trusted. An allow-all module was accepted. | That is by design, but the contract is undocumented | Medium: easy to deploy allow-all by mistake | P2: document it, and warn on allow-all |
| S9 | AppBoundry proof document | 20 `expect(true)` placeholder tests | Real assertions | Medium: false confidence | P2 |
| S10 | `@appport/*` published from two repos (1.0.0 and 1.0.1) | Supply-chain ambiguity | A single source | Medium | P1 |
| S11 | Managed FeltDB | Single machine | Redundant | Availability | P1 |
| S12 | Compute CI | Runs only on tags and manual dispatch | Every change validated | Medium: regressions land unseen | P2 (the non-goal is building a new CI system; enabling the existing workflow on push is not new infrastructure) |
| S13 | FeltDB API key for Compute | Env var only; never written to configuration or state | As intended | Low | – |

## 17. Developer experience test

This was run from a clean directory on the audit host, using Node 22, with no prior platform configuration.

| Step | Command | Result | Time |
|---|---|---|---|
| Scaffold | `npx create-appport@0.1.8 audit-app` | OK; dependencies installed | 7 s |
| Run | `node server.js` | Listens on **fixed `127.0.0.1:8787`**, which is also Compute's default API port. `PORT` is ignored. | <1 s |
| Call | `curl /` | `401`. That is correct, but the next step is not documented. | – |
| Create an API key | Needs `APPPORT_AUTHORITY` | The identity-field shape is **undocumented** (`principalId`, `principalType`, `tenantId`). It worked with a custom allow-all module. | about 20 min, reading source |
| Call with the key | `curl -H 'x-api-key: …' /` | **`UNAUTHENTICATED`**. The template calls `listJobs()` without passing the principal, which is a template bug. | – |
| Package for Compute | `compute deps create` | **Rejected** the `node_modules/.bin` symlinks; they had to be removed from a copy | – |
| Run on Compute | Workload with `dependencies.capsule` | The capsule goes on `NODE_PATH`, which **ES modules ignore**. The workaround was a `start.mjs` wrapper that links `node_modules` and writes `PORT` into `appport.toml`, plus `NODE_OPTIONS=--preserve-symlinks`. | – |
| Bundle | `compute bundle create` | Refuses to overwrite an existing output; delete it first | – |
| Release | `compute release` | Complete. Stable endpoint 20000 → instance 30000, answering with the app's own 401. | 28 s |
| State | – | The app's FeltDB lives in the ephemeral instance workspace and is **lost on every release** unless `FELTDB_URL` points at a server | – |

**Totals**

- **Time to first running app, locally:** under 1 minute to a 401. About 20 minutes to an authenticated call, and even then the route fails because of the template bug.
- **Time to first running app on Compute:** about 1 hour for someone who knows Compute, because of four workarounds.
- **Credentials needed:** none for Compute, apart from an optional API token. An authority module is needed for the app.
- **Configuration files:** `appport.toml`, `compute.toml`, the workload manifest, and the dependency capsule manifest.
- **Repositories needed:** none. Everything is on npm, and Compute is built from source.
- **Undocumented assumptions:**
  - the authority identity shape
  - the fixed port
  - Node ESM and `NODE_PATH`
  - symlinks in dependency capsules
  - state loss without `FELTDB_URL`

## 18. Product vs platform boundary

| Platform: shared, reusable, no product opinions | Product: opinionated, user-facing |
|---|---|
| Compute (execution, releases, networking) | AppBoundry portal |
| feltdb-server and `@feltdb/core` | Managed FeltDB (a hosted product built on FeltDB) |
| AppPort protocol and SDK | Factory |
| AppPort Services (as a library) | Attn |
| rust-ml-runtime | jev-ui, simple-jev, open-jev |
| AuthBoundry, as *one* authority implementation | AuthBoundry hosted API |
| PAX, create-appport | web-monitor (example) |

Boundary violations found:

1. **AppBoundry forks the AppPort platform packages** instead of consuming them.
2. **AppPort Services docs name AuthBoundry as "the only authority"**, even though the code accepts any module.
3. **Factory hard-codes AuthBoundry**, which is product coupling inside a runner.

## 19. Final readiness matrix

| Component | Build | Tests | Local run | Deployed | Operable | Secure | Upgradeable | Overall |
|---|---|---|---|---|---|---|---|---|
| Compute | verified | verified | verified | implemented (Hetzner kit) | partial | **missing** (S1–S3) | partial | **Needs hardening** |
| feltdb-server | verified | implemented (284 files) | verified | implemented | partial | partial | partial | **Needs hardening** |
| Managed FeltDB | unknown | implemented | – | unknown | unknown | unknown | unknown | **Unknown** |
| AuthBoundry | implemented | implemented | not run | unknown | unknown | partial (development-grade) | unknown | **Needs hardening** |
| AppPort | implemented | implemented | verified (via app) | n/a (library) | n/a | implemented | implemented | **Ready (as a library)** |
| AppPort Services | verified | verified | verified | per app | partial | partial (S8) | verified | **Needs hardening** (template, docs) |
| AppBoundry | implemented | partial (placeholders) | not run | unknown | unknown | unknown | unknown | **Missing** (no Compute path) |
| Factory | implemented | implemented | not run | unknown | unknown | depends on AuthBoundry | partial | **Needs hardening** |
| rust-ml-runtime | implemented | implemented | not run | n/a | n/a | missing (server) | implemented | **Ready (in-process) / Missing (as a service)** |
| Laya | – | – | not run | – | – | – | – | **Prototype** |
| Jev | – | – | not run | – | – | – | – | **Prototype** |
| Attn | implemented | implemented | blocked (macOS) | desktop | – | – | – | **Prototype** |
| PAX | verified | – | verified | n/a | n/a | n/a | – | **Ready** |
| llm | implemented | implemented | not run | n/a | n/a | n/a | implemented | **Ready (as a library)** |

**Ready** (for the scope stated):

- Compute single-node releases for *trusted* workloads on a *trusted* network
- AppPort libraries
- rust-ml-runtime in-process
- PAX
- llm
- FeltDB backup and restore

**Needs hardening:**

- Compute: sandbox, API authentication/TLS, data-plane survival, the concurrency defect, and FeltDB-backend latency
- feltdb-server: its own readiness document lists the gaps
- AuthBoundry: development-grade identity
- AppPort Services: template, fixed port, authority documentation
- Factory
- FeltDB version alignment

**Missing:**

- AppBoundry → Compute integration
- Metrics and alerts
- Shared inference, and Laya/Jev as services
- Compute HA
- A shared secret service
- A Compute binary upgrade procedure

## 20. Prioritized backlog

| P | Problem | Evidence | Components | Customer impact | Platform impact | Scope | Depends on |
|---|---|---|---|---|---|---|---|
| **P0** | Process runtimes are unsandboxed | §6 runtime table; S1 | Compute | Any workload can read other tenants' data and node secrets | Blocks multi-tenant use | L | – |
| **P0** | Compute API: open reads, one token, no TLS | §9; S2, S3 | Compute | Configuration and logs readable by anyone who reaches the port | Blocks remote operation | M | – |
| **P0** | Same-task concurrent runs lose receipts (79/80 return 403) | §6 API; `execute.rs:697` | Compute | Work runs, but the caller is told it didn't; evidence is lost | Breaks the receipt guarantee | S | – |
| **P1** | Endpoints die with the daemon | §8 | Compute, compute-network | 4.7 s outage on every daemon restart or upgrade | No zero-downtime *platform* upgrade | L | – |
| **P1** | Compute will not start while FeltDB is down | §8 | Compute | Node reboot during an outage means a total outage | Couples availability | M | – |
| **P1** | FeltDB-backend control-plane latency (0.4–5 s per call) | §7; drills | Compute, compute-state-feltdb | Slow CLI and UI | Limits automation | M | – |
| **P1** | AppBoundry → Compute path missing | §3 | AppBoundry, Compute | Portal promise not met | Product gap | L | P0 API auth |
| **P1** | Two sources for `@appport/*` | §2; S10 | AppPort, AppBoundry | Version confusion | Supply chain | S | – |
| **P1** | Managed FeltDB is a single machine | §2 | Managed FeltDB | Tenant outage or data loss window | – | M | – |
| **P1** | Metrics and alerts absent | §4 | All | Failures unseen | Operability | M | – |
| **P2** | Self-hash adds about 75 ms to each WASM/native run | §6 | compute-runtime | Latency | 4× throughput loss for WASM | S | – |
| **P2** | O(n) reconcile and views (0.9 s per add at 300 services); sequential shutdown | §6 capacity | Compute | Slow at scale | Density ceiling | M | – |
| **P2** | Node ESM apps can't use dependency capsules; `deps create` rejects `.bin` symlinks | §17 | Compute | App onboarding friction | DX | S | – |
| **P2** | create-appport: fixed port 8787, `PORT` ignored, `listJobs()` template bug, undocumented authority shape | §17 | AppPort Services | First app fails | DX | S | – |
| **P2** | FeltDB versions drift (0.11.0 / 0.11.2 / 0.11.6 / 0.11.7) | §11 | 8 consumers | Behavior skew | Upgrade risk | S each | feltdb.mjs |
| **P2** | `backup create/restore` exit 1 with a bare relative path | backup drill | feltdb-server | Scripts misreport success | Operability | S | – |
| **P2** | Unauthenticated inference server | S6 | rust-ml-runtime | – | Blocks a shared Laya | S | – |
| **P2** | AppBoundry placeholder tests | S9 | AppBoundry | – | False confidence | S | – |
| **P2** | Compute CI not run on push | S12 | Compute | – | Regressions | S | – |
| **P2** | App state lost on release without `FELTDB_URL` | §17 | Compute, AppPort Services | Data loss | – | S (docs or default) | – |
| **P3** | No scheduled workloads in Compute | §4 | Compute | – | Platform jobs need an external cron | M | – |
| **P3** | Attribution of who performed operations | S4 | Compute | – | Audit | M | P0 API auth |
| **P3** | Exercise the pnpm and bun upgrade paths, and cross-version feltdb-server data upgrade | §11 | FeltDB | – | Upgrade confidence | S | – |

## 21. Reproducing this audit

```bash
cargo build --release -p compute-cli
python3 scripts/platform-audit/measure_compute.py --out docs/platform-audit-evidence/compute-measurements.json
FELTDB_SERVER_BIN=/path/to/feltdb-server python3 scripts/platform-audit/control_plane_drills.py \
    --out docs/platform-audit-evidence/control-plane-drills.json
FELTDB_SERVER_BIN=/path/to/feltdb-server scripts/platform-audit/feltdb_backup_drill.sh
node scripts/platform/feltdb.mjs status --root ~/src --json
```

These harnesses start real daemons, FeltDB servers and hundreds of service processes. Run them on a disposable host.
