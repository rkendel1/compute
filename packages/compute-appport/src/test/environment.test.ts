import assert from "node:assert/strict";
import { spawn, spawnSync, type ChildProcess } from "node:child_process";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import test from "node:test";
import { permissionAuthorizer } from "@appport/authorization";
import { createRequest, type AppResponse, type Session } from "@appport/protocol";
import { ComputeDaemonClient, createComputeApplication } from "../index.js";

const computeBinary = resolve(process.cwd(), "../../target/debug/compute");

const wasm = new Uint8Array([
  0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
  0x01, 0x04, 0x01, 0x60, 0x00, 0x00,
  0x03, 0x02, 0x01, 0x00,
  0x07, 0x0a, 0x01, 0x06, 0x5f, 0x73, 0x74, 0x61, 0x72, 0x74, 0x00, 0x00,
  0x0a, 0x04, 0x01, 0x02, 0x00, 0x0b,
]);

const capabilities = [
  "compute.environment.list", "compute.environment.inspect", "compute.environment.status",
  "compute.environment.create", "compute.environment.start", "compute.environment.stop",
  "compute.environment.restart", "compute.environment.project.add", "compute.environment.project.remove",
  "compute.project.list", "compute.project.inspect", "compute.project.start", "compute.project.stop",
  "compute.project.restart", "compute.deployment.inspect", "compute.deployment.create",
  "compute.deployment.promote", "compute.deployment.rollback",
  "compute.domain.list", "compute.domain.inspect", "compute.domain.create", "compute.domain.remove",
  "compute.dns.inspect", "compute.dns.reconcile", "compute.certificate.inspect", "compute.certificate.renew",
];

function session(permissions: string[]): Session {
  return {
    id: "environment-session",
    applicationId: "compute-appport-environment-tests",
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

function freePort(): Promise<number> {
  return new Promise((resolvePort, reject) => {
    const server = createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      server.close(() => resolvePort(typeof address === "object" && address ? address.port : 0));
    });
  });
}

async function waitFor(client: ComputeDaemonClient): Promise<void> {
  for (let attempt = 0; attempt < 100; attempt += 1) {
    try {
      await client.request("GET", "/status");
      return;
    } catch {
      await new Promise((resolveDelay) => setTimeout(resolveDelay, 100));
    }
  }
  throw new Error("the Compute daemon did not start");
}

async function wasmBundle(root: string): Promise<number[]> {
  await writeFile(join(root, "module.wasm"), wasm);
  await writeFile(join(root, "workload.json"), JSON.stringify({
    version: "1", runtime: "wasm", entrypoint: "module.wasm", network: "none",
  }));
  const bundle = join(root, "workload.compute");
  const created = spawnSync(computeBinary, [
    "bundle", "create", "--workload", join(root, "workload.json"), "--output", bundle, "--json",
  ], { encoding: "utf8" });
  assert.equal(created.status, 0, created.stderr);
  return [...await readFile(bundle)];
}

test("environment capabilities are versioned; every one requires authorization", () => {
  const manifest = createComputeApplication({ computeBinary }).manifest();
  for (const name of capabilities) {
    const capability = manifest.capabilities.find((item) => item.name === name);
    assert.ok(capability, name);
    assert.equal(capability.definitions[0]?.version, 1, name);
    assert.notDeepEqual(capability.authorization, [], name);
  }
  const effect = (name: string) =>
    manifest.capabilities.find((item) => item.name === name)!.definitions[0]?.effect;
  assert.equal(effect("compute.environment.inspect"), "observation");
  assert.equal(effect("compute.environment.create"), "consequential");
  assert.equal(effect("compute.environment.project.add"), "consequential");
  assert.equal(effect("compute.domain.inspect"), "observation");
  assert.equal(effect("compute.deployment.rollback"), "consequential");
  assert.equal(effect("compute.certificate.renew"), "consequential");
});

