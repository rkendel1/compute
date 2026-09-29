# Environments on a computer

> **A Compute environment is a durable computer, not a deployment.** It is
> created once, placed on a target that can host it, and then changed in
> place: repositories move to new revisions, packages are installed,
> applications, services, and agents start and stop — without provisioning
> a new machine. Only a change to the machine itself (its CPU, memory,
> architecture, features) replaces it, and only when asked.
>
> How deployment, the UI's Manage and Work modes, work sessions, and
> embedding applications all act on this one environment is
> [environment-control-plane.md](environment-control-plane.md).

```text
Compute daemon (compute start)          the authority
  ├── Environment  myapp                FeltDB: desired state, owner, contents
  └── Computer     cmp_…                FeltDB: status, generation, target, session,
        │                                       observed contents, evidence
        │ placement (requirements → target)
        ▼
Target  target-a (compute serve)        answers; never an authority
  └── persistent session  ses_…         the machine: a workspace or a container
        ├── repos/app       @ v2        ← durable job: git sync
        ├── package deps                ← durable job: install
        └── process api     running     ← durable job: start / stop / probe
```

## What it is

| | |
| --- | --- |
| Identity | The environment's name and `environment_id`; the computer's record ID is derived from it (`cmp_…`) |
| Owner | The operator that created it. Only the owner reads, changes, executes in, or connects to it |
| Lifecycle | `persistent` (the default: kept until destroyed) or `ephemeral` (destroyed when its TTL passes) |
| Requirements | CPU, memory, disk, architecture, network, isolation, session capabilities, target features |
| Target | Where it runs. Placement chooses; `--target` only constrains the choice |
| Desired contents | Repositories, packages, and processes (applications, services, agents, plain processes), with a `generation` |
| Observed contents | What the computer holds, each item with the job and execution that proves it |
| Generation | The requirements' generation (`spec_generation`) and the record's version; every provider answer is fenced by both |

Status:

| Status | Meaning |
| --- | --- |
| `pending` | Recorded; not placed yet |
| `provisioning` | Placed on a target; the session is being created |
| `running` | The machine is up; contents are reconciled continuously |
| `stopping` / `stopped` | Stopped on request; the workspace and checkouts are kept |
| `resuming` | Being started again |
| `failed` | Could not be made or kept (the `failure` says in which phase, with what code, and whether a retry can help) |
| `destroying` / `destroyed` | Destroyed on request; the record, contents, and evidence remain |
| `expired` | An ephemeral computer whose TTL passed; torn down, record kept |
| `unreachable` | Its target did not answer (`target_unreachable`) or refused this control plane's credential (`credential_rejected`). Still wanted; Compute keeps asking, and it is `running` again when the target answers with the same machine |
| `lost` | Its target answered without the machine: the session is gone (`session_missing`), ended, or its provider no longer has the environment (`machine_missing`). Still wanted; never re-provisioned on its own. Replace it (a new machine, the same contents) or destroy it; a reconcile asks the target again |

## Observed reality

Status is what Compute last established, never what the environment wants.
Every running computer is confirmed with its target every 10 s
(`computer_liveness`, answered within `computer_liveness_timeout`),
whatever runs in it; a process that exited is a process failure (the
process probe restarts it), a machine that is gone is a lost computer. Each
change is a durable transition with an event (`computer.unreachable`,
`computer.recovered`, `computer.lost`), and each observation is applied only
to the record version it was made against, so an answer that arrives after a
newer observation found the machine gone cannot bring it back.

The API, `compute environment status`, the UI, and AppPort all show the same
`reality`:

| Field | Meaning |
| --- | --- |
| `desired` | `running`, `stopped`, or `destroyed`: what the environment asks for |
| `observed` | `starting`, `running`, `unverified` (running by its record, not confirmed recently), `reconciling`, `unreachable`, `lost`, `stopping`, `stopped`, `failed`, `destroyed`, `expired` |
| `confirmed_at` | When the target last confirmed the machine, while it runs |
| `since` | When it became unreachable or lost |
| `explanation` | What it means and what to do |

An environment on a computer is what its computer was observed to be:
`compute environment status` reports `actual degraded, health unhealthy`
for an unreachable computer and `actual failed` for a lost one. Work in an
unreachable computer fails with `runtime_unavailable`; in a lost one, with
`conflict`, naming the state.

## Quick start

