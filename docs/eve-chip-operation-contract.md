# EVE/Chip operation contract (as it exists)

Scope: the contract Attn can rely on today through `@appport/chip` 0.54.4
(`POST /eve/v1/session` …). **Nothing here is a proposal for a new API**; where
a semantic is absent it says so. Source: reading the installed package's docs
and `dist/` (minified; function names cited instead of lines) and
`compatibility/published-stack/agent/`. Statements marked *unverified* have not
been exercised against a running Chip.

## Start

`POST /eve/v1/session` with `Authorization: Bearer <COMPUTE_CONFIGURED_CHIP_TOKEN>`
(enforced by the configured agent in `agent/channels/eve.ts`, not by Chip).
Body: `message` (required), `operationId`, `clientContext`, `mode`,
`outputSchema`, `callback`, `capabilities.requestInput`. `inputResponses` and
`continuationToken` are rejected with 400.

- Success: `202 {"ok":true,"sessionId":"wrun_…","status":"accepted"}` and header
  `x-eve-session-id`. 202 means the Workflow run was accepted, not that
  anything executed; an immediate follow-up can return `409 session_not_active`.
- **Operation identifier.** The session id (`wrun_…`) is the only
  authoritative handle. `operationId` is an idempotency key scoped to the
  authenticated principal (SHA-256 over authenticator, issuer, principal type,
  principal id, operationId → continuation token `eve:op:<hex>`).
- **Idempotency.** Re-POST of the same `operationId` while the session is
  resumable returns 202 with the *existing* `sessionId`; `message` and
  `clientContext` are ignored. There is no race protection (concurrent first
  POSTs may return different candidate ids; only the claiming one runs). After
  the session ends (default lifetime 30 days) the old id is **not guaranteed**.
- `clientContext` is prompt context only: not persisted, not echoed on the
  stream, **not a correlation key**.
- Auth failure is HTTP 401 (or 403 for `ForbiddenError`), never a stream event.

## Status

There is **no** status endpoint and no `operationId → session` lookup.
Authoritative lifecycle is the event stream:
`GET /eve/v1/session/:id/stream?startIndex=N&includeTailIndex=1` (NDJSON,
header `x-eve-stream-tail-index` = index of last event, `-1` if none). Unknown
session: 404. Every event is `{type, data, meta:{id, at}}`.

| Event | Meaning |
|---|---|
| `turn.completed` → `session.waiting` | A turn ended. **Not** a session end and **not** proof tools succeeded. |
| `turn.failed` | Turn failed; conversation sessions return to waiting. |
| `turn.cancelled` → `session.waiting` | Cancel observed. Not a failure. |
| `session.failed`, `session.completed` | Terminal. |

Consequence: a client must derive "done" from the stream, and must not equate
`turn.completed` with success (see Failure).

## Evidence

Tool use is on the stream, keyed by `callId` (for FX, the provider's
`tool_calls[].id`) within a `turnId`:
`action.input.appended` (raw argument text) → `actions.requested` (validated
calls, before execution) → `action.partial` → `action.result`
`{result, status: completed|failed|rejected, error?:{code,message}}`.
There are no `tool.call`/`tool.result` events. Arguments and full results are on
the stream (redaction is the tool author's job).

Relation to the operation: session id + `turnId` + `callId`. Retried steps
re-emit events under new `meta.id`s, and an interrupted step may re-run a tool
(**at-least-once** for tool side effects on restart).

**Not present:** `compute.receipt@1` anywhere in Chip. Chip has no receipt
concept. Any Compute, AppBoundry or AuthBoundry evidence reaches Attn only if a
tool puts it in its output (`action.result.result.output`) or in a failure's
`details`. That is tool-author work, and Attn must treat it as evidence from the
tool, to be verified, not as a Chip guarantee.

## Cancel

`POST /eve/v1/session/:id/cancel` exists (body optional `{turnId?, tasks?,
taskId?}`). `202 accepted` = durably queued; `200 no_active_turn` = unknown or
terminal. **Observed** only as `turn.cancelled` then `session.waiting`.

Guaranteed: the request is queued once accepted; `turnId` scoping prevents a
late cancel hitting a newer turn; the session stays usable.
**Not guaranteed:** timing (cooperative); interrupting an in-flight tool (only
if the tool honours `ctx.abortSignal`); rollback of completed or in-progress
side effects; the race with `turn.completed` (unspecified). Posting a follow-up
message defaults to `turnPolicy:"steer"` and also cancels the active turn.

Attn must present cancel as *requested*, then *cancelled* only after
`turn.cancelled` is observed, and must never claim side effects were undone.

## Failure taxonomy

| Condition | Where it surfaces |
|---|---|
| Denied/missing credential | HTTP 401/403 on the route. No stream. |
| Malformed request | HTTP 400. |
| Model/provider error | `step.failed` → `turn.failed` (`MODEL_CALL_FAILED`, FX `failed.kind`; retryable kinds retried up to 4× per step). |
| `malformed_tool_call` (tool call written as text) | FX failure kind → model failure path. |
| Invalid tool arguments | Thrown by the AI SDK (`Invalid input for tool …`). Event shape *unverified*; to be exercised. |
| Tool error | `action.result` `status:"failed"`, `error.code:"ACTION_RESULT_FAILED"`. The turn usually continues and may end `turn.completed`. |
| Approval denial | tool result `isError`, `output.code:"TOOL_EXECUTION_DENIED"`. |
| Stream persistence failure | `turn.failed` code `WORKFLOW_STREAM_WRITE_FAILED`; session is **parked for retry**, not terminal. |
| Incomplete execution | Stream without a terminal/boundary event; derive "in progress", never "completed". |

**Contract gap (Attn):** Attn today maps `turn.completed` → succeeded
(`appport/attn-control-plane.ts` ~883-905) and consumes only
`turn.*`/`session.failed`. That cannot distinguish a turn in which every tool
failed from success. Success must require an `action.result` with
`status:"completed"` for the expected call, plus verified evidence.

## Durability

Chip sessions, stream and operation ownership persist in the Workflow world
(default: local disk `.eve/.workflow-data`), **not FeltDB**. They survive a
Chip restart only if that path is persistent. Authoritative *application*
state therefore lives in Attn's FeltDB (`attn_work*`); Chip is the execution
and observation source it reconciles from, and `experimental.workflow.retention`
can delete a finished run's stream, so Attn must persist the evidence it needs
before relying on it.

## Tools and model

The configured agent defines **no tools**. Tools come from `agent/tools/*.ts`
(`defineTool`) or connections (`defineMcpClientConnection`, static token or
no-auth loopback), where remote tools are discovered through `connection_search`
and called as `<connection>__<tool>`. The model is
`fx(await createFxModel({baseUrl, model, apiKeyEnv}))`, read at `step.started`
from `FX_BASE_URL`, `FX_MODEL`, `FX_API_KEY_ENV`; a scripted OpenAI-compatible
server on loopback is sufficient and needs no gateway credential.

`createFxAgent({model})` is a *different* agent loop (FX's own). Chip does not
run it, so under the "use the existing EVE/Chip path" requirement the FX
surface exercised end to end is `createFxModel()` through Chip's `fx()` adapter.
`createFxAgent` can be verified separately against the same scripted server
(`@appport/fx` has such a test) but is not the executor in this slice.
