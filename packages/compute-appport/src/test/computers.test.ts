import assert from "node:assert/strict";
import { createServer, type IncomingMessage } from "node:http";
import type { AddressInfo } from "node:net";
import test from "node:test";
import {
  ComputeDaemonClient,
  createComputeEnvironment,
  destroyComputeEnvironment,
  executeComputeEnvironment,
  getComputeEnvironment,
  listComputeEnvironments,
  listComputeTargets,
  updateComputeEnvironment,
  submitComputeEnvironment,
  releaseComputeEnvironment,
  runComputeProjectCommand,
  openWorkSession,
  closeWorkSession,
  listWorkSessions,
  setComputeEnvironmentLifetime,
  workModeUrl,
  proposeProject,
  publishVersion,
  deployVersion,
  promotionPlan,
  promoteVersion,
  rollbackVersion,
  getRollout,
  listSoftware,
  restartProcess,
  waitForOperation,
} from "../index.js";

interface Seen {
  method: string;
  path: string;
  authorization?: string;
  body?: unknown;
}

const view = {
  environment: "my-app",
  environment_id: "env_1",
  owner: "alice",
  lifecycle: "persistent",
  status: "running",
  requirements: { cpu_count: 4 },
  spec_generation: 1,
  running_generation: 1,
  target: "railway-1",
  session_id: "ses_1",
  desired: { repositories: [{ name: "app", url: "https://example.invalid/app.git", revision: "main" }], generation: 3 },
  observed: { converged_generation: 3 },
  converged: true,
  generation: 9,
  created_at: "2026-09-27T00:00:00Z",
};

async function body(request: IncomingMessage): Promise<unknown> {
  let text = "";
  for await (const chunk of request) text += chunk;
  return text ? JSON.parse(text) : undefined;
}

