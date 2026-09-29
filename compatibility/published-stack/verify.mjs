import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import process from 'node:process';

import { fingerprintManifest } from '@appport/core';
import {
  createGitHubIntegration,
  githubAppPortManifest,
  parseGitHubCapabilityDeclaration,
} from '@appport/github';
import { SERVICE_CAPABILITY_MANIFEST } from '@appport/services';
import { createFeltDB, parseFlowSpec, validateFlowSpec } from '@feltdb/core';

const here = new URL('.', import.meta.url);
const stack = JSON.parse(await readFile(new URL('stack.json', here), 'utf8'));
const lock = JSON.parse(await readFile(new URL('package-lock.json', here), 'utf8'));
const checks = [];
const pass = (name, detail) => checks.push({ name, result: 'pass', detail });

assert.equal(stack.format, 'compute.distribution-profile@1');
assert.equal(stack.profile, 'configured');
assert.equal(stack.distribution.identity, `compute-configured-${stack.compute}-${stack.platform}`);
assert.ok(['certified', 'preview'].includes(stack.distribution.certification_status));
assert.equal(stack.runtime.implementation, 'base-compute');
assert.equal(stack.runtime.shared_binary, true);
assert.equal(stack.runtime.shared_cli, true);
assert.equal(stack.runtime.shared_ui, true);
assert.equal(stack.state.root, 'COMPUTE_HOME');
assert.equal(stack.state.authoritative_durable_backend, 'compute-state-feltdb');
assert.equal(stack.state.shared_with_base, true);
assert.deepEqual(stack.state.additional_stores, []);
assert.equal(stack.configuration.activation, 'COMPUTE_STACKS');
assert.equal(stack.configuration.idempotent, true);
assert.deepEqual(stack.migrations, []);
if (process.env.COMPUTE_INSTALLED_VERSION) {
  assert.equal(process.env.COMPUTE_INSTALLED_VERSION, stack.compute, 'Base Compute version drift');
}
pass('distribution_profile', `configured is an additive ${stack.distribution.certification_status} profile over the same Compute binary, UI, CLI, and FeltDB-backed state model`);
for (const [name, version] of Object.entries(stack.packages)) {
  assert.equal(lock.packages[`node_modules/${name}`]?.version, version, `${name} version drift`);
}
for (const [path, entry] of Object.entries(lock.packages)) {
  if (!path.startsWith('node_modules/')) continue;
  assert.match(entry.resolved, /^https:\/\/registry\.npmjs\.org\//, `${path} is not registry-resolved`);
  assert.ok(entry.integrity, `${path} has no registry integrity`);
}
pass('package_compatibility', `${Object.keys(stack.packages).length} exact top-level registry packages resolved with integrity`);

const versions = new Map();
for (const [path, entry] of Object.entries(lock.packages)) {
  if (!path.includes('node_modules/') || !entry.version) continue;
  const name = path.slice(path.lastIndexOf('node_modules/') + 'node_modules/'.length);
  if (!name.startsWith('@appport/') && name !== '@feltdb/core') continue;
  const found = versions.get(name) ?? new Set();
  found.add(entry.version);
  versions.set(name, found);
}
for (const [name, found] of versions) {
  if (found.size < 2) continue;
  const allowed = stack.allowed_version_skew[name]?.versions ?? [];
  assert.deepEqual([...found].sort(), [...allowed].sort(), `${name} has undeclared version skew`);
}
pass('version_skew', 'every duplicate AppPort/FeltDB version is explicit in the compatibility manifest');

const declaration = parseGitHubCapabilityDeclaration('use github {\n  repositories = true\n}');
assert.deepEqual(declaration.groups, ['repositories']);
assert.ok(declaration.operations.includes('github.repository.read'));
assert.equal(githubAppPortManifest.protocol, stack.contracts.appport);
assert.ok(githubAppPortManifest.provides.some(({ name }) => name === 'github.repository.read'));
assert.ok(SERVICE_CAPABILITY_MANIFEST);
assert.ok(fingerprintManifest(githubAppPortManifest));
pass('appport_contract', 'published GitHub DSL, capability manifest, services exports, and AppPort verifier agree');

const work = await mkdtemp(join(tmpdir(), 'compute-stack-compatibility-'));
try {
  const databasePath = join(work, 'feltdb');
  const feltClients = [
    ['0.11.1', await import('./node_modules/@authboundry/core/node_modules/@appport/services/node_modules/@feltdb/core/dist/index.js')],
    ['0.11.5', await import('./node_modules/@authboundry/core/node_modules/@feltdb/core/dist/index.js')],
    ['0.11.9', { createFeltDB }],
  ];
  for (const [index, [version, client]] of feltClients.entries()) {
    const db = client.createFeltDB({ mode: 'local', path: databasePath, namespace: 'compatibility' });
    const records = db.collection('CompatibilityEvidence');
    if (index === 0) {
      await records.insert({ value: version }, 'record');
    } else {
      assert.equal((await records.get('record'))?.value, feltClients[index - 1][0]);
      await records.update('record', { value: version });
    }
    await db.close();
  }
  pass('feltdb_persistence', 'published FeltDB 0.11.1 → 0.11.5 → 0.11.9 wrote, restarted, recovered, and updated the same durable state');

  const originalFetch = globalThis.fetch;
  const requestedUrls = [];
  globalThis.fetch = async (request) => {
    const url = String(request);
    requestedUrls.push(url);
    const data = url.includes('/commits/')
      ? { sha: 'a'.repeat(40), commit: { message: 'compatibility fixture' } }
      : {
          id: 1,
          node_id: 'repository-node',
          owner: { login: 'rkendel1' },
          name: 'compute',
          full_name: 'rkendel1/compute',
          private: false,
          archived: false,
          default_branch: 'main',
          html_url: 'https://github.com/rkendel1/compute',
        };
    return new Response(JSON.stringify(data), {
      status: 200,
      headers: { 'content-type': 'application/json' },
    });
  };
  try {
    const authority = {
      session: async () => ({
        authenticated: true,
        principal: { id: 'compatibility-principal' },
        tenant: { id: 'compatibility-tenant' },
        capabilities: ['github.repository.read'],
      }),
      authorize: async (capability) => capability === 'github.repository.read',
    };
    const github = createGitHubIntegration({ authority, felt: { memory: true } });
    await github.upsertConnection({
      id: 'public-github', tenantId: 'compatibility-tenant', applicationId: 'compatibility',
      environment: 'test', provider: 'github', authMechanism: 'public', status: 'configured',
      capabilities: ['github.repository.read'], createdAt: new Date(0).toISOString(), updatedAt: new Date(0).toISOString(),
    });
    const source = await github.repositories.source(
      { connectionId: 'public-github', owner: 'rkendel1', repository: 'compute' },
      { applicationId: 'compatibility' },
    );
    assert.deepEqual(source, {
      source: 'git', url: 'https://github.com/rkendel1/compute.git', owner: 'rkendel1',
      repository: 'compute', ref: 'main', commit: 'a'.repeat(40),
    });
    assert.ok(requestedUrls.some((url) => url.includes('/repos/rkendel1/compute')));
    assert.ok(requestedUrls.some((url) => url.includes('/commits/main')));
    pass('github_git_source', 'published @appport/github produced a provider-neutral immutable Git source');

    const flow = await github.flow();
    const parsed = parseFlowSpec(flow);
    assert.deepEqual(validateFlowSpec(parsed), []);
    pass('flowspec_contract', 'the FlowSpec shipped in the GitHub tarball parses and validates with published FeltDB');
  } finally {
    globalThis.fetch = originalFetch;
  }
} finally {
  await rm(work, { recursive: true, force: true });
}

const certificationMaterial = JSON.stringify({
  profile: stack,
  lockfile_version: lock.lockfileVersion,
  packages: Object.fromEntries(Object.entries(lock.packages).map(([path, entry]) => [path, {
    version: entry.version,
    resolved: entry.resolved,
    integrity: entry.integrity,
  }])),
});
const certificationId = `sha256:${createHash('sha256').update(certificationMaterial).digest('hex')}`;
const evidence = {
  format: 'compute.stack-compatibility-evidence@1',
  result: 'pass',
  status: stack.distribution.certification_status,
  certification_id: certificationId,
  certified_at: new Date(Number(process.env.SOURCE_DATE_EPOCH ?? 0) * 1000).toISOString(),
  platform: stack.platform,
  host_platform: `${process.platform}-${process.arch}`,
  node: process.version,
  compute: stack.compute,
  distribution: stack.distribution,
  state: stack.state,
  packages: Object.fromEntries(Object.entries(stack.packages).map(([name, version]) => [name, {
    version,
    source: lock.packages[`node_modules/${name}`].resolved,
    integrity: lock.packages[`node_modules/${name}`].integrity,
  }])),
  resolved: Object.fromEntries([...versions].map(([name, found]) => [name, [...found].sort()])),
  contracts: stack.contracts,
  checks,
};
const output = process.env.COMPUTE_COMPATIBILITY_EVIDENCE;
if (output) await writeFile(output, `${JSON.stringify(evidence, null, 2)}\n`);
console.log(JSON.stringify(evidence, null, 2));
