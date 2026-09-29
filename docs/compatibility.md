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

Both are Homebrew products built around the same certified executable:

```sh
brew install compute             # Base Compute
brew install compute-configured  # Base plus the pinned configured artifact
```

`compute-configured` depends on `compute`; it never builds or carries another Compute
binary. Its release asset contains the exact registry packages, lockfile, configured
stack, verifier, and evidence produced by distribution CI. The wrapper exposes the
installed stack through `COMPUTE_STACKS`; `compute-configured-verify` reruns the shipped
contract without resolving packages or consulting `latest`.
`compute-configured-setup` is an idempotent validation/activation check; activation is
process configuration, so it does not create another state directory or mutate
`COMPUTE_HOME`.

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
and validates every locked component. A passing report says `certified` and carries a
content-derived certification identity. Version or manifest drift fails the verifier;
the installation is then present but must not be described as a certified composition.
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
├── @appport/services 0.4.6 ── @feltdb/core 0.11.9
├── @feltdb/core 0.11.9
├── @authboundry/core 1.15.3
│   ├── @feltdb/core 0.11.5
│   └── @appport/services 0.3.2 ── @feltdb/core 0.11.1
└── @appport/appboundry 1.1.1
```

These are runtime dependencies from the published manifests, not inferred product
relationships. FeltDB's React peer dependencies are optional. TypeScript and Node type
packages in these releases are development-only. The three FeltDB clients are a real
published-version skew and are therefore recorded explicitly rather than flattened or
hidden. The configured verifier fails if any unrecorded skew appears.

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
