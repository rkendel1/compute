// The configured runtime smoke test:
//
//   compute-configured-chip start  (the configured launcher, the real Chip)
//     -> configured agent           (Chip's fx() adapter)
//     -> @appport/fx                (createFxModel, native OpenAI-compatible transport)
//     -> local Chat Completions fixture
//
// Only the model endpoint is a fixture. Chip, FX, the launcher and the built
// agent are the installed or assembled artifacts. The test also records how the
// pinned Chip actually treats operationId -- first create, concurrent
// duplicates, repeats after ownership, a restart, and a different operation --
// without retrying or otherwise compensating for it.
//
// Usage: node configured-chip-fx.mjs <compute-configured-chip launcher> [evidence.json]

import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import process from 'node:process';

const launcher = process.argv[2];
const evidencePath = process.argv[3];
assert.ok(launcher, 'usage: configured-chip-fx.mjs <compute-configured-chip> [evidence.json]');

const OPERATION = 'deterministic-test-id';
const TOKEN = randomBytes(24).toString('hex');
const MODEL = 'fx-smoke';
const KEY_VARIABLE = 'COMPUTE_SMOKE_MODEL_KEY';
const KEY = `smoke-${randomBytes(8).toString('hex')}`;
const results = {};
const observations = {};
const record = (name, ok, detail) => {
  results[name] = ok ? 'PASS' : 'FAIL';
  if (detail !== undefined) observations[name] = detail;
  console.error(`${ok ? 'PASS' : 'FAIL'} ${name}${detail === undefined ? '' : `: ${JSON.stringify(detail)}`}`);
};

// ---- The model endpoint fixture --------------------------------------------

const requests = [];
const provider = createServer((req, res) => {
  let raw = '';
  req.on('data', (chunk) => { raw += chunk; });
  req.on('end', () => {
    const body = JSON.parse(raw || '{}');
    const user = [...(body.messages ?? [])].reverse().find(({ role }) => role === 'user');
    const prompt = typeof user?.content === 'string' ? user.content : JSON.stringify(user?.content ?? '');
    requests.push({ url: req.url, authorization: req.headers.authorization, model: body.model, stream: body.stream, prompt });
    if (prompt.includes('smoke-fail')) {
      res.writeHead(400, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ error: { message: 'fixture rejected the request', type: 'invalid_request' } }));
      return;
    }
    res.writeHead(200, { 'content-type': 'text/event-stream' });
    const chunk = (delta, finish = null) => res.write(`data: ${JSON.stringify({
      id: 'chatcmpl-smoke', object: 'chat.completion.chunk', model: body.model,
      choices: [{ index: 0, delta, finish_reason: finish }],
    })}\n\n`);
    for (const content of ['FX ', 'smoke ', 'reply']) chunk({ content });
    chunk({}, 'stop');
    res.end('data: [DONE]\n\n');
  });
});
await new Promise((resolve) => provider.listen(0, '127.0.0.1', resolve));
const baseUrl = `http://127.0.0.1:${provider.address().port}/v1`;
const turnsFor = (marker) => requests.filter(({ prompt }) => prompt.includes(marker)).length;

// ---- The configured Chip ------------------------------------------------

const home = await mkdtemp(join(tmpdir(), 'compute-configured-smoke-'));

function environment(extra) {
  const env = { ...process.env };
  // Nothing Vercel, Gateway or FX may leak in from the caller: the only
  // provider configuration is the one this test supplies.
  for (const name of Object.keys(env)) {
    if (/^(AI_GATEWAY_|VERCEL|FX_)/.test(name)) delete env[name];
  }
  return {
    ...env,
    COMPUTE_HOME: home,
    COMPUTE_CONFIGURED_CHIP_TOKEN: TOKEN,
    EVE_TELEMETRY_DISABLED: '1',
    FX_BASE_URL: baseUrl,
    FX_MODEL: MODEL,
    ...extra,
  };
}

async function startChip(extra = {}) {
  const child = spawn(launcher, ['start', '--host', '127.0.0.1', '--port', '0'], {
    env: environment(extra),
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let output = '';
  child.stdout.on('data', (chunk) => { output += chunk; });
  child.stderr.on('data', (chunk) => { output += chunk; });
  const origin = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`chip start did not listen:\n${output}`)), 120_000);
    child.stdout.on('data', () => {
      const match = /Listening on: (http:\/\/[^\s/]+)/.exec(output);
      if (match) { clearTimeout(timer); resolve(match[1]); }
    });
    child.once('exit', (code) => { clearTimeout(timer); reject(new Error(`chip start exited ${code}:\n${output}`)); });
  });
  return {
    origin,
    output: () => output,
    async stop() {
      if (child.exitCode !== null) return;
      const exited = new Promise((resolve) => child.once('exit', resolve));
      child.kill('SIGTERM');
      await Promise.race([exited, new Promise((resolve) => setTimeout(resolve, 10_000))]);
      if (child.exitCode === null) child.kill('SIGKILL');
    },
  };
}

