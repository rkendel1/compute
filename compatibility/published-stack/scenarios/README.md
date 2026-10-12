# Chip → FX → AppBoundry outcome scenarios

Opt-in, real-boundary tests of what `@appport/chip@0.54.4` actually reports when a scripted
`createFxModel()` endpoint drives a tool call into a real AppBoundry host running a verified WASM
application. Only the model endpoint is scripted. Chip, FX, MCP, the AppBoundry host and the WASM are real.

## Why it is opt-in (blocked in plain CI)

The AppBoundry **MCP host** (`/apps/<selector>/mcp`) is not in any published release: `appport@1.1.20`
and `@appport/appboundry@1.1.1` do not contain it (`APPPORT_MCP_TOKEN` is absent from their `dist`),
while `rkendel1/appboundry` source at the same version number does. Without `APPBOUNDRY_DIR` every test
is reported **skipped (blocked)**, never passed.

## Run

```sh
# once: build AppBoundry from the pinned commit (4e50aeb)
git -C "$APPBOUNDRY_DIR" rev-parse HEAD            # must be 4e50aebe1bab8df1db75cd2a4f8296f54e7867ea
(cd "$APPBOUNDRY_DIR" && pnpm install --frozen-lockfile && pnpm build)

cd compatibility/published-stack
npm ci --ignore-scripts                              # Node >= 24
APPBOUNDRY_DIR=... node --test scenarios/outcomes.test.mjs
APPBOUNDRY_DIR=... RECORD_DIR=./recordings node --test scenarios/outcomes.test.mjs   # also writes the streams
```

The suite builds `examples/todos` with the published `appport@1.1.20` CLI and asserts the pinned
digests (WASM `sha256:e2e29af8…a3ea`, package identity `sha256:53fca71e…98b7`), verifies it with
`appport app verify`, builds the configured agent plus one test connection to the host **with the
connection URL set in the build environment** (`APPBOUNDRY_MCP_URL`), and runs each scenario against a fresh
host and Chip. The connection is a test input; the shipped configured agent is unchanged.

`RECORD_DIR` output is the fixture set `rkendel1/attn` replays (`appport/fixtures/chip-recordings`).
