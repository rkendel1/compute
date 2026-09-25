import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { compile, manifestPath } from './compile.mjs';

test('the checked-in manifest is exactly compute.flow compiled', () => {
  assert.equal(readFileSync(manifestPath, 'utf8'), compile());
});

test('every Compute collection is in the model with an access policy', () => {
  const manifest = JSON.parse(compile());
  const collections = manifest.collections.map((collection) => collection.name).sort();
  assert.deepEqual(collections, [
    'Artifact', 'ArtifactChunk', 'Audit', 'Certificate', 'Deployment', 'DnsRecord', 'Domain', 'Environment', 'EnvironmentProject', 'Event', 'Execution',
    'OperatorCredential', 'Project', 'ProjectRevision', 'Provider', 'Receipt', 'Service', 'TrafficAssignment', 'Workload', 'WorkloadInstance', 'WorkloadStatus',
  ]);
  assert.deepEqual(manifest.policies.map((policy) => policy.resource).sort(), collections);
});