const auth = { authorization: `Bearer ${TOKEN}`, 'content-type': 'application/json' };

async function create(origin, message, operationId, headers = auth) {
  const response = await fetch(`${origin}/eve/v1/session`, {
    method: 'POST', headers, body: JSON.stringify({ message, ...(operationId && { operationId }) }),
  });
  const body = await response.json().catch(() => null);
  return { status: response.status, body };
}

/** Read a session's NDJSON stream until `done(events)` holds. */
async function stream(origin, sessionId, done, deadlineMs = 60_000) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), deadlineMs);
  const events = [];
  try {
    for (let attempt = 0; ; attempt += 1) {
      const response = await fetch(`${origin}/eve/v1/session/${sessionId}/stream`, { headers: auth, signal: controller.signal });
      // The run may not be readable for a moment after its 202.
      if (response.status === 404 && attempt < 50) { await new Promise((r) => setTimeout(r, 200)); continue; }
      assert.equal(response.status, 200, `stream ${sessionId} answered ${response.status}`);
      const decoder = new TextDecoder();
      let buffered = '';
      for await (const chunk of response.body) {
        buffered += decoder.decode(chunk, { stream: true });
        let newline;
        while ((newline = buffered.indexOf('\n')) >= 0) {
          const line = buffered.slice(0, newline).trim();
          buffered = buffered.slice(newline + 1);
          if (!line) continue;
          events.push(JSON.parse(line));
          if (done(events)) return events;
        }
      }
      throw new Error(`stream ${sessionId} ended early: ${JSON.stringify(events)}`);
    }
  } finally {
    clearTimeout(timer);
    controller.abort();
  }
}
const settled = (turns) => (events) => events.filter(({ type }) => type === 'session.waiting').length >= turns;
const text = (events) => events
  .filter(({ type, data }) => type === 'message.appended' && data?.messageDelta !== undefined)
  .map(({ data }) => data.messageDelta).join('');

