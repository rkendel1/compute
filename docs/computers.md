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

## Quick start

```sh
# A target: any node that offers sessions.
compute serve --listen 0.0.0.0:8080 --public-url https://target-a.example \
  --job-store /var/lib/compute/jobs --session-store /var/lib/compute/sessions

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
4. probe processes (every `computer_probe`, 15 s by default): a process
   that died is started again, with its exit recorded.

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