```sh
# A target: any node that offers sessions, trusting this control plane.
compute target credential issue --credentials /var/lib/compute/target-credentials.json \
  --control-plane prod-cp --token-file target-a.token   # copy the token to the controller
compute serve --listen 0.0.0.0:8080 --public-url https://target-a.example \
  --job-store /var/lib/compute/jobs --session-store /var/lib/compute/sessions \
  --credentials /var/lib/compute/target-credentials.json

# The controller, whose pool names the target and its token:
#   [providers.target-a]  kind = "remote"  endpoint = "https://target-a.example"
#   token_file = "/etc/compute/target-a.token"

# The controller, whose pool names the target.
compute start --detach --pool-config compute-pool.toml

compute target list
compute environment create myapp --cpu 4 --memory 8Gi --persistent
compute environment repo add myapp app --url https://git.example/app.git --revision main
compute environment package install myapp deps --repository app -- npm ci
compute environment service add myapp api --repository app -- npm start
compute environment info myapp

compute environment repo update myapp app --url https://git.example/app.git --revision v2
# The same computer checks out v2 and restarts `api`. Nothing is redeployed.

compute environment exec myapp -- npm test
compute environment logs myapp --process api
compute environment connect myapp
compute environment stop myapp      # kept, with its workspace
compute environment start myapp     # resumed: the same machine
compute environment destroy myapp   # the machine goes; the record stays
```

## Targets

A **target** is a member of the daemon's provider pool that hosts
sessions (`compute serve --offer sessions`, the default). `compute target
list` and `GET /targets` show each one: health, platform, resources,
session capabilities, and **features** — what the machine offers beyond
CPU and memory:

| Feature | Detected when |
| --- | --- |
| `kvm`, `virtualization` | `/dev/kvm` is readable and writable |
| `firecracker` | `kvm`, and a `firecracker` binary on `PATH` |
| `containers` | `docker` or `podman` on `PATH` |
| `gpu` | `/dev/nvidia0` exists |

`compute serve --target-feature kvm` declares a feature explicitly and
replaces detection. Features are part of the target's capability
descriptor (and its `capability_version`), so placement records exactly
what it matched against.

## Placement

A computer is described by what it needs, never by where it runs:

```sh
compute environment create build --cpu 8 --memory 16Gi --feature kvm
```

Placement ([placement.md](placement.md)) evaluates every target with the
computer's requirements as a session request, and records the decision:
compatible targets, incompatible ones with their reasons (`cpu_unavailable`,
`architecture_mismatch`, `target_feature_unsupported`,
`session_capability_unsupported`, …), and the selection. A persistent
computer also requires `claim`, since it must never expire. When nothing
fits, `create` fails with every target's reasons and records nothing.

`--target target-a` constrains placement to that target; an incompatible
target is refused with its reasons rather than tried.

## Desired and observed contents

```json
{
  "repositories": [{ "name": "app", "url": "https://git.example/app.git", "revision": "v2" }],
  "packages": [{ "name": "deps", "install": ["npm", "ci"], "repository": "app" }],
  "processes": [
    { "name": "api", "kind": "service", "command": ["npm", "start"], "repository": "app", "desired": "running" },
    { "name": "reviewer", "kind": "agent", "command": ["./agent"], "env": { "MODE": "review" } }
  ],
  "generation": 7
}
```

Every change is a new generation of this document, written to FeltDB
with an event (`environment.contents_changed`). The controller compares it
with what the computer holds and runs exactly the durable jobs that close
the difference, in order:

1. remove repositories no longer wanted; sync the rest (`repos/<name>`,
   checked out detached at the revision; the commit is recorded);
2. install packages whose definition or repository commit changed;
3. stop processes that are removed or wanted stopped; start (or restart)
   processes whose command, environment, or repository commit changed
   (`setsid`, pid and log under `.compute/processes/`);
4. probe processes (every `computer_probe`, 15 s by default; every second
   while one is not yet ready): exits, exit statuses, and readiness, from
   inside the computer; a process that stopped running as it should is
   restarted only as its restart policy says (below).

Each job's result is written back per item, with its job and execution
ID and its receipt (`OperationEvidence`). A failed item is recorded as
failed and not retried until its definition changes or someone asks
(`compute environment reconcile`), so a broken command never loops. When
every item matches, the computer is `converged` at that generation
(`environment.contents_converged`).

Ways to change contents, all authorized and recorded:

