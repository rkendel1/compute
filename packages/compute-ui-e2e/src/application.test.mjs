// Certify, in a real browser, that an application is its computer: launched
// the way an operator launches Compute (`compute up`), an application
// deployed with `compute deploy` appears as the computer environment
// `application-<name>`, with its process, endpoint, and log; its versions and
// rollouts are the project's; and a rollback made from the UI's version
// screen is the application's next version. There is no application-only
// screen, lifecycle, or record to find.
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

/// A runtime catalog whose shell and Python artifacts report the pinned
/// versions and run the host's own: placement evaluates the application
/// without a network. Never used outside tests.
function fixtureCatalog(directory, python) {
  const pinned = JSON.parse(readFileSync(resolve(process.cwd(), '../../distribution/runtime-lock.json'), 'utf8'));
  mkdirSync(directory, { recursive: true });
  const architecture = process.arch === 'arm64' ? 'aarch64' : 'x86_64';
  const fixture = (kind, script) => {
    const artifact = join(directory, `${kind}-fixture`);
    writeFileSync(artifact, script);
    chmodSync(artifact, 0o755);
    const locked = pinned.runtimes[kind];
    return {
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
    };
  };
  const catalog = join(directory, 'runtime-catalog.json');
  writeFileSync(catalog, JSON.stringify({
    schema_version: 2,
    runtimes: {
      wasm: pinned.runtimes.wasm,
      native: pinned.runtimes.native,
      shell: fixture('shell', `#!/bin/sh\ncase "$1" in --version|--help) echo "BusyBox v${pinned.runtimes.shell.version}"; exit 0;; esac\nexec /bin/sh "$@"\n`),
      python: fixture('python', `#!/bin/sh\ncase "$1" in --version|--help) echo "Python ${pinned.runtimes.python.version}"; exit 0;; esac\nexec ${python} "$@"\n`),
    },
  }));
  return catalog;
}

async function body(url) {
  try {
    const response = await fetch(url, { signal: AbortSignal.timeout(2000) });
    return response.ok ? (await response.text()).trim() : undefined;
  } catch {
    return undefined;
  }
}

async function until(what, check, timeout = 60_000) {
  const deadline = Date.now() + timeout;
  for (;;) {
    const value = await check();
    if (value) return value;
    assert.ok(Date.now() < deadline, `timed out waiting for ${what}`);
    await new Promise((done) => setTimeout(done, 200));
  }
}

