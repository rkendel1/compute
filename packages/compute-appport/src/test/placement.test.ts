import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtemp, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import test from "node:test";
import { permissionAuthorizer } from "@appport/authorization";
import { createRequest, type AppResponse, type Session } from "@appport/protocol";
import { createComputeApplication, type BundleExecutionRequest } from "../index.js";

const computeBinary = resolve(process.cwd(), "../../target/debug/compute");

const wasm = new Uint8Array([
  0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
  0x01, 0x04, 0x01, 0x60, 0x00, 0x00,
  0x03, 0x02, 0x01, 0x00,
  0x07, 0x0a, 0x01, 0x06, 0x5f, 0x73, 0x74, 0x61, 0x72, 0x74, 0x00, 0x00,
  0x0a, 0x04, 0x01, 0x02, 0x00, 0x0b,
]);

function session(permissions: string[]): Session {
  return {
    id: "placement-session",
    applicationId: "compute-appport-placement-tests",
    createdAt: new Date(0).toISOString(),
    permissions,
  };
}

function envelope(capability: string, input: unknown) {
  return createRequest({
    requestId: `${capability}-${Math.random()}`,
    capability: { name: capability, version: 1 },
    input,
  });
}

function output<T>(response: AppResponse): T {
  assert.equal(response.ok, true, response.ok ? undefined : `${response.error.message} ${JSON.stringify(response.error.details ?? {}).slice(0, 1500)}`);
  return response.output as T;
}

/** An isolated directory (no compute-pool.toml) holding a WASM bundle. */
async function wasmBundle(isolation?: "strict"): Promise<{ root: string; request: BundleExecutionRequest }> {
  const root = await mkdtemp(join(tmpdir(), "compute-appport-placement-"));
  await writeFile(join(root, "module.wasm"), wasm);
  await writeFile(join(root, "workload.json"), JSON.stringify({
    version: "1",
    runtime: "wasm",
    entrypoint: "module.wasm",
    network: "none",
    ...(isolation ? { isolation: { profile: isolation } } : {}),
  }));
  const bundle = join(root, "workload.compute");
  const created = spawnSync(computeBinary, [
    "bundle", "create", "--workload", join(root, "workload.json"), "--output", bundle, "--json",
  ], { encoding: "utf8" });
  assert.equal(created.status, 0, created.stderr);
  return { root, request: { bundle: { data: [...await readFile(bundle)] } } };
}

function application(root: string, poolConfig?: string) {
  return createComputeApplication({
    computeBinary,
    cwd: root,
    authorizer: permissionAuthorizer(),
    capabilityCache: join(root, "capabilities.json"),
    ...(poolConfig ? { poolConfig } : {}),
  });
}

test("placement and pool capabilities are published with the right effects and scopes", () => {
  const manifest = createComputeApplication({ computeBinary }).manifest();
  const find = (name: string) => manifest.capabilities.find((item) => item.name === name)!;
  for (const name of [
    "compute.provider.list", "compute.provider.inspect", "compute.provider.capabilities",
    "compute.placement.inspect",
  ]) {
    assert.equal(find(name).definitions[0]?.effect, "observation", name);
    assert.deepEqual(find(name).authorization, [], name);
  }
  assert.deepEqual(find("compute.pool.run").authorization, ["compute.run"]);
  assert.deepEqual(find("compute.pool.submit").authorization, ["compute.submit"]);
  for (const name of ["compute.pool.run", "compute.pool.submit"]) {
    assert.equal(find(name).definitions[0]?.effect, "consequential");
    assert.equal(find(name).definitions[0]?.authorizationContract?.required, true);
  }
  // compute.run@1 and compute.submit@1 are unchanged.
  assert.deepEqual(find("compute.run").authorization, ["compute.run"]);
  assert.deepEqual(find("compute.submit").authorization, ["compute.submit"]);
});

test("provider discovery capabilities validate against their schemas", async () => {
  const { root } = await wasmBundle();
  const app = application(root);
  const listed = output<any>(await app.handleRequest(envelope("compute.provider.list", {})));
  assert.deepEqual(listed.providers.map((item: any) => item.provider_id), ["local"]);
  assert.equal(listed.providers[0].discovery, "discovered");
  const inspected = output<any>(await app.handleRequest(
    envelope("compute.provider.inspect", { provider: "local" }),
  ));
  assert.equal(inspected.status, "discovered");
  assert.equal(inspected.descriptor.provider_kind, "local");
  const capabilities = output<any>(await app.handleRequest(
    envelope("compute.provider.capabilities", { provider: "local" }),
  ));
  assert.equal(capabilities.protocol, "compute.local@1");
  assert.ok(capabilities.runtime_artifacts);
});

