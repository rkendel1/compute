# GitHub runner ↔ service protocol map

**Status:** map for design purposes. Nothing here is implemented.
Companion to [factory-control-plane.md](factory-control-plane.md).

## Evidence quality

This map has three tiers of evidence. Do not read them as equal.

- **Observed**: an endpoint Local CI's emulation serves and a real runner
  (Local CI pins release v2.331.0 for its macOS path,
  `packages/cli/src/runner/macos-vm/runner-binary.ts:10`; the Linux image
  version is not pinned in the sources read) was seen to use, cited by file
  and line in the Local CI clone at `c8597bb`.
- **Inferred**: what Local CI's *comments* say the runner does (for example
  `routes/actions/index.ts:813`, `:936`).
- **Unverified**: anything about `actions/runner` source, newer runner
  releases, or github.com's production service that was not read in this
  evaluation. `actions/runner` was **not** cloned. Two specific risks:
  1. Recent runners talk to a separate *broker* / *run service* for listening
     and job acquisition on github.com. Local CI's emulation serves the
     older `_apis/distributedtask/pools/…/messages` flow and found no
     `broker`/`runservice` strings in its tree (search returned none). Whether
     a current runner accepts the older flow against a self-hosted
     server-URL depends on the release; the proof of concept must **pin a
     runner version** and treat upgrading as a deliberate, tested change.
  2. Artifact and cache protocols have version generations (Twirp v4 and
     legacy; cache v1 and v2). Local CI serves v4 artifacts and the v1 cache
     REST shape.

## The interaction sequence (observed in the emulation)

```
 register ─▶ create session ─▶ long-poll messages ─▶ (job message) ─▶ run
                                                        │
                    renew lock ◀──── every ≤60 s ───────┤
                    timelines / logs / feed / outputs ◀─┤  while running
                    artifacts / cache / action tarballs ◀┤
                    finish job request (result) ────────┘
```

| # | Phase | Runner calls | Emulation evidence | Runner needs | Class |
| --- | --- | --- | --- | --- | --- |
| 1 | Registration token | `POST /repos/{o}/{r}/actions/runners/registration-token` (and org/enterprise forms) | `routes/github.ts:92,106` | A token the config step will accept | GH |
| 2 | Runner registration | `POST /actions/runner-registration` (tenant URL + token exchange) | `routes/github.ts:142-143` | Tenant `url` and an access token | GH |
| 3 | Credentials | OAuth client-credentials at `…/_apis/oauth2/token`, RSA params | credential files at `container-config.ts:182` | RSA key pair and token endpoint | RUN |
| 4 | Pool and agent | Pools, agents create/update, session create/delete | `routes/actions/index.ts` (~455–600) | Agent id, pool id, session id | GH |
| 5 | Wait for work | `GET /_apis/distributedtask/pools/:poolId/messages?sessionId=` (long poll, ~20 s then 204) | `index.ts:~560-620`, `dtu.ts:101-149` | A 200 with a message or a 204 | GH |
| 6 | Job message | `MessageType: PipelineAgentJobRequest`, body = plan, timeline, steps, variables, contexts, repositories, endpoints, workspace, mask hints, env tokens | `routes/actions/generators.ts:271-349`, `types.ts:79` | A fully resolved job | GH |
| 7 | Lock renewal / finish | `PATCH /_apis/distributedtask/jobrequests` | `index.ts` (jobrequests handler) | lock `now+60 s`; on finish `result`, `finishTime` | GH |
| 8 | Timeline | `PATCH/POST …/timelines/:id/records`, `GET …/timelines/:id?includeRecords` | `index.ts:704-839` | Ability to read back records (or the job defaults to Failed) | GH |
| 9 | Logs | `POST …/plans/:plan/logs` (create; response must carry `path`), `POST …/logs/:id/lines`, `POST …/timelines/:t/records/:r/feed` | `index.ts:932-1096` | 201 with `id`, `path`, `createdOn` | GH |
| 10 | Step outputs | `POST …/hubs/:hub/plans/:plan/outputs` | `index.ts:842` | 200 | GH |
| 11 | Action download info | `POST …/plans/:plan/actiondownloadinfo` | `index.ts:881-923` | tarball URLs for `uses:` actions | GH |
| 12 | Repository checkout | `actions/checkout` calls REST/git against the server URL and `GITHUB_SERVER_URL` | Local CI replaces `git` with a shim (`local-job.ts:777`) rather than serving git | Either a real git source or a shim | GH + DEV |
| 13 | Artifacts | Twirp `CreateArtifact` / `FinalizeArtifact` / `ListArtifacts` / `GetSignedArtifactURL` + blob PUT/GET; legacy `_apis/artifacts*` | `routes/artifacts.ts` | Signed URLs and blob endpoints reachable from the runner | GH |
| 14 | Cache | `_apis/artifactcache/{caches,cache,artifacts}` | `routes/cache.ts:90-247` | key/restore-key lookup, reserve, chunk upload, commit, download URL | GH |
| 15 | OIDC / job tokens | not exercised | Local CI issues a mock JWT (`generators.ts:272`) | `ACTIONS_RUNTIME_TOKEN`-class token for artifact/cache calls | GH (must be real in Factory) |

## What a *real* runner needs from its host (execution side)

None of this is GitHub-specific. All of it is ordinary execution:

| Need | Local CI's answer | Compute today |
| --- | --- | --- |
| A long-running process, restartable per job | `run.sh --once` in a container (`container-config.ts:224`) | `ProcessSpec` under a Computer with restart policy (`docs/computers.md`); or one job |
| Env vars for the runner (server URL, repo, sha) | container `Env` (`buildContainerEnv`) | `SessionCommand.env` (`sessions.rs:498`) and process env |
| A writable work dir, persistent for the job | bind-mounted `_work` | session workspace; **no per-job workspace identity** |
| Network path runner → server (Factory) | `host.docker.internal` etc. | `compute-network`; **no declared "reach Factory" requirement** |
| Network path to the internet (action tarballs, registries) | host network | `NetworkPolicy` on the job (`jobs.rs:227`) |
| Tool cache (`/opt/hostedtoolcache`) | bind mount | none; would be a `DependencyCapsule`-like input |
| Container / docker-in-docker | docker.sock mount | **not portable; non-goal** |
| A user/uid the runner accepts | runner image `runner` user (uid 1001) | provider-dependent |
| Log capture | Docker log follow | job logs are fetched whole when complete (`jobs.rs:339`); **no follow** |
| Cancellation | container kill | `POST /compute/jobs/{id}/cancel` (`compute-provider/src/lib.rs:1780`) |

## Emulation burden (what Factory would have to serve)

Counting routes in the Local CI emulation (`grep` of `app.get|post|patch|put`
in `routes/`): 57 registrations, of which 3 are `/_dtu/*` control routes and
1 is a catch-all 404, so about 53 protocol handlers. Of those, the ones a **first proof of concept** needs are rows 1–11 and 13 (log,
timeline, finish); artifacts (13), cache (14) and action tarballs (11) can be
deferred by choosing a workflow that uses only `run:` steps. Everything in
row 12 disappears if the job is a `run:`-only workflow whose workspace is
seeded by Compute (see PoC in the primary doc).

## What is genuinely GitHub-specific emulation

1. The **shape** of every message above (Azure DevOps-lineage "distributed
   task" API: plans, timelines, records).
2. Token semantics (registration token, runtime token, OIDC).
3. `GITHUB_*` context construction (`toContextData`).
4. Artifact/cache wire protocols.
5. Concurrency/`needs` scheduling, which the *service* does in production.

None of these belong in Compute.
