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
    id: "policy-session",
    applicationId: "compute-appport-policy-tests",
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
  assert.equal(response.ok, true, response.ok ? undefined : response.error.message);
  return response.output as T;
}

/** A directory whose compute.toml applies a strict local policy. */
async function strictProject(): Promise<{ root: string; request: BundleExecutionRequest }> {
  const root = await mkdtemp(join(tmpdir(), "compute-appport-policy-"));
  await writeFile(join(root, "module.wasm"), wasm);
  await writeFile(join(root, "workload.json"), JSON.stringify({
    version: "1", runtime: "wasm", entrypoint: "module.wasm", network: "none",
  }));
  const bundle = join(root, "workload.compute");
  const created = spawnSync(computeBinary, [
    "bundle", "create", "--workload", join(root, "workload.json"), "--output", bundle, "--json",
  ], { encoding: "utf8" });
  assert.equal(created.status, 0, created.stderr);
  await writeFile(join(root, "strict.json"), JSON.stringify({
    version: 1, name: "strict-policy", minimum_isolation: "strict",
  }));
  await writeFile(join(root, "compute.toml"), "[policy]\npath = \"strict.json\"\n");
  return { root, request: { bundle: { data: [...await readFile(bundle)] } } };
}

test("policy capabilities are public observations; existing capabilities are unchanged", () => {
  const manifest = createComputeApplication({ computeBinary }).manifest();
  const find = (name: string) => manifest.capabilities.find((item) => item.name === name)!;
  for (const name of ["compute.policy.inspect", "compute.policy.check", "compute.policy.explain", "compute.admission"]) {
    assert.equal(find(name).definitions[0]?.effect, "observation", name);
    assert.deepEqual(find(name).authorization, [], name);
  }
  assert.deepEqual(find("compute.run").authorization, ["compute.run"]);
  assert.deepEqual(find("compute.submit").authorization, ["compute.submit"]);
});

test("policy inspection reports the intersected sources", async () => {
  const { root } = await strictProject();
  const app = createComputeApplication({ computeBinary, cwd: root });
  const inspected = output<any>(await app.handleRequest(envelope("compute.policy.inspect", {})));
  assert.deepEqual(inspected.sources.map((source: any) => source.kind), ["baseline", "local"]);
  assert.equal(inspected.policy.minimum_isolation, "strict");
});

test("admission evidence is transported, not redefined", async () => {
  const { root, request } = await strictProject();
  const app = createComputeApplication({ computeBinary, cwd: root, authorizer: permissionAuthorizer() });

  const denied = output<any>(await app.handleRequest(envelope("compute.policy.check", { request })));
  assert.equal(denied.admission.status, "denied");
  assert.deepEqual(denied.reasons.map((reason: any) => reason.code), ["isolation_below_minimum"]);
  const decision = output<any>(await app.handleRequest(envelope("compute.admission", { request })));
  assert.equal(decision.admission_version, "compute.admission@1");
  assert.equal(decision.admission_id, denied.admission.admission_id);
  assert.equal(decision.admitted, false);

  const admitted = output<any>(await app.handleRequest(
    envelope("compute.admission", { request, isolation: "strict" }),
  ));
  assert.equal(admitted.admitted, true);
  const explained = output<any>(await app.handleRequest(
    envelope("compute.policy.explain", { request, isolation: "strict" }),
  ));
  assert.ok(explained.explanation.includes("Admission: admitted"));

  // compute.run@1 itself is unchanged, and a denied execution runs nothing.
  const run = output<any>(await app.handleRequest(
    envelope("compute.run", { request }), { session: session(["compute.run"]) },
  ));
  assert.equal(run.kind, "failure");
  assert.equal(run.failure.kind, "admission_denied");
  assert.match(run.failure.message, /weaker than the policy minimum strict/);
});
