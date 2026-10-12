import { createServer } from 'node:http';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
const here = new URL('.', import.meta.url);
const requests = [];
const script = JSON.parse(process.env.SCRIPT ?? '[]'); // array of steps: {text}|{tool,args}
let step = 0;
const server = createServer((req, res) => {
  let raw = ''; req.on('data', c => raw += c);
  req.on('end', () => {
    const body = JSON.parse(raw || '{}'); requests.push(body);
    const next = script[step++] ?? { text: 'done' };
    const chunk = (delta, finish = null) => res.write(`data: ${JSON.stringify({ id: 'c', object: 'chat.completion.chunk', model: body.model, choices: [{ index: 0, delta, finish_reason: finish }] })}\n\n`);
    res.writeHead(200, { 'content-type': 'text/event-stream' });
    if (next.tool) {
      chunk({ tool_calls: [{ index: 0, id: `call_${step}`, type: 'function', function: { name: next.tool, arguments: next.rawArgs ?? JSON.stringify(next.args) } }] });
      chunk({}, 'tool_calls');
    } else { chunk({ content: next.text }); chunk({}, 'stop'); }
    res.end('data: [DONE]\n\n');
  });
});
await new Promise(r => server.listen(0, '127.0.0.1', r));
const base = `http://127.0.0.1:${server.address().port}/v1`;
const token = 'chip-token-' + 'x'.repeat(24);
const env = { ...process.env, FX_BASE_URL: base, FX_MODEL: 'scripted', COMPUTE_CONFIGURED_CHIP_TOKEN: token, EVE_TELEMETRY_DISABLED: '1' };
const child = spawn(process.execPath, ['node_modules/.bin/chip', 'start', '--host', '127.0.0.1', '--port', '0'], { cwd: fileURLToPath(here), env, stdio: ['ignore', 'pipe', 'pipe'] });
let out = ''; child.stdout.on('data', c => out += c); child.stderr.on('data', c => out += c);
const origin = await new Promise((ok, bad) => { const t = setTimeout(() => bad(new Error(out)), 90000); child.stdout.on('data', () => { const m = /Listening on: (http:\/\/[^\s/]+)/.exec(out); if (m) { clearTimeout(t); ok(m[1]); } }); });
const h = { authorization: `Bearer ${token}`, 'content-type': 'application/json' };
const c = await fetch(`${origin}/eve/v1/session`, { method: 'POST', headers: h, body: JSON.stringify({ message: process.env.MSG ?? 'Create a todo titled "Certify the stack".', operationId: process.env.OP ?? 'op-1' }) });
const { sessionId } = await c.json(); console.log('session', c.status, sessionId);
const events = [];
const ac = new AbortController(); setTimeout(() => ac.abort(), 60000);
for (let a = 0; a < 50; a++) {
  const r = await fetch(`${origin}/eve/v1/session/${sessionId}/stream`, { headers: h, signal: ac.signal });
  if (r.status === 404) { await new Promise(r => setTimeout(r, 200)); continue; }
  let buf = ''; const dec = new TextDecoder(); let done = false;
  for await (const ch of r.body) { buf += dec.decode(ch, { stream: true }); let i; while ((i = buf.indexOf('\n')) >= 0) { const l = buf.slice(0, i).trim(); buf = buf.slice(i + 1); if (!l) continue; const e = JSON.parse(l); events.push(e); if (['session.waiting', 'session.completed', 'session.failed'].includes(e.type)) { done = true; break; } } if (done) break; }
  break;
}
console.log(JSON.stringify({ types: events.map(e => e.type) }));
for (const e of events) if (/action|failed|actions/.test(e.type)) console.log(e.type, JSON.stringify(e.data).slice(0, 700));
if (process.env.DUMP) console.log(JSON.stringify(requests[0].tools.filter(t=>t.function.name==='connection_search'))); console.log('MODEL REQUESTS', requests.length);
for (const [i, r] of requests.entries()) console.log(i, 'tools:', (r.tools ?? []).map(t => t.function?.name).join(','), '| last:', JSON.stringify(r.messages.at(-1)).slice(0, 300));
child.kill('SIGTERM'); server.closeAllConnections(); server.close(); process.exit(0);
