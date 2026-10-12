# Attn → Compute-configured → FX → AppPort: dependency and authority map

Status: **audit only, not a certification.** Sources are file reads of shallow
clones taken 2026-10-12 (attn, chip-rs, 3fx, pax, compute). Nothing here was
run except the Compute-side `npm run verify` noted below. `appport`,
`appport-services` and `flow_db` (FeltDB) were not cloned; claims about them
come from Compute's pins and docs only.

## Who owns what (as the code stands)

| Concern | Owner | Interface actually present |
|---|---|---|
| Intent, oversight, status UI | Attn | Web UI → REST adapter `/api/*` (`appport/http-api.ts`). The UI does not call AppPort capabilities. |
| Attn durable state | FeltDB via `attn.flow` | Collections `attn_work`, `attn_work_agency`, `attn_work_evidence`, `attn_work_dispatch`. Phase is derived, not stored. |
| Agency | npm Chip (`@appport/chip` 0.54.4) | `POST /eve/v1/session`, `GET /eve/v1/session/:id/stream` (NDJSON). Attn's only agency dependency. |
| Model call | FX (`@appport/fx` 0.1.2) | `createFxModel`, `createFxAgent({model})`; OpenAI-compatible `/chat/completions`; no gateway key. Reached from Chip through `@appport/chip/models/fx`. |
| Tools for FX agents | FX MCP adapter | `createMcpAdapter(client, …)` from `@appport/fx/mcp`. |
| Execution, durable execution records | Compute | CLI `compute run/exec`, daemon HTTP, `ExecutionRecord` in `compute-state` (FeltDB in production). |
| Observation of projects | PAX 0.5.0 | `pax --json observe`, `pax --json test` (`pax.execution-result.v1`). Read-only; does not consume Compute execution records. |
| Distribution | compute-configured | `compatibility/published-stack` in this repo (`stack.json`, `stack.toml`, lockfile, `verify.mjs`). |
| Durable authority | FeltDB (`rkendel1/flow_db`) | `@feltdb/core`; `FELTDB_SERVER_BIN` for server tests. |

`compute-configured` is not a separate repository; it is the `configured`
profile in this one.

## Findings that change the plan

1. **Two different Chips.** Attn targets the TypeScript Chip session API
   (`/eve/v1/session`). `chip-rs` is a separate Rust runtime (`chip serve`,
   `/v1/work`) with no AppPort, MCP, FeltDB or Attn integration. The pinned FX
   path runs through the npm Chip. They are not interchangeable.
2. **Attn has no operation record or cancel.** No AppPort capability exists for
   status, evidence, cancel or native-work start. Attn's docs mark Chip's cancel
   as out of scope, and Attn's AGENTS.md forbids Attn-side session state.
3. **Compute evidence does not reach Attn.** Attn displays it only if Chip
   supplies it; the UI says it is unavailable through the current protocol.
4. **PAX does not read Compute executions.** Compute embeds a `project` block in
   `compute.receipt@1` and `compute receipt verify` recomputes it. Any "PAX
   verifies the outcome" step must use that path.
5. **No checked-in WASM fixture.** The nearest deterministic shape is
   `wasm_workload_spec_executes_through_existing_contract`
   (`crates/compute-cli/tests/cli.rs:966`), which builds its module inline.
6. **No executable Attn import-boundary check.** The rule against FX, Compute
   or Chip internals is prose. Compute has `architecture.rs` and
   `execution_paths.rs` for its own side.
