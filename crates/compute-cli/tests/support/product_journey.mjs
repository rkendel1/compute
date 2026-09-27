// The complete product, from `compute` to rollback, in a browser, with a
// screenshot of every surface. Nothing here uses the CLI after launch.
//
// node product_journey.mjs '<json: module, chromium, base, shots, repos, ports, commit>'
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdirSync } from 'node:fs';
import { join } from 'node:path';

const config = JSON.parse(process.argv[2]);
const { chromium } = await import(config.module);
mkdirSync(config.shots, { recursive: true });
const browser = await chromium.launch({ executablePath: config.chromium });
const page = await browser.newPage({ viewport: { width: 1360, height: 900 } });
const errors = [];
page.on('pageerror', (error) => errors.push(error.message));
page.setDefaultTimeout(120000);
let shots = 0;

async function shot(name) {
  shots += 1;
  await page.addStyleTag({ content: '.bar { position: static !important; } #toast { display: none !important; }' });
  await page.waitForTimeout(250);
  await page.screenshot({ path: join(config.shots, `${String(shots).padStart(2, '0')}-${name}.png`), fullPage: true });
}

const api = (method, path, body) => page.evaluate(async ({ method, path, body }) => {
  const response = await fetch(path, { method, headers: { 'Content-Type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body) });
  const text = await response.text();
  return { status: response.status, body: text ? JSON.parse(text) : null };
}, { method, path, body });

async function until(what, check, timeout = 180000) {
  const deadline = Date.now() + timeout;
  let last;
  for (;;) {
    last = await check();
    if (last) return last;
    assert.ok(Date.now() < deadline, `timed out waiting for ${what}`);
    await page.waitForTimeout(250);
  }
}

const computer = (name) => api('GET', `/environments/${name}/computer`).then((result) => result.body);
const converged = (name) => until(`${name} to converge`, async () => {
  const view = await computer(name);
  return view && view.converged && view.status === 'running' ? view : null;
});
async function go(path) {
  await page.goto(`${config.base}/${path}`);
  await page.waitForTimeout(300);
}
async function tile(action) {
  await go('#/');
  await page.click(`[data-action="${action}"]`);
}
async function runProject(url, computerName, port, { temporary = false } = {}) {
  await page.fill('#run-url', url);
  if (computerName) {
    if (await page.locator('[data-choice="new"]').count()) await page.check('[data-choice="new"]');
    await page.fill('#run-computer-name', computerName);
    if (temporary) await page.selectOption('#run-lifetime', 'ephemeral');
  }
}

async function journey() {
// 1. `compute` opened the control plane: what do you want to do?
await go('');
await page.waitForSelector('[data-action="run"]');
await shot('home');

// 2–4. Run a project: a repository, a new computer, the proposal, GO.
await tile('run');
await page.waitForSelector('[data-step="source"]');
await runProject(config.repos.web, 'dev', config.ports.web);
await shot('run-a-project');
await page.click('[data-inspect]');
await page.waitForSelector('[data-step="proposal"]', { timeout: 180000 });
await page.fill('[data-proposed-process="web-app"] input[type="number"]', String(config.ports.web));
await page.press('[data-proposed-process="web-app"] input[type="number"]', 'Tab');
await page.waitForSelector(`text=on port ${config.ports.web}`);
await shot('proposed-assembly');
await page.click('[data-step="proposal"] [data-go]');

// 5–6. It builds and runs, on the same computer, in Work.
await page.waitForSelector('[data-view="work"]');
await shot('work-reconciling');
let dev = await converged('dev');
assert.equal(dev.observed.processes['web-app'].state, 'running');
assert.equal(dev.observed.builds['web-app'].evidence.outcome, 'succeeded');
await go('#/work/dev');
await page.waitForSelector('[data-process="web-app"]');
await shot('work-application-running');
const machine = dev.machine.resource;

// 7–9. Another project on the same computer.
await go('#/work/dev');
await page.click('text=Add another project');
await page.waitForSelector('[data-step="source"]');
await page.fill('#run-url', config.repos.api);
await page.selectOption('#run-existing', 'dev');
await page.click('[data-inspect]');
await page.waitForSelector('[data-step="proposal"]');
await page.fill('[data-proposed-process="api"] input[type="number"]', String(config.ports.api));
await page.click('[data-step="proposal"] [data-go]');
await page.waitForSelector('[data-view="work"]');
dev = await converged('dev');
assert.deepEqual(Object.keys(dev.observed.processes).sort(), ['api', 'web-app']);
assert.equal(dev.machine.resource, machine, 'both projects on the same computer');
await go('#/work/dev');
await page.waitForSelector('[data-process="api"]');
await shot('two-projects-one-computer');

// A first version, deployed to test and promoted to production.
await go('#/');
await page.click('[data-action="computer"]');
for (const name of ['test', 'production']) {
  if (name === 'production') { await go('#/'); await page.click('[data-action="computer"]'); }
  await page.fill('#environment-name', name);
  await page.fill('#environment-cpu', '1');
  await page.fill('#environment-memory', '1');
  if (name === 'test') await shot('create-a-computer');
  await page.click('dialog button.primary');
  await converged(name);
}
async function publish(expected) {
  await go('#/software/web-app');
  await page.click('[data-publish]');
  await page.waitForSelector('#publish-version');
  assert.equal(await page.inputValue('#publish-version'), expected);
  if (expected === '0.1.1') await shot('publish-review');
  await page.click('dialog [data-go]');
  await page.waitForSelector('[data-operation="published"]', { timeout: 180000 });
}
async function deploy(version, environment) {
  await go('#/software/web-app');
  await page.click('[data-deploy]');
  await page.waitForSelector('#deploy-version');
  await page.selectOption('#deploy-version', version);
  await page.selectOption('#deploy-environment', environment);
  await page.waitForTimeout(500);
  if (version === '0.1.1') await shot('deploy-review');
  await page.click('dialog [data-go]');
  await page.waitForSelector('[data-operation="active"]', { timeout: 180000 });
}
async function promote(expected) {
  await go('#/software/web-app');
  await page.click('[data-promote]');
  await page.waitForSelector('#promote-from');
  await page.selectOption('#promote-from', 'test');
  await page.selectOption('#promote-to', 'production');
  await page.waitForSelector('[data-review] .promotion');
  assert.match(await page.innerText('[data-review]'), new RegExp(`Version ${expected.replaceAll('.', '\\.')}`));
  if (expected === '0.1.1') await shot('promote-review');
  await page.click('dialog [data-go]');
  await page.waitForSelector('[data-operation="active"]', { timeout: 180000 });
}
await publish('0.1.0');
await deploy('0.1.0', 'test');
await promote('0.1.0');

// 10–11. A code change (a new commit) and a configuration change: GO.
execFileSync('sh', ['-c', config.commit]);
await go('#/work/dev');
await page.click('[data-pull="web-app"]');
await page.waitForSelector('text=Change repository web-app');
await page.fill('input[aria-label="Set: name"]', 'GREETING');
await page.fill('input[aria-label="Set: value"]', 'hello');
await page.click('button:text-is("Set")');
await page.waitForSelector('text=Set GREETING');
await shot('local-changes-before-go');
await page.click('[data-view="work"] [data-go]');
await until('the new commit', async () => {
  const view = await computer('dev');
  return view.converged && view.observed.builds['web-app'] && view.observed.builds['web-app'].commit === view.observed.repositories['web-app'].commit
    && view.observed.repositories['web-app'].commit !== dev.observed.repositories['web-app'].commit ? view : null;
});
await go('#/software/web-app');
await page.click('[data-run="dev/build"]');
await page.waitForSelector('[data-output="succeeded"]');
await page.click('[data-run="dev/test"]');
await page.waitForFunction(() => document.querySelector('[data-output]') && document.querySelector('[data-output]').dataset.output !== 'running');
assert.equal(await page.getAttribute('[data-output]', 'data-output'), 'succeeded');
await shot('build-and-test');

// 12. Publish a new version.
await publish('0.1.1');
await shot('version-published');
await go('#/software/web-app/versions/0.1.1');
await page.waitForSelector('[data-version-view]');
await shot('version-evidence');

// 13–14. Deploy it to test; review test's reality.
await deploy('0.1.1', 'test');
await shot('deployed-to-test');
await go('#/environments/test');
await page.waitForSelector('[data-machine="computer"]');
await shot('test-reality');

// 15–16. Promote to production; observe production.
await promote('0.1.1');
await shot('promoted-to-production');
await go('#/environments/production');
await page.waitForSelector('[data-machine="computer"]');
let production = await computer('production');
const productionMachine = production.machine.resource;
const served = await page.evaluate(async (url) => (await fetch(url).catch(() => null)) !== null, production.endpoints[0].url);
assert.ok(production.endpoints[0].serving || served);
await shot('operate-production');
await go('#/software/web-app');
await page.waitForSelector('[data-software-view]');
await shot('software-versions-and-history');

// 17. Roll back to the previous version.
await page.click('[data-rollback]');
await page.waitForSelector('#rollback-environment');
await page.selectOption('#rollback-environment', 'production');
await page.selectOption('#rollback-to', '0.1.0');
await shot('rollback-review');
await page.click('dialog [data-go]');
await page.waitForSelector('[data-operation="active"]', { timeout: 180000 });
await shot('rolled-back');
production = await computer('production');
assert.equal(production.machine.resource, productionMachine, 'rollback changes what the computer runs, not the computer');

// 18. Stop and resume the computer.
await go('#/environments/production');
await page.waitForSelector('[data-machine="computer"]');
await page.click('.title button.danger:text-is("Stop")');
await page.click('[data-confirm]');
await until('production to stop', async () => (await computer('production')).status === 'stopped');
await go('#/environments/production');
await page.waitForSelector('[data-machine="computer"]');
await shot('computer-stopped');
await page.click('.title button:text-is("Start")');
if (await page.locator('[data-confirm]').count()) await page.click('[data-confirm]');
production = await until('production to resume', async () => {
  const view = await computer('production');
  return view.status === 'running' && view.converged ? view : null;
});
assert.equal(production.machine.resource, productionMachine, 'resumed, not replaced');

// 19. Terminal, logs, files.
await go('#/work/dev');
await page.fill('#work-command', 'cat repos/web-app/VERSION');
await page.press('#work-command', 'Enter');
await page.waitForSelector('[data-output="succeeded"]');
assert.equal((await page.innerText('[data-output] pre')).trim(), 'v2');
await shot('terminal');
await page.fill('#work-path', 'repos');
await page.click('[data-files="list"]');
await page.waitForFunction(() => document.querySelector('[data-output] pre') && document.querySelector('[data-output] pre').innerText.includes('web-app'));
await shot('files');
// An agent beside the software, on the same computer: GO.
await go('#/work/dev');
await page.fill('input[aria-label="Add: name"]', 'eve');
await page.fill('input[aria-label="Add: command"]', 'sleep 600');
await page.fill('input[aria-label="Add: kind"]', 'agent');
await page.click('button:text-is("Add")');
await page.waitForSelector('text=Add process eve');
await page.click('[data-view="work"] [data-go]');
await until('the agent', async () => {
  const view = await computer('dev');
  return view.observed.processes.eve && view.observed.processes.eve.state === 'running' ? view : null;
});
await go('#/work/dev');
await page.waitForSelector('[data-process="eve"]');
await shot('agent-running');
await go('#/environments');
await shot('manage-environments');

// 20. A temporary computer to try another piece of software.
await tile('try');
await page.waitForSelector('[data-step="source"]');
await page.fill('#run-url', config.repos.api);
await page.check('[data-choice="new"]');
await page.fill('#run-computer-name', 'try-api');
await page.selectOption('#run-lifetime', 'ephemeral');
await page.click('[data-inspect]');
await page.waitForSelector('[data-step="proposal"]');
await page.fill('[data-proposed-process="api"] input[type="number"]', String(config.ports.trial));
await page.click('[data-step="proposal"] [data-go]');
const trial = await converged('try-api');
assert.equal(trial.lifecycle, 'ephemeral');
assert.ok(trial.expires_at);
await go('#/work/try-api');
await page.waitForSelector('[data-process="api"]');
await shot('temporary-computer');
await go('#/');
await page.waitForSelector('[data-software="web-app"]');
await shot('home-with-software');

assert.deepEqual(errors, []);
await browser.close();
console.log(`ok: ${shots} screenshots`);
}

try {
  await journey();
} catch (error) {
  try {
    await page.screenshot({ path: join(config.shots, 'failure.png'), fullPage: true });
    console.error('PAGE:', (await page.innerText('main')).slice(0, 4000));
  } catch { /* the page is gone */ }
  console.error(error);
  process.exit(1);
}
