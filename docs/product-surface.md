# Product surface

Every way a person, script, or agent reaches Compute, audited 2026-09-27 at
`69b70d9`. The inventories are generated from [audit.json](audit.json)
(`cli`, `api`, `ui`), which is itself checked against the code by
`crates/compute-cli/tests/audit.rs`: a command or route added without an
audit entry fails that test. Screenshots of every UI page in the verified
journey are in [product-surface/](product-surface/README.md).

## The four surfaces

| Surface | Reaches | Size | Tested by |
| --- | --- | --- | --- |
| UI (browser, Work/Manage) | the daemon API | 19 routes, 72 API routes used | `crates/compute-cli/tests/product_journey.rs` (Playwright; skipped without Chromium, so **not in CI**) |
| CLI (`compute`) | the daemon API, targets directly, or nothing (local) | 182 commands | Rust integration tests; 87 commands never invoked |
| API (daemon) | control state, targets, supervisor | 129 routes, every one scoped | `compute-environment` integration tests |
| AppPort (TypeScript) | the daemon API | 52 API routes | `npm test` against a stub daemon |

## Parity

What a person can do from each surface, for the operations in the product
journey and for setup and diagnosis.

| Operation | UI | CLI | API | AppPort |
| --- | --- | --- | --- | --- |
| Launch / stop the control plane | — (it is the UI) | `compute`, `compute down` | — | — |
| Create a computer (persistent / temporary) | yes (Manage dialog, Run a project) | `compute environment create --cpu … --persistent/--ephemeral` | `POST /environments` | yes |
| Run a project (inspect → propose → GO) | yes | `compute environment propose`, then `project add` / `contents apply` (GO) | yes | yes |
| Several projects in one computer | yes | yes | yes | yes |
| Change contents / configuration, GO | yes | yes | yes | yes |
| Build, test, named commands | yes | `compute environment build/test/run` | yes | yes |
| Exec a command / terminal | line-by-line jobs | `compute environment exec`, `connect` | yes | yes |
| Files | list and read | via exec | via exec | via exec |
| Publish / deploy / promote / rollback | yes | `compute versions …` | yes | yes |
| Logs, processes, restart | yes | yes | yes | yes |
| Stop / resume / replace / destroy a computer | yes | yes | yes | yes |
| Lifetime change | yes | yes | yes | yes |
| Work sessions (open/close) | yes (Work mode) | `compute session open/close/opened` | yes | yes |
| Targets and their features | **no** | `compute target list` | `GET /targets` | no |
| Placement explanation | reasons on failure only | `compute placement explain` | yes | no |
| Operator credentials, audit trail | **no** | `compute auth …` | yes | no |
| Runtimes, doctor, isolation | **no** | `compute runtimes`, `doctor`, `isolation` | no (local) | no |
| Target sessions (not computers) | **no** | `compute session create…destroy` (direct to a target) | target API | no |
| Metrics | **no** | **no** | `GET /metrics` | no |
| Node environments, bundle projects, domains | yes (Manage) | yes | yes | partial |
| Applications (`init`/`deploy <dir>`) | shown as a project | `compute init`, `compute deploy` | yes | no |
| FeltDB provisioning / upgrade | **no** | `compute control-plane …` | — | no |

## UI

<!-- audit:ui_routes -->
| Route | View | Mode |
| --- | --- | --- |
| `#/` | homeView | both |
| `#/run` | runView | both |
| `#/software` | softwareListView | both |
| `#/software/{project}` | softwareView | both |
| `#/software/{project}/versions/{v}` | versionView | manage |
| `#/operations/version/{project}/{v}` | operationView | manage |
| `#/operations/rollout/{id}` | operationView | manage |
| `#/environments` | environmentsView | manage |
| `#/environments/{e}` | environmentView | manage |
| `#/environments/{e}/projects/{p}/{tab}` | projectView (bundle projects, 8 tabs) | manage |
| `#/work` | workHomeView | work |
| `#/work/{e}` | workView | work |
| `#/projects` | projectsView (bundle) | manage |
| `#/projects/{p}` | projectDetailView (bundle) | manage |
| `#/services` | servicesView | manage |
| `#/deployments/{id}` | deploymentView (bundle) | manage |
| `#/domains` | domainsView | manage |
| `#/domains/{d}` | domainView | manage |
| `#/events` | eventsView | manage |
<!-- /audit -->

**Work mode** is for doing: the Work home lists computers you can enter;
a computer's page shows its projects, processes, endpoints, a terminal
(one durable job per line), files, and GO. **Manage mode** is for owning:
environments, software (versions and where they run), operations,
services, domains, events. The action home (`#/`) is shared: "What do you
want to do?" — run a project, create a computer, publish, deploy.

Not in the UI at all:

<!-- audit:not_in_ui -->
- targets and their features
- operator credentials and the audit trail
- node upgrade/rollback
- runtimes, doctor, isolation, capabilities
- placement explanation
- policy
- target sessions (compute session create…)
- metrics
- network status
- certificate/dns status tables
- application deploy/pack
- connect grant (credentials)
- bundle revision registration (compute project push)
- FeltDB provisioning/upgrade
<!-- /audit -->

UI usability findings (from the journey and the screenshots):

- The first page is useful: it offers the next actions and the software
  and computers that exist. Environments without a computer (node
  environments) do not appear on it.
- The create dialog offers options that cannot be placed (persistent
  storage, public endpoint) — G-PLACE-1.
- A computer whose target is down is shown as unreachable, and one whose
  machine is gone as lost, each with what it means and what to do (Check
  now; Replace machine…, Destroy); the home, Work, and Manage pages agree
  with the CLI and the API (G-ARCH-4, closed).
- Every event re-fetches and re-renders the whole page; fine at three
  computers (home ready in 83 ms), unmeasured at scale.
- The browser certification package (`packages/compute-ui-e2e`) is fixed for
  the action home and runs in CI with Chromium, with a computer-reality
  journey (G-UI-2, closed).

## CLI

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

Usability findings:

