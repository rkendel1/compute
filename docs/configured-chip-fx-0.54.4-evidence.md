# Configured Chip 0.54.4 + FX 0.1.0: runtime evidence

This records what the configured runtime does with the published packages pinned in
`compatibility/published-stack`. It is the post-upgrade evidence for Chip's session
contract: the `operationId` results below are those of the actual Chip 0.54.4 release,
not assumptions carried over from 0.54.3.

## Stack under test

| Component | Version | Source |
| --- | --- | --- |
| Compute | release version (`stack.json` `compute`) | GitHub release asset, through the `compute` formula |
| `@appport/chip` | 0.54.4 | `https://registry.npmjs.org/@appport/chip/-/chip-0.54.4.tgz` |
| `@appport/fx` | 0.1.0 | `https://registry.npmjs.org/@appport/fx/-/fx-0.1.0.tgz` |
| Node | 24.18.0 | bundled by base Compute (`distribution/runtime-lock.json`) |

The path tested:

```text
compute-configured-chip start
  -> Chip 0.54.4 (configured agent, chip start: the production server)
  -> fx()            @appport/chip/models/fx
  -> createFxModel() @appport/fx, native OpenAI-compatible transport
  -> loopback Chat Completions fixture
```

Only the model endpoint is a fixture. Chip, FX, the launcher (rendered from the formula
template) and the built agent are the shipped artifacts. Every `AI_GATEWAY_*`, `VERCEL*`
and `FX_*` variable from the caller is removed before Chip starts.

Reproduce:

```sh
# against an assembled configured tree
COMPUTE_TEST_CONFIGURED_ROOT=compute-configured distribution/tests/configured-chip-fx.sh
# against an installation
COMPUTE_TEST_LAUNCHER="$(brew --prefix)/bin/compute-configured-chip" distribution/tests/configured-chip-fx.sh
```

## Results

| Check | Result | Observation |
| --- | --- | --- |
| Chip starts (`GET /eve/v1/health`) | PASS | `{"ok":true,"status":"ready"}` |
| Unauthenticated create refused | PASS | 401 without `COMPUTE_CONFIGURED_CHIP_TOKEN` bearer |
| `POST /eve/v1/session` | PASS | 202, body exactly `{ok, sessionId, status: "accepted"}` |
| `GET /eve/v1/session/:id/stream` | PASS | NDJSON through `session.waiting` |
| Chip → FX → provider | PASS | Streamed deltas `FX `, `smoke `, `reply` reached the session; the fixture saw `POST /v1/chat/completions`, `model: fx-smoke`, `stream: true` |
| No Gateway credential | PASS | No `Authorization` header when `FX_API_KEY_ENV` is unset |
| Provider selection with credential | PASS | With `FX_API_KEY_ENV` naming a variable, FX sent `Authorization: Bearer <that value>` |
| Provider error propagation | PASS | HTTP 400 from the provider → `step.failed`, `turn.failed`, then `session.waiting`; no `message.completed`; the provider message is in the failure |
| Durable session across restart | PASS | A follow-up to the pre-restart session ran its second turn after Chip restarted on the same `COMPUTE_HOME` |

### `operationId` on Chip 0.54.4

All creates use the same authenticated principal (the configured token).

| Case | Result | Actual behavior |
| --- | --- | --- |
| First create, `operationId = deterministic-test-id` | PASS | 202, session S1; one provider turn |
| Repeated create after ownership (same id, after S1's turn) | PASS | 202, **S1** returned; no new provider turn |
| Concurrent duplicate creates (5 simultaneous, same new id) | PASS (create-once turn) | All 5 returned 202 with **5 different** accepted session ids. Exactly **one** provider turn ran. A later create with the same id returned the canonical session, which was one of the 5 |
| Chip restart, same id | PASS | 202, **S1** returned; no new provider turn |
| Different `operationId` | PASS | 202, a new session distinct from S1 |

The provisional window is retained in 0.54.4. Simultaneous creates do not wait for the
operation's owner to publish, so each can receive a different accepted candidate id. Only
the candidate that claims the operation runs its first turn. Once ownership is
published, every create with that id, before or after a restart, returns the canonical
session and dispatches nothing. Chip's own documentation describes this behavior
(`docs/channels/eve.mdx`, "Start and continue a session").

A caller that needs the canonical session id for an operation it may have created
concurrently must repeat the create after startup. Compute-configured does not retry,
cache or rewrite on the caller's behalf.

## Notes

- Chip's default tool set includes the Gateway `web_search` tool. Through FX it is
  reported as an unsupported provider-defined tool (an AI SDK warning on stderr) and is
  not sent to the provider.
- Chip records its build directory in the compiled manifest as provenance. The release
  builds at the fixed root `/tmp/compute-configured-agent`, and the relocated build is
  served from the launcher's working directory; the recorded root is not read at run time.
- `chip start` keeps durable sessions under its working directory and ignores
  `WORKFLOW_LOCAL_DATA_DIR`. For that reason the launcher runs Chip in
  `$COMPUTE_HOME/configured/chip`, never in the Homebrew keg.
- `ChipAgentExecutor` (`compute-configured-chip invoke` from `COMPUTE_CONFIGURED_HOME`)
  completes a turn through FX with this release. On 0.1.18 it failed with "No eve project
  contains …/libexec" because no agent shipped. The controller route
  `POST /compute/execute-agent` is still not wired to that executor (no controller calls
  `with_agent_executor`), so it answers `OperationUnsupported`. That route predates this
  release and is unchanged by it.

## Pre-release Homebrew installation

Run in a clean `linux/amd64` Ubuntu 22.04 container (the certified Linux ABI baseline)
as an unprivileged user, before any release existed:

1. Install Homebrew, then `brew tap rkendel1/compute` and `brew install compute`, which
   installs the published base Compute 0.1.18 from the live tap.
2. Assemble the configured asset from this branch with the CI commands: `npm ci
   --ignore-scripts`, `build-configured-agent.sh`, `verify`, assembly, and
   `check-configured-paths.sh`.
3. Render both formulas with `distribution/render-homebrew-tap.sh`. The test copy differs
   from the release in one way: its `url` names the local asset and pins `version`,
   because no release asset exists yet.
4. Install with `brew install` and test the installed product.

| Check | Result |
| --- | --- |
| `brew install compute-configured` | PASS |
| `compute-configured --version` | PASS (`compute 0.1.18`) |
| `compute-configured-verify` | PASS: all 10 checks, including `model_provider` and `agent_session`; versions Chip 0.54.4, FX 0.1.0, Node v24.18.0 |
| `compute-configured-setup` | PASS (`certified and active`) |
| `compute-configured-chip --version` | PASS (`0.54.4`) |
| Installed `@appport/fx` | PASS (`@appport/fx@0.1.0`) |
| `brew test compute-configured` | PASS |
| Installed keg and launchers name no build-machine path | PASS |
| Smoke test against the installed launcher | PASS: all 12 checks; concurrent creates again returned 5 accepted ids with 1 provider turn |
