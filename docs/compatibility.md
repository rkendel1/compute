# Base and configured Compute compatibility

Compute has two release contracts.

**Base Compute** is the independently installable execution substrate: environments,
computers, sessions, executions, workspaces, providers, placement, Reality, evidence,
receipts, checkpoints, restore, clone, replace, fork, and configuration. Its certified
Linux distribution includes the Compute binary and pinned runtime bundle. It has no
runtime dependency on GitHub, AppPort, AuthBoundry, Factory, Attn, or an npm install.
The memory and file `compute-state` implementations make standalone operation real;
FeltDB is the configured durable authority for managed control-plane deployments, not
an npm dependency of the Base Compute executable.

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
platform, Node version, checks, and timestamp. It contains no environment values or
credentials.

The release invariant is:

> Base Compute is a complete execution substrate. Configured Compute is an assembly of
> Base Compute with independently released capabilities and services. No package is
> ecosystem-compatible merely because it compiles in a source workspace; a supported
> assembly has a reproducible published-artifact compatibility contract.