| | |
| --- | --- |
| Item by item | `repo add/update/remove`, `package install/remove`, `process add/start/stop/remove`, `service add`, `agent add` |
| All at once | `compute environment contents apply myapp contents.json --expected-generation 7`: refused with `conflict` if someone changed them since generation 7 |
| The UI | The environment page's **Computer** panel. Edits stay local to the page until **GO** submits the whole draft as one fenced change |
| Programs | `POST /environments/{environment}/contents` with `expected_generation`; `updateComputeEnvironment` in `@compute/appport` |

## Processes: readiness and restart policy

A durable process in a computer has a desired state, an observed state, an
HTTP readiness contract when it serves, and a bounded restart policy. All
of it is in control state, so a controller that restarts carries on from
the record.

```json
{
  "name": "web", "command": ["npm", "start"], "port": 3000,
  "readiness": { "path": "/health", "expect": "2xx", "request_timeout_seconds": 2, "deadline_seconds": 60 },
  "restart_policy": "on_failure", "max_restarts": 5
}
```

```sh
compute environment process add myapp web --port 3000 \
  --ready-path /health --ready-deadline 60 --restart on-failure -- npm start
compute environment info myapp
#   web   service   running   ready   4812   1   npm start
#     readiness: ready (GET /health expects 2xx; last: HTTP 200)
#     restart: on_failure (0 in a row of at most 5)
#     last failure: web exited with status 143 (exited, …): restart 1 of 5 at …
```

