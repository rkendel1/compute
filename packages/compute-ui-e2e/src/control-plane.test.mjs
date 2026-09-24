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

/// A project with one long-running Python service and one task.
function project(root) {
  const source = join(root, 'feltdb');
  mkdirSync(join(source, 'api'), { recursive: true });
  mkdirSync(join(source, 'migrate'), { recursive: true });
  writeFileSync(join(source, 'api/main.py'), 'import time\nprint("serving", flush=True)\nwhile True:\n    time.sleep(1)\n');
  writeFileSync(join(source, 'api/workload.json'), JSON.stringify({ version: '1', runtime: 'python', entrypoint: 'main.py', network: 'network' }));
  writeFileSync(join(source, 'migrate/main.py'), 'print("migrated")\n');
  writeFileSync(join(source, 'migrate/workload.json'), JSON.stringify({ version: '1', runtime: 'python', entrypoint: 'main.py', network: 'network' }));
  writeFileSync(join(source, 'compute.project.toml'), [
    '[project]', 'name = "feltdb"', '',
    '[[workload]]', 'name = "api"', 'kind = "service"', 'workload = "api/workload.json"', '',
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
  const daemon = spawn(compute, [
    'start', '--listen', `127.0.0.1:${port}`, '--state', 'memory',
    '--state-dir', join(root, 'node'), '--reconcile-interval-ms', '300',
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
  assert.equal(deployed.status, 'healthy');

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

  assert.deepEqual(errors, [], 'no script errors');
});
