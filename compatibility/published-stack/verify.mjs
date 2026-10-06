import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { cp, mkdtemp, readFile, rm, stat, symlink, writeFile } from 'node:fs/promises';
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

// ---- The configured default agent runtime --------------------------------
//
// Chip is installed here, not resolved later: the artifact ships the package,
// its dependency closure, and the Node it needs. Every check below reads the
// installed tree, so a missing or wrong agent runtime fails certification
// rather than degrading silently to "no agent".

const agent = stack.agent;
assert.ok(agent, 'the configured profile declares no agent runtime');
assert.ok(agent.default, 'the configured profile names no default agent runtime');
const defaultRuntime = agent.runtimes.find(({ name }) => name === agent.default);
assert.ok(defaultRuntime, `the default agent runtime ${agent.default} is not declared`);

// Package identity is the contract. Chip's published bin target is an internal
// filename (`bin/eve.js`) and its pre-rename package name was `eve`; neither is
// a dependency this artifact may acquire.
assert.equal(defaultRuntime.package, '@appport/chip');
const forbidden = Object.keys(lock.packages).filter((path) => {
  const name = path.slice(path.lastIndexOf('node_modules/') + 'node_modules/'.length);
  return name === 'eve' || name === 'chip-framework';
});
assert.deepEqual(forbidden, [], 'the configured artifact must not depend on eve or chip-framework');

const chipRoot = new URL('node_modules/@appport/chip/', here);
const chipManifest = JSON.parse(await readFile(new URL('package.json', chipRoot), 'utf8'));
assert.equal(chipManifest.version, defaultRuntime.version, 'the installed Chip is not the certified version');
assert.equal(chipManifest.name, '@appport/chip');
// Chip requires Node >= 24, and the profile declares the same floor; the Node
// this verifier runs on is the one the distribution bundles.
assert.match(chipManifest.engines?.node ?? '', />=\s*24/, 'Chip requires Node >= 24');
const profilePackage = JSON.parse(await readFile(new URL('package.json', here), 'utf8'));
assert.match(profilePackage.engines?.node ?? '', />=\s*24/, 'the configured profile must require the Node its agent runtime needs');
assert.ok(Number(process.versions.node.split('.')[0]) >= 24, `the configured Node is ${process.version}; Chip requires >= 24`);

// The executable must exist and run on the Node this distribution ships.
const executable = new URL(defaultRuntime.executable, here);
assert.ok(await stat(executable).then((s) => s.isFile()).catch(() => false),
  `${defaultRuntime.executable} is not in the configured artifact`);
const { execFileSync } = await import('node:child_process');
const reported = execFileSync(process.execPath, [fileURLToPath(executable), ...defaultRuntime.health.command], {
  encoding: 'utf8', timeout: 120_000,
}).trim();
assert.equal(reported, defaultRuntime.health.expect, 'the configured agent runtime did not report its certified version');
assert.equal(defaultRuntime.lifecycle, 'execution', 'an agent runtime is invoked inside an execution');
assert.ok(defaultRuntime.node, 'the agent runtime does not name the Node it runs on');
pass('agent_runtime', `${defaultRuntime.name}@${defaultRuntime.version} (${defaultRuntime.package}) is the installed default, reports ${reported}, and runs inside an execution`);

// ---- FX: Chip's model provider layer --------------------------------------
//
// Chip owns the agent loop; FX owns the model call and its transport. The
// configured agent joins them through Chip's public adapter
// (`@appport/chip/models/fx`) and FX's public model API (`createFxModel`).
// Compute itself never loads either. Every check below crosses that boundary
// with the installed packages; only the model endpoint is a local fixture.

const fxDeclaration = defaultRuntime.model_provider;
assert.ok(fxDeclaration, 'the configured agent runtime declares no model provider');
assert.equal(fxDeclaration.package, '@appport/fx');
assert.equal(fxDeclaration.adapter, '@appport/chip/models/fx');
assert.equal(stack.packages['@appport/fx'], fxDeclaration.version, 'the FX pin and the agent runtime disagree');
const fxManifest = JSON.parse(await readFile(new URL('node_modules/@appport/fx/package.json', here), 'utf8'));
assert.equal(fxManifest.name, '@appport/fx', 'the installed model provider is not @appport/fx');
assert.equal(fxManifest.version, fxDeclaration.version, 'the installed FX is not the certified version');
assert.equal(lock.packages['node_modules/@appport/fx']?.resolved,
  `https://registry.npmjs.org/@appport/fx/-/fx-${fxDeclaration.version}.tgz`,
  '@appport/fx must come from its published registry tarball');
// Vercel's unrelated `libfx` package shares FX's lineage but not its identity.
assert.equal(lock.packages['node_modules/libfx'], undefined, 'the configured artifact must not depend on libfx');

