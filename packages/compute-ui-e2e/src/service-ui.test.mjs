// Certify, in a real browser against a real daemon, how the Services page
// exposes a registered service's contributed UI: as links to the service's own
// pages, with nothing the service sends ever interpreted as script or markup.
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { existsSync, mkdtempSync, rmSync } from 'node:fs';
import { createServer as createHttp } from 'node:http';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import test from 'node:test';
import { chromium } from 'playwright-core';

const compute = resolve(process.cwd(), '../../target/debug/compute');
const chromiumPath = process.env.CHROMIUM_PATH
  ?? ['/opt/pw-browsers/chromium', '/opt/pw-browsers/chromium-1194/chrome-linux/chrome', chromium.executablePath()].find((path) => path && existsSync(path));

function freePort() {
  return new Promise((resolvePort, reject) => {
    const server = createServer();
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => { const { port } = server.address(); server.close(() => resolvePort(port)); });
  });
}

function cli(endpoint, ...args) {
  const result = spawnSync(compute, [...args, '--json'], { encoding: 'utf8', env: { ...process.env, COMPUTE_DAEMON: endpoint } });
  assert.equal(result.status, 0, `compute ${args.join(' ')}: ${result.stderr}${result.stdout}`);
  return JSON.parse(result.stdout);
}

/** A stand-in service whose `/v1/ui` answers `body`. */
async function stub(body, status = 200) {
  const port = await freePort();
  const server = createHttp((request, response) => {
    if (request.url === '/v1/ui') {
      response.writeHead(status, { 'content-type': 'application/json' });
      response.end(typeof body === 'string' ? body : JSON.stringify(body));
    } else { response.writeHead(404); response.end(); }
  });
  await new Promise((done) => server.listen(port, '127.0.0.1', done));
  return { url: `http://127.0.0.1:${port}`, close: () => server.close() };
}

const surface = (id, route, title = id) => ({ id, title, route, capabilities: [] });
const nav = (id, label, order = 10) => ({ id: `n.${id}`, label, group: 'Hostile <b>group</b>', order, surface: id });
const document = (surfaces, navigation, requires = []) => ({
  protocol: 'AppPort/ui/1', product: { id: 'demo', version: '1.0.0' }, surfaces, navigation, composition: { requires },
});

