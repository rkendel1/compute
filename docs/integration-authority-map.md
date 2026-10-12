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
