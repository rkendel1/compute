// Certify the control-plane UI in a real browser against a real daemon.
// The UI must operate Compute through the Compute API alone; this test
// drives it the way an operator would.
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import test from 'node:test';
import { chromium } from 'playwright-core';

const compute = resolve(process.cwd(), '../../target/debug/compute');
const chromiumPath = process.env.CHROMIUM_PATH
  ?? ['/opt/pw-browsers/chromium', '/opt/pw-browsers/chromium-1194/chrome-linux/chrome'].find(existsSync);

function freePort() {
  return new Promise((resolvePort, reject) => {
    const server = createServer();
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const { port } = server.address();
      server.close(() => resolvePort(port));
    });
  });
}

function cli(endpoint, ...args) {
  const result = spawnSync(compute, [...args, '--daemon', endpoint, '--json'], { encoding: 'utf8' });
  assert.equal(result.status, 0, `compute ${args.join(' ')}: ${result.stderr}${result.stdout}`);
  return JSON.parse(result.stdout);
}

/// A project with one Python HTTP service and one task.
function project(root, version = 'v0.11.7') {
  const source = join(root, 'feltdb');
  mkdirSync(join(source, 'api'), { recursive: true });
  mkdirSync(join(source, 'migrate'), { recursive: true });
  writeFileSync(join(source, 'api/main.py'), [
    'import http.server, os',
    'print("serving", flush=True)',
    'class H(http.server.BaseHTTPRequestHandler):',
    '    def do_GET(self):',
    `        body = b"${version}"`,
    '        self.send_response(200); self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)',
    '    def log_message(self, *args): pass',
    'http.server.ThreadingHTTPServer(("127.0.0.1", int(os.environ["PORT"])), H).serve_forever()',
    '',
  ].join('\n'));
  writeFileSync(join(source, 'api/workload.json'), JSON.stringify({ version: '1', runtime: 'python', entrypoint: 'main.py', network: 'network' }));
  writeFileSync(join(source, 'migrate/main.py'), 'print("migrated")\n');
  writeFileSync(join(source, 'migrate/workload.json'), JSON.stringify({ version: '1', runtime: 'python', entrypoint: 'main.py', network: 'network' }));
  writeFileSync(join(source, 'compute.project.toml'), [
    '[project]', 'name = "feltdb"', '',
    '[[workload]]', 'name = "api"', 'kind = "service"', 'workload = "api/workload.json"', 'ports = [{ name = "http", port = 8000 }]', '',
    '[[workload]]', 'name = "migrate"', 'kind = "task"', 'workload = "migrate/workload.json"', '',
  ].join('\n'));
  return source;
}

