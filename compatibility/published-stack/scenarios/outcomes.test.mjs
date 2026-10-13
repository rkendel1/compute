// Chip → FX (createFxModel, scripted endpoint) → MCP → AppBoundry host → verified WASM: what the real
// stream says for success, malformed input, denial, application failure, cancellation and restart.
//
// OPT-IN. Needs an AppBoundry source checkout (the published `appport` 1.1.20 has no MCP host):
//   APPBOUNDRY_DIR=/path/to/appboundry   # built: pnpm install --frozen-lockfile && pnpm build
//   RECORD_DIR=/path                     # optional: write the observed streams (Attn's fixtures)
//   node --test scenarios/outcomes.test.mjs
// Without APPBOUNDRY_DIR every test is reported as skipped (blocked), never as passed.
import { after, before, describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import * as H from './harness.mjs';

const PINNED = {
  appboundryCommit: 'd96e3a918da431d60354467f369388a9c8d768ee', // 4e50aeb plus the host HTTP token boundary
  wasmSha256: 'e2e29af8cec7d6398aa453b6cbc1a8161831f6008ea060488723ac56cefca3ea',
  packageIdentity: 'sha256:53fca71eced88dc59cd584321e1001ce0eea46ca19b30fc176f88f31f24198b7',
};
const checkout = H.appboundryCheckout();
const skip = checkout ? false : 'blocked: APPBOUNDRY_DIR is not set (the AppBoundry MCP host is not in any published release)';
const search = { tool: 'connection_search', args: { keywords: 'todo' } };
const create = (title) => ({ tool: 'appboundry__todo_create', args: { title } });

let work; let bundle; let agentDir; let port;
before(async () => {
  if (!checkout) return;
  work = mkdtempSync(join(tmpdir(), 'chip-appboundry-'));
  bundle = H.prepareBundle(checkout, work);
  port = await H.allocatePort();
  // The connection URL is a build input of the agent, exactly as it would be for compute-configured.
  agentDir = H.prepareAgent(work, `http://127.0.0.1:${port}/apps/todos/mcp`);
});
after(() => { if (work) rmSync(work, { recursive: true, force: true }); });

const record = (name, hostTruth, events, note) => {
  if (!process.env.RECORD_DIR) return;
  mkdirSync(process.env.RECORD_DIR, { recursive: true });
  writeFileSync(join(process.env.RECORD_DIR, `${name}.json`), JSON.stringify({
    scenario: name,
    recording: { chip: '@appport/chip@0.54.4', appboundryHost: `rkendel1/appboundry@${checkout.commit} (source build)`, bundle: `examples/todos (package ${bundle.packageIdentity}, wasm sha256:${bundle.wasmSha256})`, model: 'scripted OpenAI-compatible endpoint through @appport/fx createFxModel', ...(note ? { note } : {}) },
    hostTruth: { todos: hostTruth }, events,
  }, null, 1));
};

/** One host + scripted model + Chip, torn down afterwards. */
async function stack({ scopes, steps, mcpToken, name }, fn) {
  const host = await H.startHost({ checkout, bundle: bundle.bundle, port, scopes });
  const model = await H.startModel(typeof steps === 'function' ? steps : (_b, n) => steps[n] ?? { text: 'done' });
  const chip = await H.startChip({ agentDir, modelBase: model.base, mcpUrl: host.mcp, mcpToken: mcpToken ?? host.token });
  try {
    const run = async (message = 'do the todo task') => {
      const c = await H.create(chip, { message, operationId: `${name}-${Date.now()}` });
      assert.equal(c.status, 202, 'Chip accepts the session');
      return { sessionId: c.body.sessionId, ...(await H.readUntilSettled(chip, c.body.sessionId)) };
    };
    return await fn({ host, model, chip, run });
  } finally { await chip.stop(); model.stop(); host.stop(); }
}
const results = (events) => events.filter((e) => e.type === 'action.result').map((e) => e.data);
const appResult = (events) => results(events).find((r) => r.result?.toolName?.startsWith('appboundry__'));

describe('distribution integrity of the fixture', { skip }, () => {
  it('the AppBoundry checkout is the pinned commit', () => assert.equal(checkout.commit, PINNED.appboundryCommit));
  it('the agent was built against the URL of the port the host listens on, with no placeholder default', () => {
    const baked = `http://127.0.0.1:${port}/apps/todos/mcp`;
    assert.ok(H.builtOutputMentions(agentDir, baked), 'the build output carries the URL the host is started on');
    if (port !== 4400) assert.ok(!H.builtOutputMentions(agentDir, ':4400/apps/todos/mcp'), 'no default port is baked into the build');
  });
  it('the application builds to the pinned digests and passes AppBoundry verification', () => {
    assert.equal(bundle.wasmSha256, PINNED.wasmSha256);
    assert.equal(bundle.packageIdentity, PINNED.packageIdentity);
    assert.match(bundle.verification, /Capability conformance: PASS/);
  });
});

describe('Chip stream outcomes', { skip, concurrency: false }, () => {
  it('valid invocation: the real WASM runs, and Chip carries AppBoundry\'s evidence', async () => {
    await stack({ name: 'valid', steps: [search, create('Certify'), { text: 'created' }] }, async ({ host, run }) => {
      const r = await run();
      assert.deepEqual(types(r.events).filter((t) => !t.startsWith('step.') && !t.startsWith('message.')), ['session.started', 'turn.started', 'actions.requested', 'action.result', 'actions.requested', 'action.result', 'turn.completed', 'session.waiting']);
      const out = appResult(r.events);
      assert.equal(out.status, 'completed');
      const ev = out.result.output._meta['appboundry/evidence'];
      assert.deepEqual({ outcome: ev.outcome, capability: ev.capability, artifactHash: ev.artifactHash, packageIdentity: ev.packageIdentity }, { outcome: 'success', capability: 'todo.create@1', artifactHash: PINNED.wasmSha256, packageIdentity: PINNED.packageIdentity });
      assert.match(ev.executionUnitId, /^exec_/);
      assert.deepEqual((await H.todos(host)).map((t) => t.title), ['Certify'], 'the host, read independently, holds the state change');
      record('valid', await H.todos(host), r.events);
    });
  });

  it('schema-invalid arguments: the attempt and its rejection are observable (ACTION_INPUT_INVALID), the model is still told, the tool never runs', async () => {
    await stack({ name: 'invalid-args-schema', steps: [search, { tool: 'appboundry__todo_create', args: { wrong: 1, secret: 'sk-live-0123456789' } }, { text: 'ok' }] }, async ({ host, model, run }) => {
      const r = await run();
      const requested = r.events.filter((e) => e.type === 'actions.requested' && e.data.actions?.[0]?.toolName === 'appboundry__todo_create');
      assert.equal(requested.length, 1, 'the attempted call is on the stream');
      assert.deepEqual(requested[0].data.actions[0].input, {}, 'the rejected arguments are not echoed');
      const rejected = results(r.events).find((x) => x.result.toolName === 'appboundry__todo_create');
      assert.equal(rejected.status, 'failed');
      assert.equal(rejected.error.code, 'ACTION_INPUT_INVALID');
      assert.match(rejected.error.message, /title/, 'names the offending field');
      assert.ok(!JSON.stringify(r.events.filter((e) => e.type === 'action.result' || e.type === 'actions.requested')).includes('sk-live-0123456789'), 'never echoes the rejected values');
      assert.ok(types(r.events).includes('turn.completed'), 'the model turn still completes: that is exactly why the action status matters');
      assert.match(JSON.stringify(model.requests.at(-1).messages.at(-1)), /Invalid input for tool/, 'the model was told');
      assert.deepEqual(await H.todos(host), []);
      record('invalid-args-schema', await H.todos(host), r.events);
    });
  });

  it('unparseable arguments: FX fails the step and the turn is observably failed', async () => {
    await stack({ name: 'invalid-args-json', steps: [search, { tool: 'appboundry__todo_create', rawArgs: '{not json' }, { text: 'ok' }] }, async ({ host, run }) => {
      const r = await run();
      const failed = r.events.find((e) => e.type === 'turn.failed');
      assert.equal(failed.data.code, 'MODEL_CALL_FAILED');
      assert.equal(failed.data.message, 'InvalidToolArguments');
      assert.deepEqual(await H.todos(host), []);
      record('invalid-args-json', await H.todos(host), r.events);
    });
  });

  it('insufficient scope: the write tool is never offered, the call dies before dispatch, nothing runs', async () => {
    await stack({ name: 'scope-denied', scopes: 'todos.read', steps: [search, create('Nope'), { text: 'created it' }] }, async ({ host, model, run }) => {
      const r = await run();
      assert.ok(!model.requests.at(-1).tools.some((t) => t.function.name === 'appboundry__todo_create'), 'todo_create was never offered');
      assert.ok(model.requests.at(-1).tools.some((t) => t.function.name === 'appboundry__todo_list'));
      assert.equal(r.events.find((e) => e.type === 'turn.failed').data.message, 'InvalidToolName');
      assert.ok(!r.events.some((e) => e.type === 'action.result' && e.data.result?.toolName === 'appboundry__todo_create'));
      assert.deepEqual(await H.todos(host), []);
      record('scope-denied', await H.todos(host), r.events);
    });
  });

  it('a wrong host token: the connection is reported as needing authorization, no tool is offered, the turn fails', async () => {
    await stack({ name: 'bad-token', mcpToken: 'wrong-token-0123456789abcdef0123', steps: [search, create('Nope'), { text: 'created it' }] }, async ({ host, run }) => {
      const r = await run();
      assert.equal(results(r.events)[0].result.output[0].needsAuthorization, true);
      assert.equal(r.events.find((e) => e.type === 'turn.failed').data.message, 'InvalidToolName');
      assert.deepEqual(await H.todos(host), []);
      record('bad-token', await H.todos(host), r.events);
    });
  });

  it('application failure: an MCP isError result is a FAILED action, with the application\'s own record kept for the model', async () => {
    await stack({ name: 'app-failure', steps: [search, { tool: 'appboundry__todo_complete', args: { id: 'todo_999' } }, { text: 'completed it' }] }, async ({ host, run }) => {
      const r = await run();
      const out = appResult(r.events);
      assert.equal(out.status, 'failed', 'Chip no longer reports an application error as a completed action');
      assert.equal(out.error.code, 'ACTION_RESULT_FAILED');
      assert.match(out.error.message, /NOT_FOUND: todo todo_999 not found/);
      assert.equal(out.result.isError, true);
      assert.equal(out.result.output.isError, true, 'the MCP result is kept verbatim');
      assert.equal(out.result.output._meta['appboundry/error'].code, 'NOT_FOUND');
      assert.equal(out.result.output._meta['appboundry/evidence'].outcome, 'failure');
      assert.ok(types(r.events).includes('turn.completed'), 'the model turn still completes');
      assert.deepEqual(await H.todos(host), []);
      record('app-failure', await H.todos(host), r.events);
    });
  });

  it('a model provider failure is an observable failed turn', async () => {
    await stack({ name: 'model-error', steps: () => ({ error: 500 }) }, async ({ run }) => {
      const r = await run();
      assert.equal(r.events.find((e) => e.type === 'turn.failed').data.code, 'MODEL_CALL_FAILED');
      record('model-error', [], r.events);
    });
  });
});

describe('cancellation', { skip, concurrency: false }, () => {
  it('during the model call, before any tool runs: turn.cancelled, the requested call has no result, nothing ran', async () => {
    // Event-driven, not timed: a cancel sent in the first instants after a session is created can reach it before
    // it accepts commands and is answered no_active_turn (see the contract). Cancelling a turn that has started,
    // by its turnId, is deterministic.
    await stack({ name: 'cancel-before', steps: [{ ...search, delayMs: 3000 }, create('Late'), { text: 'x' }] }, async ({ host, chip }) => {
      const c = await H.create(chip, { message: 'do the todo task', operationId: `cb-${Date.now()}` });
      let k;
      const r = await H.readUntilSettled(chip, c.body.sessionId, { onEvent: async (e) => { if (e.type === 'step.started' && !k) k = await H.cancel(chip, c.body.sessionId, { turnId: e.data.turnId }); } });
      assert.equal(k.status, 202);
      assert.ok(types(r.events).includes('turn.cancelled'));
      assert.ok(!r.events.some((e) => e.type === 'action.result'), 'no tool ran');
      assert.deepEqual(await H.todos(host), []);
      record('cancel-before', await H.todos(host), r.events);
    });
  });

  it('racing an in-flight MCP call: either outcome is honest, and the host decides whether the effect landed', async () => {
    await stack({ name: 'cancel-race', steps: [search, create('Race'), { text: 'done' }] }, async ({ host, chip }) => {
      const c = await H.create(chip, { message: 'do the todo task', operationId: `cr-${Date.now()}` });
      let k;
      const r = await H.readUntilSettled(chip, c.body.sessionId, { onEvent: async (e) => { if (e.type === 'actions.requested' && e.data.actions?.[0]?.toolName === 'appboundry__todo_create' && !k) k = await H.cancel(chip, c.body.sessionId); } });
      assert.equal(k.status, 202);
      const t = types(r.events);
      assert.ok(t.includes('turn.cancelled') || t.includes('turn.completed'), 'the turn ended, one way or the other');
      const result = r.events.find((e) => e.type === 'action.result' && e.data.result?.toolName === 'appboundry__todo_create');
      const landed = (await H.todos(host)).map((x) => x.title);
      if (result) assert.deepEqual(landed, ['Race'], 'a recorded result means the effect landed');
      else assert.ok(landed.length <= 1, 'an abandoned call has no result; the effect may or may not have landed — never undone, never claimed');
      record('cancel-race', await H.todos(host), r.events, `cancel sent as the call was requested; ${result ? 'the call finished first' : 'the call was abandoned'}; host truth: ${JSON.stringify(landed)}`);
    });
  });

  it('host frozen mid-call: the cancel reaches the blocked call and turn.cancelled does not wait for the server', async () => {
    await stack({ name: 'cancel-frozen', steps: [search, create('Frozen'), { text: 'x' }] }, async ({ host, chip }) => {
      const c = await H.create(chip, { message: 'do the todo task', operationId: `cf-${Date.now()}` });
      let ack; let sentAt; let cancelledAt;
      const phase1 = await H.readUntilSettled(chip, c.body.sessionId, { timeoutMs: 15000, onEvent: async (e) => {
        if (e.type === 'actions.requested' && e.data.actions?.[0]?.toolName === 'appboundry__todo_create' && !ack) { host.signal('SIGSTOP'); await H.sleep(1000); sentAt = Date.now(); ack = await H.cancel(chip, c.body.sessionId, { turnId: e.data.turnId }); }
        if (e.type === 'turn.cancelled') cancelledAt = Date.now();
      } });
      assert.equal(ack.status, 202);
      assert.equal(phase1.settled, true, 'the turn settled while the host was still frozen');
      assert.ok(types(phase1.events).includes('turn.cancelled'));
      assert.ok(cancelledAt - sentAt < 3000, `turn.cancelled took ${cancelledAt - sentAt}ms after the cancel; it must not wait for the server`);
      assert.ok(!phase1.events.some((e) => e.type === 'action.result' && e.data.result?.toolName === 'appboundry__todo_create'), 'the abandoned call has no result: it may or may not have executed');
      host.signal('SIGCONT');
      record('cancel-frozen-host', await H.todos(host), phase1.events, 'host process frozen (SIGSTOP) while the MCP call was in flight; cancel sent ~1s later; turn.cancelled followed promptly. The abandoned call may still have executed once the host resumed (cooperative cancellation, no rollback).');
    });
  });

  it('after completion, and repeated: no_active_turn (200), not a misleading 202', async () => {
    await stack({ name: 'cancel-after', steps: [search, create('Done'), { text: 'created' }] }, async ({ chip, run }) => {
      const r = await run();
      const sid = r.sessionId;
      for (const body of [{}, {}, { turnId: 'turn_0' }]) {
        const k = await H.cancel(chip, sid, body);
        assert.equal(k.status, 200);
        assert.deepEqual(k.body, { ok: true, status: 'no_active_turn' });
      }
      assert.equal((await H.cancel(chip, 'wrun_01NEVEREXISTED000000000000')).body.status, 'no_active_turn', 'an unknown session');
      const later = await H.readUntilSettled(chip, sid, { startIndex: r.events.length, timeoutMs: 3000 });
      assert.deepEqual(later.events, [], 'and nothing follows');
    });
  });

  it('while a turn is active the cancel is accepted (202) and confirmed by turn.cancelled', async () => {
    await stack({ name: 'cancel-active', steps: [{ ...search, delayMs: 3000 }, create('Active'), { text: 'x' }] }, async ({ chip }) => {
      const c = await H.create(chip, { message: 'do the todo task', operationId: `ca-${Date.now()}` });
      let k;
      const r = await H.readUntilSettled(chip, c.body.sessionId, { onEvent: async (e) => { if (e.type === 'step.started' && !k) k = await H.cancel(chip, c.body.sessionId, { turnId: e.data.turnId }); } });
      assert.equal(k.status, 202);
      assert.equal(k.body.status, 'accepted');
      assert.ok(types(r.events).includes('turn.cancelled'));
    });
  });

  it('restart: the same operationId returns the same session, replays the same stream, calls the model zero times', async () => {
    await stack({ name: 'restart', steps: [search, create('Once'), { text: 'created' }] }, async ({ host, model, chip, run }) => {
      const op = `restart-${Date.now()}`;
      const c = await H.create(chip, { message: 'do the todo task', operationId: op });
      const first = await H.readUntilSettled(chip, c.body.sessionId);
      const calls = model.requests.length;
      await chip.stop();
      const chip2 = await H.startChip({ agentDir, modelBase: model.base, mcpUrl: host.mcp, mcpToken: host.token });
      try {
        const again = await H.create(chip2, { message: 'a different message', operationId: op });
        assert.equal(again.body.sessionId, c.body.sessionId);
        const replay = await H.readUntilSettled(chip2, c.body.sessionId);
        assert.deepEqual(types(replay.events), types(first.events));
        assert.equal(model.requests.length, calls);
        assert.deepEqual((await H.todos(host)).map((t) => t.title), ['Once'], 'executed exactly once');
        const bad = await fetch(`${chip2.origin}/eve/v1/session/${c.body.sessionId}/stream`, { headers: { authorization: 'Bearer nope' } });
        assert.equal(bad.status, 401);
      } finally { await chip2.stop(); }
    });
  });
});

describe('authority at the AppBoundry host', { skip, concurrency: false }, () => {
  it('MCP: no or wrong token is 401; a scope-denied call is FORBIDDEN and runs nothing; a spoofed scope header is ignored', async () => {
    const host = await H.startHost({ checkout, bundle: bundle.bundle, port, scopes: 'todos.read' });
    try {
      assert.equal((await H.mcp(host, 'tools/list', {}, {})).status, 401);
      assert.equal((await H.mcp(host, 'tools/list', {}, { authorization: 'Bearer wrong-wrong-wrong-wrong-wrong-xx' })).status, 401);
      const denied = await H.mcp(host, 'tools/call', { name: 'todo_create', arguments: { title: 'sneak' } });
      assert.equal(denied.body.result.isError, true);
      assert.equal(denied.body.result._meta['appboundry/error'].code, 'FORBIDDEN');
      assert.deepEqual(denied.body.result._meta['appboundry/error'].details.missingScopes, ['todos.write']);
      const spoof = await H.mcp(host, 'tools/call', { name: 'todo_create', arguments: { title: 'spoof' } }, { authorization: `Bearer ${host.token}`, 'x-appport-scopes': 'todos.write' });
      assert.equal(spoof.body.result._meta['appboundry/error'].code, 'FORBIDDEN');
      assert.deepEqual(await H.todos(host), []);
    } finally { host.stop(); }
  });

  it('plain HTTP: a self-declared x-appport-scopes header neither authenticates a caller nor widens the operator\'s scopes once a host token is set', async () => {
    const host = await H.startHost({ checkout, bundle: bundle.bundle, port, scopes: 'todos.read' });
    try {
      const url = `${host.url}/todos`;
      const post = (headers) => fetch(url, { method: 'POST', headers: { 'content-type': 'application/json', ...headers }, body: JSON.stringify({ title: 'self-declared-write' }) });
      assert.equal((await post({ 'x-appport-scopes': 'todos.write' })).status, 401, 'no token: the header is a claim, not a credential');
      assert.equal((await post({ authorization: 'Bearer wrong-wrong-wrong-wrong-wrong-xx', 'x-appport-scopes': 'todos.write' })).status, 401);
      assert.equal((await post({ authorization: `Bearer ${host.token}`, 'x-appport-scopes': 'todos.write' })).status, 403, 'a valid token carries only the operator\'s scopes (read), however many the header claims');
      assert.deepEqual(await H.todos(host), [], 'nothing was written by any of them');
      const read = await fetch(url, { headers: { authorization: `Bearer ${host.token}` } });
      assert.equal(read.status, 200, 'the operator-granted read scope works with the token');
    } finally { host.stop(); }
  });
});

describe('built-in Chip tools in the default configured agent', { skip, concurrency: false }, () => {
  it('bash, read_file, write_file and web_fetch are not offered, and a model that calls them is refused before anything runs', async () => {
    const probe = join(work, 'must-not-exist.txt');
    const steps = [{ tool: 'bash', args: { command: 'id' } }, { text: 'done' }];
    await stack({ name: 'builtins', steps }, async ({ model, run }) => {
      const r = await run('use the shell');
      const offered = model.requests[0].tools.map((t) => t.function.name);
      for (const name of ['bash', 'read_file', 'write_file', 'web_fetch']) assert.ok(!offered.includes(name), `${name} must not be offered`);
      assert.ok(offered.includes('connection_search'), 'the AppBoundry connection is still reachable');
      assert.equal(r.events.find((e) => e.type === 'turn.failed')?.data.message, 'InvalidToolName', 'calling a removed tool is refused by name');
      assert.ok(!r.events.some((e) => e.type === 'action.result' && e.data.result?.toolName === 'bash'), 'nothing was dispatched');
      assert.equal((await import('node:fs')).existsSync(probe), false);
    });
  });
});

function types(events) { return H.types(events); }