const { fx } = await import('@appport/chip/models/fx');
const { createFxModel } = await import('@appport/fx');
const fixture = await startModelFixture();
try {
  const model = fx(await createFxModel({ baseUrl: fixture.baseUrl, model: 'compute-configured-verify' }));
  assert.equal(model.specificationVersion, 'v4');
  assert.equal(model.provider, 'fx.openai-compatible');
  const prompt = [
    { role: 'system', content: 'Reply with the fixture text.' },
    { role: 'user', content: [{ type: 'text', text: 'ping' }] },
  ];
  const streamed = [];
  const reader = (await model.doStream({ prompt })).stream.getReader();
  for (let part = await reader.read(); !part.done; part = await reader.read()) streamed.push(part.value);
  assert.equal(streamed.filter(({ type }) => type === 'text-delta').map(({ delta }) => delta).join(''), 'FX fixture reply');
  assert.equal(streamed.find(({ type }) => type === 'finish')?.finishReason?.unified, 'stop');
  await assert.rejects(
    model.doGenerate({ prompt: [{ role: 'user', content: [{ type: 'text', text: 'fixture-fail' }] }] }),
    (error) => /fixture rejected the request/.test(`${error.message} ${error.responseBody ?? ''}`),
    'a provider failure must reach Chip as an error, never as a reply',
  );
  const [first] = fixture.requests;
  assert.equal(first.url, '/v1/chat/completions');
  assert.equal(first.body.model, 'compute-configured-verify');
  assert.equal(first.authorization, undefined, 'no credential is sent unless FX_API_KEY_ENV names one');
} finally {
  await fixture.stop();
}
pass('model_provider', `@appport/fx@${fxManifest.version} served Chip's fx() adapter: streamed a reply from an OpenAI-compatible endpoint and surfaced its failure, with no Gateway credential`);

// The agent Chip serves is the configured one, built from the pinned Chip.
const built = JSON.parse(await readFile(new URL('.output/eve-cache.json', here), 'utf8').catch(() => 'null'));
assert.ok(built, 'the configured agent has not been built (.output is missing)');
assert.equal(built.eveVersion, defaultRuntime.version, 'the configured agent was built with a different Chip');
const agentSource = await readFile(new URL(`${defaultRuntime.project}/agent.ts`, here), 'utf8');
assert.match(agentSource, /from "@appport\/chip\/models\/fx"/);
assert.match(agentSource, /from "@appport\/fx"/);
await verifyChipServes();
pass('agent_session', `the installed Chip served the configured agent: POST /eve/v1/session returned 202, and its stream carried the FX reply to session.waiting`);

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
  // What this installation is, in one place: the configured release is the
  // Compute release it extends.
  versions: {
    'compute-configured': stack.compute,
    compute: stack.compute,
    chip: defaultRuntime.version,
    fx: fxDeclaration.version,
    node: process.version,
  },
  checks,
};
const output = process.env.COMPUTE_COMPATIBILITY_EVIDENCE;
if (output) await writeFile(output, `${JSON.stringify(evidence, null, 2)}\n`);
console.log(JSON.stringify(evidence, null, 2));

// ---- Fixtures and the served-agent check ---------------------------------

/**
 * A deterministic OpenAI-compatible Chat Completions endpoint on loopback. It
 * stands in for a model only; FX's real transport and Chip's real adapter talk
 * to it exactly as they would to a hosted provider.
 */
