# Provider matrix

Audited 2026-09-27 at `69b70d9`. Machine-readable: the `capabilities`
entries with area `provider` and `placement` in [audit.json](audit.json).

"Provider" names three things in Compute:

1. **Pool members** — what placement chooses between: `local` (the engine
   in-process) and `remote` (`compute serve`, `compute.remote@1`). A daemon
   node is also a provider (`/compute/*`).
2. **Session providers** — the substrate inside a `compute serve` target
   that makes computers: `workspace` and `container`.
3. **DNS providers** — for domains and ACME.

Only remote pool members with a session provider are **targets**: the things
computers are placed on.

## Matrix

| Provider | Kind | Hosts computers | Runs jobs | Provisioned by Compute | Authenticated | Verified here | Status |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `local` | pool member | no (`sessions_unsupported`) | yes | n/a (in process) | n/a | yes | IMPLEMENTED + VERIFIED |
| `compute serve` | pool member / target | yes | yes | no — started by `compute up` locally, by hand elsewhere | **no** (`AllowAllAuthorizer`) | yes | IMPLEMENTED + VERIFIED (unauthenticated) |
| daemon node (`/compute/*`) | pool member | no | yes | n/a | yes (daemon scopes) | tests | IMPLEMENTED + VERIFIED |
| workspace | session provider | yes | yes (in session) | n/a | via its target | yes | IMPLEMENTED + VERIFIED |
| container (docker/podman) | session provider | yes | yes (in session) | n/a | via its target | fake docker only | IMPLEMENTED |
| Firecracker / KVM | session provider | — | — | — | — | — | MISSING |
| Fly Machines | provisioning adapter | — | — | — | — | — | MISSING |
| Railway | provisioning adapter | — | — | — | — | — | MISSING |
| Render | provisioning adapter | — | — | — | — | — | MISSING |
| Cloud VM / bare metal | provisioning adapter | — | — | — | — | — | MISSING |
| DNS providers | network | n/a | n/a | n/a | provider credentials | tests | IMPLEMENTED + VERIFIED |

A target is any machine that already runs `compute serve` and is named in the
pool file. Compute cannot create one: there is no provisioning interface.
"Put this software on Railway" is not expressible today.

## Capabilities

<!-- audit:capabilities area=provider,targets -->
| ID | Capability | Status | User can use | In complete model | Notes | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| `target-inventory` | Targets listed with health, platform, resources, capabilities, features | **IMPLEMENTED + VERIFIED** | CLI/API only | yes | Not shown in the UI. | `crates/compute-cli/tests/computers.rs`<br>CLI `compute target list`<br>API `GET /targets` |
| `provider-local` | Local provider (in-process engine) | **IMPLEMENTED + VERIFIED** | yes | yes | Runs workloads; does not host computers (sessions_unsupported). | `crates/compute-provider/src/lib.rs#LocalProvider`<br>`crates/compute-cli/tests/cli.rs` |
| `provider-remote` | Remote provider (`compute serve`, compute.remote@1) | **IMPLEMENTED + VERIFIED** | yes | yes | The only kind of target that hosts computers. | `crates/compute-provider/src/lib.rs#RemoteProvider`<br>`crates/compute-provider/tests/remote.rs`<br>`crates/compute-cli/tests/remote_pool.rs` |
| `provider-daemon-node` | A daemon node as a provider (deployments, /compute/*) | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-cli/tests/product.rs` |
| `provider-fly` | Fly Machines | **MISSING** | no | yes | — | — |
| `provider-railway` | Railway | **MISSING** | no | yes | — | — |
| `provider-render` | Render | **MISSING** | no | yes | — | — |
| `provider-cloud-vm` | Cloud VMs / bare metal provisioning | **MISSING** | no | yes | — | — |
| `provider-dns` | DNS providers (for domains) | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-network/src/dns.rs`<br>`crates/compute-network/tests/providers.rs`<br>`crates/compute-environment/tests/network.rs` |
<!-- /audit -->

## Placement

Placement matches a computer's requirements against each target and gives a
reason for every refusal.

<!-- audit:capabilities area=placement -->
| ID | Capability | Status | User can use | In complete model | Notes | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| `placement` | Capability-matched placement with reasons | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-placement/src/matching.rs`<br>`crates/compute-placement/tests/matching.rs`<br>`crates/compute-placement/tests/selection.rs` |
| `placement-override` | Constrain placement to a target | **IMPLEMENTED + VERIFIED** | yes (dialog field) | yes | — | `crates/compute-environment/tests/computers.rs#placement_chooses_a_target_by_what_the_computer_needs` |
| `placement-dead-options` | UI offers persistent storage / public endpoint | **BROKEN** | no | yes | No provider offers either; any computer requesting them is refused (with reasons). | `crates/compute-provider/src/sessions.rs#capabilities`<br>`crates/compute-provider/src/containers.rs#capabilities`<br>journey `experiments.json#placement_refusals` |
<!-- /audit -->

<!-- audit:placement -->
| Placement understands | How |
| --- | --- |
| gpu | label only |
| networking | network policy only |
| persistence | capability no provider offers |
| runtimes | workload runtimes yes; computer substrates no |
| storage | disk bytes only |
| virtualization | label only |
<!-- /audit -->

Refusals observed on this host (from `experiments.json#placement_refusals`):

| Requested | Result |
| --- | --- |
| `persistent_storage` | refused: `session_capability_unsupported` — no provider offers it |
| `public_endpoint` | refused: `session_capability_unsupported` |
| `terminal` | refused: `session_capability_unsupported` |
| feature `gpu` | refused: `target_feature_unsupported` |
| feature `containers` | **accepted and placed as a workspace** — the feature is inferred from a docker binary with no engine |

The UI's "Create a computer" dialog offers persistent storage and a public
endpoint as checkboxes; selecting either makes the computer impossible to
place (G-PLACE-1).
