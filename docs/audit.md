# Compute: product, architecture, and capability audit

Audited **2026-09-27** at **`69b70d9`** (branch `claude/great-galileo-z1xed0`).
This replaces the audit of 2026-09-25, kept as
[audit-2026-09-25.md](audit-2026-09-25.md).

**Re-audited for the foundation** on 2026-09-27, on `dfe7704` plus the
foundation change (branch `claude/pensive-brown-3qgk9f`): authenticated
targets (G-ARCH-1), durable-state semantics (G-ARCH-3), truthful machine
reality (G-ARCH-4), and browser certification in CI (G-UI-2) — all four
closed. The security and recovery journeys were re-run against a clean
`compute` launch by `audit-evidence/2026-09-27/foundation.py`
(`experiments.json#foundation`): the boundary is demonstrated, not
described. Findings that the foundation did not touch keep their original
evidence; where a finding changed, the table says what it was and what it
is now.

This is an audit, not a design. Every claim is classified and carries its
evidence: source, a test, a command that was run, or an experiment whose
output is recorded. Nothing is counted as a capability because a document
describes it. Where something could not be run here, it says so.

| Deliverable | What it holds |
| --- | --- |
| [audit.json](audit.json) | Everything below, machine-readable: 72 capabilities, 19 journeys, 182 CLI commands, 128 API routes, UI routes, tests, experiments, models, gaps, readiness, backlog |
| [architecture.md](architecture.md) | The architecture as built: processes, modules, model, execution paths, state, authority |
| [runtime-matrix.md](runtime-matrix.md) | Workload runtimes and computer substrates |
| [provider-matrix.md](provider-matrix.md) | Pool members, session providers, provisioning adapters, placement |
| [product-surface.md](product-surface.md) | UI, CLI, API, AppPort inventories and parity; agent usability |
| [gap-analysis.md](gap-analysis.md) | Every gap: current, desired, impact, evidence, next step; the ordered path |
| [base-vs-complete-compute.md](base-vs-complete-compute.md) | Today vs. the complete product, and the readiness matrix |
| [audit-evidence/2026-09-27/](audit-evidence/2026-09-27/) | The harness and raw results: `experiments.py` → `experiments.json`, `cli.json`, `api.json`, `tests.json`, `ui_perf.mjs`, and the generators |
| [product-surface/](product-surface/README.md) | 28 screenshots of the verified journey |

`crates/compute-cli/tests/audit.rs` keeps the audit honest: it fails when a
CLI command or API route exists that `audit.json` does not list (or the
reverse), when a status is outside the vocabulary, or when cited evidence
does not exist. The tables in these documents are generated from
`audit.json` by `audit-evidence/2026-09-27/render_docs.py`.

**Vocabulary.** Capabilities: IMPLEMENTED + VERIFIED (code, and a test or
experiment proves it works), IMPLEMENTED (code, no proof here), PARTIAL,
DOCUMENTED ONLY, STUB, UNUSED, BROKEN, MISSING, UNKNOWN. Journeys: PASS,
PARTIAL, FAIL, NOT IMPLEMENTED. Documents: DOCUMENTED CORRECTLY, OUTDATED,
INCOMPLETE, MISLEADING, MISSING.

