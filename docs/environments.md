# Environments

Compute runs software in **environments**. An environment is a deployed,
isolated place such as `dev`, `staging`, or `prod` on one machine. It holds
**projects**, and each project holds **workloads**. Each time a workload runs,
that run is an **execution**.

```text
Compute (the daemon)
└── Environment   staging            deployed instance, isolated
    ├── Project   authboundry        software, at a revision
    │   ├── Workload  api            service: stays running
    │   └── Workload  migrate        task: runs to completion
    │       └── Execution exec_…     one invocation, with a receipt
    └── Project   billing
```

The model is `compute.environment@1`. The persistent Compute daemon operates
it ([docs/daemon.md](daemon.md)). You don't need systemd, Docker, or
Kubernetes.

Compute is not a package manager, a deployment DSL, Compose, a CI engine, a
secrets manager, or a service mesh. A project's manifest says which already
built bundles to operate. It does not say how to build them.

## Quick start

```sh
compute start --detach                       # persistent daemon on 127.0.0.1:8787
compute environment apply compute.environment.toml
compute environment status staging
compute workload run authboundry/migrate --environment staging
compute workload logs authboundry/api --environment staging
compute project restart authboundry --environment staging
compute stop                                 # desired state is kept
```

## Desired and actual state

Every level stores a **desired state**, `running` or `stopped`, which is what
you asked for. It also reports an **actual state**, which is what the daemon
observes. The daemon reconciles the two continuously and after every change.

| Actual state | Meaning |
| --- | --- |
| `pending` | Not started yet, or a task that has not been run |
| `starting` / `stopping` | Transition in progress |
| `running` | The service process is alive |
| `stopped` | Stopped on request, or the service exited successfully by itself |
| `completed` | The task finished successfully |
| `failed` | Exited non-zero, timed out, or could not start |
| `denied` | Admission refused the execution. Nothing ran |
| `degraded` | Project or environment level only: some children are not healthy |

A service runs when its own desired state and those of its project and
environment are all `running`. Stopping a parent keeps each child's own
desired state, so starting the parent again restores exactly what was
running before.

A service that exits on its own stays down until you start it again, unless
its restart policy says otherwise. The daemon does not loop a crashing
process.

| `restart` | Behavior |
| --- | --- |
| `never` (default) | An exit is final until an explicit start or restart |
| `on_failure` | A non-zero exit or timeout restarts the service after a short delay. A denial never restarts |

## Lifecycle invariant

> Stopping a child never stops its parent or its siblings.

| Operation | Affects | Never affects |
| --- | --- | --- |
| `compute stop` (daemon) | Every service, which stops. Desired state is kept, and `compute start` restores it | Stored definitions |
| `environment stop/restart` | That environment's services | Other environments |
| `project stop/restart` | That project's services | Sibling projects, and the environment |
| `project add` (new revision) | That project, which is replaced and restarted | Sibling projects |
| `project remove` | That project, which is stopped and deleted | Sibling projects |
| `workload stop/restart` | That service | Sibling workloads |
| a service failure | That service | Anything else |

The certification suite checks every row
(`crates/compute-environment/tests/environments.rs`).

## Isolation

Each environment has its own:

- **Filesystem.** State, bundles, logs, and receipts live under
  `<state>/environments/<name>/`. Every execution also runs in its own
  isolated Compute workspace.
- **Configuration.** Environment and project `env` values are visible only
  to that environment's workloads.
- **Dependencies.** Each workload is a self-contained bundle. Dependency
  capsules are part of the bundle identity.
