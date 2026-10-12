# Chip/EVE invocation outcomes: certification report

Date: 2026-10-12. Linux x86_64, Node 24.21.0 (compute stack), Bun 1.4.2 (Attn), Rust stable.
**This certifies Attn's outcome and cancellation semantics against recorded real Chip behaviour. It does
not certify the full Attn → … → PAX slice** (see Blocked).

## Versions and commits used

| Component | Version / commit |
|---|---|
| `rkendel1/compute` | branch `claude/confident-mayer-r8pw7b` (workspace 0.1.19; configured profile default 0.1.17, synced at release) |
| `rkendel1/attn` | branch `claude/confident-mayer-r8pw7b` @ `e85de1c` |
| `@appport/chip` | 0.54.4 |
| `@appport/fx` | 0.1.2, sha512-zGMlEAjMa57nAblg07mAvFvJTy7gNEapJ0jmmPkaKHQgVUl2xTL1IUyHPu1SDqhV8J9eMksvYTVgV4mGwYs4xw== |
| `appport` CLI / `@appport/sdk` (build+verify the app) | 1.1.20 / 1.1.19 (npm) |
| AppBoundry host | `rkendel1/appboundry@4e50aebe1bab8df1db75cd2a4f8296f54e7867ea`, source build (not published) |
| Fixture application | `examples/todos`; WASM `sha256:e2e29af8cec7d6398aa453b6cbc1a8161831f6008ea060488723ac56cefca3ea`; package `sha256:53fca71eced88dc59cd584321e1001ce0eea46ca19b30fc176f88f31f24198b7` (rebuilt from scratch to the same digests) |
| PAX | 0.5.0 built from `rkendel1/pax@31b4871` (installed; not used by any passing check) |

## Passed (real execution unless noted)

| Check | Command | Result |
|---|---|---|
| Rust workspace | `cargo test --workspace --locked` | 722 passed, 0 failed, 19 ignored |
| Installer contract | `distribution/tests/installer.sh` | passed |
| Homebrew contract | `distribution/tests/homebrew.sh` | passed |
| Product acceptance | `cargo test --locked -p compute-cli --test product` | 3 passed |
| Stack compatibility | `npm run verify` (compatibility/published-stack, Node 24.21.0) | 11/11 checks (incl. FX 0.1.2 via Chip's `fx()` adapter, no gateway credential) |
| Chip/FX/AppBoundry scenarios | `APPBOUNDRY_DIR=… node --test scenarios/outcomes.test.mjs` | 17/17 (real Chip, real FX transport, real MCP, real AppBoundry host and WASM; **only the model endpoint is scripted**) |
| Attn unit/integration | `bun run appport:test` | 232 pass, 0 fail (198 prior + 34 new; events replayed from the recordings above, **not** a live Chip) |
| Attn web | `bun run types`, `bun run build:web` | clean |
| Attn browser | `bun appport/test-browser-{e2e,parity,queue,outcome,dependencies}.ts` | all PASS (stand-in Chip replaying real event shapes) |
| Mutation check | reintroduce "`turn.completed` ⇒ succeeded" | 10 of the 30 new outcome tests fail; restored ⇒ all pass |

Existing Attn tests that asserted the old rule were updated to script a real invocation; one asserted that
`POST /api/work/:id/cancel` must not exist and had that entry removed (the route is now deliberate).

## Failing, not caused by this work

- `test-browser-world.ts`: fails on the untouched `main` baseline.
- `test-browser-compute.ts`: fails at "degraded Compute appears in Attn" in 3 of 4 baseline reruns (an immediate
  visibility check after navigation); flaky at baseline. Passed once with these changes, failed twice.

## Blocked / not verified

| Item | Why |
|---|---|
| Full slice Attn → AppPort → Chip → Compute → FX → AppBoundry → AuthBoundry → FeltDB → PAX | Not built. Gaps below. |
| **PAX verification of an invocation** | PAX has no supported observation contract for AppBoundry invocations. Attn reports `pax: not_supported`. |
| **AuthBoundry enforcement** | AuthBoundry mode is opt-in on the host and was not configured; nothing here demonstrates it. |
| AppBoundry MCP host in clean CI | Exists only in source; scenarios are skipped (blocked) without `APPBOUNDRY_DIR`. |
| FeltDB as authority for the application's own records | AppBoundry's host CLI keeps application state and invocation records in a JSON file; FeltDB ledger is library-only. Attn's own operation state *is* FeltDB-backed (tested across reopen). |
| Compute in the execution path | AppBoundry executes the WASM; Compute is not on that path (no code connects them). |
| Schema-invalid tool arguments as an observable failure | Needs a Chip change; Attn reports `unverified`. |
| Live Attn ↔ real Chip | Attn's own Tier 3 (`runtime:verify`, needs Homebrew-installed Compute-configured) not run. The cancel/outcome path was exercised against real Chip only through the recorder, not through Attn's HTTP client. |
| Level B (Ollama/qwen3-coder) smoke test | Not written. |

## Not run

`cargo fmt --check`; ignored FeltDB-server tests (`FELTDB_SERVER_BIN`); `distribution-certification` pipeline and
live-tap scripts (need release assets); `packages/*` npm and UI e2e suites; AppBoundry, AuthBoundry, AppPort,
appport-services, FeltDB, PAX, 3fx and chip-rs own suites. Nothing in them is claimed.

## Security and policy findings (owners outside this change)

1. AppBoundry plain-HTTP route honours a self-declared `x-appport-scopes` (bypasses MCP token/scope limits).
2. Chip built-ins `bash`/`read_file`/`write_file`/`web_fetch` are offered and ungoverned; inert only by packaging.
3. Chip cancel is cooperative: a racing in-flight call still lands; with the host frozen, `turn.cancelled` was
   unobservable until it resumed; a 202 for a finished session means nothing.
