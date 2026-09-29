# Local CI audit

**Status:** evaluation only. Nothing here is implemented. Companion to
[factory-control-plane.md](factory-control-plane.md).

Subject: [redwoodjs/local-ci](https://github.com/redwoodjs/local-ci) at commit
`c8597bb` (successor of Agent CI), read from a shallow clone. Paths below are
relative to that repository. A Rust rewrite (`crates/local-ci*`) is in
progress beside the TypeScript implementation; this audit reads the
TypeScript one (`packages/cli`, `packages/dtu-github-actions`) and cites the
Rust crates only where noted.

## What it is, in one paragraph

Local CI does **not** reimplement the GitHub Actions runner. It runs the
**unmodified official runner** (`./run.sh --once`,
`packages/cli/src/docker/container-config.ts:224`) inside a Docker container
and replaces GitHub's *server* with a local emulation, the "DTU"
(`packages/dtu-github-actions`). The runner is told, through pre-written
`.runner` / `.credentials` files, that its server is `http://<host>:<port>`
(`container-config.ts:182`). The DTU answers the runner's protocol calls
(sessions, message long-poll, job lock renewal, timelines, logs, outputs,
artifacts, cache, action tarballs). The CLI parses the workflow itself
(`packages/cli/src/workflow/workflow-parser.ts`), builds a job, seeds it into
the DTU (`POST /_dtu/seed`, `local-job.ts:753`), starts the container, and
watches. There is no scheduler and no multi-tenancy: one CLI process, one
DTU, one container per job.

## Audit table

Classification key: **GH** GitHub-specific · **RUN** runner-specific ·
**CP** general control plane · **EXEC** execution · **DEV** local-dev
convenience.

| # | Area | What Local CI does (evidence) | Class |
| --- | --- | --- | --- |
| 1 | Runner interaction | Official runner binary, unmodified; launched as `./run.sh --once` with an ephemeral `.runner` config (`"ephemeral":true`, `container-config.ts:182,224`). A second path (`crates/local-ci-runtime/src/macos_vm`) downloads `actions-runner-osx-arm64` v2.331.0 into a tart VM | RUN |
| 2 | Control-plane APIs | Route surface of the DTU: `_apis/distributedtask/*` (pools, agents, sessions, messages, jobrequests, timelines, logs, feed, outputs, actiondownloadinfo), `_apis/artifactcache/*`, Twirp `…/CreateArtifact` etc. + `_apis/artifactblob/*`, and a few `repos/{o}/{r}/*` REST endpoints (`routes/actions/index.ts`, `routes/artifacts.ts`, `routes/cache.ts`, `routes/github.ts`) | GH |
| 3 | Registration | Registration is faked: token is the constant `mock_local_token` (`local-job.ts:772`); `POST …/actions/runners/registration-token` and `POST /actions/runner-registration` return canned data (`routes/github.ts:92-143`); credentials/RSA params are pre-baked into the container command (`container-config.ts:182`); agent + session creation handled in `routes/actions/index.ts` (~455–600) | GH + RUN |
| 4 | Job acquisition / dispatch | Push-into-long-poll. `GET …/pools/:poolId/messages?sessionId=` parks the request (`state.pendingPolls`); `POST /_dtu/seed` writes the job into `state.runnerJobs[runnerName]` and answers the parked poll with a `PipelineAgentJobRequest` (`routes/dtu.ts:69-168`). Jobs are pinned to a runner by name to stop cross-runner theft (`local-job.ts:747`). Poll times out with 204 after ~20 s | GH (protocol) / CP (routing) |
| 5 | Job description | `createJobResponse` synthesizes the whole `PipelineAgentJobRequest`: plan, timeline, steps (with fresh ids), variables, `github` context, `env`, repository resource, workspace path, and a **mock JWT** as the `SystemVssConnection` OAuth token (`routes/actions/generators.ts:271-349`). `steps`, `needs`, `strategy`, `matrix` contexts are left empty (`generators.ts:260-264`) | GH |
| 6 | Lifecycle | queued → (poll answered) → runner executes → `PATCH …/jobrequests` renews lock (`lockedUntil = now+60s`) or finishes with `result`/`finishTime`. There is no server-side state machine beyond "seeded / dispatched / finished"; the CLI derives display state from `timeline.json` (`local-job.ts:1025-1032`) | GH + CP |
| 7 | Logs / events | Runner posts log create (`POST …/logs`, `index.ts:932`), lines (`…/logs/:id/lines`, `:948`), and timeline feed (`…/records/:id/feed`, `:1073`). DTU strips `##[group]`, `[command]`, and runner-internal lines and writes per-step `steps/<name>.log` (`:964-1069`). Timeline records merged by id/order into `timeline.json` (`:704-805`). No event stream to consumers; the CLI polls the file every 100 ms | GH (wire) / DEV (files) |
| 8 | Outputs | `POST …/plans/:planId/outputs` flattened into `outputs.json` (`:842-878`); custom `::local-ci-output::k=v` lines for cross-job passing (`:992`) | GH + DEV |
| 9 | Artifacts | Twirp artifact API (v4) and legacy `_apis/artifacts` (v1–3) plus blob upload/download, stored on the local disk (`routes/artifacts.ts`, 11 routes) | GH |
| 10 | Cache | `actions/cache` REST protocol implemented (`routes/cache.ts:90-247`: lookup, reserve, chunked PATCH upload, commit, download) with local storage; additionally *bind-mounted* package-manager stores and "virtual cache patterns" that skip tar entirely (`dtu.ts:197`, `container-config.ts:150-161`) | GH (protocol) / DEV (bind mounts) |
| 11 | Workspace | Host directory prepared per run (rsync of the repo at `headSha`, `prepareWorkspace`, `local-job.ts:783`), bind-mounted as `_work`; a git **shim** replaces `/usr/bin/git` so checkouts resolve locally (`local-job.ts:777`, `container-config.ts:192`); `node_modules` restored from a lockfile-keyed snapshot (`local-job.ts:793`) | EXEC + DEV |
| 12 | Actions resolution | `actiondownloadinfo` answered with a local proxy URL that caches tarballs from github.com (`index.ts:881-923`); `resolvedSha` is a hash of `name@ref`, **not** the real SHA | GH + DEV |
| 13 | Persistence | `store.ts` is process memory. On disk: logs, `timeline.json`, `outputs.json`, caches. A DTU restart loses queued jobs, sessions, and runner registrations | — |
| 14 | Failure / retry | Steps are wrapped so a failure writes a `paused` signal file and waits for a `retry` file (`local-job.ts:744`, `:974`, `:1001`); the container stays alive; the CLI (`retry` command, Enter key) rewrites the workspace (`syncWorkspaceForRetry`) and drops `retry`. Retry is *step-level, in the same container*, not a new job | DEV (**the interesting idea**) |
| 15 | Process / container lifecycle | `dockerode` `createContainer` → `start` → follow logs → `wait` (`local-job.ts:934-1064`); labelled `local-ci.pid`; stale-container reaping by dead-PID label (`docker/shutdown.ts`); `SIGINT` handler cleans up (`:728`); service containers on a private network (`docker/service-containers.ts`) | EXEC |
| 16 | Pause / resume | Only the failure pause above. No suspend of a running job, no checkpoint | DEV |
| 17 | Networking | Runner reaches the DTU via `host.docker.internal` / bridge gateway / `host-gateway` (`container-config.ts:236-239`); services on a user network; optional docker.sock bind mount (`:150`) | EXEC |
| 18 | Auth assumptions | None real. A mock JWT, a constant registration token, an OAuth endpoint that accepts anything; DTU listens on `0.0.0.0` (`container-config.ts:174`); `/_dtu/*` control routes guarded by a header and log paths validated under an allowed root (`dtu.ts:40-65`) | — (not a model to adopt) |
| 19 | GitHub API compat surface | Only what the official runner and common actions touch: `compare`, `commits/:sha/pulls`, `tarball/:ref`, `installation`, `access_tokens`, `actions/jobs/:id` (`routes/github.ts`). Everything else 404s (`index.ts:1099`) | GH |
| 20 | `runs-on` handling | Ignored except for classification: `macos*`/`windows*` → skipped with a warning (or a tart VM), everything else, including custom self-hosted labels, lands in the same Linux container (`runner/runs-on-compat.ts:30-47`) | GH |
| 21 | Workflow semantics | Parsed **locally** in the CLI (matrix, needs, `if`, reusable workflows, services) rather than by the runner | GH |
| 22 | Concurrency | Many runners/jobs against one DTU are keyed by runner name and plan id; state maps `sessionToRunner`, `planToLogDir`, `timelineToLogDir` are the multiplexing (`store.ts`) | CP (single-process) |

## What Local CI teaches

1. **The unmodified runner can be pointed at an emulated server.** That is
   the load-bearing result. It is real and shipping. It is *also* evidence of
   how much surface must be emulated: 57 route registrations (3 of them `/_dtu/*` control routes, one a catch-all 404), several of them
   sensitive to details (a `null` `path` in a log response crashes the
   runner; a missing `GET timelines/:id` makes the runner default the job to
   Failed — comments at `index.ts:813`, `:936`).
2. **The runner is only the *step interpreter*.** Everything an execution
   substrate must provide is: a process environment, a workspace, a network
   route to the server, and time.
3. **Job description and workflow semantics are separable.** The runner does
   not read YAML; it receives a fully resolved job message. Whoever builds
   that message owns workflow semantics.
4. **Pause-on-failure with retry in place is what people actually want
   locally**, and it needs exactly one primitive: keep the machine and
   workspace alive, let a human or controller repair it, run the failed step
   again.
5. **What it does not have** is anything Compute is for: durability,
   identity, placement, admission, evidence, authorization, multi-node
   operation. Its "control plane" is in-memory maps.

## What should not be copied

- Mock JWTs and open registration: Factory must issue real, scoped,
  short-lived credentials (see security in the primary doc).
- File-based signalling (`paused`, `retry`) and 100 ms file polling as a
  status transport.
- Host bind-mount caches as *the* cache design. They are a single-host
  optimization; Compute's answer is content-addressed `DependencyCapsule`s
  (compute.deps@1) and artifacts.
- A local `resolvedSha` fiction and stripped logs: Factory's GitHub view must
  be reconstructible from evidence, not edited.

## Mapping: Local CI concept → Compute concept

"Equivalent?" is judged against code that exists today in Compute. A gap is
listed whenever the answer is not a plain yes.

| Local CI concept | Compute concept (today) | Equivalent? | Gap | Owner of the gap |
| --- | --- | --- | --- | --- |
| Runner (registered agent) | None. A **Session** is a machine handle (`compute-core/src/sessions.rs`), an **Environment/Computer** is a durable machine with declared processes (`compute-state/src/model.rs:259,289`) | No. A runner is a *GitHub-visible identity that polls for work*; Compute has no such thing | The runner process is a workload; its GitHub identity is Factory's | Factory (identity), Compute (process supervision, already exists: `ProcessSpec`) |
| Runner environment (image, tools) | Environment desired contents (repos, packages, processes) + `DependencyCapsule` + optional container provider image | Partial | No notion of "runner image" as a first-class, digest-pinned input; tool cache | Compute (input identity), Factory (which image) |
| Job execution | `ExecutionJob` (`compute-core/src/jobs.rs:251`): durable id, status, receipt, cancellation | Yes for *one command*. A GH job is many steps under one runner process | See "job = one long-lived runner process" in the primary doc | Compute (durability), Factory (steps) |
| Workspace | Session workspace / Computer workspace; jobs never own it (`docs/persistent-environments.md`) | Yes | No per-job clean workspace guarantee, no `workspace_id` on a job | Compute |
| Job events | `JobEvent {sequence, type, timestamp}` (`compute-provider/src/jobs.rs:63`), `SessionEvent` (`sessions.rs:572`), environment event bus `/events`, `/events/stream` | Partial: events exist but a job event has only a type and time, no payload/step/log reference | Step/timeline records are not Compute events | Factory derives from runner callbacks; Compute records the *process*-level events |
| Job result | `JobResult` + `ExecutionReceipt` (`jobs.rs:302,332`) | Yes for process exit. GH `conclusion` (success/failure/cancelled/skipped/neutral) is richer | Mapping is Factory's | Factory |
| Runner labels | `PlacementRequirements` + closed `TARGET_FEATURES` + `SessionCapabilities` | **No.** Labels are free-form strings; Compute's capabilities are a closed, named set that errors on unknown names | Translation, not equivalence (see primary doc §6) | Factory (label policy), Compute (placement) |
| GitHub job dispatch | None. `POST /compute/jobs` submits *work*, never *waits for* work | No | Compute must not gain a poll-for-work queue for GitHub | Factory |
| Workflow semantics (`needs`, `if`, matrix, reusable) | None | No | none in Compute, deliberately | Factory |
| GitHub API compatibility | None | No | | Factory |
| Artifacts | `JobArtifact {digest,size,data}` returned inline, capped by the request limit (`jobs.rs:310`); `ArtifactStore` | Partial: digest-verified, but inline, not streamed, and not addressable across jobs by name | Large / streamed artifacts; naming is Factory's | Compute (bytes + digest), Factory (name/scope/retention) |
| Actions cache | `DependencyCapsule` (compute.deps@1) is *dependency-shaped*, not key/restore-key-shaped | No | A GH cache key→blob store | Factory owns the key namespace; bytes go to an artifact-like Compute store **only if proven necessary** |
| Retry | `retry` policy for processes (`docs/computers.md`); no job retry; job reruns are new jobs | No | Step-level retry in place needs a live session | Compute (session stays), Factory (decides) |
| Pause / resume on failure | Session `stop`/`resume` keep the disk, not processes (`docs/persistent-environments.md`); no "hold on failure" | No | Hold-on-failure policy (session not destroyed) | Compute (minimum primitive: keep the session; see primary §7) |
| Long-poll message queue | none | n/a | Factory-only | Factory |
| Timeline / log endpoints | `GET /compute/jobs/{id}/logs` returns full stdout+stderr with a `complete` flag (`jobs.rs:339`); **no streaming** | No | Live log delivery | Compute (a followable log), Factory (GH shape) |
| Service containers | Environment processes / container provider | Partial | Sidecar networking between job and services | Compute (network), Factory (declaration) |
| docker.sock mount | none; deliberately not portable | No | Non-goal | — |
| Persistence of runner/job state | FeltDB via `compute-state` | Yes | Factory needs its own FeltDB collections | Factory (uses FeltDB directly, not through Compute) |