7. **Version skew to reconcile.** Compute profile 0.1.17; Attn pins Compute and
   Compute-configured at 0.1.19 and labels FX 0.54.4 (apparently Chip's number);
   `@feltdb/core` is 0.11.7 (Attn), 0.11.8 (Compute docs), 0.11.9 (stack).
8. **Attn's real-runtime test (Tier 3) is outside CI** and stops at "Chip
   accepted the session". The model-and-execution tier (Tier 4) does not exist.

## Added by the second audit (appboundry, authboundry, appport, appport-services, flow_db read)

9. **Compute is not on the AppBoundry path.** AppBoundry runs `AppPort/application-bundle/1`
   modules (`wasm/1` JSON ABI: `memory`, `appport_alloc`, `appport_result_len`,
   `exported_<capability>`; no imports) in its own host via Node `WebAssembly`.
   `crates/compute-runtime-wasm` runs WASI `_start` modules. Compute's own docs
   (`docs/stacks.md`, `docs/platform-audit.md`) record "AppBoundry → Compute path
   missing". Both are WASM; they are different runtimes.
10. **Existing path from an agent to an AppBoundry app:** Chip MCP connection
    (`defineMcpClientConnection`) → AppBoundry host `POST /apps/<selector>/mcp`
    (off unless `APPPORT_MCP_TOKEN`; scopes from `APPPORT_MCP_SCOPES`) → packaged
    WASM. Proven only in AppBoundry's own tests; not wired in the configured agent.
11. **AppBoundry verification** is static (`verifyPackagedWasm`: sha256 against
    `artifact.hash`, then ABI) plus behavioural (`appport app verify`, record
    `AppPort/application-verification/1`). Invocation records are
    `AppPort/application-invocation/1`. The host CLI writes them to a JSON file
    under `APPPORT_RUNTIME_DIR`; FeltDB persistence (`FeltDbExecutionLedger`,
    collection `AppPortState`) is the library path, not the host CLI path.
12. **PAX has no contract for AppBoundry invocations.** `compute.receipt@1`'s
    `project` block describes a project Compute ran. An AppBoundry invocation
    has no Compute receipt. See "Evidence" below.
13. **AuthBoundry is optional by design** (Base Compute has no dependency; the
    AppBoundry host enables it via `APPPORT_AUTHBOUNDRY_URL` and companions;
    half-configured is a 503, never a weaker mode). The repo's
    `@authboundry/core/appport` export is absent from the published 1.15.3
    tarball even at the same version number.
14. **Checked-in deterministic WASM exists:** `integrations/feltdb/test/fixtures/counter-wasm.wasm`
    in `rkendel1/appboundry` (sha256 `57afdef5…334f31`; `exported_counter_increment`,
    `_fail`, `_spin`), and `examples/todos/appport/wasm/application.wasm`.
15. **Chip persists in its Workflow world on disk, not FeltDB**, and has a real
    `/cancel` endpoint. See `docs/eve-chip-operation-contract.md`.
16. **`createFxAgent` is not on the Chip path.** Chip calls `createFxModel()` via
    its `fx()` adapter and runs its own loop.

## Version resolution (authoritative sources)

| Item | Finding | Action |
|---|---|---|
| Compute | Workspace `Cargo.toml` and latest release are **0.1.19**. The profile's `compute: 0.1.17` is a checked-in default that `distribution/scripts/update-stack-version.sh` rewrites during `distribution-certification` ("Sync published-stack version with current release"). | No change. Not drift to patch by hand. |
| FX | `@appport/fx` **0.1.2** (3fx `sdk/package.json`, npm). Attn's `FX_VERSION=0.54.4` is Chip's number, mislabelled, and is unused outside the unimplemented Tier 4. | Fix in `attn` (`integration/runtime-versions.env`). |
| Chip | `@appport/chip` **0.54.4** (profile, lockfile, Attn `CHIP_VERSION`). Attn's fixture agent pins `eve` 0.54.3, the pre-rename name. | Align the fixture in `attn`. |
| FeltDB | `@feltdb/core` **0.11.9** in `flow_db`, npm, the stack and `compute-state-model`. Attn pins 0.11.7. `docs/feltdb-0.11.8-consumer-certification.md` certifies 0.11.8. Nested 0.11.1/0.11.5 copies are declared skew (`allowed_version_skew`, protocol 1). | Attn skew is not changed here: it needs its own compatibility run. 0.11.8 cert doc is stale relative to the pin and says so. |
| `@appport/services` | `package.json`, `stack.json`, lockfile, installed: 0.4.11. `stack.toml` and `docs/compatibility.md` said 0.4.10. | Fixed in this branch: `stack.toml` and `docs/compatibility.md` now say 0.4.11, matching `stack.json`, `package.json` and the lockfile. (Docs' note that 0.4.10 "ships the runnable host" holds for 0.4.11 per `stack.json`'s skew reason, which names 0.4.11 as that release.) |
| `@authboundry/core` | 1.15.3, but repo contents differ from the tarball (`./appport` export). | Record; do not assume the repo source is what ships. |

## Verified so far

- `@appport/fx` 0.1.2 pinned consistently in `package.json`, lockfile,
  `stack.json`, `stack.toml`; integrity
  `sha512-zGMlEAjMa57nAblg07mAvFvJTy7gNEapJ0jmmPkaKHQgVUl2xTL1IUyHPu1SDqhV8J9eMksvYTVgV4mGwYs4xw==`.
- Installed package exports `createFxModel` and `createFxAgent`.
- `npm run verify` under Node 24.21.0: all checks pass, including the
  gateway-free FX model path and the Chip-served agent session.
- PAX 0.5.0 built from `rkendel1/pax@31b4871`.

## Not yet done

Level A and Level B tests, failure-case tests, Attn boundary invariants, the
certification report, and the full Compute CI suites. Blocked on the interface
gaps above; see the decision requested in the session.