test('an operator runs Compute from the control-plane UI', { timeout: 180_000 }, async (context) => {
  assert.ok(existsSync(compute), `build the CLI first: ${compute}`);
  assert.ok(chromiumPath, 'Chromium is required; set CHROMIUM_PATH');
  const root = mkdtempSync(join(tmpdir(), 'compute-ui-'));
  const port = await freePort();
  const endpoint = `http://127.0.0.1:${port}`;
  const ingress = await freePort();
  const daemon = spawn(compute, [
    'start', '--listen', `127.0.0.1:${port}`, '--state', 'memory',
    '--state-dir', join(root, 'node'), '--reconcile-interval-ms', '300',
    '--ingress-http', `127.0.0.1:${ingress}`,
  ], { stdio: 'ignore' });
  context.after(() => {
    daemon.kill('SIGINT');
    rmSync(root, { recursive: true, force: true });
  });
  for (let attempt = 0; ; attempt += 1) {
    try { await fetch(`${endpoint}/status`); break; } catch {
      assert.ok(attempt < 100, 'the daemon started');
      await new Promise((done) => setTimeout(done, 100));
    }
  }

  // Set up through the CLI; the UI must see it.
  for (const name of ['preprod', 'production']) cli(endpoint, 'environment', 'create', name);
  const deployed = cli(endpoint, 'deploy', 'feltdb', '--environment', 'preprod', '--source', project(root), '--revision', 'v0.11.7', '--wait');
  assert.equal(deployed.status, 'complete');

  const browser = await chromium.launch({ executablePath: chromiumPath });
  context.after(() => browser.close());
  const page = await browser.newPage();
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));

  // Environments first.
  await page.goto(`${endpoint}/`);
  await page.waitForSelector('[data-environment="preprod"]');
  assert.match(await page.textContent('[data-environment="preprod"]'), /1 projects · 2 workloads/);
  assert.match(await page.textContent('[data-environment="preprod"]'), /Healthy|Running/);
  assert.match(await page.textContent('[data-environment="production"]'), /0 projects/);

  // Environment detail → project detail.
  await page.click('[data-environment="preprod"]');
  await page.waitForSelector('tr[data-project="feltdb"]');
  assert.match(await page.textContent('tr[data-project="feltdb"]'), /v0\.11\.7/);
  await page.click('tr[data-project="feltdb"]');
  await page.waitForSelector('text=FELTDB / PREPROD');
  for (const tab of ['Workloads', 'Deployments', 'Logs', 'Resources', 'Configuration', 'Receipts', 'Events']) {
    await page.click(`.tabs button:has-text("${tab}")`);
    await page.waitForSelector(`.tabs button.active:has-text("${tab}")`);
  }

  // Run a task from the UI.
  await page.click('.tabs button:has-text("Workloads")');
  await page.click('tr[data-workload="migrate"] button:has-text("Run")');
  await page.waitForSelector('dialog pre:has-text("migrated")');
  await page.click('dialog button:has-text("Close")');

  // Stop asks first, and says what it will not affect.
  await page.click('.title button:has-text("Stop")');
  await page.waitForSelector('dialog[open]');
  const impact = await page.textContent('dialog');
  assert.match(impact, /This will affect:[\s\S]*feltdb \/ preprod · api/);
  assert.match(impact, /It will not affect:[\s\S]*Compute daemon/);
  await page.click('dialog [data-confirm]');
  await page.waitForSelector('.title .state:has-text("Stopped")');
  assert.equal(cli(endpoint, 'project', 'status', 'feltdb', '--environment', 'preprod').desired_state, 'stopped');

  // A change made elsewhere (the CLI) appears live, through the event stream.
  cli(endpoint, 'project', 'start', 'feltdb', '--environment', 'preprod');
  await page.waitForSelector('.title .state:has-text("Healthy"), .title .state:has-text("Running")', { timeout: 30_000 });

  // Promote the exact revision to production from the UI.
  await page.click('.title button:has-text("Promote")');
  await page.selectOption('#promote-target', 'production');
  await page.click('dialog [data-apply]');
  // The release page follows it through every step to complete.
  await page.waitForSelector('.progress', { timeout: 30_000 });
  await page.waitForSelector('.title .state:has-text("Complete")', { timeout: 60_000 });
  assert.match(await page.textContent('.progress'), /Pending[\s\S]*Network ready[\s\S]*Switching[\s\S]*Draining[\s\S]*Complete/);
  await page.waitForSelector('tr[data-instance] .state:has-text("Serving")');
  await page.click('.title button:has-text("Receipt")');
  await page.waitForSelector('dialog pre:has-text("compute.deployment-receipt@1")');
  await page.click('dialog button:has-text("Close")');
  await page.goto(`${endpoint}/#/environments/production`);
  await page.waitForSelector('tr[data-project="feltdb"]');
  assert.match(await page.textContent('tr[data-project="feltdb"]'), /v0\.11\.7/);
  const production = cli(endpoint, 'project', 'status', 'feltdb', '--environment', 'production');
  const preprod = cli(endpoint, 'project', 'status', 'feltdb', '--environment', 'preprod');
  assert.equal(production.revision_digest, preprod.revision_digest, 'production runs the promoted revision');

  // Membership from the project's side: remove from production.
  await page.goto(`${endpoint}/#/projects/feltdb`);
  await page.waitForSelector('[data-environment="production"] input[type=checkbox]:checked');
  await page.click('[data-environment="production"] input[type=checkbox]');
  await page.waitForSelector('dialog[open]');
  assert.match(await page.textContent('dialog'), /It will not affect:[\s\S]*feltdb \/ preprod/);
  await page.click('dialog [data-confirm]');
  await page.waitForSelector('[data-environment="production"] input[type=checkbox]:not(:checked)');
  assert.equal(cli(endpoint, 'project', 'list').find((item) => item.name === 'feltdb').environments.length, 1);

  // Release a new revision, then roll it back from its release page.
  const second = cli(endpoint, 'deploy', 'feltdb', '--environment', 'preprod', '--source', project(root, 'v0.11.8'), '--revision', 'v0.11.8', '--wait');
  assert.equal(second.status, 'complete');
  await page.goto(`${endpoint}/#/deployments/${second.deployment_id}`);
  await page.waitForSelector('.title .state:has-text("Complete")');
  await page.click('[data-rollback]');
  await page.waitForSelector('dialog[open]');
  assert.match(await page.textContent('dialog'), /A new release of v0\.11\.7 to preprod/);
  await page.click('dialog [data-confirm]');
  await page.waitForFunction((id) => location.hash.startsWith('#/deployments/') && !location.hash.includes(id), second.deployment_id);
  await page.waitForSelector('.title .state:has-text("Complete")', { timeout: 60_000 });
  assert.equal(cli(endpoint, 'project', 'status', 'feltdb', '--environment', 'preprod').revision, 'v0.11.7');

  // Domains: add one to preprod, inspect it, remove it.
  await page.goto(`${endpoint}/#/domains`);
  await page.click('.title button:has-text("Add domain")');
  await page.fill('#domain-name', 'feltdb.preprod.test');
  await page.selectOption('#domain-environment', 'preprod');
  await page.selectOption('#domain-project', 'feltdb');
  await page.click('dialog [data-apply]');
  await page.waitForSelector('h1:has-text("feltdb.preprod.test")');
  assert.match(await page.textContent('main'), /preprod\/feltdb\/api\/http/);
  await page.waitForSelector('.title .state:has-text("Healthy")', { timeout: 30_000 });
  assert.match(await page.textContent('main'), /DNS is managed outside Compute/);
  assert.match(await page.textContent('main'), /TLS is disabled/);
  const domains = cli(endpoint, 'domain', 'list');
  assert.equal(domains[0].routing.status, 'healthy');
  await page.click('.title button:has-text("Remove")');
  await page.click('dialog [data-confirm]');
  await page.waitForSelector('text=No domains yet.');

  assert.deepEqual(errors, [], 'no script errors');
});
