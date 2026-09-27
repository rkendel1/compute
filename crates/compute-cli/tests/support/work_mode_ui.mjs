// Drives the control plane in a browser: Manage and Work are modes of one
// environment; local edits stay local until GO; a stale GO is refused.
// Usage: node work_mode_ui.mjs PLAYWRIGHT_MODULE CHROMIUM BASE_URL TOKEN ENVIRONMENT
import assert from 'node:assert/strict';

const [module, executablePath, base, token, environment] = process.argv.slice(2);
const { chromium } = await import(module);
const headers = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
const browser = await chromium.launch({ executablePath });
const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
const errors = [];
page.on('pageerror', (error) => errors.push(error.message));
const api = (method, path, body) => page.evaluate(async ({ method, path, body, headers }) => {
  const response = await fetch(path, { method, headers, body: body === undefined ? undefined : JSON.stringify(body) });
  return { status: response.status, body: await response.json() };
}, { method, path, body, headers });

await page.goto(`${base}/`);
await page.evaluate((token) => sessionStorage.setItem('compute.token', token), token);

// Manage: the machine, and the way into Work.
await page.goto(`${base}/#/environments/${environment}`);
await page.waitForSelector('[data-machine="computer"]');
const manageId = await page.getAttribute('[data-view="manage"]', 'data-environment-id');
assert.equal(await page.evaluate(() => document.body.dataset.mode), 'manage');

// Work: the same environment.
await page.click('[data-work]');
await page.waitForSelector('[data-view="work"]');
assert.equal(await page.evaluate(() => document.body.dataset.mode), 'work');
assert.equal(await page.getAttribute('[data-view="work"]', 'data-environment-id'), manageId);
await page.waitForFunction(() => document.body.innerText.includes('Converged') || document.querySelector('[data-project] .state'), null, { timeout: 60000 });

// Local edits stay local until GO.
const before = (await api('GET', `/environments/${environment}/computer`)).body;
const revision = page.locator('input[aria-label="app revision"]');
await revision.fill('v2');
await revision.press('Tab');
await page.waitForFunction(() => document.body.innerText.includes('Change repository app'));
assert.equal((await api('GET', `/environments/${environment}/computer`)).body.desired.generation, before.desired.generation, 'nothing changed before GO');
await page.click('[data-go]');
await page.waitForFunction(() => document.body.innerText.includes('No local changes'));
const went = (await api('GET', `/environments/${environment}/computer`)).body;
assert.equal(went.desired.repositories[0].revision, 'v2');
assert.equal(went.desired.generation, before.desired.generation + 1);

// Someone else changes the environment: this page's GO is refused.
await revision.fill('v1');
await revision.press('Tab');
await page.waitForFunction(() => document.body.innerText.includes('Change repository app'));
assert.equal((await api('POST', `/environments/${environment}/config`, { OTHER: 'change' })).status, 200);
await page.click('[data-go]');
await page.waitForSelector('[data-conflict]');
assert.match(await page.innerText('[data-conflict]'), /Environment changed since you loaded it/);
assert.equal((await api('GET', `/environments/${environment}/computer`)).body.desired.repositories[0].revision, 'v2', 'not overwritten');
await page.click('[data-conflict] button');
await page.waitForFunction(() => !document.querySelector('[data-conflict]'));

// The computer reconciles the change in place.
for (let attempt = 0; ; attempt += 1) {
  const view = (await api('GET', `/environments/${environment}/computer`)).body;
  if (view.converged && view.observed.repositories.app.revision === 'v2') {
    assert.equal(view.machine.resource, before.machine.resource, 'the same machine');
    break;
  }
  assert.ok(attempt < 600, 'reconciled');
  await page.waitForTimeout(100);
}

// Back to Manage: the same environment.
await page.click('[data-mode-link="manage"]');
await page.waitForSelector('[data-machine="computer"]');
assert.equal(await page.getAttribute('[data-view="manage"]', 'data-environment-id'), manageId);
assert.deepEqual(errors, []);
await browser.close();
console.log('ok');
