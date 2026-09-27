# One environment, one computer

> **Compute is one control plane for one computer.** Manage mode controls
> which computer exists and how it is operated. Work mode controls what is
> inside that computer and what it is doing. Deployment is not a different
> product: it is reconciliation of the same environment. A provider
> supplies the computer; Compute owns the environment.

```text
Environment  myapp  (env_…)                    one durable record in FeltDB
│
├── Machine        4 CPU · 8 GiB · target-a     the computer backing it
│                                                (a provider's resource: metadata)
├── Repositories   app @ v2
├── Projects       app: build · test · migrate   software in a repository
├── Applications   api   ● running  :8080
├── Services       redis ● running
├── Agents         eve   ● running
├── Packages       deps
├── Configuration  DATABASE_URL, …
├── Endpoints      api → http://target-a:8080
└── Work sessions  who is working in it
```

The machine is the computer; everything else says what that computer
should contain or do. It is the same record, the same API, and the same
authority (the environment's owner) whether it is changed by a deployment,
from the UI, by an agent through `@compute/appport`, or from the CLI.

## Manage and Work

The control plane's UI has two modes, switched at the top of every page.
They are views of the same environment: the same ID, the same API calls,
the same permissions. Neither holds state of its own, and switching keeps
the environment you are looking at.

| Manage | Work |
| --- | --- |
| Environments, targets, placement | Repositories and their revisions |
| The machine: requirements, target, lifetime, health | Projects: build, test, named commands |
| Replacement (a new machine) | Applications, services, agents, processes |
| Start, stop, destroy | Packages, configuration, endpoints |
| Deployment state, events, receipts | Terminal (commands as durable jobs), logs |
| Domains, DNS, certificates, services | Work sessions, lifetime, **GO** |

`#/environments/<name>` is Manage; `#/work/<name>` is Work.

## GO: the common mutation boundary

In Work, every change is local until GO: a revision, a new service, a
package, a configuration value, a lifetime. GO submits the whole draft as
**one** request:

```text
POST /environments/myapp/contents
{ "contents": {…}, "config": {…}, "lifecycle": {…}, "expected_generation": 7 }
```

It is an ordinary authorized operation (scope `operate`, and only the
environment's owner), written to FeltDB with its event in one fenced
transaction, and reconciled in place by the computer. If anyone changed
the environment since the page loaded it, the request is refused
(`conflict`), nothing is applied, and the page says:

```text
Environment changed since you loaded it.   [ Refresh ]
```

Nothing is silently overwritten. The CLI (`compute environment contents
apply --expected-generation`) and AppPort (`submitComputeEnvironment`) use
the same fence.

## Deployment is reconciliation

```text
Desired environment ──▶ reconciler ──▶ the existing computer ──▶ applications, services, processes
```

A deployment is one way of changing desired state. Releasing a revision
of a project moves its repository to that revision; the computer checks
it out, runs the project's build, and restarts what runs from the
repository — in place, on the same machine:

```sh
compute deploy app --environment myapp --revision v2     # the same as:
compute environment release myapp app --revision v2
```

| Change | What happens on the computer |
| --- | --- |
| repository revision `main → feature/auth` | fetch, checkout, build, restart what runs from it |
| application `api`: new configuration | restart `api` |
| process `worker`: restart | stop, start |
| service `redis`: add | start it |
| requirements `2 CPU → 16 CPU` | **replacement**: a new machine (explicit, below) |

The build is desired state: it runs whenever its repository's commit, its
command, or the configuration changes, before anything from the
repository restarts. A build that fails leaves what is running as it was,
records the failure with its job's evidence, and is not retried until
something changes (or `compute environment reconcile`).

A bundle is never deployed to the control-plane node for an environment
with a computer: `POST /deployments` (and `compute project add`,
`compute promote`) on such an environment is refused with the release
command to use instead.

## Work runs in the computer

Every piece of environment work is a durable job inside the environment's
computer, submitted by the daemon and recorded with its receipt:

```sh
compute environment build myapp            # the project's build
compute environment test myapp app         # its tests
compute environment run myapp app migrate  # a named command
compute environment exec myapp -- make lint
compute environment logs myapp --process api
```

The daemon coordinates and records; it never runs an environment's work
on its own node. The events (`environment.command`, `environment.exec`)
name the target, the session, the provider resource, the job, and the
execution.

## Replacement stays explicit

```text
Change what's on the computer   ≠   Replace the computer
```

`compute environment replace myapp --cpu 16` (or **Replace machine…** in
Manage) is the only operation that provisions another machine. Everything
in Work, every release, and every configuration or lifetime change keeps
the same provider resource (`machine.resource` in the environment's view).

## Lifetime

Every environment has one lifecycle setting, in the same UI and API:

| | |
| --- | --- |
| **Temporary** (`ephemeral`) | Expires after its TTL; the computer is torn down and the record and evidence remain |
| **Keep running** (`persistent`) | Kept until it is destroyed |

Changing it is in place. Compute takes over the machine's expiry from the
target (a `claim`), then keeps or ends it itself:

```sh
compute environment lifetime myapp --keep
compute environment lifetime myapp --temporary --ttl 2h
```

A target that cannot claim sessions cannot change a machine's lifetime;
the request is refused before anything is written.

## Work sessions

A session is a way into an environment's computer — "give me a computer I
can run work on" — never the definition of the environment itself. Work
sessions are durable records (`WorkSession` in FeltDB) owned by the
operator who opened them.

```text
Temporary                                  Persistent
compute session open --cpu 2               compute environment create myapp --persistent
  → an ephemeral environment (work-…)      compute session open myapp
  → its computer                             → work
  → work                                   compute session close wks_…
compute session close wks_…                  → the environment and computer remain
  → its computer is destroyed              compute session open myapp   (again: the same computer)
  → the records remain
```

| | Attached | Ephemeral |
| --- | --- | --- |
| Opened with | an environment you own | requirements (no environment) |
| Closing it | leaves the environment as it is | destroys its environment's computer |
| When the computer ends | the session is closed with the reason | same |

`GET /sessions`, `POST /sessions`, `GET /sessions/{session}`,
`DELETE /sessions/{session}`; only the owner can read or close a session.
The lower-level `compute session create` still makes a session directly
on a target through the pool: that is the primitive an environment's
machine is made of.

## Choosing a computer, not a provider

The UI asks *what kind of computer do you need?* — CPUs, memory,
persistent storage, a public endpoint, machine features (`containers`,
`kvm`, `gpu`), and lifetime. Placement chooses the target
([placement.md](placement.md)); a target may be named only to constrain
it. The provider appears as metadata (`Target: target-a · container`),
never as the resource you manage.

## Operations: where each one runs

Every operation below is an authorized request to the Compute API with
the scope shown; the environment's owner is the boundary for everything
on an environment with a computer.

| Operation | Changes desired state | Runs in the computer | Replaces the machine | Evidence | Scope |
| --- | --- | --- | --- | --- | --- |
| `environment create` (a computer) | yes | provisions it | — (first machine) | `Environment`, `Computer`, `computer.*` events | operate |
| `repo`, `package`, `process`, `service`, `agent`, `project` add/update/remove | yes | sync, install, build, start/stop jobs | no | `contents_changed`, per-item job evidence | operate (project: deploy) |
| GO (`contents` with `config`/`lifecycle`) | yes, fenced | as above | no | one `contents_changed` | operate |
| `release`, `deploy` (computer) | yes (a revision) | checkout, build, restart | no | `environment.release`, build evidence | deploy |
| `config` | yes | restarts dependents | no | `contents_changed` (keys only) | operate |
| `lifetime` | yes | claim on the target | no | `computer.lifecycle_changed` | operate |
| `build`, `test`, `run`, `exec` | no | a durable job | no | `environment.command`/`exec`, receipt | execute |
| `logs` | no | a job reads the log | no | the job | read |
| `connect` | no | a grant from the target | no | `environment.connected` | execute |
| `reconcile` | no (retries failures) | probe, retry | no | `reconcile_requested` | operate |
| `stop`, `start` | desired state | stop/resume the same machine | no | `computer.stopped/resumed` | operate |
| `replace` | yes (requirements) | a new machine | **yes, explicitly** | `computer.replacing` | operate |
| `destroy` | yes | teardown | ends it; record kept | `computer.destroyed` | operate |
| `session open/close` | a `WorkSession` | ephemeral: creates/destroys its environment | no | `work_session.*` | operate |
| bundle `deploy`, `project add`, `promote` on a computer environment | — | — | — | refused, with the release to use | — |
| Environments **without** a computer: `project add`, `deploy`, `promote`, `rollback`, `workload run`, `application deploy` | yes | on the control-plane node (`machine.kind = node`, shown in Manage) | — | deployments, executions, receipts | deploy/operate/execute |

The last row is the one path that still runs on the daemon's node: an
environment created without a computer. It is shown as such everywhere
(`machine: { kind: "node" }`, and "runs on this control-plane node" in
Manage) and kept for compatibility; an environment with a computer never
uses it.

## Attn

Attn does not contain a Compute UI. "Work on this" opens the control plane
in Work mode, scoped to the environment:

```text
https://compute.example/#/work/<environment>      workModeUrl(endpoint, environment)
```

Attn provides attention, context, work association, notifications, and
human judgment; Compute provides the computer, the environment's state,
execution, lifecycle, and placement. Attn reads and changes the same
environment through `@compute/appport` with its operator credential —
`getComputeEnvironment`, `submitComputeEnvironment`,
`runComputeProjectCommand`, `openWorkSession` — and never talks to a
target or provider.

## Try This Software

Try This Software is a consumer of these primitives, not a second
implementation:

```text
openWorkSession({ computer: { requirements, ttl_seconds } })   create an ephemeral environment
        ↓                                                      placement chooses a computer
submitComputeEnvironment(…repositories, processes with a port) populate the repository, run the application
        ↓
computer.endpoints[0].url                                      the endpoint the user interacts with
        ↓
TTL passes (or closeWorkSession)                               the computer is torn down
        ↓
getComputeEnvironment(…)                                       the records and evidence remain
```

To keep a trial, change its lifetime to persistent (`--keep`): the same
machine, in place.

## Programmatic API

`@compute/appport` is the programmatic interface, and has nothing the UI
lacks or the UI anything it lacks:

| | |
| --- | --- |
| create / inspect | `createComputeEnvironment`, `getComputeEnvironment`, `listComputeEnvironments` |
| change desired state | `updateComputeEnvironment`, `submitComputeEnvironment` (GO), `releaseComputeEnvironment`, `setComputeEnvironmentConfig`, `setComputeEnvironmentLifetime` |
| execute | `executeComputeEnvironment`, `runComputeProjectCommand` |
| connect / sessions | `connectComputeEnvironment`, `openWorkSession`, `closeWorkSession`, `listWorkSessions` |
| reconcile / replace / destroy | `reconcileComputeEnvironment`, `replaceComputeEnvironment`, `destroyComputeEnvironment` |

## Limitations

- A release restarts what runs from the repository: there is no traffic
  switch between two versions inside one computer yet, so a restart is a
  short interruption. The zero-downtime release pipeline, ingress,
  domains, and certificates still serve environments without a computer
  on the control-plane node.
- Endpoints are the target's host and the process's port. The container
  provider does not publish ports yet.
- The Work terminal runs each command as a durable job; it is not an
  interactive PTY.
- Attn and Try This Software are not in this repository; their integration
  is the API above.
