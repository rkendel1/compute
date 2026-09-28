# Stacks

A **stack** is a reusable, versioned, declarative description of the software a
Computer is configured with. An **application** is software that runs on that
configured Computer. Compute is the execution fabric that materializes the
stack on a Computer, runs the application, and produces the evidence.

```text
                     PAX
              project requirements                 (what the project needs)
                      │
                      ▼
                 Stack (data)                      (the configured environment)
     @feltdb/core  @appport/core  @appport/sdk
     @appport/services  @appport/appboundry  @authboundry/core
                      │
                      ▼
                   Compute ── plan · place · materialize · verify ──▶ Computer A
                      │                                              Computer B
                      ▼
            application bundle runs on it              (appboundry.app)
              manifest + application.wasm
                      │
                      ▼
                   Receipt
```

The stack is not `appboundry.app`. The Randy stack *enables* it, and any other
application could run on the same configured Computer.

| Layer | What it is | Where it is declared | Artifact type |
| --- | --- | --- | --- |
| project requirements | what the software needs | PAX (`package.json`, …) | — |
| stack | the configured environment | `stacks/<name>/stack.toml` | **package** (`npm:`) |
| application bundle | the software that runs there | project `compute.toml` `[artifact]` | **manifest + WASM module** |
| Computer capabilities | what a target offers | discovery (`compute capabilities`) | runtimes, platform, … |
| verification | what was observed | the receipt | probe receipts |

## A stack is data

```toml
schema = "compute.stack@1"

[stack]
name = "randy"
version = "0.3.0"

[requirements]
runtime = "node"
version = ">=20"

[[component]]
name = "feltdb"
kind = "package"
source = "npm:@feltdb/core"
version = "0.11.9"
```

It holds artifacts *by identity* and constraints — no commands, no paths, no
secrets. `[[component]]` fields other than these are refused (there is no
place to put an install script or an API key), a credential a component needs
is referenced by **name** (`credentials = ["FELTDB_TOKEN"]`), and the manifest
is refused if a value is pasted where a name belongs. Versions are constraints
in a small grammar: exact, `=`, `>=`, `>`, `<=`, `<`, `^`, `~`.

The **fingerprint** is `sha256` over the canonical declarative contents (name,
version, runtime, components with their sorted credentials and platforms). It
excludes the description, comments, formatting, component order, timestamps,
and paths, so two Computers asked to realize the same stack carry the same
fingerprint. It does not mean the machines are identical.

`randy` and `minimal-node` (`stacks/`) are two instances of one mechanism.
Nothing in Compute knows either name; a new stack is a new directory.
`--stack NAME` looks in `<project>/stacks`, `./stacks`, and `$COMPUTE_STACKS`;
`--stack PATH` names a directory or file; a project may set `[stack] name` in
`compute.toml`.

## How a stack flows through Compute

A stack is not a branch in the executor. It is more requirements:

1. **Requirements.** `Stack::apply` adds the stack's runtime constraint to the
   project's and each package as a runtime dependency (`origin: stack:<name>`).
   They are planned, and materialized, like PAX's own dependencies — a
   `dependency_unavailable` failure names `@feltdb/core (stack:randy)`.
2. **Resolution.** Each package is resolved against the dependency capsule
   (`--deps`) by its constraint. A version outside the constraint, or an absent
   package, is unresolved.
3. **Placement.** The capsule's runtime, version, platform and any application
   runtime take part in capability matching. Component `platforms` are checked
   against the selected Computer: the same stack is realizable on some
   Computers and refused on others (`stack_component_unsupported`).
4. **Verification on the Computer.** After placement, ordinary workloads (probes)
   run on *that* Computer, each with a receipt of its own: one reads each
   package's installed version inside the materialized capsule. Nothing is
   marked verified because it was declared.
5. **Evidence.** The execution's receipt carries the stack (`stack`) next to the
   project, placement, dependencies and result.

## Evidence: declared → resolved → materialized → verified

For each package component the receipt records three checks, recomputed from
the receipt's own evidence when it is verified:

| Check | Satisfied when |
| --- | --- |
| `package resolved` | the capsule inventory has a version satisfying the constraint |
| `package materialized` | the capsule it resolved from is the one verified at execution |
| `package verified` | a probe on the Computer observed exactly the resolved version |

Component state (`declared` → `resolved` → `materialized` → `verified`, or
`verification unavailable`, `unsupported`, `failed`) is derived from these. A
receipt that would record a `failed`, unresolved, or `unsupported` component is
rejected: an execution receipt cannot claim what did not hold.

## The application layer

The application bundle is the *project's*, declared in `compute.toml`:

```toml
[artifact]
application = "dev.appboundry.portal"
version = "1.0.0"
runtime = "wasm"
abi = "wasm/1"
artifact = "sha256:fef9a8b0…"   # optional pin of the module
```