test('an application is a computer: deploy, status, logs, and rollback in the UI', { timeout: 300_000 }, async (context) => {
  assert.ok(existsSync(compute), `build the CLI first: ${compute}`);
  assert.ok(chromiumPath, 'Chromium is required; set CHROMIUM_PATH');
  const python = spawnSync('sh', ['-c', 'command -v python3'], { encoding: 'utf8' }).stdout.trim();
  if (!python) {
    context.skip('python3 is not installed');
    return;
  }
  const root = mkdtempSync(join(tmpdir(), 'compute-application-'));
  const home = join(root, 'home');
  const listen = `127.0.0.1:${await freePort()}`;
  const endpoint = `http://${listen}`;
  const env = {
    ...process.env,
    COMPUTE_HOME: home,
    COMPUTE_LISTEN: listen,
    COMPUTE_TARGET_LISTEN: `127.0.0.1:${await freePort()}`,
    COMPUTE_NO_BROWSER: '1',
    COMPUTE_DAEMON: endpoint,
    COMPUTE_RUNTIME_CATALOG: fixtureCatalog(join(root, 'catalog'), python),
    COMPUTE_RUNTIME_STORE: join(root, 'runtimes'),
    COMPUTE_CAPABILITY_CACHE: join(root, 'capabilities.json'),
    NO_PROXY: '127.0.0.1,localhost',
    no_proxy: '127.0.0.1,localhost',
  };
  delete env.COMPUTE_CONFIG;
  delete env.COMPUTE_DAEMON_TOKEN;
  delete env.COMPUTE_POOL_CONFIG;
  const run = (...args) => {
    const result = spawnSync(compute, args, { env, encoding: 'utf8', cwd: root });
    assert.equal(result.status, 0, `compute ${args.join(' ')}: ${result.stderr}${result.stdout}`);
    return result.stdout;
  };
  const json = (...args) => JSON.parse(run(...args, '--json'));
  context.after(() => {
    spawnSync(compute, ['down'], { env });
    // The processes the application's computer started outlive its host.
    spawnSync('sh', ['-c', `find "$1" -path '*/.compute/processes/*.pid' | while read -r f; do kill -KILL -"$(cat "$f")" 2>/dev/null; done; true`, 'kill', root]);
    rmSync(root, { recursive: true, force: true });
  });

  assert.match(run('up'), /Compute is running/);

  // Deploy v1 from source, as a developer does.
  const source = join(root, 'hello');
  run('init', source, '--runtime', 'python');
  const v1 = json('deploy', source);
  assert.equal(v1.version, 1);
  assert.equal(v1.status, 'running');
  assert.equal(v1.environment, 'application-hello');
  assert.match(v1.deployment_id, /^rol_/);
  await until('v1 to answer', async () => (await body(v1.endpoint)) === 'Hello from Compute');
  const main = join(source, 'main.py');
  writeFileSync(main, readFileSync(main, 'utf8').replace('Hello from Compute', 'Hello v2'));
  const v2 = json('deploy', source);
  assert.equal(v2.version, 2);
  assert.equal(v2.endpoint, v1.endpoint, 'the endpoint is stable');
  await until('v2 to answer', async () => (await body(v1.endpoint)) === 'Hello v2');

  const browser = await chromium.launch({ executablePath: chromiumPath });
  context.after(() => browser.close());
  const page = await browser.newPage();
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));

  // Status: the application's environment is a running computer, holding
  // its process and serving its endpoint.
  await page.goto(`${endpoint}/#/work/application-hello`);
  await page.waitForSelector('.title .state:has-text("Running")', { timeout: 60_000 });
  await page.waitForSelector('[data-process="hello"] .state:has-text("Running")', { timeout: 60_000 });
  const served = await page.waitForSelector('[data-endpoint="hello"] a');
  assert.equal(await served.textContent(), v1.endpoint, 'the UI shows the endpoint the application API returned');
  assert.equal(json('application', 'status', 'hello').status, 'running');

  // Logs: the process's log, read in the computer.
  await body(v1.endpoint);
  await page.click('[data-process="hello"] button:has-text("Log")');
  await page.waitForSelector('[data-output] pre:has-text("GET / HTTP")', { timeout: 30_000 });
  const cliLogs = json('application', 'logs', 'hello');
  assert.match(cliLogs.stdout, /GET \/ HTTP/);

  // Versions: the application's versions are the project's versions and
  // rollouts; v2 is active in its environment.
  await page.goto(`${endpoint}/#/software/hello`);
  await page.waitForSelector('[data-software-view="hello"]');
  const history = json('application', 'history', 'hello');
  for (const version of history) {
    await page.waitForSelector(`[data-rollout="${version.deployment_id}"]`);
  }
  const v1Label = history.find((version) => version.version === 1).canonical.version;

  // Rollback from the UI's version screen: the application's next version,
  // back on v1's code.
  await page.click('[data-rollback]');
  await page.selectOption('#rollback-environment', 'application-hello');
  await page.selectOption('#rollback-to', v1Label);
  await page.click('dialog [data-go]');
  await page.waitForURL(/#\/operations\/rollout\//);
  const v3 = await until('the rollback to become v3', async () => {
    const latest = json('application', 'history', 'hello')[0];
    return latest.version === 3 && latest.state === 'active' ? latest : undefined;
  }, 90_000);
  assert.equal(v3.rollback_of, 1);
  assert.equal(v3.canonical.rollout_kind, 'rollback');
  assert.equal(v3.canonical.version, v1Label);
  await until('v1 code to answer again', async () => (await body(v1.endpoint)) === 'Hello from Compute');
  assert.equal(json('application', 'status', 'hello').version, 3);

  assert.deepEqual(errors, [], 'no script errors');
});