/** A stand-in for the Compute daemon: it records requests and answers like the API. */
async function daemon(): Promise<{ endpoint: string; seen: Seen[]; close: () => void }> {
  const seen: Seen[] = [];
  let polls = 0;
  const server = createServer(async (request, response) => {
    const entry: Seen = { method: request.method ?? "", path: request.url ?? "", body: await body(request) };
    if (request.headers.authorization) entry.authorization = request.headers.authorization;
    seen.push(entry);
    const reply = (status: number, value: unknown) => {
      response.writeHead(status, { "content-type": "application/json" });
      response.end(JSON.stringify(value));
    };
    const route = `${request.method} ${request.url}`;
    if (route === "POST /environments") return reply(201, { name: "my-app", computer: view });
    if (route === "GET /environments") return reply(200, [{ name: "plain" }, { name: "my-app", computer: "running" }]);
    if (route === "GET /environments/my-app/computer") return reply(200, view);
    if (route === "POST /environments/my-app/contents") return reply(200, { ...view, desired: (entry.body as { contents: unknown }).contents });
    if (route === "POST /environments/my-app/exec") {
      return reply(201, { environment: "my-app", target: "railway-1", session_id: "ses_1", job_id: "job_1", execution_id: "exec_1", status: "queued" });
    }
    if (route === "GET /environments/my-app/jobs/job_1") {
      polls += 1;
      if (polls < 2) return reply(200, { job: { status: "running" } });
      return reply(200, { job: { status: "failed" }, result: { result: { exit_code: 3, status: "completed", stdout: { text: "out" }, stderr: { text: "err" } } } });
    }
    if (route === "POST /environments/my-app/release") return reply(200, { ...view, desired: { ...view.desired, generation: 4 } });
    if (route === "POST /environments/my-app/run") {
      return reply(201, { environment: "my-app", target: "railway-1", session_id: "ses_1", job_id: "job_2", execution_id: "exec_2", status: "queued" });
    }
    if (route === "GET /environments/my-app/jobs/job_2") {
      return reply(200, { job: { status: "succeeded" }, result: { result: { exit_code: 0, status: "completed", stdout: { text: "built" }, stderr: { text: "" } } } });
    }
    if (route === "POST /environments/my-app/lifecycle") return reply(200, { ...view, requested_lifecycle: "ephemeral" });
    if (route === "POST /sessions") return reply(201, { session_id: "wks_1", environment: "my-app", environment_id: "env_1", owner: "alice", kind: "attached", status: "open", opened_at: "2026-09-27T00:00:00Z" });
    if (route === "GET /sessions?environment=my-app") return reply(200, [{ session_id: "wks_1", kind: "attached", status: "open" }]);
    if (route === "DELETE /sessions/wks_1") return reply(200, { session_id: "wks_1", kind: "attached", status: "closed" });
    if (route === "POST /environments/my-app/propose") return reply(200, { name: "web", runtime: "node", assembly: { processes: [{ name: "web", command: ["npm", "start"], port: 3000 }] } });
    if (route === "POST /environments/my-app/processes/web/restart") return reply(200, view);
    if (route === "GET /software") return reply(200, [{ project: "web", latest_version: "1.0.0", environments: [] }]);
    if (route === "POST /software/web/versions") return reply(201, { project: "web", version: "1.0.0", status: "publishing", steps: [] });
    if (route === "POST /software/web/deploy") return reply(201, { rollout_id: "rol_1", status: "applying", steps: [] });
    if (route === "GET /software/web/promotion?from=test&to=production") return reply(200, { version: "1.0.0", expected_generation: 7, changes: [] });
    if (route === "POST /software/web/promote") return reply(201, { rollout_id: "rol_2", kind: "promote", status: "applying", steps: [] });
    if (route === "POST /software/web/rollback") return reply(201, { rollout_id: "rol_3", kind: "rollback", status: "applying", steps: [] });
    if (route === "GET /rollouts/rol_2") {
      polls += 1;
      return reply(200, { rollout_id: "rol_2", status: polls > 1 ? "active" : "applying", steps: [{ name: "Checkout", status: polls > 1 ? "succeeded" : "running" }] });
    }
    if (route === "DELETE /environments/my-app") return reply(200, { ...view, status: "destroying" });
    if (route === "GET /targets") return reply(200, [{ target_id: "railway-1", kind: "remote", health: "healthy", hosts_computers: true, features: ["containers"] }]);
    if (route === "GET /environments/other/computer") return reply(403, { kind: "authorization_denied", message: "environment other belongs to another principal" });
    return reply(404, { kind: "not_found", message: route });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address() as AddressInfo;
  return { endpoint: `http://127.0.0.1:${port}`, seen, close: () => server.close() };
}

test("environment operations are thin requests to the Compute API", async () => {
  const stub = await daemon();
  try {
    const client = new ComputeDaemonClient({ endpoint: stub.endpoint, token: "secret" });
    await createComputeEnvironment(client, { name: "my-app", computer: { requirements: { cpu_count: 4 } } });
    assert.deepEqual(stub.seen[0]?.body, { name: "my-app", computer: { lifecycle: "persistent", requirements: { cpu_count: 4 } } });
    assert.equal(stub.seen[0]?.authorization, "Bearer secret");

    // Only environments with a computer are listed.
    const listed = await listComputeEnvironments(client);
    assert.deepEqual(listed.map((environment) => environment.environment), ["my-app"]);

    // An update edits the current desired contents and is fenced on the
    // generation it was based on.
    const updated = await updateComputeEnvironment(client, "my-app", (contents) => {
      contents.repositories![0]!.revision = "v2";
      contents.processes = [{ name: "api", command: ["npm", "start"], repository: "app" }];
    });
    const sent = stub.seen.find((request) => request.path === "/environments/my-app/contents")!.body as {
      contents: { repositories: { revision: string }[] };
      expected_generation: number;
    };
    assert.equal(sent.expected_generation, 3);
    assert.equal(sent.contents.repositories[0]!.revision, "v2");
    assert.equal(updated.desired.processes?.[0]?.name, "api");

    // A command is a durable job; its failure is a result, not an exception.
    const ran = await executeComputeEnvironment(client, "my-app", ["npm", "test"], { env: { CI: "1" }, pollMs: 1 });
    assert.equal(ran.job_id, "job_1");
    assert.equal(ran.execution_id, "exec_1");
    assert.equal(ran.exit_code, 3);
    assert.equal(ran.stdout, "out");
    assert.deepEqual(stub.seen.find((request) => request.path === "/environments/my-app/exec")?.body, { command: ["npm", "test"], env: { CI: "1" } });

    const destroyed = await destroyComputeEnvironment(client, "my-app");
    assert.equal(destroyed.status, "destroying");
    const targets = await listComputeTargets(client);
    assert.equal(targets[0]?.hosts_computers, true);
    // Refusals keep their meaning.
    await assert.rejects(getComputeEnvironment(client, "other"), (error: Error & { kind?: string; status?: number }) =>
      error.kind === "authorization_denied" && error.status === 403);
  } finally {
    stub.close();
  }
});

test("GO, releases, project commands, lifetimes, and work sessions are API requests too", async () => {
  const stub = await daemon();
  try {
    const client = new ComputeDaemonClient({ endpoint: stub.endpoint, token: "secret" });
    await submitComputeEnvironment(client, "my-app", { repositories: [] }, 3, {
      config: { MODE: "x" },
      lifecycle: { lifecycle: "ephemeral", ttl_seconds: 600 },
    });
    assert.deepEqual(stub.seen.at(-1)?.body, {
      contents: { repositories: [] },
      expected_generation: 3,
      config: { MODE: "x" },
      lifecycle: { lifecycle: "ephemeral", ttl_seconds: 600 },
    });
    const released = await releaseComputeEnvironment(client, "my-app", "app", "v2", 3);
    assert.equal(released.desired.generation, 4);
    assert.deepEqual(stub.seen.at(-1)?.body, { project: "app", revision: "v2", expected_generation: 3 });
    const built = await runComputeProjectCommand(client, "my-app", "app", "build", { pollMs: 1 });
    assert.equal(built.stdout, "built");
    assert.equal(built.exit_code, 0);
    await setComputeEnvironmentLifetime(client, "my-app", { lifecycle: "ephemeral", ttl_seconds: 60 });
    const session = await openWorkSession(client, { environment: "my-app" });
    assert.equal(session.kind, "attached");
    assert.deepEqual(stub.seen.at(-1)?.body, { environment: "my-app" });
    assert.equal((await listWorkSessions(client, "my-app")).length, 1);
    assert.equal((await closeWorkSession(client, "wks_1")).status, "closed");
    assert.equal(workModeUrl("http://127.0.0.1:8787/", "my app"), "http://127.0.0.1:8787/#/work/my%20app");
  } finally {
    stub.close();
  }
});

test("the software lifecycle is the same API the UI uses", async () => {
  const stub = await daemon();
  try {
    const client = new ComputeDaemonClient({ endpoint: stub.endpoint, token: "secret" });
    const proposal = await proposeProject(client, "my-app", "https://example.invalid/web.git", { revision: "main" });
    assert.equal(proposal.assembly.processes?.[0]?.port, 3000);
    assert.deepEqual(stub.seen.at(-1)?.body, { url: "https://example.invalid/web.git", revision: "main" });
    await restartProcess(client, "my-app", "web");
    assert.equal((await listSoftware(client))[0]?.latest_version, "1.0.0");
    assert.equal((await publishVersion(client, "web", "dev")).status, "publishing");
    assert.deepEqual(stub.seen.at(-1)?.body, { environment: "dev" });
    await deployVersion(client, "web", "test", "1.0.0", 4);
    assert.deepEqual(stub.seen.at(-1)?.body, { environment: "test", version: "1.0.0", expected_generation: 4 });
    const plan = await promotionPlan(client, "web", "test", "production");
    const promoted = await promoteVersion(client, "web", "test", "production", plan.expected_generation);
    assert.deepEqual(stub.seen.at(-1)?.body, { from: "test", to: "production", expected_generation: 7 });
    const seen: string[] = [];
    const done = await waitForOperation(() => getRollout(client, promoted.rollout_id), { pollMs: 1, onUpdate: (rollout) => seen.push(rollout.steps[0]!.status) });
    assert.equal(done.status, "active");
    assert.deepEqual(seen, ["running", "succeeded"]);
    assert.equal((await rollbackVersion(client, "web", "production")).kind, "rollback");
  } finally {
    stub.close();
  }
});
