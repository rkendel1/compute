// Compile crates/compute-state-feltdb/model/compute.flow to the FeltDB
// application manifest that `compute state provision` submits. The Rust
// adapter embeds the generated manifest; `--check` fails when it drifts.
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { flowSpecToManifest, parseFlowSpec, validateFlowSpec } from '@feltdb/core';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../../../crates/compute-state-feltdb/model');
export const flowPath = resolve(root, 'compute.flow');
export const manifestPath = resolve(root, 'compute.manifest.json');

export function compile() {
  const spec = parseFlowSpec(readFileSync(flowPath, 'utf8'));
  const errors = validateFlowSpec(spec).filter((diagnostic) => diagnostic.severity === 'error');
  if (errors.length > 0) {
    throw new Error(`compute.flow is invalid:\n${errors.map((error) => `- ${error.message}`).join('\n')}`);
  }
  // Tenant and application are filled in when Compute provisions.
  return `${JSON.stringify(flowSpecToManifest(spec, '', ''), null, 2)}\n`;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const manifest = compile();
  if (process.argv.includes('--write')) {
    writeFileSync(manifestPath, manifest);
    console.log(`wrote ${manifestPath}`);
  } else if (readFileSync(manifestPath, 'utf8') !== manifest) {
    console.error('compute.manifest.json is out of date; run `npm run generate` in packages/compute-state-model');
    process.exit(1);
  } else {
    console.log('compute.manifest.json matches compute.flow');
  }
}
