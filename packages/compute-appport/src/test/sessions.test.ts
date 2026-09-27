import assert from "node:assert/strict";
import { chmod, mkdtemp, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { ComputeSessionError, createComputeSessions } from "../index.js";

const record = {
  version: "compute.session@1",
  session_id: `ses_${"a".repeat(64)}`,
  status: "ready",
  node_id: "node",
  provider_kind: "workspace",
  job_id: `job_${"b".repeat(64)}`,
  execution_id: "exec_1_1",
  ownership: "ephemeral",
  resources: { cpu_count: 2, memory_bytes: 2147483648 },
  network: "network",
  capabilities: {
    exec: true, terminal: false, filesystem: true, network: true, public_endpoint: false,
    persistent_storage: false, suspend: true, resume: true, claim: true,
  },
  created_at: "2026-09-26T00:00:00Z",
  updated_at: "2026-09-26T00:00:00Z",
  expires_at: "2026-09-26T01:00:00Z",
  generation: 3,
};

/**
 * A stand-in `compute` that records its arguments and answers like the
 * real one, so the client's wiring is tested without a provider.
 */
async function fakeCompute(): Promise<{ binary: string; calls: () => Promise<string[][]> }> {
  const root = await mkdtemp(join(tmpdir(), "compute-sessions-"));
  const log = join(root, "calls.jsonl");
  const binary = join(root, "compute");
  await writeFile(binary, `#!/usr/bin/env node
const fs = require("node:fs");
const args = process.argv.slice(2);
fs.appendFileSync(${JSON.stringify(log)}, JSON.stringify(args) + "\\n");
const record = ${JSON.stringify(record)};
const [scope, command] = args;
if (scope !== "session") process.exit(64);
if (command === "create") {
  console.log(JSON.stringify({ placement_id: "sha256:p", provider_id: "node", session: record }));
} else if (command === "exec") {
  const separator = args.indexOf("--");
  const program = args.slice(separator + 1);
  const failed = program.includes("false");
  console.log(JSON.stringify({
    execution_id: "exec_2_2", job_id: "job_" + "c".repeat(64), status: "completed",
    exit_code: failed ? 1 : 0, stdout: { text: program.join(" ") + "\\n" }, stderr: { text: "" },
  }));
  process.exit(failed ? 1 : 0);
} else if (command === "destroy") {
  console.log(JSON.stringify({ ...record, status: "destroyed" }));
} else if (command === "resume") {
  console.error("invalid workload: operation_unsupported: session provider fake does not support resume");
  process.exit(1);
} else {
  console.log(JSON.stringify(record));
}
`);
  await chmod(binary, 0o755);
  return {
    binary,
    calls: async () => (await readFile(log, "utf8")).trim().split("\n").map((line) => JSON.parse(line) as string[]),
  };
}

test("sessions are created, used, and destroyed through compute session", async () => {
  const fake = await fakeCompute();
  const compute = createComputeSessions({ computeBinary: fake.binary, poolConfig: "pool.toml" });
  const session = await compute.create({ resources: { cpu: 2, memory: "2Gi" }, ttl: "1h", require: ["exec"] });
  assert.equal(session.id, record.session_id);
  assert.equal(session.record.job_id, record.job_id);
  assert.equal(session.providerId, "node");

  const ran = await session.exec(["echo", "hello"], { env: { MODE: "test" }, timeout: "10s" });
  assert.equal(ran.stdout, "echo hello\n");
  assert.equal(ran.exitCode, 0);
  assert.equal(ran.executionId, "exec_2_2");
  // A failing command is a result, not an exception.
  const failed = await session.exec(["false"]);
  assert.equal(failed.exitCode, 1);

  await assert.rejects(session.resume(), (error: unknown) =>
    error instanceof ComputeSessionError && error.code === "operation_unsupported");
  const destroyed = await session.destroy();
  assert.equal(destroyed.status, "destroyed");
  assert.equal(session.record.status, "destroyed");

  const calls = await fake.calls();
  assert.deepEqual(calls[0], [
    "session", "create", "--pool-config", "pool.toml", "--json", "--cpu", "2", "--memory", "2Gi",
    "--ttl", "1h", "--require", "exec", "--wait",
  ]);
  assert.deepEqual(calls[1], [
    "session", "exec", "--pool-config", "pool.toml", record.session_id, "--json",
    "--env", "MODE=test", "--timeout", "10s", "--", "echo", "hello",
  ]);
});
