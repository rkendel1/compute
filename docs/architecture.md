# Compute architecture

This is the architecture as the code has it at `69b70d9` (audited
2026-09-27; the evidence is [audit.md](audit.md) and
[audit.json](audit.json)). Where the code and the intended design differ,
this page says what the code does and names the gap.

> **FeltDB owns durable state; Compute owns execution.** Compute keeps no
> second durable database, reads targeted state through bounded, indexed
> queries and coherent snapshots, and never serves stale state as current.
> The contract, and the working state the controller may keep, is
> [feltdb.md](feltdb.md).
>
> **Production control planes use FeltDB.** Without configuration,
> `compute`, `compute up`, and `compute start` keep control state in a local
> file (`control-state.json`, the `file` backend) and say so: the launcher
> prints it, and `/info`, `compute status`, and `compute node info` report
> `durability: local-development` (FeltDB reports `production`). The file
> backend implements the same contract (conformance suite) for one
> developer's machine; it is not FeltDB. G-ARCH-3 is closed by this
> decision ([feltdb.md](feltdb.md)).

## The system

```text
 people ─ browser UI (Work / Manage)      agents ─ AppPort (TypeScript)      scripts ─ `compute …` CLI
                 │                                   │                                 │
                 └───────────────┬───────────────────┴─────────────────────────────────┘
                                 │ HTTP(S) · operator credentials · scopes · audit
                                 ▼          (loopback without TLS: no credential needed)
 ┌──────────────────── control plane: the daemon (`compute start`, :8787) ─────────────────────┐
 │ Compute API (128 routes)  ·  UI assets  ·  events stream                                    │
 │ controllers: computer drivers (one per computer) · operation drivers (publish, rollout)     │
 │              reconcile loop (node environments) · orphan sweep · process probe (15 s)       │
 │ placement (compute-placement) · policy/admission (compute-policy) · network (compute-network)│
 │ working state: desired snapshot, read cache  (derived, rebuilt on start, never authority)   │
 └───────┬───────────────────────────────────────┬──────────────────────────────┬──────────────┘
         │ fenced transactions, indexed queries,  │ compute.remote@1             │ local supervisor
         │ snapshots                              │ + the target credential     │ (`compute supervisor`)
         ▼                                        ▼                              ▼
 control state: FeltDB (production) or     targets: `compute serve` (:8788)   THE DAEMON HOST
 a file (local development)                (trust only the control planes     bundle-project workloads,
 environments, computers, contents,         they issued credentials to;       releases, ingress :80/:443,
 work sessions, versions, rollouts,         what they create is theirs)
 events, deployments, executions,          ├─ session store (the machines)    applications (`compute deploy`),
 credentials, audit                        ├─ job store (commands, receipts)  /compute/* jobs
                                           └─ session provider:
                                                workspace (default: a directory + processes on the host)
                                                container (docker/podman; unverified against an engine)
```

There are three kinds of process:

| Process | Command | What it is | Holds |
| --- | --- | --- | --- |
| Control plane (daemon) | `compute start` (started by `compute`/`compute up`) | The authority's front: API, UI, controllers, placement, audit | Nothing durable of its own; control state is a file or FeltDB |
| Target | `compute serve` (started by `compute`/`compute up` as "this-machine") | A pool member answering `compute.remote@1`: runs jobs, hosts sessions (computers) | Its session and job stores on its disk |
| Supervisor | `compute supervisor` (started by the daemon) | The daemon host's data plane for node environments | Workload processes, endpoint listeners |

`compute run` is a fourth path with no process at all: the CLI runs a
workload in-process with the local provider (or submits it to a pool with
`compute pool run`).

## Modules

