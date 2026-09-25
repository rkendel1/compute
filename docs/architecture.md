# Compute architecture

> **Compute does not implement a second durable-state system. FeltDB is the
> authoritative durable-state substrate. Compute may keep bounded, ephemeral
> working state and caches, but they are never authority.**
>
> **Compute reads targeted state through bounded, indexed FeltDB queries and
> coherent multi-record views through FeltDB snapshots. Whole-collection
> reads filtered in Compute are prohibited on production controller paths.**
>
> The contract, and the working state the controller may keep, is
> [docs/feltdb.md](feltdb.md).

Compute runs applications on a node. It has three parts, and each can fail
without taking the others down with it:

```text
operators, CLI, UI, AppPort
        │  TLS, operator credentials, scopes, audit
        ▼
controller  (`compute start`)
  ├── Compute API, reconciliation, releases, ingress (:80/:443)
  ├── working state: snapshots of control state and a read cache
  │   (derived, labelled with their freshness, never an authority)
  └── talks to ─┬─▶ control state: FeltDB (or a file) ─ durable desired state + evidence
                │     bounded indexed queries, coherent snapshots, fenced transactions
                └─▶ data plane: the supervisor (`compute supervisor`)
                        ├── workload processes (process groups, host isolation)
                        ├── endpoint listeners (host port ─▶ instance)
                        └── outcomes of units that ended, kept until acknowledged
```

- **Control state** is the durable authority: environments, projects,
  revisions, deployments, instances, traffic, domains, executions,
  receipts, events, operator credentials (verifiers only), and the audit
  trail. Nothing else is.
- **The controller** is stateless apart from caches it can rebuild. It
  reads desired state, drives the data plane toward it, and records what
  happened. It can be restarted, killed, or replaced by another build while
  workloads keep serving.
- **The data plane** runs workloads and endpoints. It knows only what it
  runs (units, routes, and outcomes), and it holds no secrets beyond the
  environment of the processes it started. Its manifest
  (`<state-dir>/supervisor/`) never contains plaintext secrets.

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
| 15 | Compute does not become an application-specific product framework. | Review: nothing in Compute names an application. |
| 16 | Controller paths read FeltDB through bounded, indexed queries and snapshots; nothing scans a collection to find a few records. | `feltdb_consumer.rs`: `the_controller_keeps_authority_in_feltdb_through_an_outage` (a quiet cycle runs no queries; scans are limited to the listed shapes); `consumer.rs`: `targeted_reads_are_indexed_and_bounded` |
| 17 | A snapshot is coherent: it never observes part of a transaction, and it is reused only while the revision it represents is current. | `compute_state::conformance` (memory, file, and a real FeltDB): concurrent paired writes, reuse, staleness, identity |
| 18 | A controller never runs on a model it would misuse, and the model is never downgraded. | `consumer.rs`: `the_upgrade_backs_up_migrates_and_verifies`, `a_newer_model_is_never_downgraded` |

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

## Non-goals

Compute does not orchestrate containers, schedule across nodes, run a
cluster control plane, provision clouds, or provide a service mesh or
distributed consensus. One node, one controller, one data plane, durable
state in FeltDB.