- `compute` alone launches everything and prints the URL: good.
- 52 commands have help that is wrong or empty: `compute run`, `compute
  runtimes`, and every `compute remote …` command describe themselves as
  "Where the caller-owned pool configuration and capability cache live"
  (a flattened argument's doc comment); others print only `Usage:`.
- `compute session` mixes two unrelated things: target sessions
  (`create`, `exec`, `destroy`, … — straight to a target, bypassing the
  daemon) and work sessions (`open`, `close`, `opened` — daemon records).
- Four command families deploy: `compute deploy` and `compute application
  deploy` (applications), `compute deployment …` (bundle projects),
  `compute versions deploy` (computers). `compute environment release` is a
  fifth, in-place release of a computer's projects.

### Every command

<details><summary>182 commands (from <code>compute --help</code>, recursively)</summary>

<!-- audit:cli -->
| Command | What it does (its help) | Help defect | Talks to | State | Authority | UI | Tests |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `compute up` | Launch the Compute control plane on this machine and open it: what `compute` alone does. Starts this machine's computer host and the daemon when they are not running | — | launcher: spawns compute serve + compute start | local processes, pool.toml, the target trust file and the control plane token | n/a (local) | the UI itself | `crates/compute-cli/tests/product_journey.rs`, `crates/compute-cli/tests/launcher.rs`, `packages/compute-ui-e2e/src/computer-reality.test.mjs` |
| `compute down` | Stop the control plane `compute` launched, and this machine's computer host. Durable state is kept | — | launcher | stops processes | n/a (local) | none | `crates/compute-cli/tests/product_journey.rs` |
| `compute versions publish` | Publish a version: build, tests, checks, and a source package, run in the environment the project is developed in | — | daemon API /software, /rollouts | durable (Version, Rollout) | daemon scopes + owner | Software pages: publish/deploy/promote/rollback, version and operation pages | — |
| `compute versions list` | A project's versions, newest first, and where they run | — | daemon API /software, /rollouts | durable (Version, Rollout) | daemon scopes + owner | Software pages: publish/deploy/promote/rollback, version and operation pages | — |
| `compute versions show` | One version: its source, package, evidence, and rollouts | — | daemon API /software, /rollouts | durable (Version, Rollout) | daemon scopes + owner | Software pages: publish/deploy/promote/rollback, version and operation pages | — |
| `compute versions deploy` | Deploy a version to an environment, in place | — | daemon API /software, /rollouts | durable (Version, Rollout) | daemon scopes + owner | Software pages: publish/deploy/promote/rollback, version and operation pages | — |
| `compute versions promote` | Promote the version running in one environment to another, after showing what it would change | — | daemon API /software, /rollouts | durable (Version, Rollout) | daemon scopes + owner | Software pages: publish/deploy/promote/rollback, version and operation pages | — |
| `compute versions rollback` | Roll an environment back to the version before the current one (or the one named) | — | daemon API /software, /rollouts | durable (Version, Rollout) | daemon scopes + owner | Software pages: publish/deploy/promote/rollback, version and operation pages | — |
| `compute init` | Create the smallest useful Compute application | — | local files | writes a directory | none | none | `crates/compute-cli/tests/cli.rs`, `crates/compute-cli/tests/computers.rs`, `crates/compute-cli/tests/product.rs`, `crates/compute-cli/tests/product_journey.rs` |
| `compute application info` | Describe an application directory or artifact: identity, runtime, requirements, environment contract. Contacts no provider | — | daemon API /applications (auto-starts a daemon in ./.compute/daemon) | durable (applications environment) | daemon scopes (no owner) | partial: application project page (rollback, stop, logs); no deploy/pack | `crates/compute-cli/tests/product.rs` |
| `compute application pack` | Package an application directory as a portable artifact | — | daemon API /applications (auto-starts a daemon in ./.compute/daemon) | durable (applications environment) | daemon scopes (no owner) | partial: application project page (rollback, stop, logs); no deploy/pack | `crates/compute-cli/tests/product.rs`, `packages/compute-appport/src/test/application.test.ts` |
| `compute application deploy` | Place the application on a provider that can host it and release a new version there | — | daemon API /applications (auto-starts a daemon in ./.compute/daemon) | durable (applications environment) | daemon scopes (no owner) | partial: application project page (rollback, stop, logs); no deploy/pack | `crates/compute-cli/tests/product.rs`, `packages/compute-appport/src/application.ts` |
| `compute application status` | The application's state on the provider that hosts it | — | daemon API /applications (auto-starts a daemon in ./.compute/daemon) | durable (applications environment) | daemon scopes (no owner) | partial: application project page (rollback, stop, logs); no deploy/pack | `crates/compute-cli/tests/product.rs`, `packages/compute-appport/src/application.ts` |
| `compute application logs` | The active version's output | — | daemon API /applications (auto-starts a daemon in ./.compute/daemon) | durable (applications environment) | daemon scopes (no owner) | partial: application project page (rollback, stop, logs); no deploy/pack | `packages/compute-appport/src/application.ts` |
| `compute application history` | Every version, newest first | — | daemon API /applications (auto-starts a daemon in ./.compute/daemon) | durable (applications environment) | daemon scopes (no owner) | partial: application project page (rollback, stop, logs); no deploy/pack | `crates/compute-cli/tests/product.rs`, `packages/compute-appport/src/application.ts` |
| `compute application rollback` | Deploy an earlier version again, as the next version | — | daemon API /applications (auto-starts a daemon in ./.compute/daemon) | durable (applications environment) | daemon scopes (no owner) | partial: application project page (rollback, stop, logs); no deploy/pack | `packages/compute-appport/src/application.ts` |
| `compute application stop` | Stop serving. Versions and evidence remain | — | daemon API /applications (auto-starts a daemon in ./.compute/daemon) | durable (applications environment) | daemon scopes (no owner) | partial: application project page (rollback, stop, logs); no deploy/pack | `packages/compute-appport/src/application.ts` |
| `compute run` | — | describes itself as the pool location (a flattened doc comment) | local engine, or a provider chosen by pool placement | none (execution record + receipt) | local user; providers: the target credential its pool names | none | `crates/compute-cli/tests/cli.rs`, `crates/compute-cli/tests/isolation.rs`, `crates/compute-cli/tests/remote_pool.rs`, `packages/compute-appport/src/provider.ts` |
| `compute bundle create` | — | no description | local | writes files | none | none | `crates/compute-cli/tests/cli.rs`, `packages/compute-appport/src/provider.ts`, `packages/compute-appport/src/test/conformance.test.ts`, `packages/compute-appport/src/test/environment.test.ts` |
| `compute bundle inspect` | — | no description | local | writes files | none | none | `crates/compute-cli/tests/cli.rs` |
| `compute bundle verify` | — | no description | local | writes files | none | none | `crates/compute-cli/tests/cli.rs` |
| `compute deps create` | — | no description | local | writes files | none | none | `crates/compute-cli/tests/cli.rs` |
| `compute deps inspect` | — | no description | local | writes files | none | none | — |
| `compute deps verify` | — | no description | local | writes files | none | none | `crates/compute-cli/tests/cli.rs` |
| `compute inspect` | — | no description | local | none | none | none | `crates/compute-cli/tests/cli.rs`, `crates/compute-cli/tests/remote_pool.rs`, `packages/compute-appport/src/schemas.ts`, `packages/compute-appport/src/test/conformance.test.ts` |
| `compute runtimes` | — | describes itself as the pool location (a flattened doc comment) | local / pool | runtime store | none | none | `crates/compute-cli/tests/cli.rs`, `crates/compute-cli/tests/remote_pool.rs`, `crates/compute-placement/tests/selection.rs` |
| `compute runtime` | — | no description | local | none | none | none | `crates/compute-cli/tests/cli.rs`, `crates/compute-cli/tests/product.rs`, `packages/compute-appport/src/test/conformance.test.ts` |
| `compute capabilities` | — | no description | local | none | none | none | `crates/compute-cli/tests/cli.rs`, `crates/compute-cli/tests/product.rs`, `crates/compute-cli/tests/remote_pool.rs`, `crates/compute-cli/tests/sessions.rs` |
| `compute isolation` | Show the versioned isolation profiles and runtime support matrix | — | local | none | none | none | `crates/compute-cli/tests/cli.rs`, `crates/compute-cli/tests/isolation.rs`, `crates/compute-policy/tests/evaluator.rs`, `crates/compute-provider/tests/remote.rs` |
| `compute exec` | — | no description | local engine (issue-description convenience) | none | none | none | `crates/compute-cli/tests/cli.rs`, `crates/compute-cli/tests/sessions.rs`, `crates/compute-placement/tests/matching.rs`, `packages/compute-appport/src/test/sessions.test.ts` |
| `compute doctor` | Diagnose this host's runtimes and, when one runs, its controller | — | local + daemon /status | none | none | none | `crates/compute-cli/tests/cli.rs` |
| `compute certify` | — | no description | local | none | none | none | `crates/compute-cli/src/certification.rs`, `crates/compute-cli/tests/cli.rs` |
| `compute distribution build` | — | no description | local | writes files | none | none | `crates/compute-cli/tests/cli.rs` |
| `compute distribution inspect` | — | no description | local | writes files | none | none | — |
| `compute distribution verify` | — | no description | local | writes files | none | none | `crates/compute-cli/tests/cli.rs` |
| `compute receipt inspect` | — | no description | local | none | none | Receipt dialog (project pages) | `crates/compute-cli/tests/cli.rs` |
| `compute receipt verify` | — | no description | local | none | none | Receipt dialog (project pages) | `crates/compute-cli/tests/cli.rs`, `crates/compute-cli/tests/product.rs`, `crates/compute-cli/tests/sessions.rs` |
| `compute version` | — | no description | local | none | none | none | `crates/compute-cli/tests/cli.rs`, `crates/compute-cli/tests/product.rs`, `crates/compute-cli/tests/upgrade.rs` |
| `compute remote run` | — | describes itself as the pool location (a flattened doc comment) | provider compute.remote@1 directly | provider job store | provider: the target credential its pool names on compute serve | none | `crates/compute-cli/tests/sessions.rs` |
| `compute remote inspect` | — | describes itself as the pool location (a flattened doc comment) | provider compute.remote@1 directly | provider job store | provider: the target credential its pool names on compute serve | none | — |
| `compute remote submit` | — | describes itself as the pool location (a flattened doc comment) | provider compute.remote@1 directly | provider job store | provider: the target credential its pool names on compute serve | none | `packages/compute-appport/src/provider.ts` |
| `compute remote status` | — | describes itself as the pool location (a flattened doc comment) | provider compute.remote@1 directly | provider job store | provider: the target credential its pool names on compute serve | none | `crates/compute-cli/tests/sessions.rs` |
| `compute remote result` | — | describes itself as the pool location (a flattened doc comment) | provider compute.remote@1 directly | provider job store | provider: the target credential its pool names on compute serve | none | — |
| `compute remote wait` | — | describes itself as the pool location (a flattened doc comment) | provider compute.remote@1 directly | provider job store | provider: the target credential its pool names on compute serve | none | — |
| `compute remote receipt` | — | describes itself as the pool location (a flattened doc comment) | provider compute.remote@1 directly | provider job store | provider: the target credential its pool names on compute serve | none | — |
| `compute remote artifacts` | — | describes itself as the pool location (a flattened doc comment) | provider compute.remote@1 directly | provider job store | provider: the target credential its pool names on compute serve | none | — |
| `compute remote cancel` | — | describes itself as the pool location (a flattened doc comment) | provider compute.remote@1 directly | provider job store | provider: the target credential its pool names on compute serve | none | — |
| `compute remote capabilities` | — | describes itself as the pool location (a flattened doc comment) | provider compute.remote@1 directly | provider job store | provider: the target credential its pool names on compute serve | none | `crates/compute-cli/tests/remote_pool.rs` |
| `compute remote health` | — | describes itself as the pool location (a flattened doc comment) | provider compute.remote@1 directly | provider job store | provider: the target credential its pool names on compute serve | none | `crates/compute-cli/tests/remote_pool.rs`, `packages/compute-appport/src/test/placement.test.ts` |
| `compute provider list` | List configured providers with their discovery status and health | — | pool config + providers | capability cache | none | none | `packages/compute-appport/src/provider.ts` |
| `compute provider inspect` | Show a provider's validated descriptor. A pool ID yields the canonical descriptor; `local` or an endpoint URL outside the pool yields the raw capability response | — | pool config + providers | capability cache | none | none | `packages/compute-appport/src/provider.ts` |
| `compute provider capabilities` | Show a provider's raw capability response | — | pool config + providers | capability cache | none | none | `packages/compute-appport/src/provider.ts` |
| `compute provider pool` | Show the configured pool and its selection policy | — | pool config + providers | capability cache | none | none | — |
| `compute provider refresh` | Rediscover capabilities and rewrite the capability cache | — | pool config + providers | capability cache | none | none | — |
| `compute capacity` | Show configured, reserved, and currently available provider capacity | — | pool | none | none | none | — |
| `compute jobs` | List durable jobs and their reservation state | — | pool | none | none | none | — |
| `compute placement inspect` | Evaluate the pool for a workload. No execution occurs | — | pool (evaluation only) | none | none | none | `packages/compute-appport/src/provider.ts` |
| `compute placement explain` | Explain what the workload requires and why each provider is or is not compatible. No execution occurs | — | pool (evaluation only) | none | none | none | — |
| `compute pool run` | Place and execute synchronously on the selected provider | — | pool placement then provider | provider job store | provider: target credential | none | `crates/compute-cli/tests/product.rs`, `packages/compute-appport/src/provider.ts` |
| `compute pool submit` | Place and submit a durable job to the selected provider | — | pool placement then provider | provider job store | provider: target credential | none | `crates/compute-cli/tests/product.rs`, `packages/compute-appport/src/provider.ts` |
| `compute target credential issue` | Issue a credential that lets a control plane control this target. The token is shown (or written to --token-file) once; the target keeps only its verifier | — | local: the target's trust file (compute serve --credentials) | the target trust file (verifiers only) | n/a (local file on the target machine) | none | `crates/compute-cli/tests/sessions.rs`, `crates/compute-cli/tests/launcher.rs` |
| `compute target credential list` | The credentials a target trusts: never a secret | — | local: the target's trust file (compute serve --credentials) | the target trust file (verifiers only) | n/a (local file on the target machine) | none | `crates/compute-cli/tests/sessions.rs`, `crates/compute-cli/tests/launcher.rs` |
| `compute target credential revoke` | Revoke a credential: the target refuses it at the next request | — | local: the target's trust file (compute serve --credentials) | the target trust file (verifiers only) | n/a (local file on the target machine) | none | `crates/compute-cli/tests/sessions.rs`, `crates/compute-cli/tests/launcher.rs` |
| `compute target list` | The computers and infrastructure the daemon can place environments on | — | daemon API /targets | none | daemon read scope | none (targets are not shown in the UI) | `crates/compute-cli/tests/computers.rs`, `crates/compute-cli/tests/launcher.rs` |
| `compute session create` | Create a session on a provider chosen by placement | — | pool placement then target sessions directly | target session store (not FeltDB) | target: target credential; owner = hash of the Authorization header | none | `crates/compute-cli/tests/sessions.rs`, `packages/compute-appport/src/sessions.ts`, `packages/compute-appport/src/test/sessions.test.ts` |
| `compute session list` | List your sessions on every provider in the pool | — | targets directly | none | target: target credential | none | `crates/compute-cli/tests/sessions.rs`, `packages/compute-appport/src/sessions.ts` |
| `compute session info` | Show a session's complete, authoritative state | — | targets directly | none | target: target credential | none | `crates/compute-cli/tests/sessions.rs`, `packages/compute-appport/src/sessions.ts` |
| `compute session connect` | Get connection details for a session | — | targets directly | target session store | target: target credential | none | `packages/compute-appport/src/sessions.ts` |
| `compute session exec` | Run a command in a session as a durable job | — | targets directly | target job store | target: target credential | none | `crates/compute-cli/tests/sessions.rs`, `packages/compute-appport/src/sessions.ts`, `packages/compute-appport/src/test/sessions.test.ts` |
| `compute session logs` | Show the output of every execution in a session | — | targets directly | none | target: target credential | none | `crates/compute-cli/tests/sessions.rs`, `packages/compute-appport/src/sessions.ts` |
| `compute session stop` | Stop active executions, keeping the environment and the record | — | targets directly | target session store | target: target credential | none | — |
| `compute session resume` | Resume a stopped session's environment. Never creates a new one | — | targets directly | target session store | target: target credential | none | — |
| `compute session claim` | Keep an ephemeral session until it is destroyed | — | targets directly | target session store | target: target credential | none | — |
| `compute session destroy` | Tear the environment down. The record remains as evidence | — | targets directly | target session store | target: target credential | none | `crates/compute-cli/tests/sessions.rs` |
| `compute session open` | Start working, through the Compute daemon: enter an environment you have (it keeps running when you close), or with no environment, get a temporary one of your own (destroyed when you close or it expires) | — | daemon API /sessions | durable (WorkSession) | daemon scopes + owner | Work: Open session; Home: Try software / Temporary environment | `crates/compute-cli/tests/computers.rs` |
| `compute session close` | Stop working. Only a session's own temporary environment goes with it | — | daemon API /sessions | durable | daemon scopes + owner | Work: Close | `crates/compute-cli/tests/computers.rs` |
| `compute session opened` | Your work sessions, through the Compute daemon | — | daemon API /sessions | none | daemon scopes + owner | Work: Sessions table | `crates/compute-cli/tests/computers.rs` |
| `compute policy inspect` | Show the effective policy: the baseline intersected with local configuration and --policy | — | local | none | none | none | `crates/compute-cli/tests/cli.rs`, `packages/compute-appport/src/provider.ts` |
| `compute policy validate` | Statically validate a compute.policy@1 document | — | local | none | none | none | `crates/compute-cli/tests/cli.rs` |
| `compute policy check` | Decide admission for a workload without executing it. Exits 0 when admitted and 2 when denied | — | local | none | none | none | `crates/compute-cli/tests/cli.rs` |
| `compute policy explain` | Explain every policy dimension of an admission decision | — | local | none | none | none | `crates/compute-cli/tests/cli.rs` |
| `compute explain` | Show the full decision chain for a workload: requirements, capabilities, policy, admission, and placement. Never executes | — | local / pool | none | none | none | — |
| `compute start` | Run the persistent Compute daemon (environments and services) | — | starts the daemon | process | n/a | none | `crates/compute-cli/tests/computers.rs`, `crates/compute-cli/tests/daemon.rs`, `crates/compute-cli/tests/product.rs`, `crates/compute-cli/tests/recovery.rs` |
| `compute stop` | Stop the Compute daemon. Desired state is kept | — | daemon API /shutdown, or an application | stops the daemon | admin scope | none | `crates/compute-cli/tests/computers.rs`, `crates/compute-cli/tests/daemon.rs`, `crates/compute-cli/tests/product.rs`, `crates/compute-cli/tests/recovery.rs` |
| `compute status` | Show the Compute daemon's status | — | daemon API | none | read | status chip in the header | `crates/compute-cli/tests/cli.rs`, `crates/compute-cli/tests/computers.rs`, `crates/compute-cli/tests/daemon.rs`, `crates/compute-cli/tests/isolation.rs` |
| `compute logs` | Read an application's live stdout and stderr | — | daemon API /applications | none | read | application logs tab | `crates/compute-cli/tests/product.rs` |
| `compute history` | List an application's durable execution history | — | daemon API /applications | none | read | application versions tab | `crates/compute-cli/tests/product.rs` |
| `compute rollback` | Activate a previous immutable application deployment as a new version | — | daemon API /applications | durable | deploy | application Rollback | `crates/compute-cli/tests/cli.rs`, `crates/compute-cli/tests/product.rs` |
| `compute environment list` | List environments | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | `crates/compute-cli/tests/daemon.rs` |
| `compute environment create` | Create an environment | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | `crates/compute-cli/tests/computers.rs`, `crates/compute-cli/tests/daemon.rs`, `crates/compute-cli/tests/recovery.rs`, `crates/compute-cli/tests/upgrade.rs` |
| `compute environment apply` | Create or update an environment and its projects from a manifest | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | — |
| `compute environment inspect` | — | no description | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | `crates/compute-cli/tests/daemon.rs` |
| `compute environment status` | — | no description | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | — |
| `compute environment start` | Start an environment, or `PROJECT/ENVIRONMENT` | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | — |
| `compute environment stop` | Stop an environment, or `PROJECT/ENVIRONMENT` | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | — |
| `compute environment restart` | Restart an environment, or `PROJECT/ENVIRONMENT` | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | — |
| `compute environment destroy` | Stop and delete an environment and all of its state. An environment on a computer has its computer destroyed; its record stays as evidence | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | `crates/compute-cli/tests/computers.rs` |
| `compute environment computer` | Show an environment's computer: desired against observed contents | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | `crates/compute-cli/tests/computers.rs` |
| `compute environment exec` | Run a command in the environment's computer as a durable job | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | `crates/compute-cli/tests/computers.rs` |
| `compute environment connect` | Get connection details for the environment's computer | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | — |
| `compute environment logs` | Every job the computer ran, or one process's log | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | — |
| `compute environment reconcile` | Check every process now, and retry what failed | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | — |
| `compute environment replace` | Replace the computer with one that meets new requirements. The only change that provisions a new machine | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | — |
| `compute environment repo add` | Add a repository, checked out at a revision | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | `crates/compute-cli/tests/computers.rs`, `crates/compute-cli/tests/work_mode_ui.rs` |
| `compute environment repo update` | Move a repository to another revision (or URL) | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | `crates/compute-cli/tests/computers.rs` |
| `compute environment repo pull` | Fetch a repository's revision again: a branch that moved | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | — |
| `compute environment repo remove` | — | no description | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | — |
| `compute environment package install` | Install a package by running a command (again whenever it changes) | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | — |
| `compute environment package remove` | — | no description | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | — |
| `compute environment process add` | Add (or change) a process and start it | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | `crates/compute-cli/tests/computers.rs` |
| `compute environment process start` | — | no description | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | — |
| `compute environment process stop` | — | no description | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | `crates/compute-cli/tests/computers.rs` |
| `compute environment process restart` | Restart it in place | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | — |
| `compute environment process remove` | — | no description | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | — |
| `compute environment service add` | Add (or change) it and start it | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | `crates/compute-cli/tests/computers.rs`, `crates/compute-cli/tests/work_mode_ui.rs` |
| `compute environment agent add` | Add (or change) it and start it | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | — |
| `compute environment contents show` | — | no description | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | — |
| `compute environment contents apply` | Replace the contents with a JSON document. With `--expected-generation`, only if nobody changed them since | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | — |
| `compute environment project add` | Add (or change) a project in one of the environment's repositories | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | `crates/compute-cli/tests/computers.rs`, `crates/compute-cli/tests/work_mode_ui.rs` |
| `compute environment project remove` | — | no description | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | — |
| `compute environment build` | Build a project inside the environment's computer | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | `crates/compute-cli/tests/computers.rs` |
| `compute environment test` | Test a project inside the environment's computer | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | `crates/compute-cli/tests/computers.rs` |
| `compute environment run` | Run one of a project's named commands inside the environment's computer | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | `crates/compute-cli/tests/computers.rs` |
| `compute environment release` | Release a revision of a project: the computer checks it out, builds it, and restarts what runs from it, in place. No redeployment | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | — |
| `compute environment config` | Show or change the configuration every process, build, and command sees. What depends on it restarts in place | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | — |
| `compute environment propose` | Inspect a project's source in the computer and propose how to run it: runtime, dependencies, build, tests, start command, ports, services, configuration. Nothing changes | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | — |
| `compute environment lifetime` | Change how long the environment lives, in place: kept until destroyed, or temporary | — | daemon API /environments | durable | daemon scopes; owner for computer environments | Manage and Work | `crates/compute-cli/tests/computers.rs` |
| `compute project list` | List projects: every project across environments, or the projects in one environment | — | daemon API (bundle projects on node environments) | durable | daemon scopes (no owner) | Manage: project pages (bundle) | — |
| `compute project add` | Register a revision from compute.project.toml and deploy it to an environment | — | daemon API (bundle projects on node environments) | durable | daemon scopes (no owner) | Manage: project pages (bundle) | `crates/compute-cli/tests/daemon.rs` |
| `compute project push` | Register an immutable revision without deploying it | — | daemon API (bundle projects on node environments) | durable | daemon scopes (no owner) | Manage: project pages (bundle) | — |
| `compute project revisions` | A project's registered revisions, newest first | — | daemon API (bundle projects on node environments) | durable | daemon scopes (no owner) | Manage: project pages (bundle) | — |
| `compute project remove` | — | no description | daemon API (bundle projects on node environments) | durable | daemon scopes (no owner) | Manage: project pages (bundle) | — |
| `compute project inspect` | Inspect a project in one environment, or across all of them | — | daemon API (bundle projects on node environments) | durable | daemon scopes (no owner) | Manage: project pages (bundle) | — |
| `compute project status` | — | no description | daemon API (bundle projects on node environments) | durable | daemon scopes (no owner) | Manage: project pages (bundle) | `crates/compute-cli/tests/recovery.rs`, `crates/compute-cli/tests/upgrade.rs` |
| `compute project start` | — | no description | daemon API (bundle projects on node environments) | durable | daemon scopes (no owner) | Manage: project pages (bundle) | — |
| `compute project stop` | — | no description | daemon API (bundle projects on node environments) | durable | daemon scopes (no owner) | Manage: project pages (bundle) | — |
| `compute project restart` | — | no description | daemon API (bundle projects on node environments) | durable | daemon scopes (no owner) | Manage: project pages (bundle) | — |
| `compute project executions` | Recent executions of a project in an environment | — | daemon API (bundle projects on node environments) | durable | daemon scopes (no owner) | Manage: project pages (bundle) | `crates/compute-cli/tests/recovery.rs` |
| `compute project receipts` | Receipt references of a project in an environment | — | daemon API (bundle projects on node environments) | durable | daemon scopes (no owner) | Manage: project pages (bundle) | — |
| `compute workload inspect` | — | no description | daemon API (bundle workloads on the daemon node) | durable / executes on the node | daemon scopes (no owner) | project Workloads tab | — |
| `compute workload start` | — | no description | daemon API (bundle workloads on the daemon node) | durable / executes on the node | daemon scopes (no owner) | project Workloads tab | — |
| `compute workload stop` | — | no description | daemon API (bundle workloads on the daemon node) | durable / executes on the node | daemon scopes (no owner) | project Workloads tab | — |
| `compute workload restart` | — | no description | daemon API (bundle workloads on the daemon node) | durable / executes on the node | daemon scopes (no owner) | project Workloads tab | — |
| `compute workload run` | Run a task to completion | — | daemon API (bundle workloads on the daemon node) | durable / executes on the node | daemon scopes (no owner) | project Workloads tab | `crates/compute-cli/tests/daemon.rs` |
| `compute workload logs` | Show the most recent output of a workload | — | daemon API (bundle workloads on the daemon node) | durable / executes on the node | daemon scopes (no owner) | project Workloads tab | — |
| `compute execution` | Inspect one execution recorded by the daemon | — | daemon API | none | read | none | `crates/compute-cli/tests/daemon.rs`, `crates/compute-cli/tests/remote_pool.rs`, `crates/compute-provider/tests/remote.rs` |
| `compute deploy` | Release a project revision to an environment, with zero downtime | — | daemon API: application, bundle release, or computer release/version, chosen by argument shape | durable | deploy scope | Deploy (software pages; bundle deploy wizard) | `crates/compute-cli/tests/computers.rs`, `crates/compute-cli/tests/product.rs`, `crates/compute-cli/tests/recovery.rs`, `crates/compute-cli/tests/upgrade.rs` |
| `compute promote` | Deploy the exact revision current in one environment to another | — | daemon API: bundle promotion, or computer version promotion | durable | deploy scope | Promote | `crates/compute-cli/tests/recovery.rs` |
| `compute deployment list` | Deployments, newest first | — | daemon API (bundle releases) | durable | deploy/read | deployment page (bundle) | `crates/compute-cli/tests/recovery.rs` |
| `compute deployment inspect` | — | no description | daemon API (bundle releases) | durable | deploy/read | deployment page (bundle) | `crates/compute-cli/tests/recovery.rs` |
| `compute deployment status` | Where a release is in its lifecycle | — | daemon API (bundle releases) | durable | deploy/read | deployment page (bundle) | — |
| `compute deployment rollback` | Roll a release back: before traffic moved it is abandoned; after, traffic returns to the revision it replaced | — | daemon API (bundle releases) | durable | deploy/read | deployment page (bundle) | — |
| `compute deployment receipt` | The deployment receipt (always JSON) | — | daemon API (bundle releases) | durable | deploy/read | deployment page (bundle) | `crates/compute-cli/tests/recovery.rs` |
| `compute domain add` | Route a domain to a project's service in one environment | — | daemon API | durable | operate | Domains pages | — |
| `compute domain list` | — | no description | daemon API | durable | operate | Domains pages | — |
| `compute domain inspect` | — | no description | daemon API | durable | operate | Domains pages | — |
| `compute domain status` | DNS, TLS, and routing state of each domain | — | daemon API | durable | operate | Domains pages | — |
| `compute domain remove` | Stop routing a domain and remove its DNS records and certificate | — | daemon API | durable | operate | Domains pages | — |
| `compute dns status` | Every DNS record Compute manages: desired, actual, and status | — | daemon API | durable | operate | Reconcile DNS | — |
| `compute dns reconcile` | Read every record back from its provider now and repair drift | — | daemon API | durable | operate | Reconcile DNS | — |
| `compute certificate status` | Every certificate: status, expiry, renewal. Keys are never shown | — | daemon API | durable | operate | Renew certificate | — |
| `compute certificate renew` | Renew a domain's certificate now | — | daemon API | durable | operate | Renew certificate | — |
| `compute network status` | Endpoints, ingress, and DNS providers on this node | — | daemon API | none | read | none | — |
| `compute events` | Show lifecycle events, or follow them | — | daemon API | none | read | Events page | `crates/compute-cli/tests/recovery.rs`, `crates/compute-cli/tests/upgrade.rs` |
| `compute service list` | Shared services registered with this control plane | — | daemon API | durable | operate | Services page | — |
| `compute service register` | Register a shared service and the capabilities it provides | — | daemon API | durable | operate | Services page | — |
| `compute service remove` | — | no description | daemon API | durable | operate | Services page | — |
| `compute service providers` | The daemon's provider pool as recorded in control state | — | daemon API | durable | operate | Services page | — |
| `compute control-plane provision` | Install the Compute control model in Managed FeltDB. Idempotent: an existing tenant and Compute application are reused | — | FeltDB directly | FeltDB model | FeltDB API key | none | `crates/compute-cli/tests/recovery.rs` |
| `compute control-plane upgrade` | Upgrade the configured Compute application to this build's model, before a controller that needs it runs: inspect the active model, refuse a downgrade, take and verify a FeltDB backup, apply the model, verify it, and smoke-test it | — | FeltDB directly | FeltDB model | FeltDB API key | none | — |
| `compute control-plane inspect` | Compare the active Compute model with this build's. Read-only | — | FeltDB directly | FeltDB model | FeltDB API key | none | — |
| `compute serve` | Serve compute.remote@1 with durable filesystem-backed jobs | — | runs a target (compute.remote@1) | job and session stores on disk | target credentials (--credentials; the named --insecure-unauthenticated is the only open mode) | none | `crates/compute-cli/tests/computers.rs`, `crates/compute-cli/tests/product.rs`, `crates/compute-cli/tests/remote_pool.rs`, `crates/compute-cli/tests/runtime_store.rs`, `crates/compute-cli/tests/sessions.rs` |
| `compute auth create` | Issue a credential. Its token is printed once and never again | — | daemon API | durable credentials | admin | none | — |
| `compute auth list` | List credentials: never their tokens | — | daemon API | durable credentials | admin | none | — |
| `compute auth revoke` | Revoke a credential now | — | daemon API | durable credentials | admin | none | — |
| `compute auth rotate` | Replace a credential with a new token for the same operator and scopes. The old one stops working now, or after --grace | — | daemon API | durable credentials | admin | none | — |
| `compute auth whoami` | Who the configured credential is, and what it may do | — | daemon API | durable credentials | admin | none | — |
| `compute auth audit` | The audit trail of remote operations, newest first | — | daemon API | durable credentials | admin | none | — |
| `compute node info` | Which controller runs: version, commit, build, security, runtimes | — | daemon API | node binary | admin/operate | none | `crates/compute-cli/tests/recovery.rs`, `crates/compute-cli/tests/upgrade.rs` |
| `compute node health` | Whether the controller answers, and whether its control plane is degraded. Needs no credential | — | daemon API | node binary | admin/operate | none | — |
| `compute node reconcile` | Reconcile now, fully, and show what the cycle examined and changed | — | daemon API | node binary | admin/operate | none | — |
| `compute node upgrade` | Replace the controller with another Compute build, without restarting or redeploying a workload. The previous controller is restored if the new one does not become ready | — | daemon API | node binary | admin/operate | none | `crates/compute-cli/tests/upgrade.rs` |
| `compute node rollback` | Return to the build that ran before the last upgrade | — | daemon API | node binary | admin/operate | none | `crates/compute-cli/tests/upgrade.rs` |
| `compute node upgrade-status` | The last upgrade or rollback on this node | — | daemon API | node binary | admin/operate | none | `crates/compute-cli/tests/upgrade.rs` |
| `compute supervisor` | The node's supervisor: service processes and endpoints that outlive the controller. `compute start` runs it | — | internal: started by compute start | n/a | n/a | none | — |
<!-- /audit -->

</details>

## API

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

Routes no client calls:

<!-- audit:api_orphans -->
- `GET /info` (Read)
- `GET /metrics` (Read)
- `GET /environments/{environment}/projects/{project}/status` (Read)
- `DELETE /environments/{environment}/repositories/{repository}` (Operate)
- `DELETE /environments/{environment}/packages/{package}` (Operate)
- `DELETE /environments/{environment}/processes/{process}` (Operate)
- `POST /environments/{environment}/processes/{process}/start` (Operate)
- `POST /environments/{environment}/processes/{process}/stop` (Operate)
- `GET /projects/{project}/status` (Read)
- `GET /applications` (Read)
- `GET /applications/{application}/deployments/{deployment}/receipt` (Read)
- `GET /compute/capabilities` (Read)
- `GET /compute/health` (Read)
- `GET /compute/capacity` (Read)
- `POST /compute/execute` (Execute)
- `POST /compute/admission` (Execute)
- `POST /compute/runtimes/resolve` (Execute)
- `POST /compute/runtimes/prepare` (Execute)
- `POST /compute/runtimes/status` (Execute)
- `POST /compute/jobs` (Execute)
- `GET /compute/jobs` (Read)
- `GET /compute/jobs/{job}` (Read)
- `GET /compute/jobs/{job}/result` (Read)
- `GET /compute/jobs/{job}/receipt` (Read)
- `GET /compute/jobs/{job}/logs` (Read)
- `POST /compute/jobs/{job}/cancel` (Execute)
<!-- /audit -->

The API is not versioned and has no published description (G-API-1). Every
route declares a scope; unknown routes need `admin`.

<details><summary>129 routes (from <code>compute_environment::api::ROUTES</code>)</summary>

<!-- audit:api -->
| Method | Path | Scope | UI | CLI | AppPort | Exercised over HTTP by a test |
| --- | --- | --- | --- | --- | --- | --- |
| GET | `/health` | Read | no | yes | no | yes |
| GET | `/info` | Read | no | no | no | no |
| GET | `/status` | Read | yes | yes | no | yes |
| GET | `/auth/whoami` | Read | no | yes | no | yes |
| GET | `/auth/credentials` | Admin | no | yes | no | yes |
| POST | `/auth/credentials` | Admin | no | yes | no | yes |
| POST | `/auth/credentials/{credential}/revoke` | Admin | no | yes | no | yes |
| POST | `/auth/credentials/{credential}/rotate` | Admin | no | yes | no | yes |
| GET | `/audit` | Admin | no | yes | no | yes |
| GET | `/metrics` | Read | no | no | no | no |
| POST | `/node/reconcile` | Operate | no | yes | no | no |
| GET | `/node/upgrade` | Read | no | yes | no | no |
| POST | `/node/upgrade` | Admin | no | yes | no | no |
| POST | `/node/rollback` | Admin | no | yes | no | no |
| POST | `/shutdown` | Admin | no | yes | no | yes |
| GET | `/environments` | Read | yes | yes | yes | yes |
| POST | `/environments` | Operate | yes | yes | yes | yes |
| GET | `/environments/{environment}` | Read | yes | yes | yes | yes |
| DELETE | `/environments/{environment}` | Operate | yes | yes | yes | yes |
| GET | `/environments/{environment}/status` | Read | no | no | yes | no |
| POST | `/environments/{environment}/start` | Operate | yes | no | no | yes |
| POST | `/environments/{environment}/stop` | Operate | yes | no | no | yes |
| POST | `/environments/{environment}/restart` | Operate | yes | no | no | no |
| GET | `/environments/{environment}/projects` | Read | yes | yes | yes | no |
| POST | `/environments/{environment}/projects` | Deploy | yes | yes | yes | no |
| GET | `/environments/{environment}/projects/{project}` | Read | yes | yes | yes | yes |
| DELETE | `/environments/{environment}/projects/{project}` | Deploy | yes | yes | yes | yes |
| GET | `/environments/{environment}/projects/{project}/status` | Read | no | no | no | no |
| POST | `/environments/{environment}/projects/{project}/start` | Operate | yes | no | no | no |
| POST | `/environments/{environment}/projects/{project}/stop` | Operate | yes | no | no | yes |
| POST | `/environments/{environment}/projects/{project}/restart` | Operate | yes | no | no | no |
| GET | `/environments/{environment}/projects/{project}/executions` | Read | no | yes | no | no |
| GET | `/environments/{environment}/projects/{project}/receipts` | Read | yes | yes | no | no |
| GET | `/environments/{environment}/projects/{project}/workloads/{workload}` | Read | no | yes | no | no |
| POST | `/environments/{environment}/projects/{project}/workloads/{workload}/start` | Operate | yes | no | no | no |
| POST | `/environments/{environment}/projects/{project}/workloads/{workload}/stop` | Operate | yes | no | no | no |
| POST | `/environments/{environment}/projects/{project}/workloads/{workload}/restart` | Operate | yes | no | no | no |
| POST | `/environments/{environment}/projects/{project}/workloads/{workload}/run` | Execute | yes | no | no | no |
| GET | `/environments/{environment}/projects/{project}/workloads/{workload}/logs` | Read | yes | no | no | no |
| GET | `/environments/{environment}/computer` | Read | yes | yes | yes | yes |
| POST | `/environments/{environment}/contents` | Operate | yes | yes | yes | no |
| POST | `/environments/{environment}/repositories` | Operate | no | yes | no | no |
| DELETE | `/environments/{environment}/repositories/{repository}` | Operate | no | no | no | no |
| POST | `/environments/{environment}/packages` | Operate | no | yes | no | no |
| DELETE | `/environments/{environment}/packages/{package}` | Operate | no | no | no | no |
| POST | `/environments/{environment}/processes` | Operate | no | yes | no | no |
| DELETE | `/environments/{environment}/processes/{process}` | Operate | no | no | no | no |
| POST | `/environments/{environment}/processes/{process}/start` | Operate | no | no | no | no |
| POST | `/environments/{environment}/processes/{process}/stop` | Operate | no | no | no | no |
| POST | `/environments/{environment}/reconcile` | Operate | yes | yes | yes | no |
| POST | `/environments/{environment}/replace` | Operate | yes | yes | yes | no |
| POST | `/environments/{environment}/clone` | Operate | no | yes | no | no |
| POST | `/environments/{environment}/workspace/export` | Execute | no | yes | no | no |
| POST | `/environments/{environment}/workspace/seed` | Operate | no | yes | no | no |
| POST | `/environments/{environment}/workspace/verify` | Execute | no | yes | no | no |
| POST | `/environments/{environment}/exec` | Execute | yes | yes | yes | no |
| POST | `/environments/{environment}/connect` | Execute | no | yes | yes | no |
| GET | `/environments/{environment}/jobs/{job}` | Read | yes | yes | yes | no |
| GET | `/environments/{environment}/logs` | Read | yes | yes | no | no |
| POST | `/environments/{environment}/run` | Execute | yes | yes | yes | no |
| POST | `/environments/{environment}/release` | Deploy | no | yes | yes | no |
| POST | `/environments/{environment}/config` | Operate | no | yes | yes | yes |
| POST | `/environments/{environment}/lifecycle` | Operate | no | yes | yes | no |
| POST | `/environments/{environment}/propose` | Execute | yes | yes | yes | no |
| POST | `/environments/{environment}/processes/{process}/restart` | Operate | yes | no | yes | no |
| GET | `/targets` | Read | no | yes | yes | no |
| GET | `/software` | Read | yes | no | yes | no |
| GET | `/software/{project}` | Read | yes | yes | yes | no |
| GET | `/software/{project}/versions` | Read | yes | yes | yes | no |
| POST | `/software/{project}/versions` | Deploy | yes | yes | yes | no |
| GET | `/software/{project}/versions/{version}` | Read | yes | yes | yes | no |
| POST | `/software/{project}/deploy` | Deploy | yes | yes | yes | no |
| GET | `/software/{project}/promotion` | Read | yes | yes | yes | no |
| POST | `/software/{project}/promote` | Deploy | yes | yes | yes | no |
| POST | `/software/{project}/rollback` | Deploy | yes | yes | yes | no |
| GET | `/rollouts` | Read | yes | no | no | no |
| GET | `/rollouts/{rollout}` | Read | yes | yes | yes | no |
| GET | `/sessions` | Read | yes | yes | yes | no |
| POST | `/sessions` | Operate | yes | yes | yes | no |
| GET | `/sessions/{session}` | Read | yes | yes | yes | no |
| DELETE | `/sessions/{session}` | Operate | yes | yes | yes | no |
| GET | `/projects` | Read | yes | yes | yes | no |
| GET | `/projects/{project}` | Read | yes | yes | yes | no |
| GET | `/projects/{project}/status` | Read | no | no | no | no |
| GET | `/projects/{project}/revisions` | Read | yes | yes | no | no |
| POST | `/projects/{project}/revisions` | Deploy | yes | yes | no | no |
| GET | `/deployments` | Read | yes | yes | yes | yes |
| POST | `/deployments` | Deploy | yes | yes | yes | yes |
| POST | `/deployments/promote` | Deploy | yes | yes | yes | yes |
| GET | `/deployments/{deployment}` | Read | yes | yes | yes | yes |
| GET | `/deployments/{deployment}/receipt` | Read | yes | yes | no | yes |
| POST | `/deployments/{deployment}/rollback` | Deploy | yes | yes | yes | yes |
| GET | `/domains` | Read | yes | yes | yes | yes |
| POST | `/domains` | Operate | yes | yes | yes | yes |
| GET | `/domains/{domain}` | Read | yes | yes | yes | yes |
| DELETE | `/domains/{domain}` | Operate | yes | yes | yes | yes |
| GET | `/dns` | Read | no | yes | yes | no |
| POST | `/dns/reconcile` | Operate | yes | yes | yes | yes |
| GET | `/certificates` | Read | no | yes | yes | no |
| POST | `/certificates/{domain}/renew` | Operate | yes | yes | yes | yes |
| GET | `/network` | Read | no | yes | no | no |
| GET | `/executions/{execution}` | Read | no | yes | no | no |
| GET | `/receipts/{receipt}` | Read | yes | no | no | yes |
| GET | `/events` | Read | yes | yes | no | yes |
| GET | `/events/stream` | Read | no | yes | no | yes |
| GET | `/providers` | Read | yes | yes | no | no |
| GET | `/services` | Read | yes | yes | no | no |
| GET | `/applications` | Read | no | no | no | yes |
| GET | `/applications/{application}` | Read | yes | yes | no | no |
| GET | `/applications/{application}/deployments` | Read | no | yes | no | no |
| POST | `/applications/{application}/deployments` | Deploy | no | yes | no | no |
| GET | `/applications/{application}/deployments/{deployment}` | Read | no | yes | no | no |
| GET | `/applications/{application}/deployments/{deployment}/receipt` | Read | no | no | no | yes |
| POST | `/applications/{application}/rollback` | Deploy | yes | yes | no | no |
| POST | `/applications/{application}/stop` | Operate | yes | yes | no | no |
| GET | `/applications/{application}/logs` | Read | yes | yes | no | no |
| GET | `/compute/capabilities` | Read | no | no | no | no |
| GET | `/compute/health` | Read | no | no | no | no |
| GET | `/compute/capacity` | Read | no | no | no | no |
| POST | `/compute/execute` | Execute | no | no | no | no |
| POST | `/compute/admission` | Execute | no | no | no | no |
| POST | `/compute/runtimes/resolve` | Execute | no | no | no | no |
| POST | `/compute/runtimes/prepare` | Execute | no | no | no | no |
| POST | `/compute/runtimes/status` | Execute | no | no | no | no |
| POST | `/compute/jobs` | Execute | no | no | no | no |
| GET | `/compute/jobs` | Read | no | no | no | no |
| GET | `/compute/jobs/{job}` | Read | no | no | no | no |
| GET | `/compute/jobs/{job}/result` | Read | no | no | no | no |
| GET | `/compute/jobs/{job}/receipt` | Read | no | no | no | no |
| GET | `/compute/jobs/{job}/logs` | Read | no | no | no | no |
| POST | `/compute/jobs/{job}/cancel` | Execute | no | no | no | no |
| POST | `/services` | Operate | yes | yes | no | no |
| DELETE | `/services/{service}` | Operate | yes | yes | no | no |
<!-- /audit -->

</details>

## Agents

An agent uses Compute the way a person does:

- **AppPort** (`packages/compute-appport`) has a function for every UI
  operation (52 routes), typed, and follows operations to their end. Its
  tests run against a stub daemon, not a real one.
- **The CLI** takes `--json` on the commands an agent needs, and exits
  non-zero with the failure kind.
- **Identity**: an agent holds an operator credential with scopes. There is
  no agent-specific identity, delegation, budget, or expiry tied to an owner
  (G-AGENT-1). An agent with `deploy` scope that owns both environments can
  promote to production.
- **Agent processes** can be run inside a computer (process kind `agent`;
  screenshot 25), with the computer's configuration as environment
  variables — which includes anything secret put there (SEC-5).

Everything an agent needs to reason about Compute's state is in the API, and
failures carry a `kind` ([architecture.md](architecture.md#failure-kinds)).
What an agent cannot learn from the API: whether a target is authenticated,
whether a computer's machine still exists, and whether a feature a target
advertises works.
