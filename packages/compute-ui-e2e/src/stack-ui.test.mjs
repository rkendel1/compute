// The environment page on the shared Stack design system (ui/vendor/stack-ui),
// in a real browser against a real daemon: every status says what it is in
// words, the page says how fresh its data is, and it passes axe in light and
// dark.
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import test from 'node:test';
import { chromium } from 'playwright-core';

const compute = resolve(process.cwd(), '../../target/debug/compute');
const chromiumPath = process.env.CHROMIUM_PATH
  ?? ['/opt/pw-browsers/chromium', '/opt/pw-browsers/chromium-1194/chrome-linux/chrome', chromium.executablePath()].find((path) => path && existsSync(path));
const axeSource = readFileSync(createRequire(import.meta.url).resolve('axe-core/axe.min.js'), 'utf8');

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

function project(root) {
  const source = join(root, 'web');
  mkdirSync(join(source, 'api'), { recursive: true });
  writeFileSync(join(source, 'api/main.py'), [
    'import http.server, os',
    'class H(http.server.BaseHTTPRequestHandler):',
    '    def do_GET(self):',
    '        self.send_response(200); self.send_header("Content-Length", "2"); self.end_headers(); self.wfile.write(b"ok")',
    '    def log_message(self, *args): pass',
    'http.server.ThreadingHTTPServer(("127.0.0.1", int(os.environ["PORT"])), H).serve_forever()',
    '',
  ].join('\n'));
  writeFileSync(join(source, 'api/workload.json'), JSON.stringify({ version: '1', runtime: 'python', entrypoint: 'main.py', network: 'network' }));
  writeFileSync(join(source, 'compute.project.toml'), [
    '[project]', 'name = "web"', '',
    '[[workload]]', 'name = "api"', 'kind = "service"', 'workload = "api/workload.json"', 'ports = [{ name = "http", port = 8000 }]', '',
  ].join('\n'));
  return source;
}

/// (Copied from computer-reality.test.mjs.) A runtime catalog whose shell artifact reports the pinned version and
/// runs the host's /bin/sh: computers are prepared without a network, as
/// the Rust tests' fixture catalog does. Never used outside tests.
function fixtureCatalog(directory) {
  const pinned = JSON.parse(readFileSync(resolve(process.cwd(), '../../distribution/runtime-lock.json'), 'utf8'));
  const locked = pinned.runtimes.shell;
  const script = `#!/bin/sh\ncase "$1" in --version|--help) echo "BusyBox v${locked.version}"; exit 0;; esac\nexec /bin/sh "$@"\n`;
  mkdirSync(directory, { recursive: true });
  const artifact = join(directory, 'shell-fixture');
  writeFileSync(artifact, script);
  chmodSync(artifact, 0o755);
  const architecture = process.arch === 'arm64' ? 'aarch64' : 'x86_64';
  const catalog = join(directory, 'runtime-catalog.json');
  writeFileSync(catalog, JSON.stringify({
    schema_version: 2,
    runtimes: {
      wasm: pinned.runtimes.wasm,
      native: pinned.runtimes.native,
      shell: {
        version: locked.version,
        executable: locked.executable,
        artifacts: {
          [`linux-${architecture}`]: {
            url: `file://${artifact}`,
            sha256: createHash('sha256').update(script).digest('hex'),
            format: 'file',
            install: [{ source: 'artifact', destination: locked.executable }],
          },
        },
      },
    },
  }));
  return catalog;
}

/**
 * The shared StatusIndicator's structure, from its `stack-*` classes down. The
 * same literal is asserted in FeltDB Studio's browser test
 * (packages/studio/test/browser/control-plane.browser.test.mjs): one tone
 * renders one structure in both products.
 */
const POSITIVE_STATUS_STRUCTURE = {
  tag: 'span',
  classes: ['stack-status', 'stack-status--positive'],
  tone: 'positive',
  children: [
    { tag: 'span', classes: ['stack-status__glyph'], hidden: 'true', text: '●' },
    { tag: 'span', classes: ['stack-status__label'], hidden: null },
  ],
};

