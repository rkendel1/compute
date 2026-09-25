#!/usr/bin/env node
// Upgrade FeltDB across the platform as one deliberate, observable operation.
//
//   node scripts/platform/feltdb.mjs status   [--root DIR] [--json]
//   node scripts/platform/feltdb.mjs upgrade  --to VERSION [--consumer a,b] [--apply] [--root DIR]
//   node scripts/platform/feltdb.mjs verify   [--consumer a,b] [--install] [--root DIR]
//   node scripts/platform/feltdb.mjs rollback [--consumer a,b] [--root DIR]
//
// It uses each repository's own package manager (npm, pnpm, bun) and its own
// tests. It adds no dependency system: the consumer list is
// scripts/platform/feltdb-consumers.json, and upgrades edit package.json and
// regenerate lockfiles the way a person would. `upgrade` is a dry run unless
// --apply is given; --apply snapshots every file it changes so `rollback`
// restores them byte for byte.

import { execSync, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const registry = JSON.parse(readFileSync(join(here, "feltdb-consumers.json"), "utf8"));
const PACKAGE = registry.package;
const SECTIONS = ["dependencies", "devDependencies", "peerDependencies", "optionalDependencies", "overrides", "resolutions"];

function parseArguments(argv) {
  const [command, ...rest] = argv;
  const options = { command, root: resolve(here, "../../.."), json: false, apply: false, install: false };
  for (let index = 0; index < rest.length; index += 1) {
    const flag = rest[index];
    if (flag === "--json") options.json = true;
    else if (flag === "--apply") options.apply = true;
    else if (flag === "--install") options.install = true;
    else if (flag === "--root") options.root = resolve(rest[++index]);
    else if (flag === "--to") options.to = rest[++index];
    else if (flag === "--consumer") options.consumers = rest[++index].split(",");
    else if (flag === "--state") options.state = resolve(rest[++index]);
    else throw new Error(`unknown option ${flag}`);
  }
  options.state ??= join(options.root, ".feltdb-upgrade");
  return options;
}

function checkout(options, consumer) {
  for (const candidate of [join(options.root, consumer.checkout), join(options.root, "rkendel1", consumer.checkout)]) {
    if (existsSync(join(candidate, ".git")) || existsSync(join(candidate, "package.json"))) return candidate;
  }
  return null;
}

function selected(options) {
  return registry.consumers.filter((consumer) => !options.consumers || options.consumers.includes(consumer.name));
}

/** Every place a manifest names the package: section and specifier. */
function declarations(root, consumer) {
  const found = [];
  for (const manifest of consumer.manifests) {
    const path = join(root, manifest);
    if (!existsSync(path)) {
      found.push({ manifest, missing: true });
      continue;
    }
    const json = JSON.parse(readFileSync(path, "utf8"));
    for (const section of SECTIONS) {
      const value = json[section]?.[PACKAGE];
      if (typeof value === "string") found.push({ manifest, section, specifier: value });
    }
  }
  return found;
}

/** The version the lockfile actually resolves. */
function resolved(root, consumer) {
  const path = join(root, consumer.lockfile);
  if (!existsSync(path)) return { lockfile: consumer.lockfile, missing: true, versions: [] };
  const text = readFileSync(path, "utf8");
  const versions = new Set();
  if (consumer.manager === "npm") {
    const lock = JSON.parse(text);
    for (const [key, entry] of Object.entries(lock.packages ?? {})) {
      if (key.endsWith(`node_modules/${PACKAGE}`) && entry.version) versions.add(entry.version);
    }
  } else if (consumer.manager === "pnpm") {
    for (const match of text.matchAll(new RegExp(`^\\s+'?${PACKAGE.replace("/", "\\/")}@([0-9][^(':\\s]*)`, "gm"))) versions.add(match[1]);
  } else if (consumer.manager === "bun") {
    for (const match of text.matchAll(new RegExp(`"${PACKAGE.replace("/", "\\/")}": \\["${PACKAGE.replace("/", "\\/")}@([^"]+)"`, "g"))) versions.add(match[1]);
  }
  return { lockfile: consumer.lockfile, versions: [...versions].sort() };
}

function published() {
  try {
    return JSON.parse(execSync(`npm view ${PACKAGE} version --json`, { encoding: "utf8", stdio: ["ignore", "pipe", "ignore"], timeout: 30_000 }));
  } catch {
    return null;
  }
}

function status(options) {
  const latest = published();
  const rows = selected(options).map((consumer) => {
    const root = checkout(options, consumer);
    if (!root) return { consumer: consumer.name, repository: consumer.repository, checkout: null, problem: "no local checkout under --root" };
    const declared = declarations(root, consumer);
    const lock = resolved(root, consumer);
    const head = spawnSync("git", ["-C", root, "rev-parse", "--short", "HEAD"], { encoding: "utf8" }).stdout.trim() || null;
    return {
      consumer: consumer.name,
      repository: consumer.repository,
      head,
      manager: consumer.manager,
      declared,
      resolved: lock.versions,
      transitive_via: consumer.transitive_via ?? null,
      schema_affecting: consumer.schema_affecting,
      runtime: consumer.runtime,
      current: latest !== null && lock.versions.length > 0 && lock.versions.every((version) => version === latest),
    };
  });
  const versions = [...new Set(rows.flatMap((row) => row.resolved ?? []))].sort();
  return { package: PACKAGE, latest_published: latest, distinct_resolved_versions: versions, aligned: versions.length === 1, consumers: rows };
}

function printStatus(report) {
  console.log(`${report.package}: latest published ${report.latest_published ?? "unknown"}; resolved across consumers: ${report.distinct_resolved_versions.join(", ") || "none"}${report.aligned ? " (aligned)" : " (NOT aligned)"}`);
  for (const row of report.consumers) {
    if (!row.head) {
      console.log(`  ${row.consumer.padEnd(18)} ${row.problem}`);
      continue;
    }
    const declared = row.declared.filter((item) => !item.missing).map((item) => `${item.manifest}:${item.section}=${item.specifier}`).join(" ") || (row.transitive_via ? `transitive via ${row.transitive_via}` : "none");
    console.log(`  ${row.consumer.padEnd(18)} ${row.manager.padEnd(4)} resolved ${(row.resolved.join(",") || "-").padEnd(8)} ${row.current ? "current " : "behind  "} ${declared}`);
  }
}

/** A specifier for the target that keeps the consumer's range style. */
function retarget(specifier, version) {
  if (specifier.startsWith("$") || specifier.startsWith("workspace:") || specifier.startsWith("file:")) return specifier;
  const prefix = specifier.match(/^[\^~>=<]*/)[0];
  return `${prefix}${version}`;
}

function lockCommand(consumer) {
  switch (consumer.manager) {
    case "npm": return "npm install --package-lock-only --ignore-scripts --no-audit --no-fund";
    case "pnpm": return "pnpm install --lockfile-only --ignore-scripts";
    case "bun": return "bun install --lockfile-only --ignore-scripts";
    default: throw new Error(`unknown package manager ${consumer.manager}`);
  }
}

function snapshotPath(options, consumer) {
  return join(options.state, `${consumer.name}.json`);
}

function upgrade(options) {
  if (!options.to) throw new Error("upgrade needs --to VERSION");
  const results = [];
  for (const consumer of selected(options)) {
    const root = checkout(options, consumer);
    if (!root) {
      results.push({ consumer: consumer.name, skipped: "no local checkout" });
      continue;
    }
    const changes = [];
    for (const item of declarations(root, consumer)) {
      if (item.missing) continue;
      const next = retarget(item.specifier, options.to);
      if (next !== item.specifier) changes.push({ ...item, to: next });
    }
    const before = resolved(root, consumer).versions;
    const result = { consumer: consumer.name, resolved_before: before, changes, lock_command: lockCommand(consumer), applied: false };
    if (changes.length === 0 && consumer.transitive_via) {
      result.note = `only transitive through ${consumer.transitive_via}: upgrade that package, then regenerate this lockfile`;
    }
    if (options.apply && (changes.length > 0 || before.some((version) => version !== options.to))) {
      const files = [...new Set([...consumer.manifests, consumer.lockfile])].filter((file) => existsSync(join(root, file)));
      mkdirSync(options.state, { recursive: true });
      if (!existsSync(snapshotPath(options, consumer))) {
        writeFileSync(snapshotPath(options, consumer), JSON.stringify({
          consumer: consumer.name, root, taken_at: new Date().toISOString(),
          files: Object.fromEntries(files.map((file) => [file, readFileSync(join(root, file), "utf8")])),
        }));
      }
      for (const change of changes) {
        const path = join(root, change.manifest);
        const json = JSON.parse(readFileSync(path, "utf8"));
        json[change.section][PACKAGE] = change.to;
        writeFileSync(path, `${JSON.stringify(json, null, 2)}\n`);
      }
      const lock = spawnSync(lockCommand(consumer), { cwd: join(root, consumer.install_dir), shell: true, encoding: "utf8" });
      result.lock_exit = lock.status;
      if (lock.status !== 0) result.lock_error = (lock.stderr || lock.stdout).slice(-600);
      result.resolved_after = resolved(root, consumer).versions;
      result.applied = true;
    }
    results.push(result);
  }
  return { package: PACKAGE, target: options.to, dry_run: !options.apply, results };
}

function installCommand(consumer) {
  switch (consumer.manager) {
    case "npm": return "npm ci --ignore-scripts --no-audit --no-fund";
    case "pnpm": return "pnpm install --frozen-lockfile --ignore-scripts";
    case "bun": return "bun install --frozen-lockfile --ignore-scripts";
    default: throw new Error(`unknown package manager ${consumer.manager}`);
  }
}

function tail(text) {
  return (text || "").trim().split("\n").slice(-12).join("\n");
}

function verify(options) {
  const results = [];
  for (const consumer of selected(options)) {
    const root = checkout(options, consumer);
    if (!root) {
      results.push({ consumer: consumer.name, skipped: "no local checkout" });
      continue;
    }
    const checks = [];
    const installDir = join(root, consumer.install_dir);
    if (options.install || !existsSync(join(installDir, "node_modules"))) {
      const install = spawnSync(installCommand(consumer), { cwd: installDir, shell: true, encoding: "utf8" });
      checks.push({ check: "install from the lockfile", command: installCommand(consumer), passed: install.status === 0, output: install.status === 0 ? undefined : tail(install.stderr || install.stdout) });
      if (install.status !== 0) {
        results.push({ consumer: consumer.name, passed: false, checks });
        continue;
      }
    }
    const cli = join(installDir, "node_modules", PACKAGE, "bin", "feltdb.js");
    const installed = existsSync(join(installDir, "node_modules", PACKAGE, "package.json"))
      ? JSON.parse(readFileSync(join(installDir, "node_modules", PACKAGE, "package.json"), "utf8")).version
      : null;
    for (const flow of consumer.flows) {
      if (!existsSync(cli)) {
        checks.push({ check: `validate ${flow}`, passed: null, output: `${PACKAGE} is not installed where its CLI can validate the model` });
        continue;
      }
      const run = spawnSync(process.execPath, [cli, "validate", join(root, flow)], { cwd: root, encoding: "utf8" });
      checks.push({ check: `validate ${flow} with ${PACKAGE}@${installed}`, passed: run.status === 0, output: run.status === 0 ? undefined : tail(run.stdout + run.stderr) });
    }
    for (const step of consumer.verify) {
      const missing = (step.requires ?? []).filter((name) => !process.env[name]);
      if (missing.length) {
        checks.push({ check: step.proves, command: step.run, passed: null, output: `skipped: set ${missing.join(", ")}` });
        continue;
      }
      const run = spawnSync(step.run, { cwd: join(root, step.cwd), shell: true, encoding: "utf8", env: process.env, timeout: 30 * 60_000 });
      checks.push({ check: step.proves, command: step.run, passed: run.status === 0, output: run.status === 0 ? undefined : tail(run.stdout + run.stderr) });
    }
    const decided = checks.filter((check) => check.passed !== null);
    results.push({ consumer: consumer.name, installed, passed: decided.length > 0 && decided.every((check) => check.passed), checks });
  }
  return { package: PACKAGE, results };
}

function rollback(options) {
  const results = [];
  for (const consumer of selected(options)) {
    const path = snapshotPath(options, consumer);
    if (!existsSync(path)) {
      results.push({ consumer: consumer.name, skipped: "no upgrade snapshot" });
      continue;
    }
    const snapshot = JSON.parse(readFileSync(path, "utf8"));
    for (const [file, content] of Object.entries(snapshot.files)) writeFileSync(join(snapshot.root, file), content);
    const clean = spawnSync("git", ["-C", snapshot.root, "status", "--porcelain", "--", ...Object.keys(snapshot.files)], { encoding: "utf8" }).stdout.trim();
    spawnSync("rm", ["-f", path]);
    results.push({ consumer: consumer.name, restored: Object.keys(snapshot.files), matches_git_head: clean === "", resolved: resolved(snapshot.root, consumer).versions });
  }
  return { package: PACKAGE, results };
}

function main() {
  const options = parseArguments(process.argv.slice(2));
  let report;
  switch (options.command) {
    case "status": report = status(options); break;
    case "upgrade": report = upgrade(options); break;
    case "verify": report = verify(options); break;
    case "rollback": report = rollback(options); break;
    default:
      console.error("usage: feltdb.mjs status|upgrade|verify|rollback [--root DIR] [--consumer a,b] [--to VERSION] [--apply] [--json]");
      process.exit(2);
  }
  if (options.json || options.command !== "status") console.log(JSON.stringify(report, null, 2));
  else printStatus(report);
  if (options.command === "status" && !report.aligned) process.exitCode = 1;
  if (options.command === "verify" && report.results.some((result) => result.passed === false)) process.exitCode = 1;
  if (options.command === "upgrade" && report.results.some((result) => result.lock_exit && result.lock_exit !== 0)) process.exitCode = 1;
}

main();
