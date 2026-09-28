// Certify, in a real browser, that the UI shows what Compute established
// about a computer, never what its environment merely wants: launched the
// way an operator launches Compute (`compute up`), a computer is created
// from the UI and runs; its target goes away (unreachable), comes back
// (running again, the same machine), and comes back without its sessions
// (lost); the lost computer is replaced from the UI.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import test from 'node:test';
import { chromium } from 'playwright-core';

const compute = resolve(process.cwd(), '../../target/debug/compute');
// $CHROMIUM_PATH, a preinstalled Playwright Chromium, or the one
// `npx playwright-core install chromium` put in Playwright's cache.
const chromiumPath = process.env.CHROMIUM_PATH
  ?? ['/opt/pw-browsers/chromium', '/opt/pw-browsers/chromium-1194/chrome-linux/chrome', chromium.executablePath()].find((path) => path && existsSync(path));

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

/// A runtime catalog whose shell artifact reports the pinned version and
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

test('the UI shows unreachable, lost, and recovered computers as they are', { timeout: 300_000 }, async (context) => {
  assert.ok(existsSync(compute), `build the CLI first: ${compute}`);
  assert.ok(chromiumPath, 'Chromium is required; set CHROMIUM_PATH');
  const root = mkdtempSync(join(tmpdir(), 'compute-reality-'));
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
  const hostPid = () => Number(readFileSync(join(home, 'computers', 'host.pid'), 'utf8').trim());
  const stopHost = async () => {
    process.kill(hostPid(), 'SIGKILL');
    for (let attempt = 0; attempt < 100; attempt += 1) {
      try { await fetch(`http://${target}/compute/health`); } catch { return; }
      await new Promise((done) => setTimeout(done, 100));
    }
    assert.fail('the computer host kept answering');
  };
  context.after(() => {
    spawnSync(compute, ['down'], { env });
    rmSync(root, { recursive: true, force: true });
  });

  // Launch: one command, and the host trusts only this control plane.
  const said = run('up');
  assert.match(said, /Compute is running/);
  assert.match(said, /authenticates to it with a target credential/);
  const anonymous = await fetch(`http://${target}/compute/health`, { headers: { 'X-Compute-Protocol': 'compute.remote@1' } });
  assert.equal(anonymous.status, 401, 'the computer host refuses anonymous callers');

  const browser = await chromium.launch({ executablePath: chromiumPath });
  context.after(() => browser.close());
  const page = await browser.newPage();
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));

  // Home, then create a computer from it.
  await page.goto(`${endpoint}/`);
  await page.waitForSelector('[data-action="computer"]');
  // The control plane says its state is local development, not production.
  await page.waitForSelector('#daemon [data-durability="local-development"]');
  await page.click('[data-action="computer"]');
  await page.fill('#environment-name', 'desk');
  await page.click('dialog button.primary:has-text("Create")');
  await page.goto(`${endpoint}/#/environments/desk`);
  const title = '.title .state';
  await page.waitForSelector(`${title}:has-text("Running")`, { timeout: 60_000 });
  assert.equal(await page.$('[data-reality]'), null, 'a running computer needs no explanation');
  const view = JSON.parse(run('environment', 'computer', 'desk', '--json'));
  assert.equal(view.reality.observed, 'running');
  const session = view.session_id;

  // The target goes away: unreachable, still wanted, recovering.
  await stopHost();
  await page.waitForSelector('[data-reality="unreachable"]', { timeout: 60_000 });
  assert.match(await page.textContent(title), /Unreachable/);
  const unreachable = await page.textContent('[data-reality="unreachable"]');
  assert.match(unreachable, /wanted: running/);
  assert.match(unreachable, /keeps checking/);
  await page.waitForSelector('[data-recovering]');
  assert.equal(JSON.parse(run('environment', 'computer', 'desk', '--json')).reality.observed, 'unreachable');

  // The target comes back (`compute` starts the host again): the same
  // machine, running.
  run('up');
  await page.waitForSelector(`${title}:has-text("Running")`, { timeout: 60_000 });
  await page.waitForFunction(() => !document.querySelector('[data-reality]'));
  assert.equal(JSON.parse(run('environment', 'computer', 'desk', '--json')).session_id, session);

  // The target comes back without its sessions: lost, with what to do.
  await stopHost();
  await page.waitForSelector('[data-reality="unreachable"]', { timeout: 60_000 });
  rmSync(join(home, 'computers', 'sessions'), { recursive: true, force: true });
  run('up');
  await page.waitForSelector('[data-reality="lost"]', { timeout: 60_000 });
  assert.match(await page.textContent(title), /Lost/);
  const lost = await page.textContent('[data-reality="lost"]');
  assert.match(lost, /wanted: running/);
  assert.match(lost, /replace the computer/);
  // The environment list agrees.
  await page.goto(`${endpoint}/#/work`);
  await page.waitForSelector('[data-environment="desk"] .state:has-text("Lost")');
  await page.goto(`${endpoint}/#/environments/desk`);

  // Replace it from the UI: a new machine, running.
  await page.click('[data-reality="lost"] [data-reality-action="replace"]');
  await page.click('dialog button:has-text("Replace")');
  await page.waitForSelector(`${title}:has-text("Running")`, { timeout: 90_000 });
  const replaced = JSON.parse(run('environment', 'computer', 'desk', '--json'));
  assert.equal(replaced.reality.observed, 'running');
  assert.notEqual(replaced.session_id, session);
  const events = run('events', '--environment', 'desk');
  for (const kind of ['computer.unreachable', 'computer.recovered', 'computer.lost', 'computer.replacing']) {
    assert.match(events, new RegExp(kind.replace('.', '\\.')));
  }
  assert.deepEqual(errors, [], 'no script errors');
});
