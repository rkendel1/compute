# Gap analysis

What stands between Compute as built (`69b70d9`, audited 2026-09-27) and
the complete product: one control plane that turns any machine into a
durable computer, runs any project on it, and takes software from
development to production. Every gap names its evidence in
[audit.md](audit.md) / [audit.json](audit.json) (key `gaps`); the sections
below are generated from it.

Severity reads from the order: architecture and security gaps first,
because every later capability inherits them.

## Index

<!-- audit:gap_index -->
| Gap | Area | Status | Current |
| --- | --- | --- | --- |
| G-ARCH-1 | Core architecture | closed | Closed: targets authenticate every request with a credential they issued; the daemon presents one; sessions belong to the control plane's identity (experiments.json#foundation, SEC-1, SEC-2). |
| G-ARCH-2 | Core architecture | open | Three deployment models: applications, bundle projects (node environments), computer versions/rollouts. |
| G-ARCH-3 | Core architecture | closed | Closed: FeltDB is the production authority; the file backend remains for local development and says so everywhere (launcher output, /info, `compute status`, `compute node info`: durability local-development). |
| G-ARCH-4 | Core architecture | closed | Closed: every running computer is confirmed with its target; unreachable and lost are durable, evented, fenced observed states that keep desired state (experiments.json#foundation). |
| G-RT-1 | Runtime support | open | Computers are workspaces (native processes) or unverified containers. |
| G-RT-2 | Runtime support | open | Target features describe the host (`containers` = docker on PATH), not the computer. |
| G-PROV-1 | Providers | open | No Fly/Railway/Render/cloud/bare-metal provisioning; targets must already run `compute serve`. |
| G-DISC-1 | Placement | open | Targets are configured in a pool file; network, GPU model, nested virtualization are not discovered. |
| G-PLACE-1 | Placement | open | persistent_storage, public_endpoint, terminal are requestable (UI checkboxes) but offered by no provider. |
| G-EXEC-1 | Execution | open | Bundle workloads and applications execute on the daemon host. |
| G-EXEC-2 | Execution | open | No cancellation or timeout controls in the UI; jobs have timeouts in the API. |
| G-PROJ-1 | Projects | open | Local folders must be Git repositories; no upload. |
| G-APP-1 | Applications | open | Endpoints are target-host:port; no domains, TLS, or ingress for computer applications. |
| G-SVC-1 | Services | open | Database/Redis are command templates that assume binaries on the host. |
| G-DEP-1 | Deployment | open | A release restarts processes (downtime); bundle releases have zero-downtime switching. |
| G-REL-1 | Releases | open | A version is a commit and a digest; no artifact is kept. |
| G-PROD-1 | Production | open | No approvals, no protected environments, no deploy freezes. |
| G-UI-1 | UI | open | Targets, credentials, audit, node upgrades, runtimes, placement explanation are CLI-only. |
| G-UI-2 | UI | closed | Closed: the certification is fixed for the action home and runs in CI with Chromium, with a computer-reality journey (.github/workflows/test.yml). |
| G-CLI-1 | CLI | open | 52 commands have broken or missing help; `compute session` mixes target sessions and work sessions. |
| G-API-1 | API | open | No versioning of the Compute API; routes without any client (/info, /metrics, …). |
| G-AGENT-1 | Agents | open | Agents are operators; no delegation, budgets, or per-agent audit identity. |
| G-SEC-1 | Security | open | See SEC-1…SEC-6. |
| G-OBS-1 | Observability | open | Process logs are read on demand; no log streaming, metrics, or traces for computers in the UI. |
| G-DOC-1 | Documentation | open | README and getting-started lead with a command that fails by default; daemon.md describes an old UI. |
| G-TEST-1 | Testing | open | 87 CLI commands are never invoked by a test; the container provider has no real test (target auth and machine loss now do). |
| G-PERF-1 | Performance | open | Every event re-renders and refetches the whole page; /software fans out per environment. |
<!-- /audit -->

## Gaps by area

<!-- audit:gaps -->
### Core architecture

**G-ARCH-1** (closed)

- Was: Targets accept any caller (AllowAllAuthorizer); the daemon authenticates to targets with nothing.
- Desired: Targets trust only their control plane (a credential or mTLS), and sessions belong to the daemon's identity.
- Impact: Anyone who reaches a target controls every computer on it; the daemon is not actually the authority.
- Evidence: SEC-1, SEC-2
- Next: Closed: targets authenticate every request with a credential they issued; the daemon presents one; sessions belong to the control plane's identity (experiments.json#foundation, SEC-1, SEC-2).

**G-ARCH-2**

- Current: Three deployment models: applications, bundle projects (node environments), computer versions/rollouts.
- Desired: One: versions reconciled into an environment's computer.
- Impact: Three vocabularies, three code paths, and work that still runs on the daemon host.
- Evidence: models, execution_paths
- Next: Decide the fate of node environments and applications: port their features (zero-downtime switch, ingress, domains) to computers, then retire or wrap them.

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

- Current: Bundle workloads and applications execute on the daemon host.
- Desired: The daemon coordinates; computers execute.
- Impact: The daemon is both coordinator and executor.
- Evidence: execution_paths
- Next: Covered by G-ARCH-2.

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

## Order

The gaps depend on each other. The path that respects the dependencies:

<!-- audit:backlog -->
1. **FOUNDATION (done)**
   - Done: authenticate targets; the daemon holds the credential (G-ARCH-1)
   - Done: detect machine loss and unreachable targets (G-ARCH-4)
   - Done: decide the durable-state default (G-ARCH-3)
   - Done: browser tests and the UI certification in CI; fix the home-route regression (G-UI-2)
2. **EXECUTION**
   - One deployment model: retire or port node environments and applications (G-ARCH-2, G-EXEC-1)
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

Why this order: authenticated targets and loss detection (FOUNDATION) are
preconditions for trusting anything a target reports; one deployment model
(EXECUTION) removes the work that still runs on the daemon host before more
features are built on either model; isolation (RUNTIME COVERAGE) is the
precondition for managed services, volumes, and multi-tenant computers;
release artifacts, zero-downtime rollouts, and ingress are needed before
production is more than an environment's name.