test("environment capabilities drive the daemon through the Compute API", async (context) => {
  const root = await mkdtemp(join(tmpdir(), "compute-appport-environment-"));
  const port = await freePort();
  const daemon: ChildProcess = spawn(computeBinary, [
    "start", "--listen", `127.0.0.1:${port}`, "--state-dir", join(root, "state"),
    "--require-token-env", "COMPUTE_DAEMON_TOKEN",
  ], { env: { ...process.env, COMPUTE_DAEMON_TOKEN: "secret" }, stdio: "ignore" });
  context.after(async () => {
    daemon.kill("SIGINT");
    await new Promise((resolveExit) => daemon.once("exit", resolveExit));
    await rm(root, { recursive: true, force: true });
  });
  const endpoint = `http://127.0.0.1:${port}`;
  await waitFor(new ComputeDaemonClient({ endpoint }));
  const app = createComputeApplication({
    computeBinary, daemon: endpoint, daemonToken: "secret", authorizer: permissionAuthorizer(),
  });
  const all = session([
    "compute.environment.read", "compute.project.read", "compute.deployment.read",
    "compute.domain.read", "compute.dns.read", "compute.certificate.read", ...capabilities,
  ]);

  // AppPort authorization comes first: no scope, no change.
  const unauthorized = await app.handleRequest(
    envelope("compute.environment.create", { name: "staging" }), { session: session([]) },
  );
  assert.equal(unauthorized.ok, false);

  const created = output<any>(await app.handleRequest(
    envelope("compute.environment.create", { name: "staging", env: { LOG_LEVEL: "info" } }), { session: all },
  ));
  assert.equal(created.version, "compute.environment@1");
  assert.match(created.environment_id, /^env_/);

  const project = output<any>(await app.handleRequest(envelope("compute.environment.project.add", {
    environment: "staging",
    project: {
      name: "app",
      revision: "rev-1",
      workloads: [{ name: "migrate", kind: "task", bundle: await wasmBundle(root) }],
    },
  }), { session: all }));
  assert.match(project.project_id, /^prj_/);
  assert.equal(project.workloads[0].kind, "task");

  const listed = output<any[]>(await app.handleRequest(envelope("compute.environment.list", {}), { session: all }));
  assert.deepEqual(listed.map((item) => item.name), ["staging"]);
  const inspected = output<any>(await app.handleRequest(
    envelope("compute.environment.inspect", { environment: "staging" }), { session: all },
  ));
  const status = output<any>(await app.handleRequest(
    envelope("compute.environment.status", { environment: created.environment_id }), { session: all },
  ));
  assert.equal(inspected.environment_id, status.environment_id);
  assert.deepEqual(inspected.projects.map((item: any) => item.name), ["app"]);

  // Projects, deployments, and promotion through the same API.
  output(await app.handleRequest(envelope("compute.environment.create", { name: "production" }), { session: all }));
  const projects = output<any[]>(await app.handleRequest(envelope("compute.project.list", {}), { session: all }));
  assert.deepEqual(projects.map((item) => item.name), ["app"]);
  const inStaging = output<any>(await app.handleRequest(
    envelope("compute.project.inspect", { project: "app", environment: "staging" }), { session: all },
  ));
  assert.equal(inStaging.environment, "staging");
  assert.equal(inStaging.deployment.status, "complete");
  const redeployed = output<any>(await app.handleRequest(envelope("compute.deployment.create", {
    project: "app", environment: "staging", revision: "rev-1",
  }), { session: all }));
  assert.equal(redeployed.previous, inStaging.deployment.deployment_id);
  const deployment = output<any>(await app.handleRequest(
    envelope("compute.deployment.inspect", { deployment: redeployed.deployment_id }), { session: all },
  ));
  assert.ok(deployment.workloads.every((workload: any) => workload.admitted && workload.admission_id));
  for (let attempt = 0; attempt < 50; attempt += 1) {
    const current = output<any>(await app.handleRequest(
      envelope("compute.deployment.inspect", { deployment: redeployed.deployment_id }), { session: all },
    ));
    if (current.status === "complete") break;
    await new Promise((resolveDelay) => setTimeout(resolveDelay, 100));
  }
  const promoted = output<any>(await app.handleRequest(envelope("compute.deployment.promote", {
    project: "app", from: "staging", to: "production",
  }), { session: all }));
  assert.equal(promoted.environment, "production");
  assert.equal(promoted.revision_digest, deployment.revision_digest, "the exact revision");
  assert.equal(promoted.promoted_from, redeployed.deployment_id);
  const across = output<any>(await app.handleRequest(
    envelope("compute.project.inspect", { project: "app" }), { session: all },
  ));
  assert.deepEqual(across.environments.map((item: any) => item.environment), ["production", "staging"]);
  assert.equal(across.revisions.length, 1);
  const stoppedProject = output<any>(await app.handleRequest(
    envelope("compute.project.stop", { project: "app", environment: "production" }), { session: all },
  ));
  assert.equal(stoppedProject.desired_state, "stopped");
  const inStagingAfter = output<any>(await app.handleRequest(
    envelope("compute.project.inspect", { project: "app", environment: "staging" }), { session: all },
  ));
  assert.equal(inStagingAfter.desired_state, "running", "stopping production leaves staging");
  output(await app.handleRequest(
    envelope("compute.project.start", { project: "app", environment: "production" }), { session: all },
  ));
  const denied = await app.handleRequest(
    envelope("compute.deployment.promote", { project: "app", from: "staging", to: "production" }),
    { session: session(["compute.deployment.read"]) },
  );
  assert.equal(denied.ok, false, "promotion requires its scope");

  const stopped = output<any>(await app.handleRequest(
    envelope("compute.environment.stop", { environment: "staging" }), { session: all },
  ));
  assert.equal(stopped.desired_state, "stopped");
  const started = output<any>(await app.handleRequest(
    envelope("compute.environment.start", { environment: "staging" }), { session: all },
  ));
  assert.equal(started.desired_state, "running");
  output(await app.handleRequest(envelope("compute.environment.restart", { environment: "staging" }), { session: all }));

  // Releases roll back; a complete release by releasing what it replaced.
  const rolledBack = output<any>(await app.handleRequest(
    envelope("compute.deployment.rollback", { deployment: redeployed.deployment_id }), { session: all },
  ));
  assert.notEqual(rolledBack.deployment_id, redeployed.deployment_id);
  assert.equal(rolledBack.revision, "rev-1");
  const deniedRollback = await app.handleRequest(
    envelope("compute.deployment.rollback", { deployment: redeployed.deployment_id }),
    { session: session(["compute.deployment.read"]) },
  );
  assert.equal(deniedRollback.ok, false, "rollback requires its scope");

  // Network: nothing routed yet; a domain needs a service that listens.
  assert.deepEqual(output<any[]>(await app.handleRequest(envelope("compute.domain.list", {}), { session: all })), []);
  assert.deepEqual(output<any[]>(await app.handleRequest(envelope("compute.dns.inspect", {}), { session: all })), []);
  assert.deepEqual(output<any[]>(await app.handleRequest(envelope("compute.certificate.inspect", {}), { session: all })), []);
  const noService = await app.handleRequest(envelope("compute.domain.create", {
    name: "app.example.com", environment: "staging", project: "app",
  }), { session: all });
  assert.equal(noService.ok, false);
  assert.match(noService.ok ? "" : noService.error.message, /services with ports/);
  const missing = await app.handleRequest(envelope("compute.domain.inspect", { domain: "nowhere.example.com" }), { session: all });
  assert.equal(missing.ok, false);
  assert.equal(missing.ok ? "" : missing.error.code, "NOT_FOUND");
  const noScope = await app.handleRequest(envelope("compute.domain.list", {}), { session: session([]) });
  assert.equal(noScope.ok, false, "domains require their scope");

  for (let attempt = 0; attempt < 50; attempt += 1) {
    const current = output<any>(await app.handleRequest(
      envelope("compute.deployment.inspect", { deployment: rolledBack.deployment_id }), { session: all },
    ));
    if (current.status === "complete") break;
    await new Promise((resolveDelay) => setTimeout(resolveDelay, 100));
  }
  const removed = output<any>(await app.handleRequest(
    envelope("compute.environment.project.remove", { environment: "staging", project: "app" }), { session: all },
  ));
  assert.equal(removed.removed, "app");

  // The daemon's own authorization still applies behind AppPort.
  const withoutToken = createComputeApplication({
    computeBinary, daemon: endpoint, authorizer: permissionAuthorizer(),
  });
  const refused = await withoutToken.handleRequest(
    envelope("compute.environment.create", { name: "other" }), { session: all },
  );
  assert.equal(refused.ok, false);
  assert.match(refused.ok ? "" : refused.error.message, /bearer token/);
});
