# Spike: scripted FX → Chip → MCP → AppBoundry host → verified WASM

**Status: spike evidence, not certification.** It shows the hop exists and what it
returns. It is not wired into CI, does not include Attn, AuthBoundry denial, FeltDB
persistence of the invocation, or PAX, and it depends on an AppBoundry host built
from unpublished source (see "Blocker").

## What ran (2026-10-12, Node 24.21.0, linux-x64)

1. `appport@1.1.20` (npm) built, packaged and verified `examples/todos` from
   `rkendel1/appboundry@4e50aeb`: `appport app build && appport app package &&
   appport app verify ./todos.app` → *Capability conformance: PASS* for
   `todo.create|list|complete|delete@1`. WASM sha256
   `e2e29af8cec7d6398aa453b6cbc1a8161831f6008ea060488723ac56cefca3ea` (byte-identical
   to the checked-in `application.wasm`); package identity
   `sha256:53fca71eced88dc59cd584321e1001ce0eea46ca19b30fc176f88f31f24198b7`.
2. The host was started from **source**: `pnpm install --frozen-lockfile && pnpm build`
   in `appboundry@4e50aeb`, then `node packages/cli/dist/bin.js host add ./todos.app`
   and `host start com.example.todos` with `APPPORT_MCP_TOKEN` (≥24 chars) and
   `APPPORT_MCP_SCOPES="todos.read todos.write"`.
3. The configured agent (`compatibility/published-stack/agent`) plus
   `agent/connections/appboundry.ts` (this directory) was built with the pinned
   `@appport/chip@0.54.4` and served with `chip start`. `drive.mjs` runs a scripted
   OpenAI-compatible endpoint (`FX_BASE_URL`, loopback, no credential) and one session.

## Observed

* Plain MCP JSON-RPC against `/apps/todos/mcp`: `initialize`, `tools/list`, and
  `todo_create` → `todo_1`, `todo_list` returned it. No token → 401.
* Through Chip, the model is offered `connection_search`; after it runs, the
  connection's tools become callable as `appboundry__todo_create` etc.
* Valid sequence (`connection_search{keywords}` → `appboundry__todo_create{title}`):
  stream = `actions.requested` / `action.result` ×2 → `turn.completed` →
  `session.waiting`. The `action.result.output` carries the tool's structured result
  **and** `_meta["appboundry/evidence"]`: `applicationId`, `packageIdentity`,
  `artifactHash`, `contractFingerprint`, `capability`, `executionMode`,
  `outcome: "success"`, `executionUnitId`.
* **Malformed tool arguments** (`{query}` instead of `{keywords}`): *no*
  `actions.requested`/`action.result` events. The validation error
  (`AI_InvalidToolInputError … keywords`) is returned to the model as a tool message
  and the turn still ends `turn.completed`. A client reading only the stream cannot
  see that failure.
* **Idempotency/durability:** re-POSTing the same `operationId` to a freshly started
  Chip process returned the same `sessionId` (state persisted in the Workflow world on
  disk) and dispatched nothing new.
* Chip also offers built-in `bash`, `read_file`, `write_file`, `web_fetch` to the model.
  The configured agent does not remove them.

## Blocker

The published `appport@1.1.20` and `@appport/appboundry@1.1.1` do **not** contain the MCP
host (`APPPORT_MCP_TOKEN` is absent from the published `dist`); `rkendel1/appboundry`
source at the same version number does. A clean-environment CI gate therefore has to
either build AppBoundry from a pinned commit or wait for a published release. Which is
a release decision for the owner.

`@appport/mcp` (npm 1.0.2 in the stack, 1.1.0 latest) is the library under that endpoint
(`toMcpTools`, `createMcpBridge`); it has no HTTP server, token or scope enforcement.

## Reproduce

```sh
# in rkendel1/appboundry@4e50aeb
pnpm install --frozen-lockfile && pnpm build
# build/package/verify examples/todos with the published CLI (see step 1), then:
export APPPORT_RUNTIME_DIR=$PWD/rt APPPORT_MCP_TOKEN=<24+ chars> APPPORT_MCP_SCOPES="todos.read todos.write"
node packages/cli/dist/bin.js host add ./todos.app && node packages/cli/dist/bin.js host start com.example.todos &
# in a copy of compatibility/published-stack with the connection added and `chip build` run:
SCRIPT='[{"tool":"connection_search","args":{"keywords":"todo create"}},{"tool":"appboundry__todo_create","args":{"title":"Certify the stack"}},{"text":"Created."}]' OP=op-1 node drive.mjs
```