- **Policy.** An environment policy is intersected with the daemon's policy
  (see [Admission](#admission)).
- **Ports.** Each environment gets its own host port for every logical port
  (see [Ports](#ports)).
- **Processes.** Services run in their own process groups. Stopping one
  signals only its own group.

The same project can be deployed to several environments on one machine
without conflicts. `staging` and `prod` can both run `authboundry`, on
different host ports, with different configuration and policy.

Configuration is layered, and later layers win:

1. the workload's own `env`
2. the environment's `env`
3. the project's `env` for this environment
4. Compute-owned values: `COMPUTE_PORT_<NAME>` and `PORT`

Names starting with `COMPUTE_`, and `PORT`, are reserved and rejected in
manifests. Because configuration is part of the workload, the bundle that
executes is derived from the stored bundle, and its receipt identifies what
actually ran.

## Services and tasks

| | Service | Task |
| --- | --- | --- |
| Runs | While its desired state is `running` | When invoked with `compute workload run` |
| Ends | When stopped, or when it exits | When it completes |
| Logs | Streamed live to `logs/<n>/stdout.log` and `stderr.log` | Captured in the execution record |
| Placement | The daemon's own node | Any compatible provider in the daemon's pool |
| Receipt | Written when the execution ends | Written when the execution ends |

Services use the same admission, workspace, isolation, and receipt path as
every other execution. A stop is a host-side cancellation of a controlled
execution. It kills the process group.

## Ports

A project declares **logical** ports:

```toml
ports = [{ name = "http", port = 8000 }]
```

The daemon binds each one to a stable host port from its range. The default
range is `20000-29999`. The binding is stored in `<state>/ports.json` and
kept across restarts and redeployments. The workload receives:

- `COMPUTE_PORT_HTTP=<host port>`
- `PORT=<host port>`, when the workload declares exactly one port

`compute project status` shows `http:8000→20000`. A service that declares
ports is `healthy` only while every declared port accepts TCP connections.

Compute does not proxy, load-balance, or publish ports. A reverse proxy in
front of the host ports is up to you.

## Admission

Environments reuse the existing admission boundary. There is no second
policy system. Every execution of a service or task in an environment is
admitted by the intersection of these policies:

| Source | From |
| --- | --- |
| Baseline | Always |
| Local | `compute start --policy FILE` (the daemon's policy) |
| Environment | The environment's `policy` |
| Provider | A remote provider's advertised policy, for tasks placed there |

An environment policy can only restrict. A denied execution never reaches a
runtime. The workload's state becomes `denied` with the reasons, and its
evidence carries the `admission_id`. `compute environment inspect` shows the
environment's effective `policy_id`.

## Placement

Tasks use ordinary placement over the daemon's pool. With no
`--pool-config`, the pool contains only the daemon's own node (`local`). With
`compute start --pool-config pool.toml`, a task can be placed on any
compatible member. Environment `provider = "name"` pins tasks to that member.
This uses the same explicit placement as `compute pool run --provider`. Services always run on the
daemon's node. A service whose environment pins a remote provider is
`failed` with that reason. It is never silently moved.

## Receipts

Every execution's receipt binds its scope, alongside `placement` and
admission:

```json
"scope": {
  "environment_id": "env_…", "environment": "staging",
  "project_id": "prj_…", "project": "authboundry", "revision": "abc123",
  "workload_id": "wl_…", "workload": "api", "workload_kind": "service"
}
```

Receipts are stored under `<state>/environments/<name>/receipts/` and can be
checked with `compute receipt verify`.

## Manifests

### Environment manifest

`compute environment apply compute.environment.toml` creates the environment
if needed, and then adds or redeploys each listed project:

```toml
[environment]
name = "staging"
desired_state = "running"          # default
env = { LOG_LEVEL = "info" }
provider = "hetzner-1"             # optional: pin tasks to a pool member
# policy_file = "staging-policy.json"

[environment.policy]               # optional compute.policy@1, inline
version = 1
minimum_isolation = "process"

[[project]]
name = "authboundry"
source = "../authboundry"          # directory containing compute.project.toml
revision = "abc123"                # optional: overrides the project manifest
desired_state = "running"          # optional
env = { DATABASE_URL = "postgres://…" }
```

### Project manifest

`compute.project.toml` goes at the root of the project source:

```toml
[project]
name = "authboundry"               # default: the directory name
revision = "abc123"                # default: content-<digest of the bundles>
env = { FEATURE_X = "1" }

[[workload]]
name = "api"
kind = "service"
bundle = "dist/api.compute"        # a built bundle, or:
# workload = "api/workload.json"   # a workload spec, bundled at load time
ports = [{ name = "http", port = 8000 }]
restart = "on_failure"
desired_state = "running"          # default

[[workload]]
name = "migrate"
kind = "task"
workload = "migrate/workload.json"
```

Names are 1-63 lowercase letters, digits, or `-`.

## Inspection

All inspection output is deterministic JSON with `--json`. Tables are the
default.

```sh
compute environment list
compute environment inspect staging --json
compute environment status staging
compute project list --environment staging
compute project inspect authboundry --environment staging --json
compute project status authboundry --environment staging
compute workload inspect authboundry/api --environment staging --json
compute execution exec_… --json
```

An environment view includes its ID, desired, actual, and health states,
effective policy ID, provider pin, disk use, and every project. A project
view includes its ID, revision, and revision digest. Each workload view
includes:

- its ID, kind, and runtime
- its bundle ID
- its latest execution
- its port bindings
- its restart count
- its placement (placement ID, provider, node)
- its evidence (policy ID, admission ID, receipt IDs)
- its resources (limits, disk, network)
- its log directory

These are the same documents the Compute API returns, and what a UI renders.

## Limitations

In `compute.environment@1`:

- CPU usage is not measured and is reported as `"not_measured"`. Memory and
  timeouts are limits, not usage.
- Services run only on the daemon's own node. Tasks can be placed on pool
  providers.
- There are no persistent volumes. Each execution gets a fresh workspace, so
  keep state in external services.
- Health is process liveness plus a TCP check of each declared port.
- One daemon per state directory. There is no multi-node control plane,
  autoscaling, or multi-region.