| Crate / package | Lines (src) | Owns |
| --- | ---: | --- |
| `compute-core` | 8.7k | Shared types: jobs, results, receipts, runtime kinds, host facts, sessions (`ComputeSession`), computers (`ComputerSpec`, `EnvironmentContents`, `ProjectSpec`) |
| `compute-runtime` | 1.3k | The runtime trait and the pinned runtime catalog |
| `compute-runtime-process` | 1.6k | Process runtimes (node, bun, deno, python, ruby, php, jvm, dotnet, native, shell), landlock/netns isolation where available |
| `compute-runtime-wasm` | 0.5k | wasmtime, WASI preview 1 |
| `compute-runtime-conformance` | 0.8k | One contract run against every runtime |
| `compute-provider` | 7.8k | `LocalProvider`, `RemoteProvider`, `compute serve` (`compute.remote@1`), job store, session store, session providers (`WorkspaceSessionProvider`, `ContainerSessionProvider`), target credentials (`TargetAuthorizer`) |
| `compute-placement` | 4.2k | Pools, capability matching with reasons, target inventory (`GET /targets`) |
| `compute-policy` | 1.3k | Admission policy and contracts |
| `compute-state` | 4.5k | The control-state model (generation 6) and the `ControlState` trait, conformance suite |
| `compute-state-memory` / `-file` / `-feltdb` | 0.1k / 0.3k / 1.9k | Backends: tests, the default local file, FeltDB |
| `compute-network` | 1.7k | DNS providers, ACME, TLS, ingress for node environments |
| `compute-environment` | 23.0k | The daemon: API (`api.rs`, `ROUTES`), auth, controllers (`daemon/computers.rs`, `software.rs`, `work.rs`, `release.rs`, `reconcile.rs`, …), the supervisor data plane, the UI (`ui/app.js`, 2.3k lines) |
| `compute-cli` | 17.6k | The `compute` binary: launcher, 182 commands |
| `packages/compute-appport` | TS | The agent/automation client: every UI operation |
| `packages/compute-state-model` | TS | `compute.flow`, the generated FeltDB manifest |
| `packages/compute-ui-e2e` | TS | Browser certification (currently failing; not in CI) |

## The model

<!-- audit:models -->
| Term | What it is in the code | Source | Collision |
| --- | --- | --- | --- |
| **Environment** | A durable record (Environment in control state) with a name, desired state (running/stopped), configuration, policy, and optionally an owner, a ComputerSpec, and EnvironmentContents. | `crates/compute-state/src/model.rs#EnvironmentRecord` | Two kinds share the name: an environment with a computer, and a "node environment" whose bundle projects run on the daemon host. `applications` is a reserved node environment for `compute deploy <dir>`. |
| **Computer** | The machine behind an environment with a ComputerSpec: a Computer record (status, generation, target, session, provider_resource, observed contents) driven by the daemon. | `crates/compute-state/src/model.rs#ComputerRecord` | The UI says "Computer" and "Machine"; the CLI says `environment computer`; the target calls it a session. |
| **Session (target)** | A durable record on a `compute serve` target: a workspace or container, commands run as durable jobs. A computer IS a persistent, referenced target session. | `crates/compute-core/src/sessions.rs#ComputeSession` | Stored in the target's session store, not FeltDB. `compute session create` makes one directly (no environment, no daemon). |
| **Work session** | A WorkSession record in control state: an operator entering an environment (attached) or owning a temporary one (ephemeral). | `crates/compute-state/src/model.rs#WorkSessionRecord` | `compute session open/close/opened` beside `compute session create/.../destroy`, which are target sessions. |
| **Target** | A pool member that hosts computers: a `compute serve` node offering sessions. Listed by GET /targets. | `crates/compute-placement/src/targets.rs` | Pool members are also called providers. |
| **Provider** | A pool member answering the capability API: local (in-process engine) or remote (compute.remote@1). Session providers (workspace, container) are the substrates inside a target. | `crates/compute-provider/src/lib.rs` | "Provider" names three things: pool members, session substrates, and DNS providers. |
| **Runtime** | A workload language runtime (wasm, node, python, …, native, shell) resolved from a pinned catalog; used by `compute run` and daemon workloads. | `crates/compute-core/src/lib.rs#RuntimeKind` | Not the computer substrate: computers run whatever the target host has on PATH. |
| **Project** | Two different things: (a) a computer project — a ProjectSpec in contents: a repository plus build/test/commands/checks; (b) a bundle project — registered revisions of workload bundles, released to node environments. | `crates/compute-core/src/computers.rs#ProjectSpec; crates/compute-state/src/model.rs#ProjectRecord` | Same word, same API prefix (/environments/{e}/projects), dispatched by whether the environment has a computer. |
| **Application** | Three things: (a) a process of kind application in a computer; (b) an `compute init/deploy` application with versions on a provider node; (c) a project in the reserved `applications` environment. | `crates/compute-core/src/computers.rs#ProcessKind; crates/compute-environment/src/daemon/applications.rs` | Three meanings. |
| **Service** | Three things: (a) a process of kind service in a computer; (b) a bundle workload of kind service; (c) a registered shared service (`compute service register`). | `crates/compute-core/src/computers.rs; crates/compute-environment/src/model.rs#WorkloadKind` | Three meanings. |
| **Execution job** | A durable job in a provider's job store (filesystem on the target), with a result and a receipt. Computer operations reference jobs by id; FeltDB stores the references and events, not the jobs. | `crates/compute-provider/src/jobs.rs` | Daemon node executions are Execution records in control state; target jobs are not. |
| **Version / Rollout** | Version: a published commit + package digest + assembly + step evidence. Rollout: a version made real in an environment (deploy/promote/rollback) with steps. | `crates/compute-state/src/model.rs#VersionRecord,RolloutRecord` | Parallel to bundle Revisions/Deployments and to application versions. |
<!-- /audit -->

