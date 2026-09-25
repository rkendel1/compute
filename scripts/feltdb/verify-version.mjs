// Verify that Compute resolves exactly the certified @feltdb/core.
//
// The certified version is declared once, as `CERTIFIED_FELTDB_VERSION` in
// crates/compute-state-feltdb/src/lib.rs. This script checks what npm actually
// resolved (node_modules and the lockfile), not only what package.json asks
// for, and fails on any other @feltdb/core anywhere in the workspace's
// lockfiles. Run after `npm ci` in packages/compute-state-model:
//
//   node scripts/feltdb/verify-version.mjs [--json]
import { readFileSync, readdirSync, existsSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const source = readFileSync(join(root, 'crates/compute-state-feltdb/src/lib.rs'), 'utf8');
const certified = source.match(/pub const CERTIFIED_FELTDB_VERSION: &str = "([^"]+)";/)?.[1];
const failures = [];
const resolved = {};
if (!certified) failures.push('CERTIFIED_FELTDB_VERSION is not declared in compute-state-feltdb');

// Every lockfile in packages/: whatever @feltdb/core it resolves must be the certified one.
for (const name of readdirSync(join(root, 'packages'))) {
  const lockfile = join(root, 'packages', name, 'package-lock.json');
  if (!existsSync(lockfile)) continue;
  const lock = JSON.parse(readFileSync(lockfile, 'utf8'));
  for (const [path, entry] of Object.entries(lock.packages ?? {})) {
    if (!path.endsWith('node_modules/@feltdb/core')) continue;
    resolved[`packages/${name}/package-lock.json:${path}`] = entry.version;
    if (entry.version !== certified) {
      failures.push(`packages/${name} locks @feltdb/core ${entry.version}, not ${certified}`);
    }
  }
}

// The model compiler must resolve it at run time, from node_modules.
const installed = join(root, 'packages/compute-state-model/node_modules/@feltdb/core/package.json');
if (!existsSync(installed)) {
  failures.push('packages/compute-state-model has no installed @feltdb/core; run npm ci there first');
} else {
  const version = JSON.parse(readFileSync(installed, 'utf8')).version;
  resolved['packages/compute-state-model/node_modules/@feltdb/core'] = version;
  if (version !== certified) failures.push(`node_modules resolves @feltdb/core ${version}, not ${certified}`);
}
if (!Object.keys(resolved).some((key) => key.includes('compute-state-model/package-lock.json'))) {
  failures.push('packages/compute-state-model/package-lock.json does not lock @feltdb/core');
}

const report = { certified, resolved, status: failures.length === 0 ? 'PASS' : 'FAIL', failures };
if (process.argv.includes('--json')) {
  console.log(JSON.stringify(report, null, 2));
} else if (failures.length === 0) {
  console.log(`@feltdb/core ${certified} resolved everywhere (${Object.keys(resolved).length} locations)`);
} else {
  for (const failure of failures) console.error(failure);
}
process.exit(failures.length === 0 ? 0 : 1);
