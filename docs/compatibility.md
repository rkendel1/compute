# Base and configured Compute compatibility

Compute has two release contracts.

**Base Compute** is the independently installable execution substrate: environments,
computers, sessions, executions, workspaces, providers, placement, Reality, evidence,
receipts, checkpoints, restore, clone, replace, fork, and configuration. Its certified
Linux distribution includes the Compute binary and pinned runtime bundle. It has no
runtime dependency on GitHub, AppPort, AuthBoundry, Factory, Attn, or an npm install.
Base carries the `compute-state-feltdb` boundary and model as its authoritative durable
control-state architecture. The file and memory implementations remain explicitly
development/test backends; they are not a second production state model. FeltDB is a
Base substrate, not a Configured ecosystem package or an npm dependency of the Compute
executable.

**Configured Compute** is Base Compute plus an explicitly selected, independently
released capability stack. The compatibility set is synchronized; package version
numbers are not.

Both are Homebrew products built around the same Compute executable and UI.
Certification is attached to a profile and platform, never inferred merely
from successful installation:

| Platform | Base | Configured |
| --- | --- | --- |
| Linux x86_64 | Certified | Certified |
| macOS ARM64 | Preview | Preview |

```sh
brew install compute             # Base Compute
brew install compute-configured  # Base plus the pinned configured artifact
```

`compute-configured` depends on `compute`; it never builds or carries another Compute
binary. Its release asset contains the exact registry packages, lockfile, configured
stack, verifier, and evidence produced by distribution CI. The wrapper exposes the
installed stack through `COMPUTE_STACKS` and the installed distribution root through
`COMPUTE_CONFIGURED_HOME`; `compute-configured-verify` reruns the shipped contract
without resolving packages or consulting `latest`.
`compute-configured-setup` is an idempotent validation/activation check; activation is
process configuration, so it does not create another state directory or mutate
`COMPUTE_HOME`.

## What `compute-configured` starts

The normal flow is:

```sh
compute-configured-verify
compute-configured-setup
compute-configured
```

`compute-configured` starts two processes:

| Process | Endpoint | Owns |
| --- | --- | --- |
| Compute control plane | `http://127.0.0.1:8787/` | control state, workload execution, the UI |
| AppPort Services (`@appport/services`, pinned) | `http://127.0.0.1:4100` | its own durable state, authentication, management router and UI |

You do not start or register AppPort Services yourself. Compute Configured starts it,
waits until it really serves, and registers it under the fixed name
`appport-services` with the `AppPort/ui/1` capability. Running `compute-configured`
again reuses the running process and reconciles the same registration: it never
creates `appport-services-2`.

Compute then discovers the service the same way it discovers any other registered
service — by asking it for `GET /v1/ui`, checking the document against the
`AppPort/ui/1` contract, and presenting the management links the service contributes.
There is no AppPort-specific code in Compute's UI, and Compute stores only the
registration (a name, a capability and an endpoint).

Two boundaries are deliberate and are not unified:

- **AppPort Services is a separate process with its own HTTP port.** Compute does not
  proxy it, embed it, or forward to it. The management link opens the AppPort Services
  host directly.
- **AppPort Services keeps its own authentication.** Compute passes no operator token,
  cookie or session to it, so reaching its management pages may ask for an AppPort
  Services identity. That is AppPort Services' own boundary, not a Compute one.

`compute-configured` stops both processes, and leaves the state each one owns in place
so the next start reuses it.

### Declaring other services

Which services run is data, not code: the profile lists them under `managed_services`,
each naming an executable inside the distribution, its arguments, its endpoint, and the
path that proves it is ready. Base Compute ships no such list and starts no extra
process, so this is additive.

## Distribution profiles

`compatibility/base-compute.json` and
`compatibility/published-stack/stack.json` implement
`compute.distribution-profile@1`. Each profile records its distribution identity,
Compute artifact/version, platform, exact packages, compatibility contracts,
configuration activation, state boundary, migrations, certification status, and release
provenance.

The Base and Configured profiles have different release assets but share the same
Compute binary, CLI, UI, `compute-state` model, and `COMPUTE_HOME`. Configured is
additive: Homebrew installs Base as a dependency, then installs only immutable profile
assets. Installing or removing the configured formula neither recreates environments
nor owns user state.