test('a registered service contributes links to its own pages, and nothing it sends can run in Compute', { timeout: 120_000 }, async (context) => {
  assert.ok(existsSync(compute), `build the CLI first: ${compute}`);
  assert.ok(chromiumPath, 'Chromium is required; set CHROMIUM_PATH');
  const root = mkdtempSync(join(tmpdir(), 'compute-service-ui-'));
  const port = await freePort();
  const endpoint = `http://127.0.0.1:${port}`;
  const daemon = spawn(compute, ['start', '--listen', `127.0.0.1:${port}`, '--state', 'memory', '--state-dir', join(root, 'node')], { stdio: 'ignore' });
  const services = [];
  context.after(() => { daemon.kill('SIGINT'); services.forEach((service) => service.close()); rmSync(root, { recursive: true, force: true }); });
  for (let attempt = 0; ; attempt += 1) {
    try { await fetch(`${endpoint}/status`); break; } catch {
      assert.ok(attempt < 100, 'the daemon started');
      await new Promise((done) => setTimeout(done, 100));
    }
  }

  const good = await stub(document(
    [surface('keys', '/api-keys'), surface('jobs', '/jobs')],
    [nav('keys', 'API Keys', 10), nav('jobs', 'Jobs', 20)],
  ));
  // A hostile service: markup in every text field (valid routes), and separately a script route.
  const hostile = await stub(document(
    [surface('x', '/ok', '<script>window.__pwned=1</script>')],
    [nav('x', '<img src=x onerror="window.__pwned=1">')],
  ));
  const scripted = await stub(document([surface('x', '/ok'), surface('bad', 'javascript:window.__pwned=1')], [nav('x', 'X')]));
  const escaping = await stub(document([surface('x', 'https://evil.example/steal')], [nav('x', 'Go')]));
  const wrongVersion = await stub({ ...document([], []), protocol: 'AppPort/ui/2' });
  const needsIdentity = await stub(document([surface('x', '/x')], [nav('x', 'X')], ['identity']));
  const broken = await stub('<html><script>window.__pwned=1</script></html>');
  const plain = await stub('{}');
  services.push(good, hostile, scripted, escaping, wrongVersion, needsIdentity, broken, plain);

  const register = (name, ...capabilities) => (service) => cli(endpoint, 'service', 'register', name, ...capabilities.flatMap((c) => ['--capability', c]), '--endpoint', service.url);
  register('good', 'AppPort/ui/1', 'appport.services@1')(good);
  register('hostile', 'AppPort/ui/1')(hostile);
  register('scripted', 'AppPort/ui/1')(scripted);
  register('escaping', 'AppPort/ui/1')(escaping);
  register('wrong-version', 'AppPort/ui/1')(wrongVersion);
  register('needs-identity', 'AppPort/ui/1')(needsIdentity);
  register('broken', 'AppPort/ui/1')(broken);
  register('plain', 'llm.generate@1')(plain);
  // Declared, then the service goes away.
  cli(endpoint, 'service', 'register', 'gone', '--capability', 'AppPort/ui/1', '--endpoint', 'http://127.0.0.1:1');

  const browser = await chromium.launch({ executablePath: chromiumPath });
  context.after(() => browser.close());
  const page = await browser.newPage();
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  page.on('dialog', (dialog) => { errors.push(`dialog: ${dialog.message()}`); dialog.dismiss(); });
  await page.goto(`${endpoint}/#/services`);
  await page.waitForSelector('tr:has-text("good") a', { timeout: 15000 }).catch(async (error) => { throw new Error(`${error.message}\nPAGE: ${(await page.textContent('body')).replace(/\s+/g, ' ').slice(0, 1500)}\nERRORS: ${errors.join(' | ')}`); });

  const row = (name) => page.locator('tbody tr', { has: page.locator(`strong:text-is("${name}")`) });
  await page.waitForFunction(() => ![...document.querySelectorAll('td')].some((td) => td.textContent.includes('Looking for its pages')));

  // The contributed pages are links to the service's own origin, opened apart from Compute.
  const links = row('good').locator('a');
  assert.deepEqual(await links.allTextContents(), ['API Keys', 'Jobs']);
  assert.equal(await links.first().getAttribute('href'), `${good.url}/api-keys`);
  assert.equal(await links.first().getAttribute('target'), '_blank');
  assert.match(await links.first().getAttribute('rel'), /noopener/);
  assert.match(await row('good').textContent(), /sign in there/);

  // Markup is text; the script route and the escaping route never become links.
  assert.equal(await row('hostile').locator('a').count(), 1);
  assert.equal(await row('hostile').locator('a').first().getAttribute('href'), `${hostile.url}/ok`);
  assert.equal(await row('hostile').locator('a').first().textContent(), '<img src=x onerror="window.__pwned=1">');
  assert.equal(await row('hostile').locator('img, script').count(), 0);
  assert.equal(await row('scripted').locator('a').count(), 0);
  assert.match(await row('scripted').textContent(), /invalid/);
  assert.equal(await row('escaping').locator('a').count(), 0);
  assert.match(await row('escaping').textContent(), /invalid/);

  // Every failure is a status, not a crash; and a non-AppPort service is just a service.
  assert.match(await row('wrong-version').textContent(), /invalid/);
  assert.match(await row('needs-identity').textContent(), /unsupported/);
  assert.match(await row('broken').textContent(), /invalid/);
  assert.match(await row('gone').textContent(), /unreachable/);
  assert.equal(await row('plain').locator('td').nth(4).textContent(), '');
  assert.equal(await row('plain').locator('a').count(), 0);

  // Nothing the services sent executed.
  assert.equal(await page.evaluate(() => window.__pwned), undefined);
  assert.deepEqual(errors, []);

  // Removing a service takes its links with it; the page still renders.
  const removed = spawnSync(compute, ['service', 'remove', 'good'], { encoding: 'utf8', env: { ...process.env, COMPUTE_DAEMON: endpoint } });
  assert.equal(removed.status, 0, removed.stderr);
  await page.goto(`${endpoint}/#/services`);
  await page.reload();
  await page.waitForSelector('strong:text-is("hostile")');
  assert.equal(await page.locator('strong:text-is("good")').count(), 0);
});