async function startModelFixture() {
  const requests = [];
  const server = createServer((req, res) => {
    let raw = '';
    req.on('data', (chunk) => { raw += chunk; });
    req.on('end', () => {
      const body = JSON.parse(raw || '{}');
      requests.push({ url: req.url, authorization: req.headers.authorization, body });
      const prompt = JSON.stringify(body.messages ?? []);
      if (prompt.includes('fixture-fail')) {
        res.writeHead(400, { 'content-type': 'application/json' });
        res.end(JSON.stringify({ error: { message: 'fixture rejected the request', type: 'invalid_request' } }));
        return;
      }
      const pieces = ['FX ', 'fixture ', 'reply'];
      if (!body.stream) {
        res.writeHead(200, { 'content-type': 'application/json' });
        res.end(JSON.stringify({
          id: 'chatcmpl-fixture', object: 'chat.completion', model: body.model,
          choices: [{ index: 0, message: { role: 'assistant', content: pieces.join('') }, finish_reason: 'stop' }],
          usage: { prompt_tokens: 5, completion_tokens: 3, total_tokens: 8 },
        }));
        return;
      }
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      const chunk = (delta, finish = null) => res.write(`data: ${JSON.stringify({
        id: 'chatcmpl-fixture', object: 'chat.completion.chunk', model: body.model,
        choices: [{ index: 0, delta, finish_reason: finish }],
      })}\n\n`);
      for (const content of pieces) chunk({ content });
      chunk({}, 'stop');
      res.end('data: [DONE]\n\n');
    });
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  return {
    baseUrl: `http://127.0.0.1:${server.address().port}/v1`,
    requests,
    stop: () => new Promise((resolve) => { server.closeAllConnections(); server.close(resolve); }),
  };
}

/**
 * Serve the configured agent with the installed Chip (`chip start`, the
 * production server), from a throwaway working directory linked to this
 * installation the way the configured launcher links it, and run one session
 * through FX to the fixture.
 */
async function verifyChipServes() {
  const work = await mkdtemp(join(tmpdir(), 'compute-configured-chip-'));
  const fixture = await startModelFixture();
  const token = createHash('sha256').update(`${process.pid}:${Date.now()}:${Math.random()}`).digest('hex');
  const env = { ...process.env };
  // The configured agent needs no Gateway or Vercel identity; prove it.
  for (const name of Object.keys(env)) {
    if (/^(AI_GATEWAY_|VERCEL)/.test(name) || /^FX_/.test(name)) delete env[name];
  }
  Object.assign(env, {
    FX_BASE_URL: fixture.baseUrl,
    FX_MODEL: 'compute-configured-verify',
    COMPUTE_CONFIGURED_CHIP_TOKEN: token,
    EVE_TELEMETRY_DISABLED: '1',
  });
  let child;
  try {
    // Chip locates the application through agent/ and package.json, which must
    // be real files; the installed dependencies and build output are linked.
    for (const entry of ['agent', 'package.json']) {
      await cp(fileURLToPath(new URL(entry, here)), join(work, entry), { recursive: true });
    }
    for (const entry of ['node_modules', '.output']) {
      await symlink(fileURLToPath(new URL(entry, here)), join(work, entry));
    }
    child = spawn(process.execPath, [fileURLToPath(executable), 'start', '--host', '127.0.0.1', '--port', '0'], {
      cwd: work, env, stdio: ['ignore', 'pipe', 'pipe'],
    });
    let output = '';
    child.stdout.on('data', (chunk) => { output += chunk; });
    child.stderr.on('data', (chunk) => { output += chunk; });
    const origin = await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`chip start did not listen:\n${output}`)), 120_000);
      const look = () => {
        const match = /Listening on: (http:\/\/[^\s/]+)/.exec(output);
        if (match) { clearTimeout(timer); resolve(match[1]); }
      };
      child.stdout.on('data', look);
      child.once('exit', (code) => { clearTimeout(timer); reject(new Error(`chip start exited ${code}:\n${output}`)); });
    });
    const health = await fetch(`${origin}/eve/v1/health`);
    assert.equal(health.status, 200, 'the configured Chip is not healthy');
    const refused = await fetch(`${origin}/eve/v1/session`, {
      method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ message: 'ping' }),
    });
    assert.equal(refused.status, 401, 'the configured Chip admitted an unauthenticated caller');
    const headers = { authorization: `Bearer ${token}`, 'content-type': 'application/json' };
    const created = await fetch(`${origin}/eve/v1/session`, {
      method: 'POST', headers, body: JSON.stringify({ message: 'ping', operationId: 'compute-configured-verify' }),
    });
    assert.equal(created.status, 202);
    const { sessionId, status } = await created.json();
    assert.equal(status, 'accepted');
    const events = await readSession(origin, sessionId, headers);
    const text = events.filter(({ type, data }) => type === 'message.appended' && data?.messageDelta !== undefined)
      .map(({ data }) => data.messageDelta).join('');
    assert.equal(text, 'FX fixture reply', `the FX reply did not reach the session: ${JSON.stringify(events)}`);
    assert.equal(events.at(-1)?.type, 'session.waiting');
    assert.ok(fixture.requests.some(({ body }) => body.model === 'compute-configured-verify' && body.stream === true));
  } finally {
    child?.kill('SIGTERM');
    await fixture.stop();
    await rm(work, { recursive: true, force: true });
  }
}

/** Read a session's NDJSON stream until the current turn settles. */
async function readSession(origin, sessionId, headers, deadlineMs = 60_000) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), deadlineMs);
  const events = [];
  try {
    for (let attempt = 0; ; attempt += 1) {
      // The stream may briefly report the run as not yet readable after the
      // 202; the published client retries the same window.
      const response = await fetch(`${origin}/eve/v1/session/${sessionId}/stream`, { headers, signal: controller.signal });
      if (response.status === 404 && attempt < 50) { await new Promise((r) => setTimeout(r, 200)); continue; }
      assert.equal(response.status, 200, `stream ${sessionId} answered ${response.status}`);
      const decoder = new TextDecoder();
      let buffered = '';
      for await (const chunk of response.body) {
        buffered += decoder.decode(chunk, { stream: true });
        let newline;
        while ((newline = buffered.indexOf('\n')) >= 0) {
          const line = buffered.slice(0, newline).trim();
          buffered = buffered.slice(newline + 1);
          if (!line) continue;
          const event = JSON.parse(line);
          events.push(event);
          if (event.type === 'session.waiting' || event.type === 'session.completed' || event.type === 'session.failed') return events;
        }
      }
      return events;
    }
  } finally {
    clearTimeout(timer);
    controller.abort();
  }
}
