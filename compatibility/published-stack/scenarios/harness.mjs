// Real-boundary harness for the Chip → FX → AppBoundry outcome scenarios. Nothing here mocks Chip,
// FX, the MCP transport or AppBoundry; only the model endpoint is scripted.
import { createServer } from 'node:http';
import { createServer as createTcpServer } from 'node:net';
import { spawn, execFileSync } from 'node:child_process';
import { cpSync, existsSync, mkdirSync, mkdtempSync, symlinkSync, writeFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
export const STACK = join(here, '..');
/** A free loopback port, chosen once per suite: the agent is BUILT against the URL that uses it. */
export function allocatePort() {
  return new Promise((ok, bad) => { const s = createTcpServer(); s.once('error', bad); s.listen(0, '127.0.0.1', () => { const { port } = s.address(); s.close(() => ok(port)); }); });
}
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
export const sha256 = (path) => createHash('sha256').update(readFileSync(path)).digest('hex');

/** The AppBoundry source checkout (built with `pnpm install --frozen-lockfile && pnpm build`). */
export function appboundryCheckout() {
  const dir = process.env.APPBOUNDRY_DIR;
  if (!dir) return undefined;
  const cli = join(dir, 'packages/cli/dist/bin.js');
  if (!existsSync(cli)) throw new Error(`APPBOUNDRY_DIR=${dir} is not built (missing packages/cli/dist/bin.js)`);
  const commit = execFileSync('git', ['-C', dir, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
  return { dir, cli, commit };
}

/** Build, package and verify the todos application with the published `appport` CLI. Returns its digests. */
export function prepareBundle(checkout, work) {
  const app = join(work, 'todos');
  mkdirSync(app, { recursive: true });
  cpSync(join(checkout.dir, 'examples/todos'), app, { recursive: true, filter: (s) => !s.includes('node_modules') });
  writeFileSync(join(app, 'package.json'), JSON.stringify({ name: 'todos-scenario', private: true, version: '1.0.0', type: 'module', dependencies: { '@appport/sdk': '1.1.19' }, devDependencies: { appport: '1.1.20' } }));
  const run = (args) => execFileSync(process.execPath, [join(app, 'node_modules/appport/dist/bin.js'), ...args], { cwd: app, encoding: 'utf8' });
  execFileSync('npm', ['install', '--ignore-scripts', '--no-audit', '--no-fund'], { cwd: app, stdio: 'pipe' });
  run(['app', 'build']);
  const packaged = run(['app', 'package']);
  const verified = run(['app', 'verify', './todos.app']);
  return {
    bundle: join(app, 'todos.app'),
    packageIdentity: /sha256:[0-9a-f]{64}/.exec(packaged)?.[0],
    wasmSha256: sha256(join(app, 'todos.app/application.wasm')),
    verification: verified,
  };
}

/** The configured agent plus ONE test connection to the AppBoundry host, built with the pinned Chip. */
export function prepareAgent(work, mcpUrl) {
  if (!mcpUrl) throw new Error('the AppBoundry connection URL must be set for the build (APPBOUNDRY_MCP_URL)');
  const dir = join(work, 'agent-under-test');
  mkdirSync(join(dir, 'agent/connections'), { recursive: true });
  cpSync(join(STACK, 'agent'), join(dir, 'agent'), { recursive: true });
  cpSync(join(STACK, 'package.json'), join(dir, 'package.json'));
  symlinkSync(join(STACK, 'node_modules'), join(dir, 'node_modules'));
  writeFileSync(join(dir, 'agent/connections/appboundry.ts'), `import { defineMcpClientConnection } from "@appport/chip/connections";

export default defineMcpClientConnection({
  url: process.env.APPBOUNDRY_MCP_URL!,
  description: "AppBoundry todos application: create, list, complete and delete durable todo items.",
  auth: { getToken: async () => ({ token: process.env.APPPORT_MCP_TOKEN! }) },
});
`);
  try {
    execFileSync(process.execPath, ['node_modules/.bin/chip', 'build'], { cwd: dir, env: { ...process.env, EVE_TELEMETRY_DISABLED: '1', APPBOUNDRY_MCP_URL: mcpUrl }, stdio: 'pipe' });
  } catch (error) {
    throw new Error(`chip build failed:\n${error.stdout}\n${error.stderr}`);
  }
  return dir;
}

export async function startHost({ checkout, bundle, port, token = 'host-token-0123456789abcdef012345', scopes = 'todos.read todos.write' }) {
  const rt = mkdtempSync(join(tmpdir(), 'ab-rt-'));
  const env = { ...process.env, APPPORT_RUNTIME_DIR: rt, APPPORT_MCP_SCOPES: scopes, ...(token ? { APPPORT_MCP_TOKEN: token } : {}) };
  const run = (args) => new Promise((ok, bad) => { const c = spawn(process.execPath, [checkout.cli, ...args], { env }); let o = ''; c.stdout.on('data', (d) => (o += d)); c.stderr.on('data', (d) => (o += d)); c.on('exit', (code) => (code === 0 ? ok(o) : bad(new Error(o)))); });
  await run(['host', 'add', bundle]);
  const child = spawn(process.execPath, [checkout.cli, 'host', 'start', 'com.example.todos', ...(port ? ['--port', String(port)] : [])], { env, stdio: ['ignore', 'pipe', 'pipe'] });
  let out = ''; child.stdout.on('data', (d) => (out += d)); child.stderr.on('data', (d) => (out += d));
  for (let i = 0; i < 100 && !/HTTP: (http:\/\/\S+)/.test(out); i++) await sleep(100);
  const url = /HTTP: (http:\/\/\S+)/.exec(out)?.[1];
  if (!url) throw new Error(`host did not start: ${out}`);
  return { url, mcp: `${url}/apps/todos/mcp`, token, signal: (s) => child.kill(s), stop: () => { try { child.kill('SIGCONT'); } catch {} child.kill('SIGTERM'); } };
}

export async function mcp(host, method, params, headers) {
  const r = await fetch(host.mcp, { method: 'POST', headers: { 'content-type': 'application/json', accept: 'application/json, text/event-stream', ...(headers ?? { authorization: `Bearer ${host.token}` }) }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }) });
  return { status: r.status, body: await r.json().catch(() => null) };
}
export const todos = async (host) => (await mcp(host, 'tools/call', { name: 'todo_list', arguments: {} })).body?.result?.structuredContent?.todos;

/** A scripted OpenAI-compatible endpoint. `script(body, n)` → {text} | {tool,args,rawArgs} | {error: status}. */
export async function startModel(script) {
  const requests = []; let n = 0;
  const server = createServer((req, res) => {
    let raw = ''; req.on('data', (c) => (raw += c));
    req.on('end', () => {
      const body = JSON.parse(raw || '{}'); requests.push(body);
      const next = script(body, n++);
      if (next.error) { res.writeHead(next.error, { 'content-type': 'application/json' }); res.end(JSON.stringify({ error: { message: 'scripted model failure', type: 'server_error' } })); return; }
      const chunk = (delta, finish = null) => res.write(`data: ${JSON.stringify({ id: 'c', object: 'chat.completion.chunk', model: body.model, choices: [{ index: 0, delta, finish_reason: finish }] })}\n\n`);
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      if (next.tool) { chunk({ tool_calls: [{ index: 0, id: `call_${n}`, type: 'function', function: { name: next.tool, arguments: next.rawArgs ?? JSON.stringify(next.args ?? {}) } }] }); chunk({}, 'tool_calls'); }
      else { chunk({ content: next.text ?? 'done' }); chunk({}, 'stop'); }
      res.end('data: [DONE]\n\n');
    });
  });
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  return { base: `http://127.0.0.1:${server.address().port}/v1`, requests, stop: () => { server.closeAllConnections(); server.close(); } };
}

export async function startChip({ agentDir, modelBase, mcpUrl, mcpToken, chipToken = `chip-token-${'x'.repeat(24)}` }) {
  const env = { ...process.env, FX_BASE_URL: modelBase, FX_MODEL: 'scripted', COMPUTE_CONFIGURED_CHIP_TOKEN: chipToken, EVE_TELEMETRY_DISABLED: '1', APPBOUNDRY_MCP_URL: mcpUrl, APPPORT_MCP_TOKEN: mcpToken };
  for (const name of Object.keys(env)) if (/^(AI_GATEWAY_|VERCEL)/.test(name)) delete env[name]; // no gateway identity exists
  const child = spawn(process.execPath, ['node_modules/.bin/chip', 'start', '--host', '127.0.0.1', '--port', '0'], { cwd: agentDir, env, stdio: ['ignore', 'pipe', 'pipe'] });
  let out = ''; child.stdout.on('data', (c) => (out += c)); child.stderr.on('data', (c) => (out += c));
  const origin = await new Promise((ok, bad) => { const t = setTimeout(() => bad(new Error(out)), 90000); const look = () => { const m = /Listening on: (http:\/\/[^\s/]+)/.exec(out); if (m) { clearTimeout(t); ok(m[1]); } }; child.stdout.on('data', look); child.once('exit', (c) => bad(new Error(`chip exited ${c}\n${out}`))); });
  return { origin, headers: { authorization: `Bearer ${chipToken}`, 'content-type': 'application/json' }, stop: () => new Promise((r) => { child.once('exit', r); child.kill('SIGTERM'); setTimeout(r, 5000); }) };
}

export async function create(chip, { message, operationId }) {
  const r = await fetch(`${chip.origin}/eve/v1/session`, { method: 'POST', headers: chip.headers, body: JSON.stringify({ message, operationId }) });
  return { status: r.status, body: await r.json().catch(() => null) };
}
export async function cancel(chip, sessionId, body = {}) {
  const r = await fetch(`${chip.origin}/eve/v1/session/${sessionId}/cancel`, { method: 'POST', headers: chip.headers, body: JSON.stringify(body) });
  return { status: r.status, body: await r.json().catch(() => null) };
}
/** Read the stream until a settle event. onEvent(e) may act (e.g. cancel). */
export async function readUntilSettled(chip, sessionId, { onEvent, startIndex = 0, timeoutMs = 45000 } = {}) {
  const events = []; const ac = new AbortController(); const timer = setTimeout(() => ac.abort(), timeoutMs);
  try {
    for (let a = 0; a < 60; a++) {
      const r = await fetch(`${chip.origin}/eve/v1/session/${sessionId}/stream?startIndex=${startIndex}`, { headers: chip.headers, signal: ac.signal });
      if (r.status === 404) { await sleep(200); continue; }
      let buf = ''; const dec = new TextDecoder();
      for await (const ch of r.body) { buf += dec.decode(ch, { stream: true }); let i; while ((i = buf.indexOf('\n')) >= 0) { const l = buf.slice(0, i).trim(); buf = buf.slice(i + 1); if (!l) continue; const e = JSON.parse(l); events.push(e); if (onEvent) await onEvent(e); if (['session.waiting', 'session.completed', 'session.failed'].includes(e.type)) return { events, settled: true }; } }
      return { events, settled: false };
    }
  } catch (e) { return { events, settled: false, error: String(e) }; } finally { clearTimeout(timer); ac.abort(); }
  return { events, settled: false };
}
export const types = (events) => events.map((e) => e.type);