test("placement inspection selects a provider without executing", async () => {
  const { root, request } = await wasmBundle();
  const app = application(root);
  const first = output<any>(await app.handleRequest(envelope("compute.placement.inspect", { request })));
  const second = output<any>(await app.handleRequest(envelope("compute.placement.inspect", { request })));
  assert.equal(first.outcome, "placed");
  assert.equal(first.selected.provider_id, "local");
  assert.equal(first.placement_id, second.placement_id);
  assert.ok(first.explanation.requires.includes("runtime wasm (any version)"));

  const explicit = output<any>(await app.handleRequest(
    envelope("compute.placement.inspect", { request, provider: "production" }),
  ));
  assert.equal(explicit.outcome, "placement_failed");
  assert.equal(explicit.failure.code, "provider_not_configured");

  const job = output<any>(await app.handleRequest(
    envelope("compute.placement.inspect", { request, submit: true }),
  ));
  assert.equal(job.failure.code, "no_compatible_provider");
  assert.equal(job.providers[0].reasons[0].code, "jobs_unsupported");
});

test("pool run requires authority, executes once, and binds placement into the receipt", async () => {
  const { root, request } = await wasmBundle("strict");
  const app = application(root);
  const denied = await app.handleRequest(envelope("compute.pool.run", { request }));
  assert.equal(denied.ok, false);

  const result = output<any>(await app.handleRequest(
    envelope("compute.pool.run", { request }), { session: session(["compute.run"]) },
  ));
  assert.equal(result.kind, "execution");
  assert.equal(result.receipt.placement.provider_id, "local");
  assert.equal(result.receipt.placement.selection_mode, "pool");
  assert.equal(result.receipt.placement.placement_id, result.placement.placement_id);
  assert.equal(result.receipt.isolation.effective, "strict");

  const failed = output<any>(await app.handleRequest(
    envelope("compute.pool.run", { request, provider: "production" }),
    { session: session(["compute.run"]) },
  ));
  assert.equal(failed.kind, "failure");
  assert.equal(failed.failure.kind, "placement_failed");
  assert.equal(failed.placement.outcome, "placement_failed");
});

test("pool submit places durable jobs only on job-capable providers", async (t) => {
  const { root, request } = await wasmBundle();
  const listener = spawnSync("sh", ["-c", "command -v true"]);
  if (listener.error) return t.skip("no shell available");
  const port = 18000 + Math.floor(Math.random() * 1000);
  const { spawn } = await import("node:child_process");
  const server = spawn(computeBinary, [
    "serve", "--listen", `127.0.0.1:${port}`, "--job-store", join(root, "jobs"),
  ], { stdio: "ignore" });
  try {
    await writeFile(join(root, "compute-pool.toml"), [
      "[providers.local]", "kind = \"local\"", "priority = 100", "",
      "[providers.jobs]", "kind = \"remote\"", `endpoint = "http://127.0.0.1:${port}"`, "priority = 1",
    ].join("\n"));
    const app = application(root, "compute-pool.toml");
    for (let attempt = 0; attempt < 50; attempt += 1) {
      const probe = spawnSync(computeBinary, [
        "remote", "health", "--provider", `http://127.0.0.1:${port}`, "--json",
      ]);
      if (probe.status === 0) break;
      await new Promise((done) => setTimeout(done, 100));
    }
    const denied = await app.handleRequest(envelope("compute.pool.submit", { request }));
    assert.equal(denied.ok, false);
    const submitted = output<any>(await app.handleRequest(
      envelope("compute.pool.submit", { request }), { session: session(["compute.submit"]) },
    ));
    assert.equal(submitted.provider_id, "jobs", "the higher-priority local provider cannot accept jobs");
    assert.match(submitted.job_id, /^job_[0-9a-f]{64}$/);
    assert.equal(submitted.placement.selected.provider_id, "jobs");
  } finally {
    server.kill();
  }
});