**Before this (as audited).** `START_PROCESS` started a process with
`setsid` and recorded its pid; `PROBE_PROCESSES` said only running or
exited. An exited process was started again at the next probe, forever and
uncounted; a failed start was never retried; nothing checked readiness (a
rollout's health check was a TCP connect from the controller). Process
state was `running`, `stopped`, `exited`, or `failed`.

**Readiness.** `readiness` is an HTTP `GET` of `path` on the process's port
(or `readiness.port`), made by the probe job *inside the computer*, so it
checks the process as its machine reaches it. `expect` is a status (`204`)
or a class (`2xx`, the default); redirects are not followed. The job uses
`curl`, `python3`, or `wget`, whichever the computer has; a computer with
none says so (`no HTTP client in the computer`) and the process never
becomes ready. A started process is `starting` until a check answers as
expected, then `ready`; a ready process that stops answering is `unready`.
Starting or unready for longer than `deadline_seconds` is a failure. A
process without `readiness` is `running`, never `ready`. A rollout's health
check requires `ready` for a process that has a readiness check.

**Restart policy.** When a desired-running process exits, fails to start,
or misses its readiness deadline, Compute records the failure (reason,
message, exit status, the job that established it) and what its policy
decides:

| Policy | Restarts after |
| --- | --- |
| `never` | nothing: it stays exited or failed |
| `on_failure` | a non-zero or unknown exit status, a failed start, a missed readiness deadline; not a clean exit (status 0) |
| `always` (default) | any of them, a clean exit included |

Restarts are bounded: at most `max_restarts` (5 by default) in a row, after
1, 2, 4, … seconds (at most a minute). Becoming ready, or running 30 s
without a readiness check, ends the row. After the bound the process stays
`failed` or `exited`, with why, until it changes or someone asks
(`compute environment reconcile`, which starts it again with its count
kept). A restart policy change applies in place; it restarts nothing.
Stopped means stopped: a process whose desired state is `stopped` (or
whose computer is stopped) is never restarted, whatever its policy.

**State.** `observed.processes.<name>` holds `state` (`starting`,
`running`, `stopped`, `exited`, `failed`), `readiness` (`starting`,
`ready`, `unready`, since when, the last answer, the probe job),
`restarts` (automatic restarts on this machine), `attempts` (restarts in a
row), `retry_at` (the next restart), `last_failure`, and `started_at`.
`reality.processes.<name>` is the account every surface shows: `desired`,
`process` (`pending`, `starting`, `ready`, `unready`, `running`, `stopped`,
`exited`, `failed`, or the machine's own state when it is not confirmed,
such as `lost`), `readiness`, `restarts`, `next_restart_at`, and
`last_failure`.

| From | Event | To |
| --- | --- | --- |
| — / `exited` / `failed` / `stopped` | a start is decided: new or changed, due by its policy, resumed, or asked for; recorded, fenced | `starting` |
| `starting` | its start job succeeds | `running` (readiness `starting` if it has a check) |
| `starting` | its start job fails | `failed` (policy decides) |
| `running`, readiness `starting` / `unready` | a check inside the computer answers as expected | readiness `ready` |
| `running`, readiness `ready` | a check does not | readiness `unready` |
| `running`, readiness `starting` / `unready` | past `deadline_seconds` | `failed` (still running, unready; policy decides) |
| `running` | it is no longer running | `exited`, with its exit status (policy decides) |
| any | desired `stopped` (or the computer stops) | `stopped`; nothing restarts it |

**Recovery and fencing.** A start is recorded (`starting`) in the
`Computer` record, fenced on the version the driver read, *before* its job
runs; an automatic restart is counted in that same write, with a
`process.restarting` event. A driver whose record moved on (another
controller, a replaced or lost machine) writes nothing and so starts
nothing, and a start job goes only to the session the committed record
names. A controller that restarts finds a recorded start and runs it
without deciding or counting it again; `START_PROCESS` ends whatever the
pidfile names first, so a start never runs twice side by side. A running
process is left alone; one that exited while no controller ran is found
by the next probe and handled by its policy, its count continuing. A
replacement machine starts with fresh process state: restarts are counted
per machine. Failures, readiness changes, and restarts are events
(`process.failed`, `process.ready`, `process.unready`,
`process.restarting`) naming the target job whose receipt the target holds.

## Replacement

The requirements are the only thing that provisions a new machine:

```sh
compute environment replace myapp --cpu 8 --memory 16Gi
```

Replacement increments `spec_generation`, checks that a target can host
the new requirements (refusing, with reasons, before anything changes),
and records `computer.replacing`. The controller places and provisions
the new machine, reconciles the same desired contents onto it, and only
then retires the old session (`retired`, with the time and reason). A
provider answer about the old generation can no longer change the record.

## Durability and fencing

- **FeltDB is the authority.** The `Environment` and `Computer` records
  and their events are written together; a controller that restarts reads
  them and continues. A target, a provider, or a session is never asked
  what a computer should be.
- **Every write is fenced.** A driver acts on a record it has just read by
  identity and writes it back only if its version is unchanged. A stale
  driver, a replaced generation, or a concurrent operator change loses.
- **Provisioning is idempotent.** Each provisioning attempt names its
  session with a reference (`<computer record>:<spec generation>`). A
  controller that restarts mid-provisioning finds the session it already
  made instead of creating a second.
- **Orphans are torn down.** Every 60 s the controller compares each
  target's sessions carrying a computer reference with the `Computer`
  records; a session no record claims (a superseded generation, a
  destroyed computer) is destroyed, with `computer.orphan_destroyed`.
- **Lost machines are reported, not recreated.** A target that no longer
  knows the session marks the computer `failed` with
  `environment_lost`. `compute environment replace` makes a new one.
- **Workloads don't depend on the controller.** A computer and its
  processes keep running while the controller or FeltDB is down.

## Security

- Creating, changing, reconciling, replacing, stopping, and destroying a
  computer requires `operate`; `exec` and `connect` require `execute`;
  reading requires `read`.
- The owner is the creating operator. Another operator's request is
  refused with `authorization_denied`, whatever its scopes.
- Contents are validated before they are recorded: unique names, known
  repository references, no URL or revision beginning with `-`, no control
  characters. Commands are argument vectors, passed to the shell as
  positional parameters, never interpolated.
- Every operation is audited and evented; every command that ran on the
  computer is a durable job with a receipt.

## Providers

The provider behind a target is the session provider
([sessions.md](sessions.md#providers)):

| Provider | Machine |
| --- | --- |
| `workspace` (default) | A private directory on the target node |
| `container` (`compute serve --session-provider container`) | A container per computer (`docker` or `podman`), the workspace mounted at `/workspace`, CPU and memory limits applied, `--network none` when the computer has no network |

Another backend — a Firecracker microVM, a cloud VM, a hosted machine
service — implements `SessionProvider` and is offered by a `compute serve`
node; nothing in the environment model changes. A provider must support
`claim` to host persistent computers and `resume` to be stopped and
started (a target without `resume` leaves a stopped computer `stopped`
with `resume_unsupported`, rather than replacing it silently).

## API

| Method | Path | |
| --- | --- | --- |
| `POST` | `/environments` | With a `computer` object: create an environment on a computer |
| `GET` | `/environments/{environment}/computer` | The computer: status, target, desired and observed contents, failure |
| `POST` | `/environments/{environment}/contents` | Replace the contents (`{ contents, expected_generation }`) |
| `POST` | `/environments/{environment}/repositories` \| `packages` \| `processes` \| `projects` | Add or change one item |
| `DELETE` | `/environments/{environment}/repositories/{name}` \| `packages/{name}` \| `processes/{name}` \| `projects/{name}` | Remove one item |
| `POST` | `/environments/{environment}/release` | Release a revision of a project (`{ project, revision }`), in place |
| `POST` | `/environments/{environment}/run` | Run a project's `build`, `test`, or named command as a durable job |
| `POST` | `/environments/{environment}/config` | Replace the configuration every process sees |
| `POST` | `/environments/{environment}/lifecycle` | Keep it, or make it temporary (`{ lifecycle, ttl_seconds }`), in place |
| `POST` | `/environments/{environment}/processes/{process}/start` \| `stop` | Set one process's desired state |
| `POST` | `/environments/{environment}/reconcile` | Retry failed items and probe now |
| `POST` | `/environments/{environment}/replace` | New requirements: a new machine |
| `POST` | `/environments/{environment}/clone` | A new environment seeded with this one's workspace files and declared contents ([environment-clone.md](environment-clone.md)) |
| `POST` | `/environments/{environment}/workspace/export` \| `seed` \| `verify` | Portable workspace state: export a computer's workspace, seed an empty one, verify one against a digest ([workspace.md](workspace.md)) |
| `POST` | `/environments/{environment}/exec` | Run a command as a durable job |
| `GET` | `/environments/{environment}/jobs/{job}` | That job and its result |
| `GET` | `/environments/{environment}/logs` | The computer's logs, or one process's (`?process=api&limit=200`) |
| `POST` | `/environments/{environment}/connect` | A connection grant |
| `POST` | `/environments/{environment}/stop` \| `start` | Stop or resume the computer |
| `DELETE` | `/environments/{environment}` | Destroy the computer |
| `GET` | `/targets` | The targets and what they offer |
| `GET` `POST` | `/sessions` | Your work sessions; open one ([environment-control-plane.md](environment-control-plane.md#work-sessions)) |
| `GET` `DELETE` | `/sessions/{session}` | One work session; close it |

The contents body of `POST /environments/{environment}/contents` also
takes `config` and `lifecycle`, applied in the same fenced change.

## Embedding (Attn, Try This Software)

An embedding application is a client of this API, holding an operator
credential with the scopes it needs. It never talks to targets or
providers directly, and keeps no state of its own about computers. Attn
opens the control plane in Work mode for an environment; Try This
Software is an ephemeral environment opened by a work session. Both are
described in
[environment-control-plane.md](environment-control-plane.md#attn).

```ts
import {
  computeClient, createComputeEnvironment, updateComputeEnvironment,
  executeComputeEnvironment, releaseComputeEnvironment,
} from "@compute/appport";

const compute = computeClient({ endpoint: "https://compute.example", token });
await createComputeEnvironment(compute, {
  name: "workspace-42",
  computer: { lifecycle: "persistent", requirements: { cpu_count: 4, memory_bytes: 8 * 2 ** 30 } },
});
await updateComputeEnvironment(compute, "workspace-42", (contents) => {
  contents.repositories = [{ name: "app", url, revision: "main" }];
  contents.projects = [{ name: "app", repository: "app", build: ["npm", "ci"], test: ["npm", "test"] }];
  contents.processes = [{ name: "dev", kind: "application", command: ["npm", "run", "dev"], repository: "app", port: 3000 }];
});
await releaseComputeEnvironment(compute, "workspace-42", "app", "v2");
const { exit_code, stdout } = await executeComputeEnvironment(compute, "workspace-42", ["npm", "test"]);
```

`updateComputeEnvironment` reads the current contents, applies the change
to a copy, and submits it fenced by the generation it read — the same GO
the UI performs. A concurrent change fails with `conflict`; read and try
again.

## Limitations

- Projects, builds, tests, and releases of an environment with a computer
  run in its computer. An environment created without one still runs its
  bundle projects on the control-plane node, and says so
  (`machine.kind = node`).
- Changing a process's command, configuration, or build restarts it;
  there is no rolling restart within one computer.
- Hosted machine services (Fly, Railway, Render, cloud VMs) have no
  adapter in this repository; they plug in as session providers.
