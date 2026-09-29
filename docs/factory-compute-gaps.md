# Compute gaps for a GitHub control plane, and how to close them

**Status:** analysis. Nothing here is implemented. Companion to
[factory-control-plane.md](factory-control-plane.md).

Test applied to every gap: *would this primitive be justified if GitHub did
not exist?* A gap that fails the test does not go in Compute; it stays in
Factory.

## Gaps

| ID | Gap | Evidence in code | Generic justification | Proposed shape | Phase |
| --- | --- | --- | --- | --- | --- |
| G1 | A job has no workspace identity; jobs never own the workspace | `docs/persistent-environments.md` ("jobs never own the workspace"); `ExecutionJob` has `session_id` but no workspace field (`compute-core/src/jobs.rs:251-289`) | Any repeated work in one session wants a clean or named directory | Optional `workspace` name on a session command; default unchanged | Compute 1 |
| G2 | Session exec is argv + env + timeout only: no working directory, stdin, or streaming | `SessionCommand` (`compute-core/src/sessions.rs:491-505`) | Any tool needing a cwd | Optional `cwd` (validated inside the workspace) | Compute 1 |
| G3 | Job logs returned whole, with `complete` flag; no follow | `JobLogs` (`jobs.rs:339`), client `get(".../logs")` (`compute-provider/src/lib.rs:1897`) | Every long job wants live output; `environment` events already have `/events/stream` | `GET .../logs?after=<offset>` (offset-addressed, resumable) | Compute 1 |
| G4 | Artifacts inline in one response, not streamed | `JobArtifact.data: Vec<u8>` (`jobs.rs:310-321`) | Large outputs | Range/streamed artifact read, digest verified on the client | Later |
| G5 | Nothing keeps a failed execution's machine except explicit `claim` | `SessionCapabilities.claim` (`sessions.rs:122-132`); ephemeral TTL expiry (`docs/sessions.md`) | Debuggability | None in v1 (use `claim`); revisit if racing expiry is observed | Only if needed |
| G6 | A large, pinned tool distribution as input | Capsules travel in requests capped at 64 MiB (`DEFAULT_MAX_REQUEST_BYTES`, `compute-provider/src/lib.rs:55`; `docs/stacks.md:178`); capsule creation refuses symlinks (`docs/dependencies.md`) | Any big toolchain | Verify runner tarball against both limits **before** designing; `--deps-by-reference` exists | Before Compute 1 |
| G7 | Network policy has no per-destination allow | `NetworkPolicy` on the job (`jobs.rs:227-231`) | Least-privilege egress | Allow-list in the policy | Compute 2 |
| G8 | Job events are `{sequence, type, timestamp}` only | `JobEvent` (`compute-provider/src/jobs.rs:63-69`) | Richer inspection | Optional `detail`, as `SessionEvent` has (`sessions.rs:572-582`) | Optional |
| G9 | No provider-level disclosure of max job/session lifetime relevant to long jobs | `docs/compute-capabilities.md#max-lifetime` (design) | Long-running work on ceilinged providers | as designed there | Later |
| G10 | No job retry concept; a rerun is a new job | jobs are immutable submissions (`JobSubmission`, `jobs.rs:293`) | Fine as is | none; Factory issues a new job with a new correlation attempt number | — |

## Things that are **not** gaps (already sufficient)

- Durable job identity, status, cancellation, receipt, by id after client
  loss (`docs/session-architecture.md`).
- Idempotent submission (`idempotency_key`, `compute-provider/src/lib.rs:1694`).
- Placement refusal with named reasons (`ReasonCode`).
- Hold a machine after failure (`claim`) and inspect it (session exec/logs).
- Per-operation authorization (`ProviderOperation`).

## Things deliberately rejected as gaps

| Candidate | Why rejected |
| --- | --- |
| A poll-for-work queue in Compute | GitHub dispatch semantics; Factory owns it |
| Runner registration / runner objects | GitHub identity; Factory record |
| Label matching in placement | Second capability model; labels are translated before the boundary |
| Steps in receipts | Model C; runner-reported step evidence stays in Factory |
| A GitHub cache store | GitHub-specific persistent storage; only justified by measurement |
| docker.sock passthrough | Not portable, breaks isolation |
| VM/process snapshots for step retry | Not assumed; see `checkpoint-fork-design.md` (filesystem only) |

## Ordered plan

1. **Verify G6 first** (one afternoon, no code): download the pinned runner
   release, measure size, list symlinks, decide capsule vs by-reference vs
   another input form. If it does not fit, this changes step 2.
2. **Compute 1** (three small PRs, generic): G2 → G1 → G3, each with its own
   conformance test and no GitHub vocabulary.
3. **Factory core** on `compute.remote@1` with a shell job.
4. **Label policy** and refusal reasons.
5. **Runner-service emulation, minimum** (protocol rows 1–10), against a
   **pinned** runner. Decide broker/run-service vs older flow with a spike.
6. **Durability drills**: kill Factory, kill runner, kill Compute node.
7. **Security**: delegation grants, per-job creds, isolation mapping.
8. **Observability**: consistency checker between receipt and runner report.
9. **Artifacts (G4), `uses:` actions, cache decision.**
10. **Validation** with real repositories' workflows, `run:`-only first.

## Test strategy

| Layer | Tests |
| --- | --- |
| Compute generic | conformance tests for cwd, workspace identity, log follow (offset resume after disconnect) |
| Factory core | state machine derivation table: every Compute `JobStatus` × runner report → GitHub status |
| Label policy | table-driven: label set → requirements or refusal reason; unknown label refused |
| Emulation | replay recorded runner traffic against Factory (record with a real runner once) |
| Durability | restart Factory between each protocol phase |
| Security | receipts/events/logs scanned for credential values (same test style as stack credential tests) |
| Integration | the PoC in the primary doc |

## Unknowns that could change the design

1. Whether the current official runner accepts the legacy message-poll flow
   against a custom server URL, or requires the broker/run-service flow.
2. Runner distribution size and shape versus capsule limits.
3. Whether the runner's just-in-time config can carry Factory-issued
   credentials.
4. What Factory's existing execution boundary does today that must survive
   the change of ownership (needs the Factory owner).
5. Whether Compute's provider set includes any target that can host the
   runner's expectations (uid, writable home, outbound TLS to Factory).