let chip;
let failure;
try {
  chip = await startChip();
  const { origin } = chip;

  // ---- Chip starts and keeps its HTTP contract ---------------------------
  const health = await fetch(`${origin}/eve/v1/health`).then((r) => r.json());
  record('chip_starts', health.ok === true && health.status === 'ready', health);
  const anonymous = await create(origin, 'hello', undefined, { 'content-type': 'application/json' });
  record('unauthenticated_refused', anonymous.status === 401, anonymous.status);

  // ---- First create: Chip -> FX -> provider -> Chip ----------------------
  const first = await create(origin, 'first turn', OPERATION);
  assert.equal(first.status, 202, JSON.stringify(first));
  assert.deepEqual(Object.keys(first.body).sort(), ['ok', 'sessionId', 'status']);
  assert.equal(first.body.status, 'accepted');
  const s1 = first.body.sessionId;
  const firstEvents = await stream(origin, s1, settled(1));
  const firstRequest = requests.find(({ prompt }) => prompt.includes('first turn'));
  record('session_api', first.status === 202 && firstEvents.at(-1)?.type === 'session.waiting',
    { status: first.status, sessionId: s1, last: firstEvents.at(-1)?.type });
  record('chip_to_fx_to_provider',
    text(firstEvents) === 'FX smoke reply' && firstRequest?.url === '/v1/chat/completions'
      && firstRequest.model === MODEL && firstRequest.stream === true,
    { reply: text(firstEvents), endpoint: firstRequest?.url, model: firstRequest?.model });
  record('no_gateway_credential', firstRequest?.authorization === undefined,
    'no Authorization header without FX_API_KEY_ENV');

  // ---- Repeated create after ownership --------------------------------------
  const repeat = await create(origin, 'first turn', OPERATION);
  await new Promise((r) => setTimeout(r, 1500));
  record('operation_repeat_after_ownership',
    repeat.status === 202 && repeat.body.sessionId === s1 && turnsFor('first turn') === 1,
    { sessionId: repeat.body.sessionId, sameSession: repeat.body.sessionId === s1, providerTurns: turnsFor('first turn') });

  // ---- Concurrent duplicate creates -----------------------------------------
  const concurrentOperation = `${OPERATION}-concurrent`;
  const burst = await Promise.all(Array.from({ length: 5 }, () => create(origin, 'concurrent turn', concurrentOperation)));
  const candidates = [...new Set(burst.map(({ body }) => body?.sessionId))];
  await new Promise((r) => setTimeout(r, 3000));
  const canonical = await create(origin, 'concurrent turn', concurrentOperation);
  const canonicalEvents = await stream(origin, canonical.body.sessionId, settled(1));
  await new Promise((r) => setTimeout(r, 1500));
  const concurrentTurns = turnsFor('concurrent turn');
  record('operation_concurrent_duplicates',
    burst.every(({ status }) => status === 202) && candidates.includes(canonical.body.sessionId)
      && concurrentTurns === 1 && text(canonicalEvents) === 'FX smoke reply',
    {
      requests: burst.length,
      statuses: burst.map(({ status }) => status),
      distinctAcceptedSessionIds: candidates.length,
      canonicalSessionId: canonical.body.sessionId,
      canonicalWasAmongAccepted: candidates.includes(canonical.body.sessionId),
      providerTurns: concurrentTurns,
    });

  // ---- Provider failure reaches Chip as a failed turn ----------------------
  const failing = await create(origin, 'please smoke-fail');
  const failEvents = await stream(origin, failing.body.sessionId,
    (events) => events.some(({ type }) => type === 'turn.failed') && settled(1)(events));
  const failTypes = failEvents.map(({ type }) => type);
  record('provider_error_propagates',
    failTypes.includes('turn.failed') && !failTypes.includes('message.completed')
      && JSON.stringify(failEvents).includes('fixture rejected the request'),
    { types: [...new Set(failTypes)] });

  // ---- Restart: same COMPUTE_HOME, now with a credentialed provider -------
  await chip.stop();
  chip = await startChip({ FX_API_KEY_ENV: KEY_VARIABLE, [KEY_VARIABLE]: KEY });
  const restarted = chip.origin;
  const afterRestart = await create(restarted, 'first turn', OPERATION);
  await new Promise((r) => setTimeout(r, 1500));
  record('operation_after_restart',
    afterRestart.status === 202 && afterRestart.body.sessionId === s1 && turnsFor('first turn') === 1,
    { sessionId: afterRestart.body.sessionId, sameSession: afterRestart.body.sessionId === s1, providerTurns: turnsFor('first turn') });

  // The pre-restart session is durable: a follow-up runs its second turn.
  const followUp = await fetch(`${restarted}/eve/v1/session/${s1}`, {
    method: 'POST', headers: auth, body: JSON.stringify({ message: 'follow-up turn' }),
  });
  const followEvents = await stream(restarted, s1, settled(2));
  record('durable_session_after_restart',
    followUp.status === 202 && turnsFor('follow-up turn') === 1 && followEvents.at(-1)?.type === 'session.waiting',
    { status: followUp.status, turns: followEvents.filter(({ type }) => type === 'session.waiting').length });

  // ---- A different operation is a different session -----------------------
  const other = await create(restarted, 'other turn', `${OPERATION}-other`);
  const otherEvents = await stream(restarted, other.body.sessionId, settled(1));
  const otherRequest = requests.find(({ prompt }) => prompt.includes('other turn'));
  record('operation_different_id',
    other.status === 202 && other.body.sessionId !== s1 && text(otherEvents) === 'FX smoke reply',
    { sessionId: other.body.sessionId, distinctFromFirst: other.body.sessionId !== s1 });

  // FX resolved the configured credential natively from the named variable.
  record('provider_selection',
    otherRequest?.authorization === `Bearer ${KEY}` && otherRequest.model === MODEL,
    { model: otherRequest?.model, authorization: otherRequest?.authorization ? 'Bearer <configured key>' : null });
} catch (error) {
  failure = error;
  console.error(chip?.output?.() ?? '');
} finally {
  await chip?.stop();
  await new Promise((resolve) => { provider.closeAllConnections(); provider.close(resolve); });
  await rm(home, { recursive: true, force: true });
}

const evidence = {
  format: 'compute.configured-chip-fx-smoke@1',
  result: !failure && Object.values(results).every((r) => r === 'PASS') ? 'pass' : 'fail',
  launcher,
  results,
  observations,
  ...(failure && { error: String(failure.stack ?? failure) }),
};
if (evidencePath) await writeFile(evidencePath, `${JSON.stringify(evidence, null, 2)}\n`);
console.log(JSON.stringify(evidence, null, 2));
process.exit(evidence.result === 'pass' ? 0 : 1);
