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
// COMPUTE_UI_SHOTS=DIR keeps a screenshot of each new surface, for review.
const shot = async (name) => {
  if (process.env.COMPUTE_UI_SHOTS) await page.screenshot({ path: `${process.env.COMPUTE_UI_SHOTS}/${name}.png`, fullPage: true });
};
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
// The page may learn of the change first (and say so, with GO disabled),
// or when GO is refused: either way the draft is not applied.
const noticed = await page.waitForSelector('[data-conflict]', { timeout: 1500 }).then(() => true, () => false);
if (!noticed) await page.click('[data-go]');
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

// Work: a running command can be cancelled. This target has no terminal,
// so an interactive one is refused with the reason, not offered.
await page.goto(`${base}/#/work/${environment}`);
await page.waitForSelector('[data-view="work"]');
assert.ok(await page.isDisabled('[data-interactive]'), 'no interactive terminal without the capability');
assert.match(await page.innerText('[data-no-terminal]'), /not on this machine/);
await page.fill('#work-command', 'sleep 60');
await page.press('#work-command', 'Enter');
await page.waitForSelector('[data-cancel]');
await page.click('[data-cancel]');
await page.waitForSelector('[data-output="cancelled"]', { timeout: 30000 });
await shot('work-terminal');

// Back to Manage: the same environment, and what its machine can do.
await page.click('[data-mode-link="manage"]');
await page.waitForSelector('[data-machine="computer"]');
assert.equal(await page.getAttribute('[data-view="manage"]', 'data-environment-id'), manageId);
await page.waitForSelector('[data-capability="fork"]');
const canFork = await page.getAttribute('[data-capability="fork"]', 'data-available');
assert.equal(canFork, 'false', 'a workspace target cannot fork a running machine');
await shot('manage-computer');

// Fork: what to copy is chosen; the running machine is not offered where
// it cannot be forked, and the files are.
await page.click('[data-fork]');
await page.waitForSelector('[data-fork-state="machine"]');
assert.ok(await page.isDisabled('[data-fork-state="machine"]'));
assert.ok(await page.isChecked('[data-fork-state="workspace"]'));
await shot('fork-dialog');
await page.fill('#fork-name', `${environment}-fork`);
await page.click('[data-fork-go]');
await page.waitForFunction((name) => location.hash === `#/environments/${name}`, `${environment}-fork`, { timeout: 180000 });
await page.waitForSelector('[data-machine="computer"]');
const forked = (await api('GET', `/environments/${environment}-fork/computer`)).body;
assert.equal(forked.status, 'running');
assert.notEqual(forked.machine.resource, before.machine.resource, 'a new machine');

// Checkpoint, then restore it as a new environment.
await page.goto(`${base}/#/environments/${environment}`);
await page.waitForSelector('[data-checkpoint]');
await page.click('[data-checkpoint]');
await page.waitForSelector('[data-checkpoint-id]', { timeout: 120000 });
await shot('checkpoints');
await page.click('[data-restore]');
await page.fill('#restore-name', `${environment}-restored`);
await page.click('[data-restore-go]');
await page.waitForFunction((name) => location.hash === `#/environments/${name}`, `${environment}-restored`, { timeout: 180000 });
assert.equal((await api('GET', `/environments/${environment}-restored/computer`)).body.status, 'running');

// Access: the published contract, the audit trail of what was just done,
// and credentials, whose token is shown once.
await page.goto(`${base}/#/access/api`);
await page.waitForSelector('[data-operation="environment.fork"]');
const operations = await page.locator('[data-operation]').count();
assert.equal(operations, (await api('GET', '/api')).body.routes.length, 'every published operation is listed');
await page.fill('#api-filter', 'checkpoint');
assert.ok(await page.locator('[data-operation="environment.fork"]').isHidden());
assert.ok(await page.locator('[data-operation="environment.checkpoint"]').isVisible());
await shot('access-api');
await page.click('[data-access-tab="audit"]');
await page.waitForSelector('[data-audit]');
await shot('access-audit');
assert.match(await page.innerText('[data-audit]'), /POST \/environments\/\{environment\}\/fork/);
await page.click('[data-access-tab="credentials"]');
await page.waitForSelector('[data-credential-new]');
await page.click('[data-credential-new]');
await page.fill('#credential-operator', 'reader-1');
await page.click('[data-credential-create]');
await page.waitForSelector('[data-token]');
assert.match(await page.innerText('[data-token]'), /^cmpt_/);
await shot('access-token');
await page.click('dialog button.primary');
await page.waitForSelector('[data-credential]');
const issued = (await api('GET', '/auth/credentials')).body.find((credential) => credential.operator_id === 'reader-1');
assert.deepEqual(issued.scopes, ['compute.read']);
await page.click(`[data-credential="${issued.credential_id}"] button.danger`);
await page.click('[data-confirm]');
await page.waitForFunction((id) => !document.querySelector(`[data-credential="${id}"] button.danger`), issued.credential_id);
assert.ok((await api('GET', '/auth/credentials')).body.find((credential) => credential.credential_id === issued.credential_id).revoked_at);
assert.deepEqual(errors, []);
await browser.close();
console.log('ok');
