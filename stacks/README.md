# Stacks

Each directory holds one stack: `stack.toml`, a declarative description of the
software a Computer is configured with (see [docs/stacks.md](../docs/stacks.md)).

| Stack | What it is |
| --- | --- |
| `randy` | the reference stack: FeltDB, AppPort (core, sdk, services), the AppBoundry platform package, AuthBoundry |
| `minimal-node` | Node with AppPort's capability runtime; the smallest stack |

A stack lists package artifacts by identity. It never contains an application:
applications (such as `AppBoundry.app`) run *on* a configured Computer and are
declared by the project that runs them.

Compute has no knowledge of any of these names. Add a stack by adding a
directory.
