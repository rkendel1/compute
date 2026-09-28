# Base capture vs. complete Compute

**Base capture** is what the code at `69b70d9` does and was verified to do
on 2026-09-27. **Complete Compute** is the product definition: any machine
(container, microVM, VM, bare metal, cloud) becomes a durable computer; any
project runs on it; software moves build → test → publish → deploy →
promote → operate → roll back from one control plane that people and agents
use the same way, with FeltDB as the durable authority.

The comparison never counts a design document as a capability: "Base
capture" says only what runs. Generated from [audit.json](audit.json)
(`base_vs_complete`, `readiness`).

## Comparison

<!-- audit:base_vs_complete -->
| Capability | Base capture (today) | Complete Compute | Gap |
| --- | --- | --- | --- |
| Computer abstraction | Environments with a durable computer (workspace on a `compute serve` target) | Any machine: container, microVM, VM, bare metal, cloud | Substrates beyond workspaces |
| Persistent environments | Yes, verified | Yes | None |
| Ephemeral environments | Yes, verified (expire, evidence kept) | Yes | None |
| Self-discovery | CPU/memory/disk/OS/arch, runtimes, isolation facilities; features by device/binary presence | Machines, capabilities, networks, GPUs, virtualization — live | Liveness, networks, automatic target discovery |
| Native runtime | Yes (workspace computers; process workloads) | Yes, isolated | Isolation |
| Containers | Adapter exists; unverified against a real engine | Verified, default substrate | Verification, images, volumes, ports |
| WASM | Workload engine (compute run); not a computer | WASM computers/sandboxes | Substrate |
| Firecracker | Feature label only | MicroVM computers | Everything |
| KVM | Feature label only | VM computers | Everything |
| Capability placement | Yes, with reasons; features are labels | Yes, against verified capabilities | Verified features, storage, public endpoints |
| Multiple projects | Yes, verified | Yes | None |
| Application assembly | Proposal from source + GO, verified | Yes, plus services/images/volumes | Managed services |
| Build | Yes, in the computer | Yes | None |
| Test | Yes, in the computer | Yes | None |
| Publish | Versions: commit + digest + evidence | Versions with stored artifacts | Artifact storage |
| Deploy | Rollouts to environments, in place, verified | Zero-downtime | Traffic switching |
| Promote | Reviewed plan + rollout, verified | With approvals and policy | Approvals |
| Production | An environment named production; no protection | Protected environments, domains, TLS | Policy, ingress for computers |
| Rollback | Yes, verified | Yes | None |
| Operations | Logs (on demand), restart, config, probe health | Streaming logs, metrics, alerts, scaling | Observability, scaling |
| Agent execution | AppPort covers every UI operation; agents are operators | Scoped agent identities | Delegation |
| Provider abstraction | Pool of local/remote targets; no cloud adapters | Fly/Railway/Render/cloud/bare metal | Adapters |
| UI | Work/Manage, home, run, software, operations; verified in a browser | Every capability | Targets, access, diagnosis pages |
| CLI | 182 commands; 52 with help defects | Consistent, documented | Help, naming |
| API | 129 routes, scoped | Versioned, documented | Description |
| Observability | Events, receipts, job evidence, /metrics (API only) | Live logs, metrics, traces | Streaming, dashboards |
| Durable evidence | Events, versions, rollouts, receipts; jobs on targets | Same, in one authority | Jobs outside FeltDB; file default |
| Security boundary | Daemon: real; targets: none | Every hop authenticated; computers isolated | Target auth, isolation |
<!-- /audit -->

## Product readiness

PASS means a user can do it today, end to end, and a test proves it;
PARTIAL means it works with a named limitation; FAIL means the product
promise is broken.

<!-- audit:readiness -->
| Area | Status | Evidence | Blocking gap |
| --- | --- | --- | --- |
| Install | **PARTIAL** | cargo build; release distribution certified in CI | No installer/package; runtimes download on demand |
| Launch | **PASS** | compute → UI in 3.7 s | Browser opener only on desktops |
| Discovery | **PARTIAL** | resources/runtimes/isolation real; features inferred | G-DISC-1, G-RT-2 |
| Placement | **PASS** | matching tests; reasons shown | G-PLACE-1 |
| Computer creation | **PASS** | 0.6 s to running (workspace) | Substrates (G-RT-1) |
| Project execution | **PASS** | journey | Git-only sources |
| Multi-project | **PASS** | journey | — |
| Runtime coverage | **PARTIAL** | workload runtimes yes; computer substrates: workspace only verified | G-RT-1 |
| Sessions | **PASS** | CLI + provider tests | Two session concepts (G-CLI-1) |
| Build | **PASS** | journey | — |
| Test | **PASS** | journey | — |
| Publish | **PASS** | journey | G-REL-1 |
| Deploy | **PASS** | journey | G-DEP-1 |
| Promote | **PASS** | journey | G-PROD-1 |
| Production | **PARTIAL** | an environment; no domains/TLS/approvals for computers | G-APP-1, G-PROD-1 |
| Rollback | **PASS** | journey | — |
| Operations | **PARTIAL** | restart/logs/config/health probe; target liveness | G-OBS-1 |
| UI | **PARTIAL** | journey and certification pass in CI with Chromium; unreachable/lost shown with actions | G-UI-1 |
| CLI | **PARTIAL** | 185 commands; 52 help defects; 87 untested through the CLI | G-CLI-1 |
| Agents | **PARTIAL** | AppPort parity (stub-tested) | G-AGENT-1 |
| Providers | **PARTIAL** | local + remote targets only | G-PROV-1 |
| Security | **PARTIAL** | targets authenticate every request and isolate control planes (demonstrated); computers on one host are not isolated from each other | SEC-3, SEC-4 |
| Recovery | **PASS** | daemon restart, target restart, target outage, machine and session loss, stale answers: demonstrated (experiments.json#foundation) | — |
<!-- /audit -->

## Summary

The base capture is a complete **single-host developer loop**: launch, create
a computer, run projects in it, build, test, publish, deploy, promote, roll
back, operate — every step verified in a browser and from the CLI, durable
across restarts. What separates it from complete Compute is not the loop but
**what a computer can be and who can reach it**: computers are workspaces
without isolation on targets without authentication, targets cannot be
provisioned, and production lacks ingress, zero-downtime rollouts, and
approvals. See [gap-analysis.md](gap-analysis.md) for the ordered path.