Its files (AppBoundry's `AppBoundry.app/manifest` and `application.wasm`) are
supplied with `--input`. Compute finds the bundle **by content** — a JSON file
with protocol `AppPort/application-bundle/1` declaring that application, and the
module beside it whose SHA-256 the manifest carries — not by file name. It reads
only those identity fields. **It does not parse or certify the manifest**: that
is AppBoundry's job, and it is done by AppBoundry's own API,
`@appport/appboundry`'s `evaluateAppBoundryRuntimeReadiness`, run *on the
Computer, inside the environment the stack materialized*. The stack's
`@appport/appboundry` package is what makes that possible; the application is
what it inspects. Compute records the verdict.

The application's runtime (`wasm`) takes part in placement as an additional
runtime: a Computer without it is rejected, with `runtime_unsupported` and
what it does offer, rather than the application being dropped.

The receipt's `app_bundle` records, for the application:

| Check | Meaning |
| --- | --- |
| `manifest resolved`, `module resolved` | found among the supplied files, consistent with each other |
| `manifest materialized`, `module materialized` | present, byte for byte, in the execution's inputs |
| `certified by the AppBoundry platform package` | AppBoundry's certification passed, for the artifact Compute resolved |
| `runtime capability verified` | a minimal module ran on the Computer's WASM runtime |
| `required providers available` | AppBoundry reports the application runnable (it cannot, today: see below) |
| `application launch verified` | never satisfied by Compute (see below) |

## What Compute cannot yet prove

- **Launching the application.** Compute's `wasm` runtime hosts WASI modules
  (`_start`); AppBoundry's module uses AppPort's `wasm/1` host ABI, which the
  AppBoundry host provides. Compute does not host that ABI, so *launch verified*
  is always `not_evaluated`, with the reason stated. Compute also does not
  establish the providers the application requires (`identity.session.*`,
  `request.context.current`, `feltdb.documents`, `github.repositories.list`);
  AppBoundry, asked with none, answers `PROVIDERS_UNAVAILABLE` and names them.
  The application is therefore never reported `verified`, only
  `verification unavailable`.
- **Certification needs the real package.** Without the real
  `@appport/appboundry` in the capsule, the certification check is
  `not_evaluated` ("not inspected").
- **A per-package artifact identity.** A capsule's inventory records each
  package's name and version and one digest for the whole payload, so a
  component's `artifact` is the payload digest, not the package's own.
- **A stack does not yet require credentials be *used*.** Names are checked
  (`credential_unavailable` if not supplied with `--env`/`--env-file`) and
  recorded; there is no credential store. Values reach only the workload.
- **Capabilities by name.** Computers advertise runtimes, platform, isolation,
  resources and target features. They do not advertise "FeltDB" or "AuthBoundry";
  those are evidenced per Computer by the probe, not matched in advance.
- **Distribution size.** A capsule travels in the request, capped at 64 MiB
  encoded (about 17 MiB of files). The real Randy packages are ~125 MiB, so
  they are referenced instead of embedded (`--deps-by-reference`, with the
  capsule in the target's `$COMPUTE_DEPENDENCY_CACHE` as `<digest>.deps`).
- **Application bundle format.** The manifest in the repositories is named
  `manifest` and the module `application.wasm`. No `.apph` file or
  `appboundry.wasm` exists in the repositories inspected; bundles are found by
  content, so those names would work if they appear.

## Building the capsule

Compute consumes capsules; an external tool produces them (PAX / npm):

```sh
npm install --no-bin-links --omit=dev @feltdb/core@0.11.9 … @authboundry/core@1.15.3
compute deps create --runtime node --resolved <dir containing node_modules> \
  --package @feltdb/core=0.11.9 … --output randy.deps
```

Keep npm's `node_modules/` directory *inside* the resolved directory: the
capsule then unpacks as `dependencies/node_modules/…`, so packages (and ES
modules) resolve their own dependencies. `--no-bin-links` avoids the symbolic
links a capsule refuses.

## The demo

```sh
compute up --stack randy --deps randy.deps --deps-by-reference   # configure a Computer
compute run --stack randy --deps randy.deps --deps-by-reference \
  --input AppBoundry.app/manifest --input AppBoundry.app/application.wasm
```

`compute up` prints the stack's identity, the Computer used, and each
component's actual state:

```text
Stack: randy 0.3.0 (sha256:…)
Target: this-machine
Status: ready for applications
  ✓ appboundry         verified
  ✓ appport-core       verified
  …
```

(`compute up` reads `./compute.toml` as control-plane configuration, so run it
outside a project directory whose `compute.toml` has project tables.)
Multiple Computers: `--provider ID` selects one; every receipt carries the same
stack fingerprint, so the receipts of several Computers compare directly. A
`compute test --all` would be planning and receipts over that same model; it is
not implemented.