**Environment.** Linux x86_64 VM (Firecracker guest), 4 CPU, 16 GiB, kernel
6.18, debug build. No `/dev/kvm`, no GPU, a docker client with **no engine**,
no podman, no firecracker. State backend: `file` (the launcher's default).
FeltDB-backed tests need `FELTDB_SERVER_BIN` and were not run in this audit
(17 tests); they run in CI (`feltdb-consumer.yml`).

---

## Answers

The thirty questions a reader of this audit must be able to answer.

1. **What is Compute?** A control plane (`compute start`, the daemon) that
   turns machines running `compute serve` into durable, named **computers**,
   runs projects in them, and takes software through build, test, publish,
   deploy, promote, and rollback — from a browser UI, a CLI, an HTTP API, and
   a TypeScript client for agents. Underneath is a runtime-neutral workload
   engine (`compute run`) and an older node deployment model that runs on
   the daemon host.
2. **How do I install it?** Build from source (`cargo build --release`; the
   binary is `target/release/compute`). CI builds and certifies a release
   distribution, but there is no installer or package. Language runtimes
   download on demand. *PARTIAL.*
3. **How do I launch it?** `compute`. It starts a local target
   (`compute serve`, 127.0.0.1:8788) and the control plane (127.0.0.1:8787)
   with state in `~/.compute`, and opens the UI: 3.7 s cold, 0.01 s when
   already running. `compute down` stops both. *PASS.*
4. **How do I run a project?** UI: "Run a project" → pick a Git repository
   (URL or local folder) → Compute inspects it inside a computer and
   proposes an assembly (runtime, build, test, start, ports,
   configuration) → review → GO. CLI: `compute environment propose`, then
   `project add` / `contents apply`. *PASS* (Git sources only).
5. **How do I run multiple projects?** Add another project to the same
   computer and GO again; both run side by side, and the provider resource
   is unchanged. *PASS.*
6. **What does discovery find?** Per target: CPU count, memory, disk,
   OS/architecture, language runtimes (installed/available/ready),
   isolation facilities (landlock, network namespaces, cgroups), and
   feature labels (`kvm`, `virtualization`, `firecracker`, `containers`,
   `gpu`) **inferred from device files and binaries on PATH** — `containers`
   was advertised here with no engine. It does not discover networks or
   machines; targets come from a pool file. *PARTIAL.*
7. **What runtimes exist?** Workload runtimes: wasm, node, bun, deno,
   python, ruby, php, jvm, dotnet, native, shell (verified). Computer
   substrates: workspace (verified), container (adapter, unverified).
   Firecracker, KVM, GPU, WASM computers: MISSING.
8. **What providers exist?** `local` (in process), `compute serve` targets,
   a daemon node (`/compute/*`), DNS providers. No Fly, Railway, Render,
   cloud, or bare-metal provisioning. *PARTIAL.*
9. **How does placement work?** Requirements (CPU, memory, disk,
   architecture, network, isolation, session capabilities, target features,
   an explicit target) are matched against every target; refusals carry a
   reason per target (`cpu_unavailable`, `target_feature_unsupported`, …).
   Features are matched as labels; nothing checks they work.
   *IMPLEMENTED + VERIFIED*, with dead options (persistent storage, public
   endpoint, terminal are offered by no provider).
10. **What is a Computer?** The durable machine behind an environment: a
    `Computer` record (status, generation, target, session, provider
    resource, observed contents) in control state, realised as a persistent,
    claimed session on a target.
11. **What is an Environment?** A named durable record with desired state,
    configuration, policy, and optionally an owner, a computer, and contents.
    The same word also names a "node environment" whose bundle projects run
    on the daemon host.
12. **What is a Session?** Two things. A **target session** is a machine on
    a `compute serve` target (a computer *is* one); `compute session
    create…destroy` makes one directly, bypassing the control plane. A
    **work session** is a control-state record of an operator working in an
    environment (attached) or owning a temporary one (ephemeral).
13. **What is a Target?** A pool member that hosts computers: a `compute
    serve` node with a session provider, listed by `compute target list` /
    `GET /targets` with health, resources, capabilities, and features.
14. **Where does execution happen?** For computers: on the computer's
    target, always, as durable jobs. For node environments, bundle
    projects, `compute deploy <dir>` applications, and `/compute/*`: on the
    daemon host. For `compute run`: in the CLI process. See
    [architecture.md](architecture.md#where-execution-happens).
15. **Where does durable state live?** Control state — FeltDB for a
    production control plane; without configuration, a local file that
    every surface labels `local-development` — holds environments,
    computers, contents, work sessions, versions, rollouts, events,
    credentials, audit. The machines and their jobs live in each target's
    own stores.
16. **What survives restart?** Everything durable. A control-plane restart
    (2.5 s) resumes every driver; a target restart recovers the same
    session. Processes in workspace computers restart through
    reconciliation. A lost machine or session is noticed and reported
    `lost`; an unreachable target is reported `unreachable`. *PASS*.
17. **How does reconciliation work?** One driver per computer walks its
    steps as durable jobs, each write fenced by generation; a 15 s process
    probe restarts dead processes; an orphan sweep destroys unclaimed
    sessions; publish and rollout drivers resume after restarts. Every
    running computer is confirmed with its target every 10 s: `unreachable`
    and `lost` are recorded (fenced, evented) without touching desired
    state.
18. **How does the UI work?** A single-page app served by the daemon, in two
    modes: **Work** (enter a computer: projects, processes, terminal, files,
    GO) and **Manage** (environments, software, versions, operations,
    domains, events), with an action home. Live via the event stream.
19. **How does the CLI work?** `compute …`: 182 commands. Most talk to the
    daemon API (`--daemon`, `$COMPUTE_DAEMON`, default 127.0.0.1:8787);
    `run`/`runtimes`/`doctor` run locally; `session create…` and `remote`
    talk to targets directly.
20. **How do agents use Compute?** Through AppPort (a TypeScript function
    for every UI operation), the CLI with `--json`, or the API, under an
    operator credential with scopes. Agents can also run inside computers.
21. **How do I build?** `compute environment build` or the Build button:
    the project's build command runs in its computer as a durable job.
    *PASS.*
22. **How do I test?** `compute environment test` or Test. *PASS.*
23. **How do I publish?** `compute versions publish <project> --environment
    <env>` or Publish: build, tests, checks, and a source package digest, run
    in the computer, recorded as an immutable version with step evidence.
    *PASS* — no artifact is stored.
24. **How do I deploy?** `compute versions deploy <project> <version>
    --environment <env>`: a rollout with steps, in place in the target
    computer. *PASS* — processes restart (no zero-downtime).
25. **How do I promote?** `compute versions promote --from test --to
    production`: a reviewed plan (changes, configuration differences), then
    a generation-fenced rollout. *PASS* — no approvals.
26. **How do I roll back?** `compute versions rollback --environment
    production [--to v]`. *PASS.*
27. **How do I operate?** Logs (on demand), processes, restart, health
    (15 s liveness probe), configuration, commands, stop/resume/replace.
    *PARTIAL*: no streaming logs, metrics in the UI, HTTP health checks, or
    scaling.
28. **What fails safely?** Stale GO (generation conflict, refused);
    control-state outages (`state_unavailable`, nothing half-written);
    daemon restarts; target restarts; placement that cannot be satisfied
    (refused with reasons); admission refusals; unknown bearer tokens (401).
29. **What is missing?** Isolation between computers; container
    computers verified against a real engine; microVM/VM computers;
    provisioning adapters; ingress, TLS, and zero-downtime for computer
    applications; stored release artifacts; approvals; streaming
    observability. The full list is
    [What's missing](#27-whats-missing).
30. **What is the exact gap?** Compute has the complete **developer loop on
    one trusted host**, and a control plane that is the authority for its
    targets and says what is true of its machines. It does not yet have
    **isolation between computers** (they share a host user) or **machines
    beyond a workspace** (no verified container, no microVM, no cloud).
    Production is an environment's name, not a protected, routed,
    zero-downtime place.

---

## 1. Baseline

| | |
| --- | --- |
| Commit | `69b70d9` "Make the control plane the complete product surface" |
| Code | 15 Rust crates, ~75k lines of Rust in `src/` (compute-environment 23k, compute-cli 17.6k, compute-core 8.7k, compute-provider 7.8k); UI 2.3k lines of JS; 3 TypeScript packages |
| Model | Control-state model generation 8 (`compute.flow`; generation 6 at the original audit) |
| Tests | 382 (351 Rust, 31 TypeScript/browser); 17 need a FeltDB server |
| CI | `test.yml`, `feltdb-consumer.yml`, `distribution-certification.yml` |
| Last full run | `cargo test --workspace`: pass. `packages/compute-ui-e2e`: pass, in CI with Chromium (see §22) |

Capability counts by status (of 74):

<!-- audit:capability_summary -->
| Status | Capabilities |
| --- | --- |
| IMPLEMENTED | 3 |
| IMPLEMENTED + VERIFIED | 53 |
| PARTIAL | 4 |
| DOCUMENTED ONLY | 0 |
| STUB | 0 |
| UNUSED | 0 |
| BROKEN | 1 |
| MISSING | 13 |
| UNKNOWN | 0 |
<!-- /audit -->

## 2. Architecture map

See [architecture.md](architecture.md) for the diagram, the module table, and
the invariants. In one paragraph: the **daemon** is the authority's front
(API, UI, controllers, placement, audit) over **control state** (file or
FeltDB). **Targets** (`compute serve`) host computers as sessions through a
**session provider** (workspace or container) and run their commands as
durable jobs in their own stores. A **supervisor** on the daemon host runs
the older node-environment workloads. The CLI, UI, and AppPort are clients of
the daemon API; a few CLI commands bypass it to reach targets directly.

The architecture is sound where it is used for computers: one authority,
fenced changes, durable jobs, resumable drivers, and — since the foundation
— an authenticated daemon-to-target hop and observed machine reality. It is
still inconsistent in one place: two deployment models coexist, one of which
executes on the daemon host. Durable state is FeltDB for production; the
file backend is local development and says so.

## 3. CLI inventory

185 leaf commands, each annotated in [audit.json](audit.json) (`cli`) with
its help text, help defect, what it talks to, the state it touches, the
authority it runs under, its UI equivalent, and the tests that invoke it.
Summary and the full list: [product-surface.md](product-surface.md#cli).

<!-- audit:cli_summary -->
| Group | Commands | Help defects | Never invoked by a test | Talks to |
| --- | --- | --- | --- | --- |
| `compute up` | 1 | 0 | 0 | launcher |
| `compute down` | 1 | 0 | 0 | launcher |
| `compute versions` | 6 | 0 | 6 | daemon API /software, /rollouts |
| `compute init` | 1 | 0 | 0 | local files |
| `compute application` | 8 | 0 | 0 | daemon API /applications |
| `compute run` | 1 | 1 | 0 | local engine, or a provider chosen by pool placement |
| `compute bundle` | 3 | 3 | 0 | local |
| `compute deps` | 3 | 3 | 1 | local |
| `compute inspect` | 1 | 1 | 0 | local |
| `compute runtimes` | 1 | 1 | 0 | local / pool |
| `compute runtime` | 1 | 1 | 0 | local |
| `compute capabilities` | 1 | 1 | 0 | local |
| `compute isolation` | 1 | 0 | 0 | local |
| `compute exec` | 1 | 1 | 0 | local engine |
| `compute doctor` | 1 | 0 | 0 | local + daemon /status |
| `compute certify` | 1 | 1 | 0 | local |
| `compute distribution` | 3 | 3 | 1 | local |
| `compute receipt` | 2 | 2 | 0 | local |
| `compute version` | 1 | 1 | 0 | local |
| `compute remote` | 11 | 11 | 6 | provider compute.remote@1 directly |
| `compute provider` | 5 | 0 | 2 | pool config + providers |
| `compute capacity` | 1 | 0 | 1 | pool |
| `compute jobs` | 1 | 0 | 1 | pool |
| `compute placement` | 2 | 0 | 1 | pool |
| `compute pool` | 2 | 0 | 0 | pool placement then provider |
| `compute target` | 4 | 0 | 0 | daemon API /targets, local |
| `compute session` | 13 | 0 | 3 | daemon API /sessions, pool placement then target sessions directly, targets directly |
| `compute policy` | 4 | 0 | 0 | local |
| `compute explain` | 1 | 0 | 1 | local / pool |
| `compute start` | 1 | 0 | 0 | starts the daemon |
| `compute stop` | 1 | 0 | 0 | daemon API /shutdown, or an application |
| `compute status` | 1 | 0 | 0 | daemon API |
| `compute logs` | 1 | 0 | 0 | daemon API /applications |
| `compute history` | 1 | 0 | 0 | daemon API /applications |
| `compute rollback` | 1 | 0 | 0 | daemon API /applications |
| `compute environment` | 39 | 9 | 23 | daemon API /environments |
| `compute project` | 12 | 5 | 9 | daemon API |
| `compute workload` | 6 | 4 | 5 | daemon API |
| `compute execution` | 1 | 0 | 0 | daemon API |
| `compute deploy` | 1 | 0 | 0 | daemon API |
| `compute promote` | 1 | 0 | 0 | daemon API |
| `compute deployment` | 5 | 1 | 2 | daemon API |
| `compute domain` | 5 | 2 | 5 | daemon API |
| `compute dns` | 2 | 0 | 2 | daemon API |
| `compute certificate` | 2 | 0 | 2 | daemon API |
| `compute network` | 1 | 0 | 1 | daemon API |
| `compute events` | 1 | 0 | 0 | daemon API |
| `compute service` | 4 | 1 | 4 | daemon API |
| `compute control-plane` | 3 | 0 | 2 | FeltDB directly |
| `compute serve` | 1 | 0 | 0 | runs a target |
| `compute auth` | 6 | 0 | 6 | daemon API |
| `compute node` | 6 | 0 | 2 | daemon API |
| `compute supervisor` | 1 | 0 | 1 | internal |
<!-- /audit -->

Findings: 52 commands have defective help (wrong text from a flattened
argument, or none); 87 are never invoked by any test through the CLI;
`compute session` holds two unrelated concepts; four command families
deploy.

## 4. UI audit

19 routes; Work and Manage modes over one control plane; verified end to end
in a real browser by `product_journey.rs` (57 s, 28 screenshots in
[product-surface/](product-surface/README.md)).

<!-- audit:capabilities area=ui -->
| ID | Capability | Status | User can use | In complete model | Notes | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| `ui-modes` | Work / Manage modes of one control plane | **IMPLEMENTED + VERIFIED** | yes | yes | Run in CI with Chromium (test.yml `browser`, COMPUTE_REQUIRE_BROWSER: a missing browser fails); skip elsewhere without Playwright/Chromium. | `crates/compute-environment/ui/app.js`<br>`crates/compute-environment/ui/index.html`<br>`crates/compute-cli/tests/work_mode_ui.rs`<br>`crates/compute-cli/tests/product_journey.rs` |
| `ui-home` | Action-first home ("What do you want to do?") | **IMPLEMENTED + VERIFIED** | yes | yes | Shows software and computers; environments without a computer are not on the home page. | `crates/compute-environment/ui/app.js#homeView`<br>`crates/compute-cli/tests/product_journey.rs` |
| `ui-certification-package` | packages/compute-ui-e2e browser certification | **IMPLEMENTED + VERIFIED** | n/a | yes | Fixed for the action home (`#/` → `#/environments`); runs in CI with Chromium: the operator journey, and a computer that is created, runs, becomes unreachable, recovers, is lost, and is replaced, launched with `compute up`. | `packages/compute-ui-e2e/src/control-plane.test.mjs`<br>`packages/compute-ui-e2e/src/computer-reality.test.mjs`<br>`.github/workflows/test.yml`<br>journey `experiments.json#ui_certification_package` |
<!-- /audit -->

Work mode answers "what am I working on?": computers you can enter, and
inside one its projects, processes, endpoints, terminal, files, and GO with
a conflict refresh. Manage mode answers "what exists and where does it
run?": environments, software and versions, operations, services, domains,
events. What the UI cannot do (targets, access, diagnosis, metrics, …) is
listed in [product-surface.md](product-surface.md#ui).

## 5. Journeys

Every journey was run against a real control plane and target. "PASS" means
it completed and a test asserts it.

<!-- audit:journeys -->
| Journey | Steps | Result | Notes | Evidence |
| --- | --- | --- | --- | --- |
| `first-launch` | compute → UI → first-run experience | **PASS** | The home page offers actions; there is no guided first-run flow, and a machine with no free memory for the 1 GiB default gets an admission error in the wizard. | `crates/compute-cli/tests/product_journey.rs`<br>`experiments.json#launch_seconds` |
| `run-a-project` | choose → inspect → configure → GO → running | **PASS** | — | `crates/compute-cli/tests/product_journey.rs` |
| `multi-project` | project A → GO → project B → GO → same computer | **PASS** | — | `crates/compute-cli/tests/product_journey.rs` |
| `change-in-place` | change desired state → GO → same computer changes | **PASS** | Provider resource identical before and after. | `crates/compute-cli/tests/product_journey.rs`<br>`crates/compute-environment/tests/computers.rs` |
| `build` | project → build → result | **PASS** | — | `crates/compute-cli/tests/product_journey.rs` |
| `test` | project → test → result | **PASS** | — | `crates/compute-cli/tests/product_journey.rs` |
| `publish` | project → version → publish | **PASS** | — | `crates/compute-cli/tests/product_journey.rs`<br>`crates/compute-environment/tests/computers.rs` |
| `deploy` | version → environment → deploy | **PASS** | — | `crates/compute-cli/tests/product_journey.rs`<br>`crates/compute-environment/tests/computers.rs` |
| `promote` | test → production | **PASS** | No approvals. | `crates/compute-cli/tests/product_journey.rs`<br>`crates/compute-environment/tests/computers.rs` |
| `rollback` | production → previous version | **PASS** | — | `crates/compute-cli/tests/product_journey.rs`<br>`crates/compute-environment/tests/computers.rs` |
| `operate` | logs, processes, health, restart, configuration, commands | **PARTIAL** | No streaming logs, no HTTP health, no metrics in the UI, no resource scaling short of replacement. | `crates/compute-cli/tests/product_journey.rs`<br>`crates/compute-environment/tests/computers.rs` |
| `session` | create → connect → exec → logs → stop → resume → destroy | **PASS** | Target sessions (not through the daemon). Work sessions through the daemon: open/close only. | `crates/compute-cli/tests/sessions.rs`<br>`crates/compute-provider/tests/sessions.rs` |
| `ephemeral` | create → use → expire → evidence retained | **PASS** | — | `crates/compute-environment/tests/computers.rs`<br>`crates/compute-cli/tests/product_journey.rs` |
| `replacement` | environment → replace → new computer → old retired | **PASS** | CLI/API and the Manage dialog. | `crates/compute-environment/tests/computers.rs` |
| `container-computer` | a computer in a real container | **NOT IMPLEMENTED** | Adapter exists; no engine available to verify; not in CI. | `experiments.json#environment` |
| `machine-loss` | the target loses the machine → Compute reports it | **PASS** | Lost within one liveness interval of the target answering; stays lost through reconcile and a control-plane restart; replacement brings a new machine. | `experiments.json#foundation`<br>`crates/compute-environment/tests/computers.rs`<br>`packages/compute-ui-e2e/src/computer-reality.test.mjs` |
| `target-outage` | target down → unreachable → target back → running, the same machine | **PASS** | Desired state is kept throughout. | `experiments.json#foundation`<br>`crates/compute-environment/tests/computers.rs`<br>`crates/compute-cli/tests/computers.rs`<br>`packages/compute-ui-e2e/src/computer-reality.test.mjs` |
| `stale-response` | observe A → A unavailable → lost → A's delayed answer → still lost | **PASS** | Through a proxy that holds a real answer back. | `crates/compute-environment/tests/computers.rs` |
| `target-security` | no / wrong / revoked / another control plane's credential → refused; own → accepted, across restarts | **PASS** | — | `experiments.json#foundation`<br>`crates/compute-cli/tests/sessions.rs`<br>`crates/compute-provider/tests/sessions.rs`<br>`crates/compute-cli/tests/launcher.rs` |
| `ui-certification` | packages/compute-ui-e2e | **PASS** | Runs in CI with Chromium. | `packages/compute-ui-e2e/src/control-plane.test.mjs`<br>`packages/compute-ui-e2e/src/computer-reality.test.mjs`<br>`.github/workflows/test.yml` |
| `readme-run` | `compute run script.py` from the README | **FAIL** | network "none" is unenforceable for python; needs --network. | `experiments.json#runtimes` |
| `readme-application` | `compute init my-app; compute deploy my-app` | **PASS** | Auto-starts a second control plane in ./.compute/daemon. | `experiments.json#application_journey` |
<!-- /audit -->

## 6. Computer model

<!-- audit:capabilities area=computer -->
| ID | Capability | Status | User can use | In complete model | Notes | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| `computer-environments` | Environments backed by a durable computer | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/src/daemon/computers.rs`<br>`crates/compute-core/src/computers.rs`<br>`crates/compute-environment/tests/computers.rs` |
| `persistent` | Persistent computers (no TTL, claimed) | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/tests/computers.rs` |
| `ephemeral` | Ephemeral computers that expire and keep evidence | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/tests/computers.rs#an_ephemeral_environment_expires_and_keeps_its_evidence`<br>`crates/compute-cli/tests/product_journey.rs` |
| `lifetime-change` | Change lifetime in place (claim on the target) | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/tests/computers.rs#go_changes_lifetime_and_configuration_in_place_and_refuses_stale_views` |
| `in-place-change` | Contents changed in place, no redeployment | **IMPLEMENTED + VERIFIED** | yes | yes | Same provider resource across releases, configuration changes, and controller restarts. | `crates/compute-environment/tests/computers.rs#deployment_is_reconciliation_of_the_same_computer`<br>`crates/compute-cli/tests/product_journey.rs` |
| `replacement` | Explicit replacement provisions a new machine and retires the old | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/tests/computers.rs#provider_failures_and_replacements_are_explicit`<br>`crates/compute-environment/tests/computers.rs#deployment_is_reconciliation_of_the_same_computer` |
| `stop-resume` | Stop and resume the same machine | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/tests/computers.rs`<br>`crates/compute-cli/tests/product_journey.rs` |
| `orphan-sweep` | Sessions no computer record claims are torn down | **IMPLEMENTED + VERIFIED** | no (automatic) | yes | — | `crates/compute-environment/tests/computers.rs#provisioning_survives_a_controller_restart_and_orphans_are_torn_down` |
| `machine-loss` | A machine or session the target lost makes the computer `lost` | **IMPLEMENTED + VERIFIED** | yes | yes | Every running computer is confirmed with its target (liveness every 10 s, whatever runs in it). A target that answers without the session, or whose provider no longer has the machine, makes it lost: desired state kept, never re-provisioned on its own, stays lost through reconcile and a control-plane restart until it is replaced or destroyed. | `crates/compute-environment/src/daemon/computers.rs#observe_machine,apply_observation,lost_step`<br>`crates/compute-provider/src/sessions.rs#environment_lost`<br>`crates/compute-environment/tests/computers.rs#a_machine_or_session_that_disappears_is_lost_until_replaced`<br>`crates/compute-cli/tests/computers.rs#the_cli_reports_observed_reality_not_desired_state`<br>`packages/compute-ui-e2e/src/computer-reality.test.mjs`<br>journey `experiments.json#foundation` |
| `target-down-visibility` | A computer whose target is unreachable says so | **IMPLEMENTED + VERIFIED** | yes | yes | `unreachable` (target_unreachable, or credential_rejected when the target refuses this control plane) within one liveness interval; exec answers runtime_unavailable naming it; the same machine returns to running when the target answers. | `crates/compute-environment/src/daemon/computers.rs#unreachable_step`<br>`crates/compute-environment/tests/computers.rs#an_unreachable_target_keeps_desired_state_and_recovers_the_same_machine`<br>`crates/compute-cli/tests/computers.rs`<br>`packages/compute-ui-e2e/src/computer-reality.test.mjs`<br>journey `experiments.json#foundation` |
| `stale-fencing` | A stale target answer cannot revive a lost computer | **IMPLEMENTED + VERIFIED** | no (automatic) | yes | Every observation is applied only to the record version it was made against (the generation-fenced write); lost is sticky: only an operator reconcile with a fresh answer can find the machine again. | `crates/compute-environment/src/daemon/computers.rs#apply_observation`<br>`crates/compute-environment/tests/computers.rs#a_stale_answer_from_a_target_cannot_revive_a_lost_computer` |
| `reality-surfaces` | Desired and observed state, told apart, on every surface | **IMPLEMENTED + VERIFIED** | yes | yes | One model (`reality`: desired, observed, confirmed_at, since, explanation) in the API, `compute environment status`, the UI, and AppPort. An environment on a computer is never `running` because it is meant to be: it is what its computer was last observed to be. | `crates/compute-environment/src/status.rs#ComputerReality`<br>`crates/compute-environment/ui/app.js#realityPanel`<br>`crates/compute-cli/src/computer_cmd.rs#print_computer`<br>`packages/compute-appport/src/computers.ts#ComputerReality`<br>`crates/compute-cli/tests/computers.rs#the_cli_reports_observed_reality_not_desired_state`<br>`packages/compute-ui-e2e/src/computer-reality.test.mjs`<br>`crates/compute-environment/tests/computers.rs` |
| `contents` | Repositories, packages, processes, projects, configuration as desired state | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/tests/computers.rs`<br>`crates/compute-cli/tests/product_journey.rs` |
| `endpoints` | Process ports published as endpoints (target host:port) | **IMPLEMENTED + VERIFIED** | yes | yes | No port publishing for the container provider; no ingress/TLS/domains for computer endpoints. | `crates/compute-environment/tests/computers.rs`<br>`crates/compute-cli/tests/product_journey.rs` |
| `go-fencing` | GO: one generation-fenced change; stale views refused | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/tests/computers.rs`<br>`crates/compute-cli/tests/work_mode_ui.rs` |
| `work-sessions` | Work sessions (attached / ephemeral) in FeltDB | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/tests/computers.rs#work_sessions_enter_environments_and_temporary_ones_end_with_them` |
| `files` | Files in the computer | **PARTIAL** | partial | yes | List and read (head -c 64 KiB) through exec jobs; no upload, edit, or download. | `crates/compute-environment/ui/app.js#listFiles`<br>`crates/compute-cli/tests/product_journey.rs` |
| `terminal` | Terminal | **PARTIAL** | partial | yes | Each line is a durable job; no interactive PTY. The `terminal` session capability is offered by no provider. | `crates/compute-cli/tests/product_journey.rs` |
<!-- /audit -->

A computer is created (placed, provisioned, applied) in **0.61 s** as a
workspace; changes are applied **in place** — the provider resource is
identical before and after contents, configuration, releases, and restarts.
Replacement is explicit. The model's weakness is observation: the record is
trusted until a step fails.

## 7. Durable state

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

<!-- audit:capabilities area=state -->
| ID | Capability | Status | User can use | In complete model | Notes | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| `state-feltdb` | FeltDB as the durable authority of a production control plane (model generation 8) | **IMPLEMENTED + VERIFIED** | configuration | yes | The production decision (docs/feltdb.md): `[state] backend = "feltdb"` (or `--state feltdb`); `compute` and `compute start` pass it through. /info and `compute status` report `durability: production`. | `crates/compute-state-feltdb/tests/consumer.rs`<br>`crates/compute-environment/tests/feltdb_consumer.rs` |
| `state-default-file` | Without configuration, control state is a local file, stated as local development | **IMPLEMENTED + VERIFIED** | yes | yes | The file backend is kept for local development behind the same StateStore abstraction and labelled everywhere: `compute` prints it, /info, `compute status`, and `compute node info` report `durability: local-development`. The launcher uses whatever `[state]` says; it never picks a different model silently. | `crates/compute-cli/src/control_state.rs#backend_name`<br>`crates/compute-state/src/store.rs#durability`<br>`crates/compute-cli/src/launch_cmd.rs#durability_note`<br>`crates/compute-cli/tests/launcher.rs`<br>journey `experiments.json#foundation` |
| `restart-recovery` | Control-plane restart keeps and resumes everything | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/tests/computers.rs`<br>`crates/compute-cli/tests/recovery.rs`<br>journey `experiments.json#after_control_plane_restart` |
<!-- /audit -->

Consistency with the contract ([feltdb.md](feltdb.md)): controller reads are
identity lookups, indexed equality queries, and snapshots, enforced by
`feltdb_consumer.rs` in CI. The durable-state decision is explicit: FeltDB is
the production authority; without configuration, control state is a local
file that every surface labels `local-development`. One departure remains:
target jobs are durable outside the authority (the daemon keeps references
and events, not the jobs).

## 8. Daemon

The daemon (`compute start`) serves the API and UI and runs the controllers
([architecture.md](architecture.md#reconciliation)). It is restartable
without losing anything (2.46 s to serve again, every driver resumed), its
errors carry a failure kind, and every mutation is audited. It is also still
an **executor**: node environments, bundle projects, applications, and
`/compute/*` jobs run on its host (§13).

## 9. Discovery

<!-- audit:capabilities area=discovery,targets -->
| ID | Capability | Status | User can use | In complete model | Notes | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| `target-inventory` | Targets listed with health, platform, resources, capabilities, features | **IMPLEMENTED + VERIFIED** | CLI/API only | yes | Includes how each target authenticates (`credential`, or `insecure-unauthenticated`) and whether this control plane presents a credential. Not shown in the UI. | `crates/compute-cli/tests/computers.rs`<br>`crates/compute-cli/tests/launcher.rs`<br>CLI `compute target list`<br>API `GET /targets` |
| `discovery-resources` | CPU count, memory, disk, OS/architecture discovered | **IMPLEMENTED + VERIFIED** | indirect | yes | — | journey `GET /compute/capabilities on this host: 4 CPU, 16.9 GB, 270 GB, linux-x86_64` |
| `discovery-runtimes` | Language runtimes discovered (installed/available/ready) | **IMPLEMENTED + VERIFIED** | CLI | yes | — | CLI `compute runtimes`<br>CLI `compute doctor` |
| `discovery-isolation` | Isolation facilities discovered (landlock ABI, network namespaces, cgroups) | **IMPLEMENTED + VERIFIED** | CLI | yes | — | CLI `compute isolation` |
| `discovery-features` | Target features: kvm, virtualization, firecracker, containers, gpu | **PARTIAL** | indirect | yes | `containers` is inferred from a docker/podman binary on PATH: advertised on this host with no engine running. gpu = /dev/nvidia0 exists. kvm = /dev/kvm openable. No nested-virtualization, GPU model, or engine liveness check. | `crates/compute-provider/src/lib.rs#detect_target_features`<br>journey `experiments.json#placement_refusals` |
| `discovery-network` | Network interfaces, reachability, exposed ports | **MISSING** | no | yes | Not discovered. Endpoint hosts come from the pool endpoint URL. | — |
| `discovery-automatic-targets` | Targets discovered automatically (no configuration) | **MISSING** | no | yes | Targets come from a pool file. The launcher writes one naming the local host; no LAN/cloud discovery. | — |
<!-- /audit -->

Observed targets after `compute` on this host:

| Target | Kind | Health | Hosts computers | Features |
| --- | --- | --- | --- | --- |
| `local` | local | healthy | no | — |
| `this-machine` | remote (`compute serve`) | healthy | yes | `containers` (docker binary, no engine) |

## 10. Runtime matrix

<!-- audit:runtimes -->
| Runtime | Implemented | Discoverable | Placeable | Executable | UI | CLI | Tests | Production ready |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| native (process) | yes | yes | yes | yes | computers (workspace) | yes | yes | no (no isolation boundary) |
| language runtimes (node, bun, deno, python, ruby, php, jvm, dotnet, shell) | yes | yes | yes | yes | no (workload engine) | yes | yes | partial (process isolation only; landlock/netns where available) |
| wasm (wasmtime, WASI p1) | yes | yes | yes | yes | no | yes | yes | workload engine only |
| containers (docker/podman) | yes | inferred from a binary on PATH | as a feature label, not as a substrate | unverified (fake docker only) | no | --containers / --session-provider container | fake docker | no |
| kvm | no | yes | label only | no | feature field | no | matching only | no |
| firecracker | no | binary + /dev/kvm | label only | no | feature field | no | matching only | no |
| gpu | no | /dev/nvidia0 exists | label only | no | feature field | no | matching only | no |
<!-- /audit -->

Details and the distinction between workload runtimes and computer
substrates: [runtime-matrix.md](runtime-matrix.md).

## 11. Providers

<!-- audit:capabilities area=provider -->
| ID | Capability | Status | User can use | In complete model | Notes | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| `provider-local` | Local provider (in-process engine) | **IMPLEMENTED + VERIFIED** | yes | yes | Runs workloads; does not host computers (sessions_unsupported). | `crates/compute-provider/src/lib.rs#LocalProvider`<br>`crates/compute-cli/tests/cli.rs` |
| `provider-remote` | Remote provider (`compute serve`, compute.remote@1) | **IMPLEMENTED + VERIFIED** | yes | yes | The only kind of target that hosts computers. | `crates/compute-provider/src/lib.rs#RemoteProvider`<br>`crates/compute-provider/tests/remote.rs`<br>`crates/compute-cli/tests/remote_pool.rs` |
| `provider-daemon-node` | A daemon node as a provider (deployments, /compute/*) | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-cli/tests/product.rs` |
| `provider-fly` | Fly Machines | **MISSING** | no | yes | — | — |
| `provider-railway` | Railway | **MISSING** | no | yes | — | — |
| `provider-render` | Render | **MISSING** | no | yes | — | — |
| `provider-cloud-vm` | Cloud VMs / bare metal provisioning | **MISSING** | no | yes | — | — |
| `provider-dns` | DNS providers (for domains) | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-network/src/dns.rs`<br>`crates/compute-network/tests/providers.rs`<br>`crates/compute-environment/tests/network.rs` |
<!-- /audit -->

Details: [provider-matrix.md](provider-matrix.md).

## 12. Placement

<!-- audit:capabilities area=placement -->
| ID | Capability | Status | User can use | In complete model | Notes | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| `placement` | Capability-matched placement with reasons | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-placement/src/matching.rs`<br>`crates/compute-placement/tests/matching.rs`<br>`crates/compute-placement/tests/selection.rs` |
| `placement-override` | Constrain placement to a target | **IMPLEMENTED + VERIFIED** | yes (dialog field) | yes | — | `crates/compute-environment/tests/computers.rs#placement_chooses_a_target_by_what_the_computer_needs` |
| `placement-dead-options` | UI offers persistent storage / public endpoint | **BROKEN** | no | yes | No provider offers either; any computer requesting them is refused (with reasons). | `crates/compute-provider/src/sessions.rs#capabilities`<br>`crates/compute-provider/src/containers.rs#capabilities`<br>journey `experiments.json#placement_refusals` |
<!-- /audit -->

<!-- audit:placement -->
| Placement understands | How |
| --- | --- |
| gpu | label only |
| networking | network policy only |
| persistence | capability no provider offers |
| runtimes | workload runtimes yes; computer substrates no |
| storage | disk bytes only |
| virtualization | label only |
<!-- /audit -->

Failure modes: `no_compatible_provider` with per-target reasons (shown in the
UI's create dialog and computer page); placed-then-refused by the target's
admission (`resource_unavailable`, shown as the computer's failure); and the
silent one — a feature advertised but unusable is placed anyway.

## 13. Execution

<!-- audit:execution_paths -->
| Path | Where it executes | Authority | Durable record | Canonical job path |
| --- | --- | --- | --- | --- |
| `compute run` (local) | the caller's machine, in process | none (local user) | execution record + receipt on disk | no |
| `compute pool run/submit`, `compute remote *` | the provider placement chose | provider: a target credential on compute serve; daemon /compute/* only behind the daemon API's scopes | provider job store | yes |
| `compute session create/exec` (target sessions) | the target | target credential; owner = the control plane the credential names | target session/job stores | yes |
| Computer operations (sync, install, build, start/stop, probe, inspect, publish steps) | the environment's computer | daemon controller | target jobs; evidence in FeltDB | yes |
| `environment exec/run/build/test/propose` | the environment's computer | daemon scope + owner | target jobs; events in FeltDB | yes |
| Bundle project workloads (services, tasks) and releases | THE DAEMON HOST (supervisor) | daemon scopes, no owner | Execution records in control state | no |
| Applications (`compute deploy <dir>`, `compute application …`) | the application's computer on a target of the selected daemon's pool | daemon scope + owner (the computer's) | environment, computer, version, rollout in FeltDB; target jobs and receipts | yes |
| Daemon /compute/* (node as provider) | the daemon host | daemon execute scope | daemon job store | yes |
<!-- /audit -->

Every computer operation is a durable job on the computer's target with a
result and a receipt: one canonical path. The non-canonical paths are the
local `compute run` (by design) and the older node model on the daemon host.
Timing: exec submitted in 16 ms, completed round trip in 0.13 s.

## 14. Authorization

<!-- audit:authorization -->
| Operation | Check | On every operation |
| --- | --- | --- |
| Read environments/computers/software | read scope; any operator reads any computer view (only mutations are owner-bound) | yes |
| Change a computer environment (contents, config, lifetime, replace, destroy, sessions) | operate/deploy scope + owner | yes |
| Exec / run / connect / propose | execute scope + owner | yes |
| Publish / deploy / promote / rollback versions | deploy scope + owner of the environment(s) | yes |
| Applications (deploy, rollback, stop, logs) | the computer's: scope + owner of `application-<name>` | yes |
| Node environments, bundle projects, domains | scopes only; no ownership | yes |
| Loopback daemon without TLS | no credential required (development mode); `--production` requires TLS and credentials | bypassable locally |
| Target (`compute serve`) jobs and sessions | a target credential on every request (reads included); sessions and jobs owned by the control plane it names; `--insecure-unauthenticated` only by name | yes |
| Daemon /compute/* (node as provider) | only requests the daemon API authenticated and scoped (DaemonAuthorized) | yes |
<!-- /audit -->

The daemon's authority is real and tested (`security.rs`): scopes on every
route, owner checks for computers, credential rotation, an audit trail.
The target's authority, absent at the original audit (an unauthenticated
caller listed the target's sessions and wrote a file into a computer:
`experiments.json#target_exec_without_credential`), is now real: a target
trusts only the control planes it issued a credential to, owns what they
create by their identity, and refuses anonymous, wrong, and revoked
callers, while another control plane sees none of this one's sessions
(`experiments.json#foundation`, `crates/compute-cli/tests/sessions.rs`,
`crates/compute-provider/tests/sessions.rs`).

## 15. Deployment

Three deployment models exist:

| Model | Commands | Executes on | Zero-downtime | Ingress/domains |
| --- | --- | --- | --- | --- |
| Computer versions and rollouts | `compute versions …`, UI Software | the environment's computer | no | no |
| Bundle projects on node environments | `compute project/deployment/…` | the daemon host (supervisor) | yes | yes |
| Applications | `compute init`, `compute deploy <dir>`, `compute application …` | a provider node — the daemon host by default | versions, same endpoint | no |

`compute deploy my-app` from the README works, and silently starts a second
control plane with state in `./.compute/daemon`
(`experiments.json#application_journey`).

<!-- audit:capabilities area=legacy -->
| ID | Capability | Status | User can use | In complete model | Notes | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| `bundle-projects` | Bundle projects, revisions, zero-downtime releases on the daemon node | **IMPLEMENTED + VERIFIED** | yes | reconcile | Executes on the daemon host (supervisor). Refused for environments with a computer. | `crates/compute-environment/src/daemon/release.rs`<br>`crates/compute-environment/src/daemon/deploy.rs`<br>`crates/compute-environment/tests/releases.rs` |
<!-- /audit -->

## 16. Release and version

<!-- audit:capabilities area=release -->
| ID | Capability | Status | User can use | In complete model | Notes | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| `publish` | Publish an immutable version (source, build, tests, checks, package digest) | **IMPLEMENTED + VERIFIED** | yes | yes | The version is a commit + digest + assembly; no artifact is stored (the package digest is computed, not kept). | `crates/compute-environment/src/daemon/software.rs`<br>`crates/compute-environment/tests/computers.rs#versions_are_published_deployed_promoted_and_rolled_back_in_place`<br>`crates/compute-cli/tests/product_journey.rs` |
| `deploy` | Deploy a version to an environment (rollout with steps) | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/tests/computers.rs`<br>`crates/compute-cli/tests/product_journey.rs` |
| `promote` | Promote test → production with a reviewed plan | **IMPLEMENTED + VERIFIED** | yes | yes | No approval workflow (approvals list is always empty). | `crates/compute-environment/tests/computers.rs`<br>`crates/compute-cli/tests/product_journey.rs` |
| `rollback` | Roll back to an earlier version | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/tests/computers.rs`<br>`crates/compute-cli/tests/product_journey.rs` |
| `zero-downtime` | Zero-downtime release inside a computer | **MISSING** | no | yes | A release restarts processes. Zero-downtime traffic switching exists only for bundle projects on the daemon node. | — |
| `approvals` | Promotion approvals | **MISSING** | no | yes | — | `crates/compute-environment/src/status.rs#PromotionPlan` |
| `artifact-store` | Stored build artifacts for versions | **MISSING** | no | yes | Bundle revisions store artifacts; computer versions store only the commit and digest. | — |
| `applications` | `compute init/deploy` applications: a compatibility view over a computer, a version, and a rollout | **IMPLEMENTED + VERIFIED** | yes | reconcile | Each application is its own computer environment on a target (never the daemon host): source imported by target jobs, published as a version, deployed as a rollout; runtimes that need the pinned catalog (wasm, jvm, dotnet) are refused. | `crates/compute-environment/src/daemon/applications.rs`<br>`crates/compute-environment/tests/applications.rs`<br>`crates/compute-cli/tests/product.rs`<br>journey `experiments.json#application_journey` |
<!-- /audit -->

A version is immutable: commit, package digest, assembly, and the evidence of
each step (build, tests, checks, package). Rollouts record their steps and
end states; promotion shows a plan (what changes, which configuration
differs) and is fenced by the target environment's generation, so a stale
plan is refused.

## 17. Production

"Production" is an environment name. What exists: promotion from a healthy
environment with a reviewed plan, rollback, restart, logs, a liveness probe.
What does not: protected environments, approvals, domains/TLS/ingress for
computer endpoints (endpoints are `target-host:port`), zero-downtime
rollouts, HTTP health checks, metrics and alerts in the UI, scaling short of
replacement, secrets distinct from configuration.

<!-- audit:capabilities area=operations,observability -->
| ID | Capability | Status | User can use | In complete model | Notes | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| `domains-tls` | Domains, DNS, ACME certificates, ingress | **IMPLEMENTED + VERIFIED** | yes (bundle projects) | yes | Routes to bundle-project services only; not to computer endpoints. | `crates/compute-environment/tests/network.rs`<br>`crates/compute-network/tests/ingress.rs` |
| `logs` | Process logs and job output | **IMPLEMENTED + VERIFIED** | yes | yes | Read on demand (tail); no streaming for computer processes. | `crates/compute-environment/tests/computers.rs`<br>`crates/compute-cli/tests/product_journey.rs` |
| `restart` | Restart a process in place | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/src/daemon/software.rs`<br>`crates/compute-cli/tests/product_journey.rs` |
| `health` | Process health (probe) and endpoint reachability (rollouts) | **PARTIAL** | yes | yes | The machine is confirmed with its target every 10 s; process liveness is probed every 15 s; endpoints are TCP-checked only during a rollout; no HTTP health checks for computer processes. | `crates/compute-environment/tests/computers.rs` |
| `metrics` | Metrics endpoint | **IMPLEMENTED** | API only | yes | Not surfaced in the UI or CLI. | API `GET /metrics` |
| `events` | Durable lifecycle events and a live stream | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/tests/control_plane.rs` |
| `receipts` | Verifiable execution receipts | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-core/src/receipt.rs`<br>`crates/compute-cli/tests/product.rs` |
<!-- /audit -->

## 18. Usability

- **First minute**: `compute` → UI in under 4 s, with the next actions on
  the first page. Good. The README and getting-started lead instead with
  `compute run script.py`, which **fails** by default (network policy).
- **Run a project**: inspection proposes a sensible assembly in 0.19 s for a
  Node project; the user reviews before GO. Local folders must be Git
  repositories.
- **Honest failures**: placement explains itself; GO conflicts offer a
  refresh; rollouts show the failed step. **Dishonest views**: a computer
  whose target is down or whose machine is gone shows "running".
- **Dead ends**: persistent storage / public endpoint checkboxes; the
  `containers` feature on a host with no engine.
- **Vocabulary**: environment, computer, machine, session, target, provider,
  project, application, service each mean more than one thing (§ What Needs
  to Be Reconciled).

## 19. UI / API / CLI parity

The full matrix is in [product-surface.md](product-surface.md#parity). Every
product-journey operation is available from the UI, CLI, API, and AppPort.
Setup and diagnosis are CLI-only (targets, credentials, audit, runtimes,
doctor, placement explanation, FeltDB provisioning); metrics are API-only.

<!-- audit:api_summary -->
| API | Count |
| --- | --- |
| routes | 129 |
| scope Admin | 8 |
| scope Deploy | 13 |
| scope Execute | 12 |
| scope Operate | 35 |
| scope Read | 61 |
| used by the UI | 72 |
| used by the CLI | 87 |
| used by AppPort | 52 |
| no client at all | 26 |
| path exercised over HTTP by a test | 37 |
<!-- /audit -->

## 20. Agent usability

<!-- audit:capabilities area=agents -->
| ID | Capability | Status | User can use | In complete model | Notes | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| `appport` | AppPort client for every UI operation | **IMPLEMENTED + VERIFIED** | yes | yes | Tested against a stub daemon, not a real one. | `packages/compute-appport/src/computers.ts`<br>`packages/compute-appport/src/test/computers.test.ts` |
| `agent-identity` | Agents act under operator credentials with scopes | **IMPLEMENTED** | yes | yes | No agent-specific identity or delegation; an agent is an operator. | `crates/compute-environment/src/auth.rs` |
<!-- /audit -->

An agent can do everything a person can through AppPort or the CLI with
`--json`, and every failure carries a machine-readable `kind`. It cannot
learn from the API whether a target is trustworthy, whether a machine
exists, or whether an advertised feature works. `audit.json` is written so an
agent can answer "can Compute do X here?" without reading prose.

## 21. Documentation

<!-- audit:documentation -->
| Document | Status | Notes |
| --- | --- | --- |
| `README.md` | **MISLEADING** | Leads with `compute run script.py`, which fails for Python by default; the launcher and control plane come later. |
| `docs/getting-started.md` | **MISLEADING** | Same first example; describes the workload engine, not the product. |
| `docs/architecture.md` | **OUTDATED → rewritten in this audit** | Did not show targets, sessions, the file default, or the daemon-host execution paths. |
| `docs/audit-2026-09-25.md` | **OUTDATED (historical; replaced by docs/audit.md)** | The 2026-09-25 audit predates computers, work sessions, versions. |
| `docs/platform-audit.md` | **OUTDATED** | Historical (dated). |
| `docs/hardening-audit.md` | **OUTDATED** | Historical (dated). |
| `docs/computers.md` | **DOCUMENTED CORRECTLY** | States `unreachable` and `lost`, the liveness check, and the one `reality` model. Was MISLEADING: "lost machines are reported" held only when a process probe noticed. |
| `docs/environment-control-plane.md` | **DOCUMENTED CORRECTLY** | Matches the verified journeys; its limitations list is accurate. |
| `docs/product-surface/README.md` | **DOCUMENTED CORRECTLY** | Screenshots from the passing journey. |
| `docs/sessions.md` | **INCOMPLETE** | Target sessions and their authority correct; does not explain work sessions. |
| `docs/session-architecture.md` | **DOCUMENTED CORRECTLY** | Describes the authority `compute serve` wires (TargetAuthorizer: owner = the control plane a credential names). Was INCOMPLETE: it described an authority `compute serve` wired as AllowAll. |
| `docs/providers.md` | **DOCUMENTED CORRECTLY** | Describes target feature detection as implemented (binary-on-PATH), which is itself the defect. |
| `docs/placement.md` | **DOCUMENTED CORRECTLY** | — |
| `docs/feltdb.md` | **DOCUMENTED CORRECTLY** | States the production decision (FeltDB) and the labelled local-development file backend; the working-state table includes computer confirmations. |
| `docs/daemon.md` | **OUTDATED** | "Environments are the first screen" — the UI opens on the action home with Work/Manage. |
| `docs/control-plane.md` | **DOCUMENTED CORRECTLY** | Spot-checked; notes computer environments. |
| `docs/environments.md` | **DOCUMENTED CORRECTLY** | Node environments; points to computers. |
| `docs/applications.md` | **INCOMPLETE** | Does not say `compute deploy <dir>` starts a control plane in ./.compute/daemon. |
| `docs/releases.md` | **DOCUMENTED CORRECTLY** | Bundle releases (spot-checked; tests pass). |
| `docs/networking.md` | **INCOMPLETE** | Ingress/domains apply to bundle projects only; not said. |
| `docs/jobs.md` | **DOCUMENTED CORRECTLY** | Spot-checked. |
| `docs/remote-execution.md` | **DOCUMENTED CORRECTLY** | Target credentials: issue, rotate, revoke, the trust file, and the one named open mode. |
| `docs/receipts.md` | **DOCUMENTED CORRECTLY** | Spot-checked. |
| `docs/policy.md` | **DOCUMENTED CORRECTLY** | Spot-checked. |
| `docs/admission.md` | **DOCUMENTED CORRECTLY** | Spot-checked. |
| `docs/isolation.md` | **DOCUMENTED CORRECTLY** | Workload isolation; says nothing about computers (which have none). |
| `docs/dependencies.md` | **DOCUMENTED CORRECTLY** | Spot-checked. |
| `docs/capacity.md` | **DOCUMENTED CORRECTLY** | Spot-checked. |
| `docs/provider-pools.md` | **DOCUMENTED CORRECTLY** | Spot-checked. |
<!-- /audit -->

## 22. Testing

<!-- audit:tests -->
| Kind | Tests |
| --- | --- |
| browser | 4 |
| rust-integration | 263 |
| rust-unit | 98 |
| typescript | 30 |
| total | 395 |
| ignored unless FELTDB_SERVER_BIN is set | 17 |

| CI workflow | Runs |
| --- | --- |
| `.github/workflows/test.yml` | cargo fmt --check, cargo test --workspace --locked, compute-cli product acceptance, AppPort contract (npm test) |
| `.github/workflows/feltdb-consumer.yml` | state backends conform, FeltDB backend against feltdb-server (ignored tests), controller against feltdb-server |
| `.github/workflows/distribution-certification.yml` | release build, distribution build/verify/certify |
| `.github/workflows/test.yml (browser)` | packages/compute-ui-e2e in Chromium, work_mode_ui and product_journey with COMPUTE_REQUIRE_BROWSER |

Not in CI:

- container provider against a real engine
<!-- /audit -->

What the tests prove well: the computer lifecycle, in-place change, fencing,
restart recovery, versions/rollouts, placement matching, the FeltDB access
contract, runtime conformance, daemon security, target authentication (in
process, through the `compute` binary, and through the launcher), and
target reality (unreachable, recovered, lost, replaced, and a stale answer
held back by a proxy). What no test proves: the container provider against
a real engine, AppPort against a real daemon, and 87 CLI commands through
the CLI.

The browser certification package `packages/compute-ui-e2e` failed at the
original audit: it waited for `[data-environment="preprod"]` on `#/`, which
became the action home in `69b70d9`, and it was not in CI. It is fixed, has
a second journey (a computer created from the UI that becomes unreachable,
recovers, is lost, and is replaced, launched with `compute up`), and runs in
CI with Chromium beside the Rust browser tests (`test.yml`, job `browser`),
where a missing browser fails instead of skipping.

Per-file counts: [audit.json](audit.json) (`tests.files`).

## 23. Failure and recovery

Run by `audit-evidence/2026-09-27/experiments.py` against a fresh `compute`
launch (raw results in `experiments.json`), then re-run for the foundation by
`foundation.py` (`experiments.json#foundation`). Where they differ, the
table says what was, and what is.

| Failure | What happened | Verdict |
| --- | --- | --- |
| Control-plane restart | Served again in 2.46 s; computer still running, no failure | recovers |
| Target process killed | Was: view `running`, failure `null`. Now: `unreachable` (`target_unreachable`) in 10 s, desired still `running`; exec 503 `runtime_unavailable` naming it | reported |
| Target restarted | Computer converged again on the **same session**; `computer.recovered` | recovers |
| Target lost the machine (session store wiped) | Was: `running` for 90 s, through reconcile and a restart. Now: `lost` (`session_missing`) 1.9 s after the target answered; still `lost` after reconcile and a control-plane restart; replacement brings a new machine in 0.5 s | reported, then replaced |
| A stale answer arrives after the loss is recorded | The computer stays `lost` (fenced write; `a_stale_answer_from_a_target_cannot_revive_a_lost_computer`) | fails safely |
| Anonymous, wrong, revoked, or another control plane's credential at a target | 401, 401, 401; the other control plane sees no sessions and gets `unknown_session` | refused |
| Unsatisfiable placement | Refused with a reason per target | fails safely |
| Stale GO | Refused (generation conflict); UI offers refresh | fails safely |
| Control state unreachable | Changes refused (`state_unavailable`), workloads keep running (`availability.rs`) | fails safely |
| Unknown bearer token on the daemon | 401 | fails safely |

<!-- audit:experiments -->
| Experiment | Observed |
| --- | --- |
| `launch_seconds` | 3.68 |
| `relaunch_seconds` | 0.01 |
| `ui_html_ms` | 4.8 |
| `status_ms` | 1.5 |
| `create_http` | [201, 267.2] |
| `computer_running_seconds` | 0.61 |
| `exec_submit_ms` | 16.2 |
| `exec_roundtrip_seconds` | 0.13 |
| `target_sessions_listed_without_credential` | {"http": 200, "sessions": ["ses_d72405242004ffe0b93c2e4d6fbaae2aef52e0a9492a121f49c88a889cd5a0e3"]} |
| `target_exec_without_credential` | {"http": 200, "body": "accepted"} |
| `bypass_file_seen_through_the_daemon` | "written-by-an-unauthenticated-caller" |
| `daemon_loopback_any_bearer_token_http` | 401 |
| `propose` | {"http": 200, "seconds": 0.19, "runtime": "node", "start": [["sh", "-c", "npm start"]]} |
| `exec_while_target_down` | {"http": 503, "body": {"kind": "runtime_unavailable", "message": "TransportFailure: Connection refused (os error 111)", "request_id": "req_cebf94cb5b2f0df7"}} |
| `computer_while_target_down` | {"status": "running", "failure": null} |
| `recovery_after_target_restart` | {"seconds": 0.0, "same_session": true} |
| `after_the_target_forgot_the_machine` | {"observed": "the computer stayed \"running\" with no failure for 90 s, after an explicit reconcile, and after a control-plane restart; exec answered not_found \"unknown session\"", "detected": false} |
| `after_control_plane_restart` | {"seconds": 2.46, "state_kept": true, "status": "running (still unaware the machine is gone)"} |
| `ui` | {"home_ready_ms": 83, "home_api_calls": 4, "work_ready_ms": 68, "work_api_calls": 5, "computers": 3} |
| `placement_refusals` | {"persistent_storage": "invalid: no target can host this computer: local: sessions_unsupported; this-machine: session_capability_unsupported", "public_endpoint": "same, session_capability_unsupported", "terminal": "same, session_capability_unsupported", "gpu": "same, target_feature_unsupported", "containers": "accepted and placed as a workspace computer (provider_kind workspace): the host advertises containers because a docker binary is on PATH, with no engine running"} |
| `runtimes` | {"python_default_network": "placement_failed: network_unsupported (python cannot enforce network none); `compute run main.py` fails as documented in README/getting-started", "python_with_network": "downloaded the pinned distribution (available \u2192 ready) and ran: \"hello from python\""} |
| `application_journey` | {"compute init my-app; compute deploy my-app": "running at http://127.0.0.1:20002 on provider local; the deploy auto-started a second control plane (compute start, state in ./.compute/daemon, port 8787) and a supervisor: the application runs on that daemon node"} |
| `ui_certification_package` | {"packages/compute-ui-e2e": "FAIL: waits for [data-environment=\"preprod\"] on #/ \u2014 the home route became the action home in 69b70d9; not run in CI; it also left two supervisor processes running"} |
| `foundation` | {"serve_without_credentials": {"exit": 1, "stderr": "Compute provider listening on 127.0.0.1:0 (http://127.0.0.1:0)\ninvalid workload: target credentials /tmp/compute-foundation-64zzhr3w/none.json: No such file or directory (os error 2): a target needs the credentials of the control planes it trusts; issue one with `compute target credential issue --credentials /tmp/compute-foundation-64zzhr3w/none.json --control-plane <name> --token-file <file>` (or run with --insecure-unauthenticated for local development)"}, "launch_output": ["Compute is running: http://127.0.0.1:18797/", "  computers run on this machine's computer host (127.0.0.1:18798)", "  this control plane (cp-f3d247ab08c80f27) authenticates to it with a target credential", "  control state: file on this machine (local development; production control planes use FeltDB: [state] backend = \"feltdb\")", "  state: /tmp/compute-foundation-64zzhr3w/home", "  stop it with `compute down`"], "launcher_credential": {"pool_names_token_file": true, "pool_holds_token": false, "trust_file_holds_secret": false, "trusted_control_planes": ["cp-f3d247ab08c80f27"], "token_file_mode": "0o600"}, "targets": [{"target_id": "local", "health": "healthy", "hosts_computers": false, "authentication": null, "credential": false}, {"target_id": "this-machine", "health": "healthy", "hosts_computers": true, "authentication": "credential", "credential": true}], "control_state": {"kind": "file", "durability": "local-development"}, "computer_running": {"seconds": 0.41, "status": "running", "reality": {"confirmed_at": "2026-09-27T23:46:58.265495795Z", "desired": "running", "explanation": "Running on this-machine, confirmed by the target.", "observed": "running"}, "failure": null}, "target_without_credential": {"list_http": 401, "kind": "unauthorized"}, "target_exec_without_credential": {"http": 401, "kind": "unauthorized"}, "target_wrong_credential": {"http": 401, "kind": "unauthorized"}, "target_other_control_plane": {"list_http": 200, "sessions_seen": 0, "inspect": [404, "unknown_session"], "exec": [404, "unknown_session"]}, "target_revoked_credential": {"http": 401, "message": "target credential tcred_5207d8a0539c2092 was revoked"}, "target_own_credential": {"http": 200, "owner": "control-plane:cp-f3d247ab08c80f27"}, "target_down": {"seconds_to_unreachable": 9.97, "status": "unreachable", "reality": {"desired": "running", "explanation": "this-machine is not answering for this computer: TransportFailure: Connection refused (os error 111) (target_unreachable). The environment still wants it; Compute keeps checking and it returns to running when this-machine answers with the same machine.", "observed": "unreachable", "since": "2026-09-27T23:47:08.267586062Z"}, "failure": {"code": "target_unreachable", "retryable": true}, "exec": {"http": 503, "kind": "runtime_unavailable"}}, "target_recovered": {"seconds": 1.94, "same_session": true, "status": "running", "reality": {"confirmed_at": "2026-09-27T23:47:10.278508922Z", "desired": "running", "explanation": "Running on this-machine, confirmed by the target.", "observed": "running"}, "failure": null}, "machine_lost": {"seconds_after_target_back": 1.93, "status": "lost", "reality": {"desired": "running", "explanation": "this-machine no longer has this computer's machine: the target no longer has the session: unknown session (session_missing). The environment still wants it; replace the computer to provision a new machine with the same contents, or destroy it.", "observed": "lost", "since": "2026-09-27T23:47:22.286430325Z"}, "failure": {"code": "session_missing", "retryable": false}}, "lost_after_reconcile": {"status": "lost", "reality": {"desired": "running", "explanation": "this-machine no longer has this computer's machine: the target no longer has the session: unknown session (session_missing). The environment still wants it; replace the computer to provision a new machine with the same contents, or destroy it.", "observed": "lost", "since": "2026-09-27T23:47:22.286430325Z"}, "failure": {"code": "session_missing", "retryable": false}}, "lost_after_control_plane_restart": {"status": "lost", "reality": {"desired": "running", "explanation": "this-machine no longer has this computer's machine: the target no longer has the session: unknown session (session_missing). The environment still wants it; replace the computer to provision a new machine with the same contents, or destroy it.", "observed": "lost", "since": "2026-09-27T23:47:22.286430325Z"}, "failure": {"code": "session_missing", "retryable": false}}, "replaced": {"seconds": 0.51, "new_session": true, "status": "running", "reality": {"confirmed_at": "2026-09-27T23:47:29.094111771Z", "desired": "running", "explanation": "Running on this-machine, confirmed by the target.", "observed": "running"}, "failure": null}, "events": ["computer.requested", "computer.placed", "computer.provisioned", "computer.running", "computer.unreachable", "computer.recovered", "computer.unreachable", "computer.lost", "computer.replacing", "computer.replacing", "computer.placed", "computer.provisioned", "computer.running"]} |
<!-- /audit -->

## 24. Security

<!-- audit:security -->
| ID | Severity | Status | Finding | Evidence |
| --- | --- | --- | --- | --- |
| SEC-1 | **critical** | resolved | Was: `compute serve` had no authentication (AllowAllAuthorizer); anyone who reached a target listed every session and ran commands in any computer. Now: every request needs a credential the target issued; anonymous, wrong, and revoked credentials get 401, another control plane's valid credential sees no sessions and gets unknown_session for this one's. AllowAllAuthorizer no longer exists. | experiments.json#foundation (target_without_credential, target_exec_without_credential, target_wrong_credential, target_other_control_plane, target_revoked_credential); before: experiments.json#target_exec_without_credential |
| SEC-2 | **high** | resolved | Was: the daemon presented no credential to its targets, so every computer was owned by "anonymous". Now: the launcher issues the host a credential for this control plane's persistent identity and the pool presents it (token_file); sessions belong to `control-plane:<id>` across restarts and credential rotation. | experiments.json#foundation (launcher_credential, target_own_credential); crates/compute-cli/tests/launcher.rs |
| SEC-3 | **high** | open | Workspace computers are directories under one OS user on one host: no filesystem, process, or network isolation between computers or from the target. | crates/compute-provider/src/sessions.rs#WorkspaceSessionProvider |
| SEC-4 | **medium** | open | A loopback daemon without TLS admits requests with no credential (development mode). The launcher runs this way. | crates/compute-environment/src/auth.rs; docs/daemon.md |
| SEC-5 | **medium** | open | Configuration values are passed as environment variables into every process and job in the computer; there is no secret type distinct from configuration. | crates/compute-environment/src/daemon/computers.rs#exec_in |
| SEC-6 | **low** | open | Any operator with read scope can read any computer view (desired contents, configuration keys, endpoints); only mutations are owner-bound. | crates/compute-environment/src/daemon/computers.rs#computer |
<!-- /audit -->

## 25. Performance

Debug build on a 4-CPU VM; indicative, not a benchmark.

<!-- audit:performance -->
| Measurement | Value |
| --- | --- |
| launch cold seconds | 3.68 |
| launch warm seconds | 0.01 |
| control plane restart seconds | 2.46 |
| computer create to running seconds | 0.61 |
| exec submit ms | 16.2 |
| exec roundtrip seconds | 0.13 |
| project inspection seconds | 0.19 |
| ui home ready ms | 83 |
| ui work ready ms | 68 |
| full product journey seconds | 57 |

- Every lifecycle event re-renders the whole current page (a full refetch).
- GET /software computes every computer view and queries rollouts per environment and the latest version per project on each call.
- Computer drivers re-read their records by identity every step; rollout drivers poll every 250 ms.
- Debug build on a 4-CPU VM; not a benchmark.
<!-- /audit -->

## 26. What we have

Everything that is implemented and proven by a test or an experiment:

<!-- audit:capabilities status=IMPLEMENTED_+_VERIFIED -->
| ID | Capability | Status | User can use | In complete model | Notes | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| `launch` | `compute` launches the control plane and a local computer host | **IMPLEMENTED + VERIFIED** | yes | yes | Starts `compute serve` (127.0.0.1:8788) trusting only this control plane (a generated target credential, a persistent control-plane identity) and `compute start` (127.0.0.1:8787) with a generated pool that names the token file; says which control state it uses. 3.7 s cold, 0.01 s when running. Opens a browser with xdg-open/open. | `crates/compute-cli/src/launch_cmd.rs`<br>`crates/compute-cli/tests/product_journey.rs`<br>`crates/compute-cli/tests/launcher.rs`<br>`packages/compute-ui-e2e/src/computer-reality.test.mjs`<br>journey `first-launch` |
| `ui-modes` | Work / Manage modes of one control plane | **IMPLEMENTED + VERIFIED** | yes | yes | Run in CI with Chromium (test.yml `browser`, COMPUTE_REQUIRE_BROWSER: a missing browser fails); skip elsewhere without Playwright/Chromium. | `crates/compute-environment/ui/app.js`<br>`crates/compute-environment/ui/index.html`<br>`crates/compute-cli/tests/work_mode_ui.rs`<br>`crates/compute-cli/tests/product_journey.rs` |
| `ui-home` | Action-first home ("What do you want to do?") | **IMPLEMENTED + VERIFIED** | yes | yes | Shows software and computers; environments without a computer are not on the home page. | `crates/compute-environment/ui/app.js#homeView`<br>`crates/compute-cli/tests/product_journey.rs` |
| `ui-certification-package` | packages/compute-ui-e2e browser certification | **IMPLEMENTED + VERIFIED** | n/a | yes | Fixed for the action home (`#/` → `#/environments`); runs in CI with Chromium: the operator journey, and a computer that is created, runs, becomes unreachable, recovers, is lost, and is replaced, launched with `compute up`. | `packages/compute-ui-e2e/src/control-plane.test.mjs`<br>`packages/compute-ui-e2e/src/computer-reality.test.mjs`<br>`.github/workflows/test.yml`<br>journey `experiments.json#ui_certification_package` |
| `computer-environments` | Environments backed by a durable computer | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/src/daemon/computers.rs`<br>`crates/compute-core/src/computers.rs`<br>`crates/compute-environment/tests/computers.rs` |
| `persistent` | Persistent computers (no TTL, claimed) | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/tests/computers.rs` |
| `ephemeral` | Ephemeral computers that expire and keep evidence | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/tests/computers.rs#an_ephemeral_environment_expires_and_keeps_its_evidence`<br>`crates/compute-cli/tests/product_journey.rs` |
| `lifetime-change` | Change lifetime in place (claim on the target) | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/tests/computers.rs#go_changes_lifetime_and_configuration_in_place_and_refuses_stale_views` |
| `in-place-change` | Contents changed in place, no redeployment | **IMPLEMENTED + VERIFIED** | yes | yes | Same provider resource across releases, configuration changes, and controller restarts. | `crates/compute-environment/tests/computers.rs#deployment_is_reconciliation_of_the_same_computer`<br>`crates/compute-cli/tests/product_journey.rs` |
| `replacement` | Explicit replacement provisions a new machine and retires the old | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/tests/computers.rs#provider_failures_and_replacements_are_explicit`<br>`crates/compute-environment/tests/computers.rs#deployment_is_reconciliation_of_the_same_computer` |
| `stop-resume` | Stop and resume the same machine | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/tests/computers.rs`<br>`crates/compute-cli/tests/product_journey.rs` |
| `orphan-sweep` | Sessions no computer record claims are torn down | **IMPLEMENTED + VERIFIED** | no (automatic) | yes | — | `crates/compute-environment/tests/computers.rs#provisioning_survives_a_controller_restart_and_orphans_are_torn_down` |
| `machine-loss` | A machine or session the target lost makes the computer `lost` | **IMPLEMENTED + VERIFIED** | yes | yes | Every running computer is confirmed with its target (liveness every 10 s, whatever runs in it). A target that answers without the session, or whose provider no longer has the machine, makes it lost: desired state kept, never re-provisioned on its own, stays lost through reconcile and a control-plane restart until it is replaced or destroyed. | `crates/compute-environment/src/daemon/computers.rs#observe_machine,apply_observation,lost_step`<br>`crates/compute-provider/src/sessions.rs#environment_lost`<br>`crates/compute-environment/tests/computers.rs#a_machine_or_session_that_disappears_is_lost_until_replaced`<br>`crates/compute-cli/tests/computers.rs#the_cli_reports_observed_reality_not_desired_state`<br>`packages/compute-ui-e2e/src/computer-reality.test.mjs`<br>journey `experiments.json#foundation` |
| `target-down-visibility` | A computer whose target is unreachable says so | **IMPLEMENTED + VERIFIED** | yes | yes | `unreachable` (target_unreachable, or credential_rejected when the target refuses this control plane) within one liveness interval; exec answers runtime_unavailable naming it; the same machine returns to running when the target answers. | `crates/compute-environment/src/daemon/computers.rs#unreachable_step`<br>`crates/compute-environment/tests/computers.rs#an_unreachable_target_keeps_desired_state_and_recovers_the_same_machine`<br>`crates/compute-cli/tests/computers.rs`<br>`packages/compute-ui-e2e/src/computer-reality.test.mjs`<br>journey `experiments.json#foundation` |
| `stale-fencing` | A stale target answer cannot revive a lost computer | **IMPLEMENTED + VERIFIED** | no (automatic) | yes | Every observation is applied only to the record version it was made against (the generation-fenced write); lost is sticky: only an operator reconcile with a fresh answer can find the machine again. | `crates/compute-environment/src/daemon/computers.rs#apply_observation`<br>`crates/compute-environment/tests/computers.rs#a_stale_answer_from_a_target_cannot_revive_a_lost_computer` |
| `reality-surfaces` | Desired and observed state, told apart, on every surface | **IMPLEMENTED + VERIFIED** | yes | yes | One model (`reality`: desired, observed, confirmed_at, since, explanation) in the API, `compute environment status`, the UI, and AppPort. An environment on a computer is never `running` because it is meant to be: it is what its computer was last observed to be. | `crates/compute-environment/src/status.rs#ComputerReality`<br>`crates/compute-environment/ui/app.js#realityPanel`<br>`crates/compute-cli/src/computer_cmd.rs#print_computer`<br>`packages/compute-appport/src/computers.ts#ComputerReality`<br>`crates/compute-cli/tests/computers.rs#the_cli_reports_observed_reality_not_desired_state`<br>`packages/compute-ui-e2e/src/computer-reality.test.mjs`<br>`crates/compute-environment/tests/computers.rs` |
| `contents` | Repositories, packages, processes, projects, configuration as desired state | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/tests/computers.rs`<br>`crates/compute-cli/tests/product_journey.rs` |
| `endpoints` | Process ports published as endpoints (target host:port) | **IMPLEMENTED + VERIFIED** | yes | yes | No port publishing for the container provider; no ingress/TLS/domains for computer endpoints. | `crates/compute-environment/tests/computers.rs`<br>`crates/compute-cli/tests/product_journey.rs` |
| `go-fencing` | GO: one generation-fenced change; stale views refused | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/tests/computers.rs`<br>`crates/compute-cli/tests/work_mode_ui.rs` |
| `work-sessions` | Work sessions (attached / ephemeral) in FeltDB | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/tests/computers.rs#work_sessions_enter_environments_and_temporary_ones_end_with_them` |
| `target-inventory` | Targets listed with health, platform, resources, capabilities, features | **IMPLEMENTED + VERIFIED** | CLI/API only | yes | Includes how each target authenticates (`credential`, or `insecure-unauthenticated`) and whether this control plane presents a credential. Not shown in the UI. | `crates/compute-cli/tests/computers.rs`<br>`crates/compute-cli/tests/launcher.rs`<br>CLI `compute target list`<br>API `GET /targets` |
| `discovery-resources` | CPU count, memory, disk, OS/architecture discovered | **IMPLEMENTED + VERIFIED** | indirect | yes | — | journey `GET /compute/capabilities on this host: 4 CPU, 16.9 GB, 270 GB, linux-x86_64` |
| `discovery-runtimes` | Language runtimes discovered (installed/available/ready) | **IMPLEMENTED + VERIFIED** | CLI | yes | — | CLI `compute runtimes`<br>CLI `compute doctor` |
| `discovery-isolation` | Isolation facilities discovered (landlock ABI, network namespaces, cgroups) | **IMPLEMENTED + VERIFIED** | CLI | yes | — | CLI `compute isolation` |
| `placement` | Capability-matched placement with reasons | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-placement/src/matching.rs`<br>`crates/compute-placement/tests/matching.rs`<br>`crates/compute-placement/tests/selection.rs` |
| `placement-override` | Constrain placement to a target | **IMPLEMENTED + VERIFIED** | yes (dialog field) | yes | — | `crates/compute-environment/tests/computers.rs#placement_chooses_a_target_by_what_the_computer_needs` |
| `runtime-wasm` | WASM workloads (wasmtime, WASI p1) | **IMPLEMENTED + VERIFIED** | CLI (compute run) | yes | Workload engine only: a computer cannot be a WASM sandbox. | `crates/compute-runtime-wasm/src/lib.rs`<br>`crates/compute-runtime-wasm/tests/conformance.rs`<br>`crates/compute-runtime/tests/conformance.rs` |
| `runtime-process` | Process runtimes: node, bun, deno, python, ruby, php, jvm, dotnet, native, shell | **IMPLEMENTED + VERIFIED** | CLI (compute run), daemon workloads | yes | Pinned distributions download on demand (python verified here). `compute run script.py` fails by default: network "none" is unenforceable for process runtimes. | `crates/compute-runtime-process/src/lib.rs`<br>`crates/compute-runtime/tests/conformance.rs`<br>journey `experiments.json#runtimes` |
| `substrate-workspace` | Computers as private workspaces on the target host (native processes) | **IMPLEMENTED + VERIFIED** | yes | yes | A directory with a shell: not an isolation boundary. | `crates/compute-provider/src/sessions.rs#WorkspaceSessionProvider`<br>`crates/compute-provider/tests/sessions.rs`<br>`crates/compute-environment/tests/computers.rs`<br>`crates/compute-cli/tests/product_journey.rs` |
| `provider-local` | Local provider (in-process engine) | **IMPLEMENTED + VERIFIED** | yes | yes | Runs workloads; does not host computers (sessions_unsupported). | `crates/compute-provider/src/lib.rs#LocalProvider`<br>`crates/compute-cli/tests/cli.rs` |
| `provider-remote` | Remote provider (`compute serve`, compute.remote@1) | **IMPLEMENTED + VERIFIED** | yes | yes | The only kind of target that hosts computers. | `crates/compute-provider/src/lib.rs#RemoteProvider`<br>`crates/compute-provider/tests/remote.rs`<br>`crates/compute-cli/tests/remote_pool.rs` |
| `provider-daemon-node` | A daemon node as a provider (deployments, /compute/*) | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-cli/tests/product.rs` |
| `provider-dns` | DNS providers (for domains) | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-network/src/dns.rs`<br>`crates/compute-network/tests/providers.rs`<br>`crates/compute-environment/tests/network.rs` |
| `run-a-project` | Inspect a source in the computer and propose an assembly | **IMPLEMENTED + VERIFIED** | yes | yes | Recognises package.json, Python, Go, Rust, Makefile, Procfile, .env.example, compose/postgres/redis hints. A local folder must be a Git repository. | `crates/compute-environment/src/daemon/software.rs#propose`<br>`crates/compute-environment/src/daemon/software.rs`<br>`crates/compute-cli/tests/product_journey.rs` |
| `multi-project` | Several projects on one computer | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-cli/tests/product_journey.rs` |
| `build-test` | Build, test, checks, named commands in the computer | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/tests/computers.rs`<br>`crates/compute-cli/tests/product_journey.rs` |
| `publish` | Publish an immutable version (source, build, tests, checks, package digest) | **IMPLEMENTED + VERIFIED** | yes | yes | The version is a commit + digest + assembly; no artifact is stored (the package digest is computed, not kept). | `crates/compute-environment/src/daemon/software.rs`<br>`crates/compute-environment/tests/computers.rs#versions_are_published_deployed_promoted_and_rolled_back_in_place`<br>`crates/compute-cli/tests/product_journey.rs` |
| `deploy` | Deploy a version to an environment (rollout with steps) | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/tests/computers.rs`<br>`crates/compute-cli/tests/product_journey.rs` |
| `promote` | Promote test → production with a reviewed plan | **IMPLEMENTED + VERIFIED** | yes | yes | No approval workflow (approvals list is always empty). | `crates/compute-environment/tests/computers.rs`<br>`crates/compute-cli/tests/product_journey.rs` |
| `rollback` | Roll back to an earlier version | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/tests/computers.rs`<br>`crates/compute-cli/tests/product_journey.rs` |
| `bundle-projects` | Bundle projects, revisions, zero-downtime releases on the daemon node | **IMPLEMENTED + VERIFIED** | yes | reconcile | Executes on the daemon host (supervisor). Refused for environments with a computer. | `crates/compute-environment/src/daemon/release.rs`<br>`crates/compute-environment/src/daemon/deploy.rs`<br>`crates/compute-environment/tests/releases.rs` |
| `applications` | `compute init/deploy` applications: a compatibility view over a computer, a version, and a rollout | **IMPLEMENTED + VERIFIED** | yes | reconcile | Each application is its own computer environment on a target (never the daemon host): source imported by target jobs, published as a version, deployed as a rollout; runtimes that need the pinned catalog (wasm, jvm, dotnet) are refused. | `crates/compute-environment/src/daemon/applications.rs`<br>`crates/compute-environment/tests/applications.rs`<br>`crates/compute-cli/tests/product.rs`<br>journey `experiments.json#application_journey` |
| `domains-tls` | Domains, DNS, ACME certificates, ingress | **IMPLEMENTED + VERIFIED** | yes (bundle projects) | yes | Routes to bundle-project services only; not to computer endpoints. | `crates/compute-environment/tests/network.rs`<br>`crates/compute-network/tests/ingress.rs` |
| `logs` | Process logs and job output | **IMPLEMENTED + VERIFIED** | yes | yes | Read on demand (tail); no streaming for computer processes. | `crates/compute-environment/tests/computers.rs`<br>`crates/compute-cli/tests/product_journey.rs` |
| `restart` | Restart a process in place | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/src/daemon/software.rs`<br>`crates/compute-cli/tests/product_journey.rs` |
| `events` | Durable lifecycle events and a live stream | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/tests/control_plane.rs` |
| `receipts` | Verifiable execution receipts | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-core/src/receipt.rs`<br>`crates/compute-cli/tests/product.rs` |
| `state-feltdb` | FeltDB as the durable authority of a production control plane (model generation 8) | **IMPLEMENTED + VERIFIED** | configuration | yes | The production decision (docs/feltdb.md): `[state] backend = "feltdb"` (or `--state feltdb`); `compute` and `compute start` pass it through. /info and `compute status` report `durability: production`. | `crates/compute-state-feltdb/tests/consumer.rs`<br>`crates/compute-environment/tests/feltdb_consumer.rs` |
| `state-default-file` | Without configuration, control state is a local file, stated as local development | **IMPLEMENTED + VERIFIED** | yes | yes | The file backend is kept for local development behind the same StateStore abstraction and labelled everywhere: `compute` prints it, /info, `compute status`, and `compute node info` report `durability: local-development`. The launcher uses whatever `[state]` says; it never picks a different model silently. | `crates/compute-cli/src/control_state.rs#backend_name`<br>`crates/compute-state/src/store.rs#durability`<br>`crates/compute-cli/src/launch_cmd.rs#durability_note`<br>`crates/compute-cli/tests/launcher.rs`<br>journey `experiments.json#foundation` |
| `restart-recovery` | Control-plane restart keeps and resumes everything | **IMPLEMENTED + VERIFIED** | yes | yes | — | `crates/compute-environment/tests/computers.rs`<br>`crates/compute-cli/tests/recovery.rs`<br>journey `experiments.json#after_control_plane_restart` |
| `daemon-auth` | Daemon: operator credentials, scopes, audit; owner checks for computers | **IMPLEMENTED + VERIFIED** | yes | yes | Loopback without TLS admits requests with no credential (development mode); unknown bearer tokens get 401. | `crates/compute-environment/src/auth.rs`<br>`crates/compute-environment/tests/security.rs`<br>`crates/compute-environment/tests/computers.rs` |
| `target-auth` | Target (`compute serve`) authentication | **IMPLEMENTED + VERIFIED** | yes | yes | A target trusts only the control planes it issued a credential to (`compute target credential issue`; verifiers only, re-read on change, revocable). What a control plane creates belongs to its identity, not its token: rotation keeps it, another control plane sees nothing. `compute serve` refuses to start without a trust file; the only open mode is the named `--insecure-unauthenticated`, which the target advertises. AllowAllAuthorizer is gone; an endpoint without an authority fails closed. | `crates/compute-provider/src/credentials.rs#TargetAuthorizer,NoCredentialsConfigured,InsecureUnauthenticated`<br>`crates/compute-environment/src/auth.rs#DaemonAuthorized`<br>`crates/compute-placement/src/pool.rs#token_file`<br>`crates/compute-provider/tests/sessions.rs#a_target_is_controlled_only_by_the_control_planes_it_trusts`<br>`crates/compute-cli/tests/sessions.rs#a_target_is_controlled_only_by_the_control_planes_it_trusts`<br>`crates/compute-cli/tests/launcher.rs`<br>`crates/compute-provider/src/credentials.rs`<br>journey `experiments.json#foundation` |
| `appport` | AppPort client for every UI operation | **IMPLEMENTED + VERIFIED** | yes | yes | Tested against a stub daemon, not a real one. | `packages/compute-appport/src/computers.ts`<br>`packages/compute-appport/src/test/computers.test.ts` |
<!-- /audit -->

Implemented, without proof here:

<!-- audit:capabilities status=IMPLEMENTED -->
| ID | Capability | Status | User can use | In complete model | Notes | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| `substrate-container` | Computers as containers (docker/podman) | **IMPLEMENTED** | CLI flag only | yes | Tested against a fake docker script only; never run against a real engine in this audit (none available) or in CI. `compute up --containers` / `compute serve --session-provider container`. | `crates/compute-provider/src/containers.rs`<br>`crates/compute-provider/tests/sessions.rs#the_container_adapter_translates_the_session_contract` |
| `metrics` | Metrics endpoint | **IMPLEMENTED** | API only | yes | Not surfaced in the UI or CLI. | API `GET /metrics` |
| `agent-identity` | Agents act under operator credentials with scopes | **IMPLEMENTED** | yes | yes | No agent-specific identity or delegation; an agent is an operator. | `crates/compute-environment/src/auth.rs` |
<!-- /audit -->

## 27. What's missing

Capabilities that are partial, broken, or missing:

<!-- audit:capabilities status=PARTIAL,BROKEN,MISSING,STUB,UNKNOWN,DOCUMENTED_ONLY,UNUSED -->
| ID | Capability | Status | User can use | In complete model | Notes | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| `files` | Files in the computer | **PARTIAL** | partial | yes | List and read (head -c 64 KiB) through exec jobs; no upload, edit, or download. | `crates/compute-environment/ui/app.js#listFiles`<br>`crates/compute-cli/tests/product_journey.rs` |
| `terminal` | Terminal | **PARTIAL** | partial | yes | Each line is a durable job; no interactive PTY. The `terminal` session capability is offered by no provider. | `crates/compute-cli/tests/product_journey.rs` |
| `discovery-features` | Target features: kvm, virtualization, firecracker, containers, gpu | **PARTIAL** | indirect | yes | `containers` is inferred from a docker/podman binary on PATH: advertised on this host with no engine running. gpu = /dev/nvidia0 exists. kvm = /dev/kvm openable. No nested-virtualization, GPU model, or engine liveness check. | `crates/compute-provider/src/lib.rs#detect_target_features`<br>journey `experiments.json#placement_refusals` |
| `discovery-network` | Network interfaces, reachability, exposed ports | **MISSING** | no | yes | Not discovered. Endpoint hosts come from the pool endpoint URL. | — |
| `discovery-automatic-targets` | Targets discovered automatically (no configuration) | **MISSING** | no | yes | Targets come from a pool file. The launcher writes one naming the local host; no LAN/cloud discovery. | — |
| `placement-dead-options` | UI offers persistent storage / public endpoint | **BROKEN** | no | yes | No provider offers either; any computer requesting them is refused (with reasons). | `crates/compute-provider/src/sessions.rs#capabilities`<br>`crates/compute-provider/src/containers.rs#capabilities`<br>journey `experiments.json#placement_refusals` |
| `substrate-firecracker` | Firecracker microVM computers | **MISSING** | no | yes | Only a feature label for placement. | — |
| `substrate-kvm` | KVM virtual machine computers | **MISSING** | no | yes | Only a feature label for placement. | — |
| `substrate-wasm` | WASM computers | **MISSING** | no | yes | — | — |
| `provider-fly` | Fly Machines | **MISSING** | no | yes | — | — |
| `provider-railway` | Railway | **MISSING** | no | yes | — | — |
| `provider-render` | Render | **MISSING** | no | yes | — | — |
| `provider-cloud-vm` | Cloud VMs / bare metal provisioning | **MISSING** | no | yes | — | — |
| `zero-downtime` | Zero-downtime release inside a computer | **MISSING** | no | yes | A release restarts processes. Zero-downtime traffic switching exists only for bundle projects on the daemon node. | — |
| `approvals` | Promotion approvals | **MISSING** | no | yes | — | `crates/compute-environment/src/status.rs#PromotionPlan` |
| `artifact-store` | Stored build artifacts for versions | **MISSING** | no | yes | Bundle revisions store artifacts; computer versions store only the commit and digest. | — |
| `health` | Process health (probe) and endpoint reachability (rollouts) | **PARTIAL** | yes | yes | The machine is confirmed with its target every 10 s; process liveness is probed every 15 s; endpoints are TCP-checked only during a rollout; no HTTP health checks for computer processes. | `crates/compute-environment/tests/computers.rs` |
| `isolation-computers` | Isolation between computers on one host | **MISSING** | n/a | yes | Workspace computers are directories under one user; processes share the host network and filesystem permissions. | `crates/compute-provider/src/sessions.rs` |
<!-- /audit -->

Grouped by area, with current state, desired state, impact, evidence, and
next step:

<!-- audit:gaps -->
### Core architecture

**G-ARCH-1** (closed)

- Was: Targets accept any caller (AllowAllAuthorizer); the daemon authenticates to targets with nothing.
- Desired: Targets trust only their control plane (a credential or mTLS), and sessions belong to the daemon's identity.
- Impact: Anyone who reaches a target controls every computer on it; the daemon is not actually the authority.
- Evidence: SEC-1, SEC-2
- Next: Closed: targets authenticate every request with a credential they issued; the daemon presents one; sessions belong to the control plane's identity (experiments.json#foundation, SEC-1, SEC-2).

**G-ARCH-2** (closed)

- Was: Three deployment models: applications, bundle projects (node environments), computer versions/rollouts.
- Desired: One: versions reconciled into an environment's computer.
- Impact: Three vocabularies, three code paths, and work that still runs on the daemon host.
- Evidence: models, execution_paths
- Next: Closed for applications: `compute deploy`/`compute application` resolve to a computer environment, a version, and a rollout; source is imported by target jobs; the endpoint, logs, and receipt are the computer's (crates/compute-environment/tests/applications.rs). Node environments remain: G-ARCH-5.

**G-ARCH-5**

- Current: Node environments (bundle projects, releases, ingress) still run on the daemon host through the supervisor.
- Desired: Their features (zero-downtime switch, ingress, domains) ported to computers, then retired.
- Impact: A second deployment model remains for bundle projects (not for applications).
- Evidence: execution_paths
- Next: Port zero-downtime switching and ingress to computers (G-DEP-1, G-APP-1), then retire node environments.

**G-ARCH-3** (closed)

- Was: Default control state is a local file; FeltDB is opt-in.
- Desired: FeltDB as the one authority, or the file backend stated as a development convenience everywhere.
- Impact: Contradicts the durability contract; the launcher never uses FeltDB.
- Evidence: state-default-file
- Next: Closed: FeltDB is the production authority; the file backend remains for local development and says so everywhere (launcher output, /info, `compute status`, `compute node info`: durability local-development).

**G-ARCH-4** (closed)

- Was: Machine loss and target unreachability are not detected for computers without processes; the view keeps "running".
- Desired: Every computer is periodically confirmed with its target; unreachable/lost is visible and actionable.
- Impact: The UI shows healthy computers that do not exist.
- Evidence: experiments.json
- Next: Closed: every running computer is confirmed with its target; unreachable and lost are durable, evented, fenced observed states that keep desired state (experiments.json#foundation).

### Runtime support

**G-RT-1**

- Current: Computers are workspaces (native processes) or unverified containers.
- Desired: Containers verified; microVMs (Firecracker/KVM); WASM sandboxes; GPU.
- Impact: No isolation for computers; features advertised but not provided.
- Evidence: runtime matrix
- Next: Verify the container provider against a real engine in CI; then a Firecracker session provider.

**G-RT-2**

- Current: Target features describe the host (`containers` = docker on PATH), not the computer.
- Desired: Features describe what a computer on the target can be, verified live.
- Impact: Placement puts a "containers" computer in a workspace.
- Evidence: placement_refusals.containers
- Next: Split host features from substrate; check engine liveness.

### Providers

**G-PROV-1**

- Current: No Fly/Railway/Render/cloud/bare-metal provisioning; targets must already run `compute serve`.
- Desired: Provider adapters that materialize targets or computers.
- Impact: "Put software on Railway" is impossible.
- Evidence: provider matrix
- Next: Define a provisioning interface (materialize a target) and one adapter.

### Placement

**G-DISC-1**

- Current: Targets are configured in a pool file; network, GPU model, nested virtualization are not discovered.
- Desired: Discovery of machines and their capabilities.
- Impact: Placement only knows what a file says.
- Evidence: discovery
- Next: Liveness-checked feature discovery; optional registration of targets with the control plane.

**G-PLACE-1**

- Current: persistent_storage, public_endpoint, terminal are requestable (UI checkboxes) but offered by no provider.
- Desired: Either implemented or not offered.
- Impact: Dead-end options.
- Evidence: placement_refusals
- Next: Hide unavailable options using GET /targets; implement persistent volumes and public endpoints.

### Execution

**G-EXEC-1**

- Current: Bundle workloads of node environments execute on the daemon host (applications no longer do).
- Desired: The daemon coordinates; computers execute.
- Impact: The daemon is both coordinator and executor.
- Evidence: execution_paths
- Next: Covered by G-ARCH-5.

**G-EXEC-2**

- Current: No cancellation or timeout controls in the UI; jobs have timeouts in the API.
- Desired: Cancel/retry for every job from every surface.
- Impact: Stuck builds need the CLI or waiting.
- Evidence: api: POST /compute/jobs/{job}/cancel has no computer-level route
- Next: Add cancel for computer jobs and operations.

### Projects

**G-PROJ-1**

- Current: Local folders must be Git repositories; no upload.
- Desired: Any folder.
- Impact: Non-Git projects cannot run.
- Evidence: run-a-project
- Next: Upload a folder as an artifact into the computer.

### Applications

**G-APP-1**

- Current: Endpoints are target-host:port; no domains, TLS, or ingress for computer applications.
- Desired: Public endpoints with domains and certificates.
- Impact: Production traffic cannot reach computer applications properly.
- Evidence: endpoints, domains-tls
- Next: Route domains to computer endpoints through the existing network layer.

### Services

**G-SVC-1**

- Current: Database/Redis are command templates that assume binaries on the host.
- Desired: Managed service images/volumes.
- Impact: Templates fail where binaries are absent.
- Evidence: ui TEMPLATES
- Next: Depends on container computers and volumes.

### Deployment

**G-DEP-1**

- Current: A release restarts processes (downtime); bundle releases have zero-downtime switching.
- Desired: Zero-downtime rollouts for computers.
- Impact: Production updates interrupt traffic.
- Evidence: zero-downtime
- Next: Two instances behind a switched endpoint inside the computer.

### Releases

**G-REL-1**

- Current: A version is a commit and a digest; no artifact is kept.
- Desired: Stored, verifiable artifacts (build outputs) per version.
- Impact: A version cannot be redeployed if the repository changes history.
- Evidence: artifact-store
- Next: Store the package (and optional build outputs) in the artifact store.

### Production

**G-PROD-1**

- Current: No approvals, no protected environments, no deploy freezes.
- Desired: Promotion policy per environment.
- Impact: Anyone with deploy scope who owns both environments promotes.
- Evidence: approvals
- Next: Environment policy for promotion (approvals, required checks).

### UI

**G-UI-1**

- Current: Targets, credentials, audit, node upgrades, runtimes, placement explanation are CLI-only.
- Desired: Every capability visible.
- Impact: Operators need the terminal for setup and diagnosis.
- Evidence: ui.not_in_ui
- Next: Add Manage pages for targets and access.

**G-UI-2** (closed)

- Was: The browser certification package fails; browser tests do not run in CI.
- Desired: Browser tests in CI.
- Impact: UI regressions ship (one already did).
- Evidence: ui-certification
- Next: Closed: the certification is fixed for the action home and runs in CI with Chromium, with a computer-reality journey (.github/workflows/test.yml).

### CLI

**G-CLI-1**

- Current: 52 commands have broken or missing help; `compute session` mixes target sessions and work sessions.
- Desired: Accurate help; one session concept.
- Impact: Discoverability.
- Evidence: cli.json help_defect
- Next: Fix clap doc comments; rename target sessions (e.g. `compute target session`).

### API

**G-API-1**

- Current: No versioning of the Compute API; routes without any client (/info, /metrics, …).
- Desired: A versioned, documented API.
- Impact: Clients break silently.
- Evidence: api.json
- Next: Publish an API description generated from ROUTES.

### Agents

**G-AGENT-1**

- Current: Agents are operators; no delegation, budgets, or per-agent audit identity.
- Desired: Agent identities with bounded authority.
- Impact: An agent with deploy scope can do everything a human can.
- Evidence: agent-identity
- Next: Scoped, expiring agent credentials tied to an owner.

### Security

**G-SEC-1**

- Current: See SEC-1…SEC-6.
- Desired: Real boundaries at the target and between computers.
- Impact: Critical.
- Evidence: security
- Next: G-ARCH-1, then isolation via container/microVM substrates.

### Observability

**G-OBS-1**

- Current: Process logs are read on demand; no log streaming, metrics, or traces for computers in the UI.
- Desired: Live logs and metrics per application.
- Impact: Operating production is blind between refreshes.
- Evidence: logs, metrics
- Next: Stream process logs through the daemon; surface /metrics.

### Documentation

**G-DOC-1**

- Current: README and getting-started lead with a command that fails by default; daemon.md describes an old UI.
- Desired: Docs lead with `compute` and verified journeys.
- Impact: First impressions fail.
- Evidence: documentation
- Next: Rewrite the first pages around the verified journey.

### Testing

**G-TEST-1**

- Current: 87 CLI commands are never invoked by a test; the container provider has no real test (target auth and machine loss now do).
- Desired: Every product claim executable.
- Impact: Regressions in untested paths.
- Evidence: cli.json tests
- Next: Add the missing journeys to CI.

### Performance

**G-PERF-1**

- Current: Every event re-renders and refetches the whole page; /software fans out per environment.
- Desired: Incremental updates.
- Impact: Fine at 3 computers; unmeasured at scale.
- Evidence: performance
- Next: Measure at 100 computers; add a software index.
<!-- /audit -->

## 28. Base capture vs. complete Compute

<!-- audit:base_vs_complete -->
| Capability | Base capture (today) | Complete Compute | Gap |
| --- | --- | --- | --- |
| Computer abstraction | Environments with a durable computer (workspace on a `compute serve` target) | Any machine: container, microVM, VM, bare metal, cloud | Substrates beyond workspaces |
| Persistent environments | Yes, verified | Yes | None |
| Ephemeral environments | Yes, verified (expire, evidence kept) | Yes | None |
| Self-discovery | CPU/memory/disk/OS/arch, runtimes, isolation facilities; features by device/binary presence | Machines, capabilities, networks, GPUs, virtualization — live | Liveness, networks, automatic target discovery |
| Native runtime | Yes (workspace computers; process workloads) | Yes, isolated | Isolation |
| Containers | Adapter exists; unverified against a real engine | Verified, default substrate | Verification, images, volumes, ports |
| WASM | Workload engine (compute run); not a computer | WASM computers/sandboxes | Substrate |
| Firecracker | Feature label only | MicroVM computers | Everything |
| KVM | Feature label only | VM computers | Everything |
| Capability placement | Yes, with reasons; features are labels | Yes, against verified capabilities | Verified features, storage, public endpoints |
| Multiple projects | Yes, verified | Yes | None |
| Application assembly | Proposal from source + GO, verified | Yes, plus services/images/volumes | Managed services |
| Build | Yes, in the computer | Yes | None |
| Test | Yes, in the computer | Yes | None |
| Publish | Versions: commit + digest + evidence | Versions with stored artifacts | Artifact storage |
| Deploy | Rollouts to environments, in place, verified | Zero-downtime | Traffic switching |
| Promote | Reviewed plan + rollout, verified | With approvals and policy | Approvals |
| Production | An environment named production; no protection | Protected environments, domains, TLS | Policy, ingress for computers |
| Rollback | Yes, verified | Yes | None |
| Operations | Logs (on demand), restart, config, probe health | Streaming logs, metrics, alerts, scaling | Observability, scaling |
| Agent execution | AppPort covers every UI operation; agents are operators | Scoped agent identities | Delegation |
| Provider abstraction | Pool of local/remote targets; no cloud adapters | Fly/Railway/Render/cloud/bare metal | Adapters |
| UI | Work/Manage, home, run, software, operations; verified in a browser | Every capability | Targets, access, diagnosis pages |
| CLI | 182 commands; 52 with help defects | Consistent, documented | Help, naming |
| API | 129 routes, scoped | Versioned, documented | Description |
| Observability | Events, receipts, job evidence, /metrics (API only) | Live logs, metrics, traces | Streaming, dashboards |
| Durable evidence | Events, versions, rollouts, receipts; jobs on targets | Same, in one authority | Jobs outside FeltDB; file default |
| Security boundary | Daemon: real; targets: none | Every hop authenticated; computers isolated | Target auth, isolation |
<!-- /audit -->

## 29. Product readiness matrix

<!-- audit:readiness -->
| Area | Status | Evidence | Blocking gap |
| --- | --- | --- | --- |
| Install | **PARTIAL** | cargo build; release distribution certified in CI | No installer/package; runtimes download on demand |
| Launch | **PASS** | compute → UI in 3.7 s | Browser opener only on desktops |
| Discovery | **PARTIAL** | resources/runtimes/isolation real; features inferred | G-DISC-1, G-RT-2 |
| Placement | **PASS** | matching tests; reasons shown | G-PLACE-1 |
| Computer creation | **PASS** | 0.6 s to running (workspace) | Substrates (G-RT-1) |
| Project execution | **PASS** | journey | Git-only sources |
| Multi-project | **PASS** | journey | — |
| Runtime coverage | **PARTIAL** | workload runtimes yes; computer substrates: workspace only verified | G-RT-1 |
| Sessions | **PASS** | CLI + provider tests | Two session concepts (G-CLI-1) |
| Build | **PASS** | journey | — |
| Test | **PASS** | journey | — |
| Publish | **PASS** | journey | G-REL-1 |
| Deploy | **PASS** | journey | G-DEP-1 |
| Promote | **PASS** | journey | G-PROD-1 |
| Production | **PARTIAL** | an environment; no domains/TLS/approvals for computers | G-APP-1, G-PROD-1 |
| Rollback | **PASS** | journey | — |
| Operations | **PARTIAL** | restart/logs/config/health probe; target liveness | G-OBS-1 |
| UI | **PARTIAL** | journey and certification pass in CI with Chromium; unreachable/lost shown with actions | G-UI-1 |
| CLI | **PARTIAL** | 185 commands; 52 help defects; 87 untested through the CLI | G-CLI-1 |
| Agents | **PARTIAL** | AppPort parity (stub-tested) | G-AGENT-1 |
| Providers | **PARTIAL** | local + remote targets only | G-PROV-1 |
| Security | **PARTIAL** | targets authenticate every request and isolate control planes (demonstrated); computers on one host are not isolated from each other | SEC-3, SEC-4 |
| Recovery | **PASS** | daemon restart, target restart, target outage, machine and session loss, stale answers: demonstrated (experiments.json#foundation) | — |
<!-- /audit -->

## 30. Conclusions

### What Compute Actually Is Today

A **single-host developer control plane with a real durable model**. One
command starts it. It makes a durable computer on a target in under a
second, runs several projects in it, changes it in place under
generation-fenced GO, and carries software through build, test, publish,
deploy, promote, and rollback — every step verified in a browser and
available identically to the CLI and to agents. It survives its own
restarts and its target's.

Its computers are **workspaces**: directories and native processes on a
host, with no isolation, on targets that accept any caller. Beside it runs an
older node deployment model (bundle projects, applications) that executes
on the daemon host and owns the only ingress, domains, and zero-downtime
releases. Underneath is a solid runtime-neutral workload engine
(`compute run`) that the computer model does not use.

### What Is Already Strong

- **The durable model.** Computers, contents, versions, rollouts, and work
  sessions are records with generations; changes are fenced; drivers resume
  after restarts; in-place change keeps the same machine. Verified.
- **The product loop.** Launch → run a project → multiple projects → build
  → test → publish → deploy → promote → rollback → operate, end to end,
  in 57 s in a real browser, with screenshots.
- **Parity.** Every journey operation is the same request from the UI, CLI,
  API, and AppPort.
- **Honest placement.** Requirements are matched with a reason for every
  refusal.
- **Evidence.** Durable jobs with receipts; events; version step evidence;
  an audit trail on the daemon.
- **The state contract.** Bounded, indexed FeltDB access enforced by a test
  in CI; three conforming backends.
- **The workload engine.** Eleven runtimes behind one conformance suite,
  with explicit refusal instead of silent downgrade.

### What Is Fundamentally Missing

1. **Isolation.** Targets now authenticate their control plane (SEC-1 and
   SEC-2 resolved), but computers on a host are not isolated from each
   other (SEC-3).
2. **Machines other than workspaces.** No verified container computer, no
   microVM or VM, no WASM computer, no GPU. Features are labels.
3. **Provisioning.** Compute cannot create a target anywhere; every machine
   must already run `compute serve` and be named in a file.
4. **Production.** No ingress, TLS, or domains for computer endpoints; no
   zero-downtime rollouts; no approvals or protected environments; no
   stored artifacts; no streaming logs or metrics in the UI.

### What Needs to Be Reconciled

- **Two deployment models → one.** Computer versions/rollouts vs. bundle
  projects on node environments vs. applications. The older two execute on
  the daemon host and hold the only ingress and zero-downtime switching.
- **Durable state.** Target jobs are durable outside the authority (the
  control plane keeps references and events). The backend default is
  decided: FeltDB for production, the file stated as local development.
- **Vocabulary.** Environment (computer vs. node), Project (computer vs.
  bundle), Application (3 meanings), Service (3), Session (target vs.
  work), Provider (pool member, substrate, DNS), Runtime (workload vs.
  substrate), Computer/Machine/session for one thing.
- **Docs vs. product.** README and getting-started lead with the workload
  engine and a failing first command; the architecture's invariant 15 ("nothing
  names an application") and old non-goals no longer hold.
- **Advertised vs. real.** `containers` without an engine; UI options no
  provider offers; `session` commands that bypass the daemon.

### Path to Complete Compute

<!-- audit:backlog -->
1. **FOUNDATION (done)**
   - Done: authenticate targets; the daemon holds the credential (G-ARCH-1)
   - Done: detect machine loss and unreachable targets (G-ARCH-4)
   - Done: decide the durable-state default (G-ARCH-3)
   - Done: browser tests and the UI certification in CI; fix the home-route regression (G-UI-2)
2. **EXECUTION**
   - One deployment model: retire or port node environments (G-ARCH-5, G-EXEC-1); applications are converged (G-ARCH-2)
   - Cancel/retry for computer jobs and operations (G-EXEC-2)
3. **RUNTIME COVERAGE**
   - Container computers verified in CI, with ports and volumes (G-RT-1)
   - Live, substrate-accurate target features (G-RT-2, G-DISC-1)
   - A microVM session provider (Firecracker)
4. **PROJECT/APP ASSEMBLY**
   - Non-Git sources (G-PROJ-1)
   - Managed services on container computers (G-SVC-1)
   - Persistent storage and public endpoints, or hide them (G-PLACE-1)
5. **DEVELOPMENT WORKFLOW**
   - Interactive terminal (PTY) and file editing
   - Streaming logs (G-OBS-1)
6. **RELEASE**
   - Stored version artifacts (G-REL-1)
7. **DEPLOYMENT**
   - Zero-downtime rollouts in computers (G-DEP-1)
   - Provider adapters that materialize targets (G-PROV-1)
8. **PRODUCTION**
   - Domains/TLS/ingress for computer endpoints (G-APP-1)
   - Protected environments and approvals (G-PROD-1)
9. **OPERATIONS**
   - Metrics and alerts in the UI (G-OBS-1)
   - Scaling short of replacement
   - Agent identities and delegation (G-AGENT-1)
10. **PRODUCT POLISH**
   - README/getting-started around `compute` (G-DOC-1)
   - CLI help and naming (G-CLI-1)
   - Targets/access/diagnosis pages (G-UI-1)
   - API description (G-API-1)
   - Performance at scale (G-PERF-1)
<!-- /audit -->

Each item names its gap in [gap-analysis.md](gap-analysis.md). The first stage
is done: targets authenticate their control plane and the control plane
notices unreachable targets and lost machines, so later capabilities build
on a boundary that holds and a view that does not lie.