The profile verifier compares the installed Base version with the configured manifest
and validates every locked component. On certified Linux, a passing report says
`certified` and carries a content-derived certification identity. Version or manifest
drift fails the verifier; the installation is then present but must not be described as
a certified composition.
For macOS Preview, a passing report says `preview`: the exact composition passed its
compatibility checks, but the platform did not pass the complete Linux certification
suite. The Base runtime manifest likewise records supported Preview runtimes and
Unavailable Linux-only runtimes with reasons. A compatible remote Linux Computer may
satisfy placement; the local macOS installation never pretends native support exists.
The empty, explicit `migrations` list means the current profile has no state migration or
rollback constraint. Future irreversible migrations must declare that limitation before
release.

## Three different results

1. Package compatibility means the registry can resolve and install the declared graph.
2. Contract compatibility means FlowSpec, AppPort, capability, source, and protocol
   identities agree.
3. Runtime compatibility means the installed artifacts perform a real operation and
   recover durable state. Passing `npm install` proves only the first result.

## Code-grounded published graph

The exact graph is captured by `compatibility/published-stack/package-lock.json`; the
important direct edges in the currently supported set are:

```text
@appport/github 1.0.2
├── @appport/sdk 1.1.22
│   ├── @appport/core 1.0.3
│   ├── @appport/server 1.0.2
│   └── @appport/transport-* 1.0.2
├── @appport/services 0.4.10 ─ @appport/protocol 1.0.3, @feltdb/core 0.11.9
├── @appport/services 0.4.6 (nested under @appport/github, which pins it)
├── @feltdb/core 0.11.9
├── @authboundry/core 1.15.3
│   ├── @feltdb/core 0.11.5
│   └── @appport/services 0.3.2 ── @feltdb/core 0.11.1
└── @appport/appboundry 1.1.1
```

These are runtime dependencies from the published manifests, not inferred product
relationships. FeltDB's React peer dependencies are optional. TypeScript and Node type
packages in these releases are development-only. The four FeltDB clients, the three
`@appport/services` versions, and the two `@appport/protocol` versions are real
published-version skews and are therefore recorded explicitly rather than flattened or
hidden. The configured verifier fails if any unrecorded skew appears.

`@appport/services` 0.4.8 is the first published release that serves an `AppPort/ui/1`
document at `GET /v1/ui`; 0.4.7 and earlier publish only a
`[{protocol, id, requiredCapabilities}]` descriptor that is not an `AppPort/ui/1`
document. The configured stack pins **0.4.10**, which serves the same document and
additionally ships a runnable standalone host: `appport-services serve --host … --port …`
actually starts and answers `GET /v1/ui`. (0.4.8 declared that command, but its CLI
exited silently without running, and its `--host`/`--port` flags were unusable, so no
process could be pointed at.) Compute Configured therefore starts 0.4.10, discovers it
through `/v1/ui`, and registers it. See [service-ui.md](service-ui.md).

Compute's model compiler uses exactly `@feltdb/core 0.11.9` to compile
`compute.flow`. The Rust controller speaks FeltDB Protocol 1 through
`compute-state-feltdb`; it does not load the npm package at runtime. The checked-in model
manifest, `CERTIFIED_FELTDB_VERSION`, and the FeltDB consumer workflow remain the
authority for that boundary.

## Verification and evidence

`compatibility/base-compute.json` identifies the certified executable, release asset,
Homebrew channel, platform, and state boundary. Distribution certification proves the
release archive; the tap formula consumes that archive and its pipeline checksum. Tap CI
tests install, `compute --version`, distribution verification, runtime discovery, launch,
and upgrade with the same `COMPUTE_HOME`.

The configured fixture uses only npm registry URLs and integrity hashes. It rejects
`workspace:`, `file:`, and linked packages through its lockfile checks. It then verifies:

- exact package versions and declared version skew;
- `use github { repositories = true }` and `github.repository.read`;
- AppPort and packaged FlowSpec contracts;
- the actual nested FeltDB 0.11.1 → 0.11.5 → 0.11.9 clients writing,
  closing, reopening, reading, and updating the same durable state;
- the published GitHub package producing a provider-neutral immutable Git source.

Run it with:

```bash
cd compatibility/published-stack
npm ci --ignore-scripts
COMPUTE_COMPATIBILITY_EVIDENCE=evidence.json npm run verify
```

CI uploads evidence containing exact registry URLs, integrity hashes, resolved versions,
distribution provenance, state contract, platform, Node version, checks, and a
content-derived certification identity. Its epoch certification time makes the release
artifact reproducible; it contains no environment values or credentials.

The release invariant is:

> Base Compute is a complete execution substrate. Configured Compute is an assembly of
> Base Compute with independently released capabilities and services. No package is
> ecosystem-compatible merely because it compiles in a source workspace; a supported
> assembly has a reproducible published-artifact compatibility contract.