/** Runs in the page: a status element reduced to its shared structure. */
const statusStructure = (element) => ({
  tag: element.tagName.toLowerCase(),
  classes: [...element.classList].filter((name) => name.startsWith('stack-')),
  tone: element.getAttribute('data-tone'),
  children: [...element.children].map((child) => ({
    tag: child.tagName.toLowerCase(),
    classes: [...child.classList].filter((name) => name.startsWith('stack-')),
    hidden: child.getAttribute('aria-hidden'),
    ...(child.classList.contains('stack-status__glyph') ? { text: child.textContent } : {}),
  })),
});

async function axe(page) {
  await page.addScriptTag({ content: axeSource });
  const result = await page.evaluate(async () => window.axe.run(document, { resultTypes: ['violations'] }));
  return result.violations
    .filter((violation) => violation.impact === 'serious' || violation.impact === 'critical')
    .map((violation) => `${violation.id} (${violation.impact}): ${violation.nodes.slice(0, 3).map((node) => node.target.join(' ')).join(', ')}`);
}

test('the environment page speaks the shared status language', { timeout: 180_000 }, async (context) => {
  assert.ok(existsSync(compute), `build the CLI first: ${compute}`);
  assert.ok(chromiumPath, 'Chromium is required; set CHROMIUM_PATH');
  const root = mkdtempSync(join(tmpdir(), 'compute-stack-ui-'));
  const port = await freePort();
  const endpoint = `http://127.0.0.1:${port}`;
  const daemon = spawn(compute, [
    'start', '--listen', `127.0.0.1:${port}`, '--state', 'memory',
    '--state-dir', join(root, 'node'), '--reconcile-interval-ms', '300',
    '--ingress-http', `127.0.0.1:${await freePort()}`,
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
  cli(endpoint, 'environment', 'create', 'preprod');
  const deployed = cli(endpoint, 'deploy', 'web', '--environment', 'preprod', '--source', project(root), '--revision', 'v1', '--wait');
  assert.equal(deployed.status, 'complete');

  const browser = await chromium.launch({ executablePath: chromiumPath });
  context.after(() => browser.close());

  for (const colorScheme of ['light', 'dark']) {
    const page = await browser.newPage({ colorScheme });
    const errors = [];
    page.on('pageerror', (error) => errors.push(error.message));
    await page.goto(`${endpoint}/#/environments/preprod`);
    await page.waitForSelector('.title .state');

    // The design system arrived inside the existing assets.
    assert.equal(await page.evaluate(() => typeof window.StackUI), 'object');

    // Every status says what it is in words and carries its tone.
    const statuses = await page.$$eval('main .state', (nodes) => nodes.map((node) => ({
      text: node.innerText.replace(/[●◐▲✕○?]/g, '').trim(),
      tone: node.getAttribute('data-tone'),
    })));
    assert.ok(statuses.length >= 2, `found ${statuses.length} statuses`);
    for (const status of statuses) {
      assert.ok(status.text, 'a status has visible text');
      assert.ok(['positive', 'progress', 'attention', 'critical', 'neutral', 'unknown'].includes(status.tone), `tone ${status.tone}`);
    }

    // A running, healthy environment reads as positive, in words.
    await page.waitForSelector('.title .state[data-tone="positive"]', { timeout: 30_000 });
    assert.match(await page.textContent('.title .state'), /Healthy|Running/);

    // The page says how fresh its data is, from the daemon's own header.
    const freshness = await page.$eval('[data-freshness-of="environment"]', (node) => ({
      state: node.getAttribute('data-freshness'), text: node.innerText,
    }));
    assert.ok(['live', 'cached', 'stale'].includes(freshness.state), `freshness ${freshness.state}`);

    // The full environment ID is still on the page.
    const id = cli(endpoint, 'environment', 'status', 'preprod').environment_id;
    if (id) assert.match(await page.textContent('.title'), new RegExp(id));

    assert.deepEqual(await axe(page), [], `axe (${colorScheme})`);
    assert.deepEqual(errors, []);
    await page.close();
  }
});

test('a stale answer is labelled stale, with its as-of time', { timeout: 180_000 }, async (context) => {
  // The daemon only serves stale reads while FeltDB is unreachable, which a
  // memory-state daemon cannot be; the Rust test `reads_say_how_fresh_they_are`
  // (crates/compute-environment/tests/availability.rs) proves the header is
  // produced. Here the browser receives that exact header on a real response.
  const root = mkdtempSync(join(tmpdir(), 'compute-stack-ui-stale-'));
  const port = await freePort();
  const endpoint = `http://127.0.0.1:${port}`;
  const daemon = spawn(compute, [
    'start', '--listen', `127.0.0.1:${port}`, '--state', 'memory', '--state-dir', join(root, 'node'),
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
  cli(endpoint, 'environment', 'create', 'preprod');
  const browser = await chromium.launch({ executablePath: chromiumPath });
  context.after(() => browser.close());
  const page = await browser.newPage();
  const asOf = '2026-10-12T21:30:00+00:00';
  await page.route('**/environments/preprod', async (route) => {
    const response = await route.fetch();
    await route.fulfill({ response, headers: { ...response.headers(), 'x-compute-state': 'stale', 'x-compute-state-as-of': asOf } });
  });
  await page.goto(`${endpoint}/#/environments/preprod`);
  await page.waitForSelector('[data-freshness-of="environment"][data-freshness="stale"]');
  assert.equal(
    (await page.textContent('[data-freshness-of="environment"]')).replace(/[▲]/g, '').trim(),
    'Stale · as of 2026-10-12 21:30:00 UTC',
  );
  assert.equal(await page.getAttribute('[data-freshness-of="environment"] time', 'datetime'), '2026-10-12T21:30:00.000Z');
});

test('an environment that cannot be read says so, with what to do', { timeout: 120_000 }, async (context) => {
  const root = mkdtempSync(join(tmpdir(), 'compute-stack-ui-error-'));
  const port = await freePort();
  const endpoint = `http://127.0.0.1:${port}`;
  const daemon = spawn(compute, [
    'start', '--listen', `127.0.0.1:${port}`, '--state', 'memory', '--state-dir', join(root, 'node'),
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
  cli(endpoint, 'environment', 'create', 'preprod');
  const browser = await chromium.launch({ executablePath: chromiumPath });
  context.after(() => browser.close());
  const page = await browser.newPage();
  let fail = true;
  await page.route('**/environments/preprod', async (route) => {
    if (!fail) return route.continue();
    await route.fulfill({ status: 503, contentType: 'application/json', headers: { 'x-request-id': 'req_test' },
      body: JSON.stringify({ kind: 'state_unavailable', message: 'control state is unavailable' }) });
  });
  await page.goto(`${endpoint}/#/environments/preprod`);
  await page.waitForSelector('[data-environment-error="preprod"][role="alert"]');
  const text = await page.textContent('[data-environment-error="preprod"]');
  assert.match(text, /control state is unavailable/);
  assert.match(text, /state_unavailable/);
  assert.match(text, /req_test/);
  // Reads are safe to retry; Retry reads again and shows the environment.
  fail = false;
  await page.click('[data-environment-error="preprod"] button:has-text("Retry")');
  await page.waitForSelector('.title .state');
});

test('a computer environment speaks the shared status language, running and unreachable', { timeout: 300_000 }, async (context) => {
  assert.ok(existsSync(compute), `build the CLI first: ${compute}`);
  assert.ok(chromiumPath, 'Chromium is required; set CHROMIUM_PATH');
  const root = mkdtempSync(join(tmpdir(), 'compute-stack-ui-computer-'));
  const home = join(root, 'home');
  const listen = `127.0.0.1:${await freePort()}`;
  const target = `127.0.0.1:${await freePort()}`;
  const endpoint = `http://${listen}`;
  const env = {
    ...process.env,
    COMPUTE_HOME: home,
    COMPUTE_LISTEN: listen,
    COMPUTE_TARGET_LISTEN: target,
    COMPUTE_NO_BROWSER: '1',
    COMPUTE_DAEMON: endpoint,
    COMPUTE_RUNTIME_CATALOG: fixtureCatalog(join(root, 'catalog')),
    COMPUTE_RUNTIME_STORE: join(root, 'runtimes'),
  };
  delete env.COMPUTE_CONFIG;
  delete env.COMPUTE_DAEMON_TOKEN;
  const run = (...args) => {
    const result = spawnSync(compute, args, { env, encoding: 'utf8' });
    assert.equal(result.status, 0, `compute ${args.join(' ')}: ${result.stderr}${result.stdout}`);
    return result.stdout;
  };
  context.after(() => {
    spawnSync(compute, ['down'], { env });
    rmSync(root, { recursive: true, force: true });
  });
  assert.match(run('up'), /Compute is running/);

  const browser = await chromium.launch({ executablePath: chromiumPath });
  context.after(() => browser.close());
  const page = await browser.newPage();
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  await page.goto(`${endpoint}/`);
  await page.waitForSelector('[data-action="computer"]');
  await page.click('[data-action="computer"]');
  await page.fill('#environment-name', 'desk');
  await page.click('dialog button.primary:has-text("Create")');
  await page.goto(`${endpoint}/#/environments/desk`);
  const title = '.title .state';
  await page.waitForSelector(`${title}:has-text("Running")`, { timeout: 60_000 });

  // Running: the computer's observed reality, positive, in the shared structure.
  const header = page.locator(title);
  assert.equal(await header.getAttribute('data-field'), 'compute.computer.observed');
  assert.equal(await header.getAttribute('data-tone'), 'positive');
  const structure = await header.evaluate(statusStructure);
  assert.deepEqual(structure, POSITIVE_STATUS_STRUCTURE);
  // The machine facts are the shared MetadataList, with the status inside it.
  const facts = page.locator('dl.stack-meta[data-machine="computer"]');
  await facts.waitFor();
  const labels = await facts.locator('.stack-meta__label').allInnerTexts();
  for (const label of ['Status', 'Resources', 'Target', 'Lifetime', 'Machine', 'Placement', 'Contents']) {
    assert.ok(labels.map((text) => text.toLowerCase()).includes(label.toLowerCase()), `fact ${label} in ${labels}`);
  }
  assert.equal(await facts.locator('[data-goal]').count(), 1, 'contents say whether the goal holds');
  for (const status of await page.$$eval('main .state', (nodes) => nodes.map((node) => ({
    text: node.innerText.replace(/[●◐▲✕○?]/g, '').trim(), tone: node.getAttribute('data-tone'),
  })))) {
    assert.ok(status.text, 'a status has visible text');
    assert.ok(status.tone, 'a status carries its tone');
  }
  assert.deepEqual(await axe(page), [], 'axe (light, computer running)');
  const dark = await browser.newPage({ colorScheme: 'dark' });
  await dark.goto(`${endpoint}/#/environments/desk`);
  await dark.waitForSelector(`${title}:has-text("Running")`, { timeout: 60_000 });
  assert.deepEqual(await axe(dark), [], 'axe (dark, computer running)');
  await dark.close();

  // Unreachable: unknown, explained by the reality panel, and never called a failure.
  const hostPid = Number(readFileSync(join(home, 'computers', 'host.pid'), 'utf8').trim());
  process.kill(hostPid, 'SIGKILL');
  await page.waitForSelector('[data-reality="unreachable"]', { timeout: 60_000 });
  await page.waitForSelector(`${title}[data-tone="unknown"]`, { timeout: 30_000 });
  assert.match(await page.textContent(title), /Unreachable/);
  assert.equal(await page.locator('.stack-error[role="alert"]').count(), 0, 'an unreachable computer is not shown as failed');
  assert.deepEqual(await axe(page), [], 'axe (light, computer unreachable)');
  assert.deepEqual(errors, []);
});
