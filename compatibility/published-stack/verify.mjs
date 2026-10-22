import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtemp, readFile, rm, stat, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
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
// The backend Compute actually uses when nothing selects another (crates/compute-cli/src/control_state.rs).
assert.equal(stack.state.authoritative_durable_backend, 'compute-state-file');
assert.deepEqual(stack.state.optional_durable_backends, ['compute-state-feltdb']);
assert.equal(stack.state.shared_with_base, true);
assert.deepEqual(stack.state.additional_stores, []);
assert.equal(stack.configuration.activation, 'COMPUTE_STACKS');
assert.equal(stack.configuration.idempotent, true);
assert.deepEqual(stack.migrations, []);
if (process.env.COMPUTE_INSTALLED_VERSION) {
  assert.equal(process.env.COMPUTE_INSTALLED_VERSION, stack.compute, 'Base Compute version drift');
}
pass('distribution_profile', `configured is an additive ${stack.distribution.certification_status} profile over the same Compute binary, UI, CLI and state model (file by default, FeltDB when configured)`);
for (const [name, version] of Object.entries(stack.packages)) {
  assert.equal(lock.packages[`node_modules/${name}`]?.version, version, `${name} version drift`);
}
for (const [path, entry] of Object.entries(lock.packages)) {
  if (!path.startsWith('node_modules/')) continue;
  assert.match(entry.resolved, /^https:\/\/registry\.npmjs\.org\//, `${path} is not registry-resolved`);
  assert.ok(entry.integrity, `${path} has no registry integrity`);
}
// Licenses: open-source licenses only, except the proprietary AuthBoundry packages THIRD_PARTY.md names.
const OPEN_LICENSES = new Set(['MIT', 'Apache-2.0', 'ISC', 'BSD-2-Clause', 'BSD-3-Clause', '0BSD']);
for (const [path, entry] of Object.entries(lock.packages)) {
  if (!path.startsWith('node_modules/')) continue;
  const name = path.slice(path.lastIndexOf('node_modules/') + 'node_modules/'.length);
  if (OPEN_LICENSES.has(entry.license)) continue;
  assert.ok(entry.license === 'UNLICENSED' && name.startsWith('@authboundry/'),
    `${path} is licensed ${entry.license ?? '(none declared)'}: add it to THIRD_PARTY.md and this check, or remove it`);
}
pass('package_compatibility', `${Object.keys(stack.packages).length} exact top-level registry packages resolved with integrity; licenses open-source except the proprietary @authboundry packages`);

// The installed tree is the certified one. npm checks each tarball's integrity when it installs;
// this digest covers the unpacked files, so an installation compares what is on disk with what
// release CI certified (evidence.json, shipped beside this file).
const nodeModules = fileURLToPath(new URL('node_modules/', here));
const nodeModulesDigest = await treeDigest(nodeModules);
const shippedEvidence = await readFile(new URL('evidence.json', here), 'utf8').then(JSON.parse).catch(() => null);
if (shippedEvidence?.node_modules_digest) {
  assert.equal(nodeModulesDigest, shippedEvidence.node_modules_digest,
    'the installed node_modules differ from the certified tree: reinstall compute-configured');
  pass('installed_tree', `node_modules matches the certified tree (${nodeModulesDigest.slice(0, 19)}…)`);
} else {
  pass('installed_tree', `node_modules digest ${nodeModulesDigest.slice(0, 19)}… recorded for the release`);
}

// ---- The configured agent: stack-agent ----------------------------------
//
// stack-agent (Chip agent orchestration + FX inference) is one Rust executable, hosted by Compute
// as `compute-stack-agent`: the agent loop and the model credential stay in that process, and every
// capability runs in a Compute session of its own. No npm agent or npm model provider is part of
// this artifact.

const agent = stack.agent;
assert.ok(agent, 'the configured profile declares no agent runtime');
assert.equal(agent.default, 'stack-agent');
assert.deepEqual(agent.runtimes.map(({ name }) => name), ['stack-agent'], 'stack-agent is the only agent');
const defaultRuntime = agent.runtimes[0];
assert.equal(defaultRuntime.package, 'github:rkendel1/stack-agent');
assert.match(defaultRuntime.version, /^[0-9a-f]{40}$/, 'stack-agent is pinned to a full commit');
assert.equal(defaultRuntime.executable, 'stack-agent/bin/compute-stack-agent');
assert.equal(defaultRuntime.lifecycle, 'service');
assert.equal(defaultRuntime.environment, 'compute.session');
assert.equal(defaultRuntime.capability_execution?.where, 'compute-session');
assert.equal(defaultRuntime.model_provider?.credentials, 'agent-process-only');
assert.equal(defaultRuntime.http?.auth, 'loopback-only');
for (const name of ['@appport/chip', '@appport/fx', 'eve', 'chip-framework', 'libfx']) {
  const found = Object.keys(lock.packages).filter((path) => path.endsWith(`node_modules/${name}`));
  assert.deepEqual(found, [], `the configured artifact must not contain ${name}`);
}
// The pin the profile declares is the one Compute builds: the source tree's Cargo.toml when it is
// present (CI, before assembly), and the shipped executable itself when it is (an installation).
const cargo = await readFile(new URL('../../crates/compute-stack-agent/Cargo.toml', here), 'utf8').catch(() => null);
if (cargo) {
  const pins = [...cargo.matchAll(/git = "https:\/\/github\.com\/rkendel1\/stack-agent", rev = "([0-9a-f]{40})"/g)].map((m) => m[1]);
  assert.ok(pins.length > 0, 'compute-stack-agent does not depend on stack-agent');
  assert.deepEqual([...new Set(pins)], [defaultRuntime.version], 'the profile and Cargo.toml pin different stack-agent commits');
}
const { execFileSync, spawnSync } = await import('node:child_process');
const executable = fileURLToPath(new URL(defaultRuntime.executable, here));
let agentDetail = 'pinned in the source tree';
if (await stat(executable).then((s) => s.isFile()).catch(() => false)) {
  const run = spawnSync(executable, defaultRuntime.health.command, { encoding: 'utf8', timeout: 60_000, env: {} });
  assert.equal(run.status, 2, 'compute-stack-agent with no subcommand must print its usage and exit 2');
  assert.match(run.stderr, new RegExp(defaultRuntime.health.expect));
  // A non-loopback listener is refused before anything starts: the service has no authentication.
  const remote = spawnSync(executable, ['serve', '--host', '0.0.0.0'], { encoding: 'utf8', timeout: 60_000, env: {} });
  assert.equal(remote.status, 2, 'compute-stack-agent must refuse a non-loopback --host');
  agentDetail = 'the installed executable runs, and refuses a non-loopback listener';
} else {
  assert.ok(cargo, `${defaultRuntime.executable} is not in the artifact and no source tree pins it`);
}
pass('agent_runtime', `stack-agent@${defaultRuntime.version.slice(0, 12)} is the only agent; ${agentDetail}`);

// Managed services run on the bundled Node, from an allow-listed environment, and say what they
// do without the authority Compute cannot supply.
for (const service of stack.managed_services ?? []) {
  assert.equal(service.interpreter, 'node', `${service.name} must run on the bundled Node, not the host's`);
  assert.ok(Array.isArray(service.environment), `${service.name} must declare the variables it is given`);
  for (const name of service.environment) {
    assert.ok(!/^COMPUTE_DAEMON/.test(name) && !/^CHIP_|_API_KEY$/.test(name) || name === 'APPPORT_API_KEY',
      `${service.name} must not be given ${name}`);
  }
  if (service.authority) {
    assert.ok(service.environment.includes(service.authority.module_env), `${service.name}: the authority variable is passed through`);
    assert.match(service.authority.without, /503/, `${service.name}: the profile states what the service does without authority`);
  }
}
pass('managed_services', `${(stack.managed_services ?? []).map(({ name }) => name).join(', ')} run on the bundled Node from an allow-listed environment; management without an operator authority module is stated, not hidden`);

// Optional execution providers: declared as data, implemented by base Compute, never required.
// With COMPUTE_BIN (as compute-configured-verify sets it), the installed Compute must report each
// provider's profile and capabilities without a credential.
for (const provider of stack.execution_providers ?? []) {
  assert.equal(provider.required, false, `${provider.name}: an execution provider must be optional`);
  assert.equal(provider.implementation, 'base-compute', `${provider.name}: implemented by base Compute, not forked`);
  assert.equal(provider.credentials?.stored, false, `${provider.name}: credentials must never be stored`);
  assert.match(provider.credentials?.token_env ?? '', /^[A-Z][A-Z0-9_]*$/, `${provider.name}: credentials are referenced by variable name`);
  for (const refused of ['persistent_process', 'native_executable', 'container']) {
    assert.ok(provider.refuses.includes(refused), `${provider.name}: must declare that it refuses ${refused}`);
  }
  let detail = 'declared';
  if (process.env.COMPUTE_BIN && provider.kind === 'cloudflare') {
    const env = { ...process.env };
    delete env[provider.credentials.token_env];
    const run = (args) => execFileSync(process.env.COMPUTE_BIN, args, { encoding: 'utf8', env });
    const profile = JSON.parse(run(['cloudflare', 'profile', provider.profile, '--json']));
    assert.equal(profile.name, provider.profile);
    for (const refused of provider.refuses) {
      assert.ok(profile.capabilities && Object.entries({
        persistent_process: 'persistent_processes', interactive_shell: 'interactive_shell',
        native_executable: 'native_executables', container: 'containers', process_runtime: 'process_runtimes',
        filesystem: 'filesystem', outbound_network: 'outbound_network', local_inference: 'local_inference',
      }).every(([name, field]) => name !== refused || profile.capabilities[field] === false), `${provider.name}: the installed profile does not refuse ${refused}`);
    }
    const work = await mkdtemp(join(tmpdir(), 'compute-provider-'));
    try {
      const pool = join(work, 'pool.toml');
      await writeFile(pool, `[providers.${provider.name}]\nkind = "${provider.kind}"\ntoken_env = "${provider.credentials.token_env}"\n\n[providers.${provider.name}.cloudflare]\naccount_id = "0123456789abcdef0123456789abcdef"\nprofile = "${provider.profile}"\n`);
      const capabilities = JSON.parse(run(['provider', 'capabilities', provider.name, '--json', '--pool-config', pool, '--capability-cache', join(work, 'cache.json')]));
      assert.equal(capabilities.protocol, provider.protocol);
      assert.equal(capabilities.execution?.sessions ?? false, false);
      assert.equal(capabilities.execution?.deployments, false);
    } finally {
      await rm(work, { recursive: true, force: true });
    }
    detail = `the installed Compute reports the ${provider.profile} profile and ${provider.protocol} capabilities without a credential`;
  }
  pass(`execution_provider_${provider.name}`, `${provider.name} is optional, credential-free in configuration, and ${detail}`);
}

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
  node_modules_digest: nodeModulesDigest,
  resolved: Object.fromEntries([...versions].map(([name, found]) => [name, [...found].sort()])),
  contracts: stack.contracts,
  // What this installation is, in one place: the configured release is the
  // Compute release it extends.
  versions: {
    'compute-configured': stack.compute,
    compute: stack.compute,
    'stack-agent': defaultRuntime.version,
    node: process.version,
  },
  checks,
};
const output = process.env.COMPUTE_COMPATIBILITY_EVIDENCE;
if (output) await writeFile(output, `${JSON.stringify(evidence, null, 2)}\n`);
console.log(JSON.stringify(evidence, null, 2));

