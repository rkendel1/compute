// The control plane in the shared Stack shell (ui/vendor/stack-ui), in a real
// browser against a real daemon: every surface renders inside one frame with
// its current page marked, navigation moves focus to the new page, the theme
// preference holds Compute's own colours too, tabs and table rows work by
// keyboard, and the main pages pass axe in light and dark and at 640px
// (1280px at 200% zoom).
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
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

async function axe(page) {
  await page.addScriptTag({ content: axeSource });
  const result = await page.evaluate(async () => window.axe.run(document, { resultTypes: ['violations'] }));
  return result.violations
    .filter((violation) => violation.impact === 'serious' || violation.impact === 'critical')
    .map((violation) => `${violation.id} (${violation.impact}): ${violation.nodes.slice(0, 4).map((node) => {
      const data = node.any?.[0]?.data;
      return `${node.target.join(' ')}${data?.fgColor ? ` [${data.fgColor} on ${data.bgColor} = ${data.contrastRatio}]` : ''}`;
    }).join(', ')}`);
}

test('the control plane in the shared shell', { timeout: 300_000 }, async (context) => {
  assert.ok(existsSync(compute), `build the CLI first: ${compute}`);
  assert.ok(chromiumPath, 'Chromium is required; set CHROMIUM_PATH');
  const root = mkdtempSync(join(tmpdir(), 'compute-shell-'));
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
  assert.equal(cli(endpoint, 'deploy', 'web', '--environment', 'preprod', '--source', project(root), '--revision', 'v1', '--wait').status, 'complete');

  const browser = await chromium.launch({ executablePath: chromiumPath });
  context.after(() => browser.close());

  await context.test('every surface renders inside one shell, with its current page and trail', async () => {
    const page = await browser.newPage();
    const errors = [];
    page.on('pageerror', (error) => errors.push(error.message));
    const surfaces = [
      ['#/', 'home', null],
      ['#/software', 'software', null],
      ['#/environments', 'environments', null],
      ['#/environments/preprod', 'environments', ['Environments', 'preprod']],
      ['#/environments/preprod/projects/web/overview', 'environments', ['Environments', 'preprod', 'web']],
      ['#/projects', 'projects', null],
      ['#/domains', 'domains', null],
      ['#/services', 'services', null],
      ['#/events', 'events', null],
    ];
    for (const [hash, nav, crumbs] of surfaces) {
      await page.goto(`${endpoint}/${hash}`);
      await page.waitForFunction(() => document.querySelector('#view') && document.querySelector('#view').children.length > 0);
      assert.equal(await page.locator('.stack-shell').count(), 1, `${hash}: one shell`);
      assert.equal(await page.locator('main').count(), 1, `${hash}: one main landmark`);
      assert.equal(await page.getAttribute('main', 'id'), 'view');
      const current = await page.$$eval('.stack-shell__nav [aria-current="page"]', (nodes) => nodes.map((node) => node.dataset.nav));
      assert.deepEqual(current, [nav], `${hash}: current navigation item`);
      if (crumbs) {
        const trail = await page.$$eval('nav.stack-breadcrumbs li', (items) => items.map((item) => item.innerText.replace(/^\/\s*/, '').trim()));
        assert.deepEqual(trail, crumbs, `${hash}: breadcrumbs`);
        assert.equal(await page.getAttribute('nav.stack-breadcrumbs li:last-child [aria-current]', 'aria-current'), 'page');
        assert.equal(await page.getAttribute('nav.stack-breadcrumbs a >> nth=0', 'href'), '#/environments');
      }
    }
    assert.deepEqual(errors, []);
    await page.close();
  });

  await context.test('each mode shows only its own navigation', async () => {
    const page = await browser.newPage();
    await page.goto(`${endpoint}/#/work`);
    await page.waitForSelector('body[data-mode="work"]');
    const visible = () => page.$$eval('.stack-shell__nav a', (links) => links.filter((link) => getComputedStyle(link).display !== 'none').map((link) => link.dataset.nav));
    assert.deepEqual(await visible(), ['home', 'software', 'work']);
    await page.click('[data-mode-link="manage"]');
    await page.waitForSelector('body[data-mode="manage"]');
    assert.deepEqual(await visible(), ['home', 'software', 'environments', 'projects', 'domains', 'services', 'events']);
    await page.close();
  });

  await context.test('the skip link reaches the page without changing the route, and following a link starts at the new page', async () => {
    const page = await browser.newPage();
    await page.goto(`${endpoint}/#/environments/preprod`);
    await page.waitForSelector('.title .state');
    await page.keyboard.press('Tab');
    assert.equal(await page.evaluate(() => document.activeElement.className), 'stack-shell__skip');
    await page.keyboard.press('Enter');
    await page.waitForFunction(() => document.activeElement.id === 'view');
    // Compute routes by hash: following "#view" as a link would leave this page.
    assert.equal(await page.evaluate(() => location.hash), '#/environments/preprod');
    assert.ok(await page.isVisible('.title .state'), 'still on the environment page');
    await page.click('.stack-shell__nav a[data-nav="software"]');
    await page.waitForFunction(() => location.hash === '#/software' && document.activeElement.id === 'view');
    await page.close();
  });

  await context.test('the theme preference holds, and Compute\'s own colours follow it', async () => {
    const page = await browser.newPage({ colorScheme: 'light' });
    await page.goto(`${endpoint}/#/environments/preprod`);
    await page.waitForSelector('.title .state');
    const colours = () => page.evaluate(() => ({
      own: getComputedStyle(document.documentElement).getPropertyValue('--bg').trim(),
      body: getComputedStyle(document.body).backgroundColor,
      bar: getComputedStyle(document.querySelector('.stack-shell__bar')).backgroundColor,
    }));
    assert.equal((await colours()).own, '#f6f7f9');
    await page.selectOption('#view-theme', 'dark');
    let now = await colours();
    assert.equal(now.own, '#0f1115', 'Compute\'s palette is dark');
    assert.equal(now.bar, 'rgb(23, 26, 32)', 'the shared surfaces are dark');
    await page.reload();
    await page.waitForSelector('.title .state');
    assert.equal(await page.inputValue('#view-theme'), 'dark');
    assert.equal((await colours()).own, '#0f1115', 'the choice survives a reload');
    await page.selectOption('#view-theme', 'system');
    assert.equal((await colours()).own, '#f6f7f9');
    await page.close();

    const dark = await browser.newPage({ colorScheme: 'dark' });
    await dark.goto(`${endpoint}/#/environments/preprod`);
    await dark.waitForSelector('.title .state');
    assert.equal((await dark.evaluate(() => getComputedStyle(document.documentElement).getPropertyValue('--bg').trim())), '#0f1115');
    await dark.selectOption('#view-theme', 'light');
    now = await dark.evaluate(() => ({
      own: getComputedStyle(document.documentElement).getPropertyValue('--bg').trim(),
      bar: getComputedStyle(document.querySelector('.stack-shell__bar')).backgroundColor,
    }));
    assert.equal(now.own, '#f6f7f9', 'a light choice wins over a dark system');
    assert.equal(now.bar, 'rgb(255, 255, 255)');
    await dark.close();
  });

  await context.test('project tabs move by arrow keys and open by Enter', async () => {
    const page = await browser.newPage();
    await page.goto(`${endpoint}/#/environments/preprod/projects/web/overview`);
    await page.waitForSelector('[role="tablist"].tabs');
    assert.equal(await page.getAttribute('.tabs [role="tab"][aria-selected="true"]', 'id'), 'project-tab-Overview');
    await page.focus('#project-tab-Overview');
    await page.keyboard.press('ArrowRight');
    assert.equal(await page.evaluate(() => document.activeElement.id), 'project-tab-Workloads');
    assert.match(await page.evaluate(() => location.hash), /\/overview$/, 'moving focus does not navigate');
    await page.keyboard.press('Enter');
    await page.waitForFunction(() => /\/workloads$/.test(location.hash));
    await page.waitForSelector('#project-tab-Workloads[aria-selected="true"]');
    assert.equal(await page.getAttribute('#project-panel', 'aria-labelledby'), 'project-tab-Workloads');
    // Choosing a section keeps the keyboard user on its tab.
    await page.waitForFunction(() => document.activeElement && document.activeElement.id === 'project-tab-Workloads');
    await page.keyboard.press('ArrowLeft');
    assert.equal(await page.evaluate(() => document.activeElement.id), 'project-tab-Overview');
    await page.close();
  });

  await context.test('environment tables: one cell per header, rows open by keyboard', async () => {
    const page = await browser.newPage();
    await page.goto(`${endpoint}/#/environments/preprod`);
    await page.waitForSelector('tr[data-project="web"]');
    const shapes = await page.$$eval('.stack-table__table', (tables) => tables.map((table) => ({
      headers: table.querySelectorAll('thead th').length,
      rows: Array.from(table.querySelectorAll('tbody tr')).map((row) => row.children.length),
    })));
    assert.ok(shapes.length >= 1);
    for (const shape of shapes) for (const cells of shape.rows) assert.equal(cells, shape.headers);
    await page.focus('tr[data-project="web"]');
    await page.keyboard.press('Enter');
    await page.waitForFunction(() => location.hash === '#/environments/preprod/projects/web');
    await page.close();
  });

  await context.test('axe on the main pages, light and dark, and no page scroll at 640px', async () => {
    for (const colorScheme of ['light', 'dark']) {
      for (const width of [1280, 640]) {
        const page = await browser.newPage({ colorScheme, viewport: { width, height: 900 } });
        for (const hash of ['#/', '#/environments', '#/environments/preprod', '#/environments/preprod/projects/web/overview']) {
          await page.goto(`${endpoint}/${hash}`);
          await page.waitForFunction(() => document.querySelector('#view') && document.querySelector('#view').children.length > 0);
          await page.waitForTimeout(300);
          assert.deepEqual(await axe(page), [], `axe ${hash} (${colorScheme}, ${width}px)`);
          const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
          assert.ok(overflow <= 0, `${hash} at ${width}px overflows by ${overflow}px`);
        }
        await page.close();
      }
    }
  });
});
