import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtemp, readFile, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import test from "node:test";
import { permissionAuthorizer } from "@appport/authorization";
import { createRequest, type AppResponse, type Session } from "@appport/protocol";
import {
  createComputeApplication,
  type BundleExecutionRequest,
  type WorkloadExecutionRequest,
  type WorkloadSpec,
} from "../index.js";

const computeBinary = resolve(process.cwd(), "../../target/debug/compute");

function session(permissions: string[]): Session {
  return {
    id: "test-session",
    applicationId: "compute-appport-tests",
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

async function fixture(runtime: WorkloadSpec["runtime"], entrypoint: string, source: string | Uint8Array) {
  const root = await mkdtemp(join(tmpdir(), "compute-appport-"));
  await writeFile(join(root, entrypoint), source);
  return root;
}

async function writeWorkload(root: string, workload: WorkloadSpec): Promise<WorkloadExecutionRequest> {
  await writeFile(join(root, "workload.json"), JSON.stringify(workload, null, 2));
  return { workload, invocation: { workload_path: "workload.json" } };
}

async function bundleRequest(root: string, workload: WorkloadSpec): Promise<BundleExecutionRequest> {
  await writeFile(join(root, "workload.json"), JSON.stringify(workload, null, 2));
  const bundle = join(root, "workload.compute");
  const created = spawnSync(computeBinary, [
    "bundle", "create", "--workload", join(root, "workload.json"), "--output", bundle, "--json",
  ], { encoding: "utf8" });
  assert.equal(created.status, 0, created.stderr);
  return { bundle: { data: [...await readFile(bundle)] } };
}

function output<T>(response: AppResponse): T {
  assert.equal(response.ok, true, response.ok ? undefined : `${response.error.message} ${JSON.stringify(response.error.details ?? {}).slice(0, 1500)}`);
  return response.output as T;
}

test("manifest exposes public inspection and authorized consequential execution", () => {
  const application = createComputeApplication({ computeBinary });
  const manifest = application.manifest();
  const inspect = manifest.capabilities.find((item) => item.name === "compute.inspect")!;
  const run = manifest.capabilities.find((item) => item.name === "compute.run")!;
  assert.deepEqual(inspect.authorization, []);
  assert.equal(inspect.definitions[0]?.effect, "observation");
  assert.deepEqual(run.authorization, ["compute.run"]);
  assert.equal(run.definitions[0]?.effect, "consequential");
  assert.equal(run.definitions[0]?.authorizationContract?.required, true);
  for (const [name, scope] of [
    ["compute.submit", "compute.submit"],
    ["compute.status", "compute.status"],
    ["compute.cancel", "compute.cancel"],
    ["compute.result", "compute.result"],
    ["compute.receipt", "compute.receipt"],
  ] as const) {
    const capability = manifest.capabilities.find((item) => item.name === name)!;
    assert.deepEqual(capability.authorization, [scope]);
    assert.equal(capability.definitions[0]?.authorizationContract?.required, true);
  }
  assert.equal(application.verify().ready, true);
});

test("inspect and dry-run expose identity and data flow without executing", async () => {
  const root = await fixture("python", "main.py", "open('executed', 'w').write('bad')\n");
  const workload: WorkloadSpec = {
    version: "1",
    runtime: "python",
    entrypoint: "main.py",
    inputs: [{ path: "nested/input.txt", source: { type: "inline", data: "hello" } }],
    outputs: [{ path: "result.txt", required: true }],
    network: "network",
  };
  const request = await writeWorkload(root, workload);
  const application = createComputeApplication({ computeBinary, cwd: root });
  for (const mode of ["inspect", "dry_run"] as const) {
    const result = output<any>(await application.handleRequest(envelope("compute.inspect", { request, mode })));
    assert.equal(result.kind, "inspection");
    assert.match(result.plan.workload_id, /^sha256:[0-9a-f]{64}$/);
    assert.deepEqual(result.plan.capability.operations, ["compute.inspect", "compute.run"]);
    assert.equal(result.plan.isolation.requested, "process");
    assert.equal(result.plan.isolation.compatible, true);
    assert.deepEqual(result.plan.data_flow.steps, [
      "input", "materialization", "entrypoint", "runtime", "declared_output", "collection",
    ]);
  }
  await assert.rejects(readFile(join(root, "executed")));
});

test("inspect reports immutable dependency requirements without package semantics", async () => {
  const root = await fixture("python", "main.py", "print('not executed')\n");
  const capsule = `sha256:${"a".repeat(64)}`;
  const request = await writeWorkload(root, {
    version: "1",
    runtime: "python",
    entrypoint: "main.py",
    network: "network",
    dependencies: { capsule },
  });
  const application = createComputeApplication({ computeBinary, cwd: root });
  const result = output<any>(
    await application.handleRequest(envelope("compute.inspect", { request, mode: "inspect" })),
  );
  assert.equal(result.kind, "inspection");
  assert.deepEqual(result.plan.dependencies, {
    required: true,
    capsule_id: capsule,
    available: false,
  });
});

test("run is denied without compute.run authority before workload execution", async () => {
  const root = await fixture("python", "main.py", "open('executed', 'w').write('bad')\n");
  const request = await writeWorkload(root, {
    version: "1", runtime: "python", entrypoint: "main.py", network: "network",
  });
  const application = createComputeApplication({ computeBinary, cwd: root });
  const response = await application.handleRequest(envelope("compute.run", { request }));
  assert.equal(response.ok, false);
  if (!response.ok) assert.equal(response.error.code, "UNAUTHORIZED");
  await assert.rejects(readFile(join(root, "executed")));
});

test("authorized run materializes nested inputs and returns only declared output bytes", async () => {
  const root = await fixture("python", "main.py", [
    "import os",
    "work=os.environ['COMPUTE_WORK_DIR']",
    "out=os.environ['COMPUTE_OUTPUT_DIR']",
    "a=open(os.path.join(work,'nested/inline.txt'),'rb').read()",
    "b=open(os.path.join(work,'nested/file.txt'),'rb').read()",
    "open(os.path.join(out,'result.bin'),'wb').write(a+b)",
    "open(os.path.join(out,'unexpected.txt'),'w').write('not portable output')",
  ].join("\n"));
  await writeFile(join(root, "payload.bin"), new Uint8Array([0, 255, 7]));
  const request = await writeWorkload(root, {
    version: "1",
    runtime: "python",
    entrypoint: "main.py",
    inputs: [
      { path: "nested/inline.txt", source: { type: "inline", data: "hello" } },
      { path: "nested/file.txt", source: { type: "file", path: "payload.bin" } },
    ],
    outputs: [
      { path: "result.bin", required: true },
      { path: "optional.txt", required: false },
    ],
    network: "network",
  });
  const application = createComputeApplication({
    computeBinary,
    cwd: root,
    authorizer: permissionAuthorizer(),
  });
  const response = await application.handleRequest(
    envelope("compute.run", { request }),
    { session: session(["compute.run"]) },
  );
  const result = output<any>(response);
  assert.equal(result.kind, "execution");
  assert.deepEqual(result.result.outputs.map((item: any) => item.path), ["result.bin"]);
  assert.deepEqual(result.result.outputs[0].data, [104, 101, 108, 108, 111, 0, 255, 7]);
  assert.deepEqual(result.result.missing_outputs, [{ path: "optional.txt", required: false }]);
  assert.equal(result.isolation.profile, "process");
  assert.equal(result.isolation.filesystem, "unavailable");
  assert.deepEqual(result.receipt.isolation, result.isolation);
});

test("strict process isolation is rejected before authorized execution", async () => {
  const root = await fixture("python", "main.py", "open('executed', 'w').write('bad')\n");
  const request = await writeWorkload(root, {
    version: "1",
    runtime: "python",
    entrypoint: "main.py",
    network: "network",
    isolation: { profile: "strict" },
  });
  const application = createComputeApplication({
    computeBinary,
    cwd: root,
    authorizer: permissionAuthorizer(),
  });
  const inspected = output<any>(await application.handleRequest(
    envelope("compute.inspect", { request, mode: "dry_run" }),
  ));
  assert.equal(inspected.plan.isolation.compatible, false);
  assert.equal(inspected.plan.isolation.reason.code, "filesystem_isolation_unavailable");
  const result = output<any>(await application.handleRequest(
    envelope("compute.run", { request }),
    { session: session(["compute.run"]) },
  ));
  assert.equal(result.kind, "failure");
  assert.equal(result.failure.kind, "capability_denied");
  await assert.rejects(readFile(join(root, "executed")));
});

test("missing required output remains a distinct output_contract failure", async () => {
  const root = await fixture("python", "main.py", "print('done')\n");
  const request = await writeWorkload(root, {
    version: "1",
    runtime: "python",
    entrypoint: "main.py",
    outputs: [{ path: "missing.txt", required: true }],
    network: "network",
  });
  const application = createComputeApplication({
    computeBinary,
    cwd: root,
    authorizer: permissionAuthorizer(),
  });
  const result = output<any>(await application.handleRequest(
    envelope("compute.run", { request }),
    { session: session(["compute.run"]) },
  ));
  assert.equal(result.kind, "failure");
  assert.equal(result.failure.kind, "output_contract");
  assert.equal(result.result.error.kind, "output_contract");
});

test("provider rejects a source file that differs from the transported WorkloadSpec", async () => {
  const root = await fixture("python", "main.py", "print('ok')\n");
  const stored: WorkloadSpec = {
    version: "1", runtime: "python", entrypoint: "main.py", network: "network",
  };
  const request = await writeWorkload(root, stored);
  request.workload.args = ["changed-after-file-write"];
  const application = createComputeApplication({ computeBinary, cwd: root });
  const result = output<any>(await application.handleRequest(envelope("compute.inspect", { request })));
  assert.equal(result.kind, "failure");
  assert.equal(result.failure.kind, "invalid_execution_request");
});

test("bundle identity mismatch is rejected through compute.run before execution", async () => {
  const root = await fixture("python", "main.py", "print('must not run')\n");
  const request = await bundleRequest(root, {
    version: "1", runtime: "python", entrypoint: "main.py", network: "network",
  });
  request.bundle.expected_bundle_id = "sha256:wrong";
  const application = createComputeApplication({
    computeBinary, cwd: root, authorizer: permissionAuthorizer(),
  });
  const result = output<any>(await application.handleRequest(
    envelope("compute.run", { request }), { session: session(["compute.run"]) },
  ));
  assert.equal(result.kind, "failure");
  assert.equal(result.failure.kind, "identity_mismatch");
});

test("unsafe paths, symlinks, and destination conflicts fail at the AppPort boundary", async () => {
  const cases: Array<[string, WorkloadSpec]> = [
    ["traversal", {
      version: "1", runtime: "python", entrypoint: "main.py",
      inputs: [{ path: "../escape", source: { type: "inline", data: "bad" } }], network: "network",
    }],
    ["absolute", {
      version: "1", runtime: "python", entrypoint: "main.py",
      outputs: [{ path: "/tmp/escape", required: true }], network: "network",
    }],
    ["destination-conflict", {
      version: "1", runtime: "python", entrypoint: "main.py",
      inputs: [
        { path: "data", source: { type: "inline", data: "one" } },
        { path: "data/nested", source: { type: "inline", data: "two" } },
      ], network: "network",
    }],
  ];
  for (const [name, workload] of cases) {
    const root = await fixture("python", "main.py", "print('must not run')\n");
    const request = await writeWorkload(root, workload);
    const application = createComputeApplication({ computeBinary, cwd: root });
    const result = output<any>(await application.handleRequest(envelope("compute.inspect", { request })));
    assert.equal(result.kind, "failure", name);
    assert.equal(result.failure.kind, "invalid_workload", name);
  }

  const root = await fixture("python", "placeholder.py", "print('must not run')\n");
  const outside = await fixture("python", "real.py", "print('escaped')\n");
  await symlink(join(outside, "real.py"), join(root, "main.py"));
  const request = await writeWorkload(root, {
    version: "1", runtime: "python", entrypoint: "main.py", network: "network",
  });
  const application = createComputeApplication({
    computeBinary, cwd: root, authorizer: permissionAuthorizer(),
  });
  const result = output<any>(await application.handleRequest(
    envelope("compute.run", { request }), { session: session(["compute.run"]) },
  ));
  assert.equal(result.kind, "failure");
  assert.equal(result.failure.kind, "invalid_workload");
});

test("each AppPort run receives a fresh isolated workspace", async () => {
  const root = await fixture("python", "main.py", [
    "import os",
    "work=os.environ['COMPUTE_WORK_DIR']",
    "out=os.environ['COMPUTE_OUTPUT_DIR']",
    "marker=os.path.join(work,'runtime-marker')",
    "if os.path.exists(marker): raise RuntimeError('workspace leaked')",
    "open(marker,'w').write('created')",
    "open(os.path.join(out,'result.txt'),'w').write('fresh')",
  ].join("\n"));
  const request = await writeWorkload(root, {
    version: "1", runtime: "python", entrypoint: "main.py",
    outputs: [{ path: "result.txt", required: true }], network: "network",
  });
  const application = createComputeApplication({
    computeBinary, cwd: root, authorizer: permissionAuthorizer(),
  });
  for (let attempt = 0; attempt < 2; attempt += 1) {
    const result = output<any>(await application.handleRequest(
      envelope("compute.run", { request }), { session: session(["compute.run"]) },
    ));
    assert.equal(result.kind, "execution");
    assert.equal(result.result.outputs[0].data, "fresh");
  }
});

test("bundles complete the AppPort lifecycle across WASM and installed process runtimes", async (t) => {
  const runtimes: Array<[WorkloadSpec["runtime"], string, string | Uint8Array, "none" | "network"]> = [
    ["wasm", "module.wasm", new Uint8Array([
      0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
      0x01, 0x04, 0x01, 0x60, 0x00, 0x00,
      0x03, 0x02, 0x01, 0x00,
      0x07, 0x0a, 0x01, 0x06, 0x5f, 0x73, 0x74, 0x61, 0x72, 0x74, 0x00, 0x00,
      0x0a, 0x04, 0x01, 0x02, 0x00, 0x0b,
    ]), "none"],
    ["node", "main.js", "console.log('node')\n", "network"],
    ["python", "main.py", "print('python')\n", "network"],
    ["bun", "main.js", "console.log('bun')\n", "network"],
  ];
  for (const [runtime, entrypoint, source, network] of runtimes) {
    await t.test(runtime, async (runtimeTest) => {
      if (runtime !== "wasm") {
        const probe = spawnSync(runtime, ["--version"]);
        if (probe.error) return runtimeTest.skip(`${runtime} is not installed`);
      }
      const root = await fixture(runtime, entrypoint, source);
      const request = await bundleRequest(root, {
        version: "1",
        runtime,
        entrypoint,
        outputs: [{ path: "optional.txt", required: false }],
        network,
      });
      const application = createComputeApplication({
        computeBinary, cwd: root, authorizer: permissionAuthorizer(),
      });
      for (const mode of ["inspect", "dry_run"] as const) {
        const inspected = output<any>(await application.handleRequest(
          envelope("compute.inspect", { request, mode }),
        ));
        assert.equal(inspected.kind, "inspection");
        assert.equal(inspected.plan.bundle_verification.valid, true);
        assert.match(inspected.plan.bundle_verification.bundle_id, /^sha256:[0-9a-f]{64}$/);
      }
      const result = output<any>(await application.handleRequest(
        envelope("compute.run", { request }),
        { session: session(["compute.run"]) },
      ));
      assert.equal(result.kind, "execution");
      assert.equal(result.result.runtime, runtime);
      assert.match(result.bundle_id, /^sha256:[0-9a-f]{64}$/);
      assert.deepEqual(result.result.missing_outputs, [{ path: "optional.txt", required: false }]);
    });
  }
});
