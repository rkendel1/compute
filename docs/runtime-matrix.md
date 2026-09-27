# Runtime matrix

Audited 2026-09-27 at `69b70d9` on Linux x86_64 (4 CPU, 16 GiB, no
`/dev/kvm`, no GPU, a docker client with no engine); re-audited for the
foundation the same day. The foundation added no runtime or substrate: what
changed is that every computer substrate is now observed. A computer on a
workspace (or container) target is confirmed with its target every 10 s;
a target that does not answer makes it `unreachable`, and a substrate that
no longer has the machine (the workspace directory, the container) makes it
`lost` — verified for workspaces, unverified for containers against a real
engine. The machine-readable
form is the `runtimes` key of [audit.json](audit.json); the tables are
generated from it.

Compute has **two different things called "runtime"**, and a reader must keep
them apart:

- **Workload runtimes** run one program: `compute run`, `compute pool run`,
  jobs on a target, and workloads of node environments. They come from a
  pinned catalog (`compute runtimes`), download on demand, and are tested by
  one conformance suite.
- **Computer substrates** are what a computer *is*: the session provider on
  its target. There are two: `workspace` (the default: a private directory
  and native processes on the target host) and `container` (docker/podman).
  Inside a computer, programs run with whatever the target host has on
  `PATH`; the workload-runtime catalog is not used there.

## Matrix

<!-- audit:runtimes -->
| Runtime | Implemented | Discoverable | Placeable | Executable | UI | CLI | Tests | Production ready |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| native (process) | yes | yes | yes | yes | computers (workspace) | yes | yes | no (no isolation boundary) |
| language runtimes (node, bun, deno, python, ruby, php, jvm, dotnet, shell) | yes | yes | yes | yes | no (workload engine) | yes | yes | partial (process isolation only; landlock/netns where available) |
| wasm (wasmtime, WASI p1) | yes | yes | yes | yes | no | yes | yes | workload engine only |
| containers (docker/podman) | yes | inferred from a binary on PATH | as a feature label, not as a substrate | unverified (fake docker only) | no | --containers / --session-provider container | fake docker | no |
| kvm | no | yes | label only | no | feature field | no | matching only | no |
| firecracker | no | binary + /dev/kvm | label only | no | feature field | no | matching only | no |
| gpu | no | /dev/nvidia0 exists | label only | no | feature field | no | matching only | no |
<!-- /audit -->

## What each column means here

- **Implemented**: code exists that does it.
- **Discoverable**: a target reports it (`compute runtimes`, `compute
  isolation`, `GET /targets` features).
- **Placeable**: placement can require it and choose a target by it.
  "Label only" means placement matches a string the target advertises;
  nothing checks the capability works.
- **Executable**: something actually ran on it in this audit or in tests.

## Findings

<!-- audit:capabilities area=runtime,discovery -->
| ID | Capability | Status | User can use | In complete model | Notes | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| `discovery-resources` | CPU count, memory, disk, OS/architecture discovered | **IMPLEMENTED + VERIFIED** | indirect | yes | — | journey `GET /compute/capabilities on this host: 4 CPU, 16.9 GB, 270 GB, linux-x86_64` |
| `discovery-runtimes` | Language runtimes discovered (installed/available/ready) | **IMPLEMENTED + VERIFIED** | CLI | yes | — | CLI `compute runtimes`<br>CLI `compute doctor` |
| `discovery-isolation` | Isolation facilities discovered (landlock ABI, network namespaces, cgroups) | **IMPLEMENTED + VERIFIED** | CLI | yes | — | CLI `compute isolation` |
| `discovery-features` | Target features: kvm, virtualization, firecracker, containers, gpu | **PARTIAL** | indirect | yes | `containers` is inferred from a docker/podman binary on PATH: advertised on this host with no engine running. gpu = /dev/nvidia0 exists. kvm = /dev/kvm openable. No nested-virtualization, GPU model, or engine liveness check. | `crates/compute-provider/src/lib.rs#detect_target_features`<br>journey `experiments.json#placement_refusals` |
| `discovery-network` | Network interfaces, reachability, exposed ports | **MISSING** | no | yes | Not discovered. Endpoint hosts come from the pool endpoint URL. | — |
| `discovery-automatic-targets` | Targets discovered automatically (no configuration) | **MISSING** | no | yes | Targets come from a pool file. The launcher writes one naming the local host; no LAN/cloud discovery. | — |
| `runtime-wasm` | WASM workloads (wasmtime, WASI p1) | **IMPLEMENTED + VERIFIED** | CLI (compute run) | yes | Workload engine only: a computer cannot be a WASM sandbox. | `crates/compute-runtime-wasm/src/lib.rs`<br>`crates/compute-runtime-wasm/tests/conformance.rs`<br>`crates/compute-runtime/tests/conformance.rs` |
| `runtime-process` | Process runtimes: node, bun, deno, python, ruby, php, jvm, dotnet, native, shell | **IMPLEMENTED + VERIFIED** | CLI (compute run), daemon workloads | yes | Pinned distributions download on demand (python verified here). `compute run script.py` fails by default: network "none" is unenforceable for process runtimes. | `crates/compute-runtime-process/src/lib.rs`<br>`crates/compute-runtime/tests/conformance.rs`<br>journey `experiments.json#runtimes` |
| `substrate-workspace` | Computers as private workspaces on the target host (native processes) | **IMPLEMENTED + VERIFIED** | yes | yes | A directory with a shell: not an isolation boundary. | `crates/compute-provider/src/sessions.rs#WorkspaceSessionProvider`<br>`crates/compute-provider/tests/sessions.rs`<br>`crates/compute-environment/tests/computers.rs`<br>`crates/compute-cli/tests/product_journey.rs` |
| `substrate-container` | Computers as containers (docker/podman) | **IMPLEMENTED** | CLI flag only | yes | Tested against a fake docker script only; never run against a real engine in this audit (none available) or in CI. `compute up --containers` / `compute serve --session-provider container`. | `crates/compute-provider/src/containers.rs`<br>`crates/compute-provider/tests/sessions.rs#the_container_adapter_translates_the_session_contract` |
| `substrate-firecracker` | Firecracker microVM computers | **MISSING** | no | yes | Only a feature label for placement. | — |
| `substrate-kvm` | KVM virtual machine computers | **MISSING** | no | yes | Only a feature label for placement. | — |
| `substrate-wasm` | WASM computers | **MISSING** | no | yes | — | — |
<!-- /audit -->

Observed on this host:

- `compute run main.py` (the README's first example) fails:
  `placement_failed: network_unsupported` — the default network policy is
  `none`, which the process runtime cannot enforce for Python. With
  `--network network` the pinned Python downloaded (`available → ready`) and
  ran.
- The host advertises the `containers` feature because a `docker` binary is
  on `PATH`; no engine is running. A computer that requires `containers` is
  placed, and runs as a workspace.
- `gpu`, `kvm`, `firecracker` would be advertised from `/dev/nvidia0`,
  `/dev/kvm`, and a `firecracker` binary. Nothing can create a VM or give a
  computer a GPU.

## Runtimes by surface

| Runtime | `compute run` / pool | Target jobs | Node workloads | Computers |
| --- | --- | --- | --- | --- |
| wasm | yes | yes | yes | no |
| node, bun, deno, python, ruby, php, jvm, dotnet | yes (pinned) | yes | yes | host `PATH` only |
| native, shell | yes | yes | yes | yes (workspace) |
| container | no | no | no | adapter, unverified |
| microVM / VM | no | no | no | no |
