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

## Start here: `compute`

```sh
compute
```

launches the control plane on this machine and opens it
(`http://127.0.0.1:8787/`). It starts this machine's **computer host** — a
`compute serve` target on `127.0.0.1:8788`, where computers run as private
workspaces (`compute up --containers` for containers) — and the control
plane (`compute start`), with a pool naming that host. Running it again
reuses what runs; `compute down` stops both and keeps the state
(`$COMPUTE_HOME`, default `~/.compute`). The control plane never runs
software itself: the computer host does, like any other target.

After that, everything is in the UI: every screen of the journey from
`compute` to rollback is in [product-surface](product-surface/README.md),
captured by the acceptance test that walks it.

The machine is the computer; everything else says what that computer
should contain or do. It is the same record, the same API, and the same
authority (the environment's owner) whether it is changed by a deployment,
from the UI, by an agent through `@compute/appport`, or from the CLI.

## Home: what do you want to do?

The control plane opens on actions, not infrastructure — run a project,
work on one, add another project, build and test, publish a new version,
deploy, move test → production, roll back, run an agent, try software,
operate production, create a computer — with the software (where each
project runs, at which version) and the computers under them.

## Run a project

Source → computer → proposal → GO:

1. **Source**: a Git URL, or a folder the computer can read that is a Git
   repository.
2. **Computer**: one you have, or a new one (what it needs, and whether it
   is kept or temporary).
3. **Inspect**: a durable job in that computer clones the source to a
   scratch directory, reads what is there, and removes it. Compute
   proposes an assembly from what it found:

   | Found | Proposed |
   | --- | --- |
   | `package.json` | node; `npm ci`/`npm install`; its `build`, `test`, `lint`/`typecheck` (as checks), `dev`, `start` scripts; port 3000 |
   | `requirements.txt` / `pyproject.toml` | python; `pip install`; `pytest` when there are tests; `app.py`/`main.py`/`manage.py`; port 8000 |
   | `go.mod` / `Cargo.toml` | go or rust; build, test, vet (a check), run |
   | `Makefile` | its `build`, `test`, `lint`/`typecheck`/`check`, `run` targets (they win over guesses) |
   | `Procfile` | each process; `web` is the application, with a port |
   | `.env.example` | configuration keys, with their documented defaults |
   | PostgreSQL or Redis in compose files or configuration | a database or Redis service, proposed but not selected |

   Ports are chosen so that no environment on the same host already uses
   them. What Compute could not decide is said (no start command, an
   unrecognised language).
4. **What will happen** lists every change; **GO** submits them as one
   fenced change, and Work shows the computer converging item by item.

A published version is run by deploying it (below).

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

## Versions: publish, deploy, promote, roll back

**Publishing** records an immutable version of a project, from the
environment it is developed in (`POST /software/{project}/versions`):

```text
✓ Source    the exact commit the computer has checked out
✓ Build     the project's build, in that computer
✓ Tests     its tests
✓ Checks    its named checks (lint, typecheck, …)
✓ Package   git archive of the commit → sha256 digest (refused if the checkout is not exactly that commit, or has uncommitted changes)
✓ Version   1.8.4, with the assembly it runs with and the evidence of every step
```

**Deploying** a version to an environment, **promoting** the version
running in one environment to another, and **rolling back** to an earlier
version are all one operation — a **rollout** — which changes the
environment's desired state (the repository at the version's commit, the
project as the version built it, and what runs from it, if the environment
lacks it) and follows the computer until it is real:

```text
✓ Desired state          generation 7
✓ Checkout               web-app at 07f5aae7729b
✓ Build                  built 07f5aae7729b
● Restart applications
○ Health check           every process running, every endpoint answering
```

A failed step stops the rollout with its evidence (the job that failed);
what ran before keeps running. A rollout that becomes active supersedes
the one before it, which stays in the history. Promotion is reviewed
first (`GET /software/{project}/promotion?from=&to=`): what runs in each
environment, what will change, configuration keys that differ (names
only), the source's health, the authority it takes, and the approvals the
target requires. Every rollout records who started it, and is resumed by
a restarted controller.

Versions and rollouts are durable records in FeltDB (`Version`,
`Rollout`, model generation 6), read by indexed equalities.

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

## Everything the UI does, the CLI and API do

| In the UI | CLI | API |
| --- | --- | --- |
| `compute` opens it | `compute` / `compute up`, `compute down` | — |
| Run a project: inspect | `compute environment propose ENV --url URL` | `POST /environments/{e}/propose` |
| … GO | `compute environment contents apply ENV FILE --expected-generation N` | `POST /environments/{e}/contents` |
| Pull a moved branch | `compute environment repo pull ENV NAME` | `POST /environments/{e}/repositories` (`sync` + 1) |
| Build, test, a command | `compute environment build\|test\|run` | `POST /environments/{e}/run` |
| Terminal, files | `compute environment exec ENV -- …` | `POST /environments/{e}/exec` |
| Logs | `compute environment logs ENV --process P` | `GET /environments/{e}/logs` |
| Restart | `compute environment process restart ENV P` | `POST /environments/{e}/processes/{p}/restart` |
| Publish | `compute versions publish P --environment E` | `POST /software/{p}/versions` |
| Versions, history | `compute versions list P`, `show P V` | `GET /software/{p}`, `/versions/{v}` |
| Deploy | `compute versions deploy P V --environment E`, `compute deploy P --environment E --version V` | `POST /software/{p}/deploy` |
| Promote | `compute versions promote P --from --to`, `compute promote P --from --to` | `GET /software/{p}/promotion`, `POST /software/{p}/promote` |
| Roll back | `compute versions rollback P --environment E [--to V]` | `POST /software/{p}/rollback` |
| An operation's progress | (followed by every command above) | `GET /rollouts/{id}`, `GET /software/{p}/versions/{v}` |
| Create, stop, resume, replace, destroy a computer | `compute environment create\|stop\|start\|replace\|destroy` | as before |
| Lifetime, configuration | `compute environment lifetime\|config` | `POST /environments/{e}/lifecycle\|config` |
| Temporary computer | `compute session open --cpu 2` | `POST /sessions` |

`@compute/appport` has a function for each (`proposeProject`,
`publishVersion`, `deployVersion`, `promotionPlan`, `promoteVersion`,
`rollbackVersion`, `getRollout`, `waitForOperation`, `restartProcess`,
`listSoftware`, …). Agents use them with an operator credential: what an
agent builds, publishes, deploys, or promotes appears in the UI exactly as
a person's would, under its operator's name.

## Limitations

- A release restarts what runs from the repository: there is no traffic
  switch between two versions inside one computer yet, so a restart is a
  short interruption. The zero-downtime release pipeline, ingress,
  domains, and certificates still serve environments without a computer
  on the control-plane node.
- Endpoints are the target's host and the process's port. The container
  provider does not publish ports yet.
- The Work terminal runs each command as a durable job; it is not an
  interactive PTY. Files are listed and read through the same jobs; there
  is no in-browser editor.
- A local folder must be a Git repository the computer can read; changes
  reach the computer as commits (Pull, or a new revision).
- Approvals: promotion shows the authority it takes; there is no separate
  approval workflow yet (`approvals` is empty).
- Inspection recognises common layouts (above); anything else is proposed
  empty, to be filled in before GO.
- A rollout's health check connects to each endpoint from the control
  plane. A target whose ports the control plane cannot reach fails the
  check after a minute, even when the processes run; give such processes
  no port, or make the target's host reachable.
- Attn and Try This Software are not in this repository; their integration
  is the API above.