The complete model the product presents is: **a computer** (an environment
with a `ComputerSpec`) placed on **a target**, holding **projects** as
desired contents, changed by **GO** (one generation-fenced change),
**versions** published from one environment and **rolled out** to others,
and **work sessions** recording who is working where. Node environments,
bundle projects, and `compute init/deploy` applications are an earlier
deployment model that still runs on the daemon host (gap G-ARCH-2).

## Where execution happens

<!-- audit:execution_paths -->
| Path | Where it executes | Authority | Durable record | Canonical job path |
| --- | --- | --- | --- | --- |
| `compute run` (local) | the caller's machine, in process | none (local user) | execution record + receipt on disk | no |
| `compute pool run/submit`, `compute remote *` | the provider placement chose | provider: a target credential on compute serve; daemon /compute/* only behind the daemon API's scopes | provider job store | yes |
| `compute session create/exec` (target sessions) | the target | target credential; owner = the control plane the credential names | target session/job stores | yes |
| Computer operations (sync, install, build, start/stop, probe, inspect, publish steps) | the environment's computer | daemon controller | target jobs; evidence in FeltDB | yes |
| `environment exec/run/build/test/propose` | the environment's computer | daemon scope + owner | target jobs; events in FeltDB | yes |
| Bundle project workloads (services, tasks) and releases | THE DAEMON HOST (supervisor) | daemon scopes, no owner | Execution records in control state | no |
| Applications (`compute deploy <dir>`) | a provider node offering deployments — the daemon host by default | daemon scopes | deployments in control state | no |
| Daemon /compute/* (node as provider) | the daemon host | daemon execute scope | daemon job store | yes |
<!-- /audit -->

For environments with a computer, the daemon coordinates and records and
never runs their work on its own node: every build, test, process, and
publish step is a durable job on the computer's target. The three paths
marked "the daemon host" are the exception, and they are the older model.

## Durable state

<!-- audit:state -->
| What | Where | Survives a control-plane restart | Survives a machine restart |
| --- | --- | --- | --- |
| Environments, computers, contents, work sessions, versions, rollouts, events, deployments, executions, credentials, audit | control state: FeltDB (production) or a file (local development, stated as such) | yes | yes (FeltDB / file on disk) |
| Target trust (which control planes a target trusts: verifiers only) and each control plane's target tokens | the target's trust file; the control plane's token files, named by the pool | yes | yes |
| When each running computer was last confirmed by its target | daemon memory (live evidence; transitions are durable) | rebuilt by the next confirmation | rebuilt |
| Target sessions and jobs (the machine, its commands, their results and receipts) | the target's session and job stores on its disk | yes | target records yes; workspace processes no (restarted by reconciliation) |
| Computer workspaces (checkouts, builds, process pid/log files) | the target host filesystem | yes | files yes; processes no |
| Desired snapshot, read cache, computer/operation drivers, orphan sweep schedule | daemon memory (derived) | rebuilt | rebuilt |
| Runtime distributions | runtime store on each host | yes | yes |
<!-- /audit -->

A computer's truth is split: the **Computer** record (desired contents,
generation, status, which target and session) is in control state; the
**machine** and its **jobs** are in the target's own stores. The daemon
references jobs by id and records their outcomes as events; it does not
copy the jobs.

## Authority

<!-- audit:authorization -->
| Operation | Check | On every operation |
| --- | --- | --- |
| Read environments/computers/software | read scope; any operator reads any computer view (only mutations are owner-bound) | yes |
| Change a computer environment (contents, config, lifetime, replace, destroy, sessions) | operate/deploy scope + owner | yes |
| Exec / run / connect / propose | execute scope + owner | yes |
| Publish / deploy / promote / rollback versions | deploy scope + owner of the environment(s) | yes |
| Node environments, bundle projects, applications, domains | scopes only; no ownership | yes |
| Loopback daemon without TLS | no credential required (development mode); `--production` requires TLS and credentials | bypassable locally |
| Target (`compute serve`) jobs and sessions | a target credential on every request (reads included); sessions and jobs owned by the control plane it names; `--insecure-unauthenticated` only by name | yes |
| Daemon /compute/* (node as provider) | only requests the daemon API authenticated and scoped (DaemonAuthorized) | yes |
<!-- /audit -->

The daemon is a real authority for its own API, and its targets' authority:
every request it makes to a target carries the target credential its pool
names (`token_file`, or `token_env`), and a target accepts only the control
planes it issued a credential to (`compute target credential issue`;
verifiers only, revocable). Sessions and jobs belong to the control plane's
identity (`control-plane:<id>`), so another control plane — or anyone
else — reaches none of them. `compute serve` refuses to start without a
trust file; the only open mode is the named `--insecure-unauthenticated`,
which the target advertises in its capabilities and `compute target list`
shows. The daemon's own `/compute/*` service admits only requests its API
authenticated (SEC-1, SEC-2 in [audit.md](audit.md) are resolved).

## Reconciliation

- **Computers.** One driver per computer walks `placing → provisioning →
  applying → running` (or `stopping/stopped`, `destroying/destroyed`,
  `expired`), each step a durable job, each write fenced by the computer's
  generation. A restarted daemon resumes every driver from the record.
  A process probe every 15 s restarts processes that died. Every 10 s the
  driver confirms the machine with its target, whatever runs in it: a
  target that does not answer (or refuses the credential) makes the
  computer `unreachable`; one that answers without the session or machine
  makes it `lost`. Both keep desired state. The same machine answering
  returns an unreachable computer to `running`; a lost one waits for an
  explicit replace, destroy, or reconcile. Each observation is applied only
  to the record version it was made against, so a delayed answer cannot
  revive a computer a newer observation found gone.
- **Operations.** Publish and rollout drivers follow their steps to an end;
  they resume after a restart.
- **Node environments.** A reconcile loop drives the supervisor toward the
  desired bundle revisions, with zero-downtime switching.
- **Orphans.** Sessions on a target that no computer claims are destroyed.

## Invariants

These hold for every change to Compute. Each has regression tests; a change
that breaks one fails them.

| # | Invariant | Regression tests |
| --- | --- | --- |
| 1 | Accepted executions cannot disappear. | `executions.rs`: `eighty_concurrent_runs_of_one_task_each_produce_one_receipt`, `a_thousand_concurrent_runs_lose_no_evidence`; `recovery.rs`: `an_execution_that_ends_while_the_controller_is_down_is_recorded_after_recovery` |
| 2 | Execution identity is unique and immutable. | `executions.rs` (every record and receipt distinct); `daemon::execute` unit tests |
| 3 | Terminal execution state is idempotent. | `daemon::execute`: `the_terminal_log_remembers_each_execution_once_and_is_bounded`, `persisting_the_same_evidence_twice_records_it_once` |
| 4 | Control-plane failure must not inherently terminate healthy workloads. | `availability.rs`: `workloads_keep_running_and_changes_are_refused_while_state_is_unreachable`; `recovery.rs`: `managed_feltdb_is_the_durable_authority` (degraded start and recovery) |
| 5 | Data-plane workloads are recoverable independently of controller process state. | `recovery.rs`: `a_killed_controller_leaves_its_workloads_serving`, `a_controller_stopped_for_an_upgrade_keeps_its_workloads` (stop and SIGTERM), `a_lost_supervisor_is_replaced_and_its_orphans_are_cleaned_up` |
| 6 | FeltDB is the durable authority, not a hot-path cache. | `availability.rs`: `reads_say_how_fresh_they_are` (cached reads are labelled, and invalidated by writes) |
| 7 | Local operational state is never a competing source of durable truth. | `recovery.rs`: `managed_feltdb_is_the_durable_authority` (no local fallback when FeltDB is down; a fresh node restores everything) |
| 8 | Production remote operations are authenticated and authorized. | `security.rs`: `production_requires_tls_and_a_credential_for_every_request`, `scopes_are_enforced_and_every_mutation_is_audited`, `credentials_expire_revoke_and_rotate`; `auth` unit tests including `every_route_declares_a_scope_and_unknown_routes_need_admin` |
| 9 | Security capabilities are explicit and never silently downgraded. | `security.rs`: `development_mode_is_explicit`; `environment_cmd` unit tests for `security_mode`; `host` unit test `profiles_never_downgrade`; `isolation.rs`: `a_host_profile_is_refused_where_it_cannot_apply` |
| 10 | Workload failures are distinct from Compute/control-plane failures. | `executions.rs`: `a_failing_run_is_a_workload_failure_not_a_denial` (`failure: workload_failed`); `recovery.rs` (a lost supervisor is `runtime_unavailable` and restarts regardless of restart policy) |
| 11 | Compute upgrades preserve workload identity and durable state. | `upgrade.rs`: `the_controller_upgrades_and_rolls_back_under_traffic_without_touching_workloads`, `a_new_build_that_fails_or_hangs_is_rolled_back` |
| 12 | Receipt identity stays verifiable without repeated expensive executable hashing. | `receipt`: `cached_file_identities_follow_the_file_and_ignore_an_untrusted_cache`; `cli.rs`: `execution_receipt_is_canonical_verifiable_and_binds_artifacts` |
| 13 | Compute remains runtime-neutral. | The runtime conformance suite (`compute-runtime-conformance`) runs the same contract against every runtime. |
| 14 | Compute does not require AuthBoundry to execute an application. | Every test above runs Compute alone; operator credentials are Compute's own. |
| 15 | Compute does not become an application-specific product framework. | Review. Not held at `69b70d9`: `compute init`/`compute deploy <dir>` and `ApplicationIdentity` name applications ([audit.md](audit.md#what-needs-to-be-reconciled)). |
| 16 | Controller paths read FeltDB through bounded, indexed queries and snapshots; nothing scans a collection to find a few records. | `feltdb_consumer.rs`: `the_controller_keeps_authority_in_feltdb_through_an_outage` (a quiet cycle runs no queries; scans are limited to the listed shapes); `consumer.rs`: `targeted_reads_are_indexed_and_bounded` |
| 17 | A snapshot is coherent: it never observes part of a transaction, and it is reused only while the revision it represents is current. | `compute_state::conformance` (memory, file, and a real FeltDB): concurrent paired writes, reuse, staleness, identity |
| 18 | A controller never runs on a model it would misuse, and the model is never downgraded. | `consumer.rs`: `the_upgrade_backs_up_migrates_and_verifies`, `a_newer_model_is_never_downgraded` |
| 19 | A session is durable before any provider acts, keeps its identities across restarts, and a stale provider answer never revives it. | `compute-provider/tests/sessions.rs`: `a_session_is_durable_before_the_provider_answers_and_survives_a_restart_while_provisioning`, `a_ready_session_and_its_evidence_survive_a_restart`, `a_stale_provider_response_cannot_resurrect_a_destroyed_session`; `compute-cli/tests/sessions.rs` |
| 20 | Session providers are executors, never authorities: every session operation is authorized and bound to its owner, and commands in a session are ordinary durable jobs. | `sessions.rs`: `every_operation_is_authorized_and_bound_to_its_owner`, `a_session_lives_its_whole_lifecycle_on_any_provider`, `a_provider_without_optional_capabilities_is_still_a_complete_provider`. Held by the provider contract and at the target: `compute serve` authenticates every request with a target credential, and the owner is the control plane it names (`a_target_is_controlled_only_by_the_control_planes_it_trusts`). |

## Failure kinds

Every error the API returns, and every failed execution, says which of
these it is. A workload's failure is never reported as Compute's, and
Compute's is never blamed on the workload.

| Kind | HTTP | Meaning |
| --- | --- | --- |
| `admission_denied` | 403 | Policy refused the execution; nothing ran. |
| `authentication_failed` | 401 | No credential, or one that is unknown, expired, or revoked. |
| `authorization_denied` | 403 | A valid credential without the scope the operation needs. |
| `state_unavailable` | 503 | Control state (FeltDB) is unreachable; the change was not made. Compute never reports success for a change that has not reached durable state. |
| `runtime_unavailable` | 503 | The runtime, or the data plane that runs the workload, is unavailable or lost it. |
| `workload_failed` | — | The workload ran and failed (non-zero exit, killed, timed out). Recorded as the execution's `failure` and in its `task.failed` / `service.failed` event. |
| `controller_unavailable` | 503 | The controller cannot be reached, or is stopping or handing over. |
| `endpoint_unavailable` | — | An endpoint cannot listen on its host port. Recorded as an `endpoint.unavailable` event and in `compute network` / `compute doctor`. |
| `upgrade_failed` | 500 | An upgrade was refused or rolled back. |

## Structured events

Lifecycle events carry the time, the request ID, the operator and
credential that caused them when an operator did, and the resource and
execution they are about. They never carry secrets.

| Event | When |
| --- | --- |
| `controller.started`, `controller.ready`, `controller.degraded`, `controller.stopped` | Controller lifecycle |
| `workload.discovered`, `workload.reattached`, `workload.restarted`, `workload.orphaned` | Recovery of the data plane after a controller start |
| `data_plane.restarted` | A new supervisor replaced a lost one |
| `reconcile.started`, `reconcile.finished` | Full reconciliation cycles that changed something, with duration, resources examined, changed, and errors |
| `upgrade.started`, `upgrade.ready`, `upgrade.completed`, `upgrade.failed`, `upgrade.rolled_back` | Controller upgrades |
| `feltdb.unavailable`, `feltdb.recovered` | Control-state outages |
| `control_model.upgraded` | A controller gave records an older controller wrote their indexed identity |
| `auth.authentication_failed`, `auth.authorization_denied` | Refused requests (at most 60 a minute are recorded) |
| `credential.created`, `credential.revoked`, `credential.rotated`, `credential.bootstrapped` | Operator credentials |
| `endpoint.unavailable` | An endpoint could not listen |

## Boundaries as built

- One control plane per state store. Placement spans the targets in its pool;
  targets are configured, not discovered.
- Computers are workspaces (native processes in a private directory on the
  target host) or containers (docker/podman, unverified against a real
  engine). There are no microVM, VM, or WASM computers; `kvm`,
  `firecracker`, and `gpu` are placement labels only.
- No cloud provisioning: a target must already run `compute serve`.
- No service mesh or distributed consensus; ingress, domains, and TLS serve
  node environments only.

The full inventory, with status and evidence for each claim, is
[audit.md](audit.md); the runtime and provider coverage is in
[runtime-matrix.md](runtime-matrix.md) and
[provider-matrix.md](provider-matrix.md).