/**
 * sha256 over every file and symlink under `root`, in sorted path order: each entry contributes its
 * relative path, its kind, and its content (a file's bytes, a link's target). Modes and times are
 * left out, so a copy of the same tree has the same digest. Native executables and libraries (ELF,
 * Mach-O) contribute only their path: Homebrew rewrites their load paths when it installs a keg, so
 * their bytes legitimately differ from what CI certified. npm's tarball integrity covers them.
 */
async function treeDigest(root) {
  const { readdir, readlink, lstat } = await import('node:fs/promises');
  const entries = [];
  const walk = async (dir, prefix) => {
    for (const name of (await readdir(dir)).sort()) {
      const path = join(dir, name);
      const relative = prefix ? `${prefix}/${name}` : name;
      const info = await lstat(path);
      if (info.isSymbolicLink()) entries.push([relative, 'link', Buffer.from(await readlink(path))]);
      else if (info.isDirectory()) await walk(path, relative);
      else if (info.isFile()) {
        const bytes = await readFile(path);
        entries.push(native(bytes) ? [relative, 'native', Buffer.alloc(0)] : [relative, 'file', bytes]);
      }
    }
  };
  const native = (bytes) => {
    if (bytes.length < 4) return false;
    const magic = bytes.readUInt32BE(0);
    return [0x7f454c46, 0xfeedface, 0xfeedfacf, 0xcefaedfe, 0xcffaedfe, 0xcafebabe].includes(magic);
  };
  await walk(root, '');
  const hash = createHash('sha256');
  for (const [relative, kind, content] of entries.sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))) {
    hash.update(`${relative}\0${kind}\0${content.length}\0`);
    hash.update(content);
  }
  return `sha256:${hash.digest('hex')}`;
}
