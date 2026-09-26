import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chmod, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { request as httpRequest } from "node:http";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import test from "node:test";
import { permissionAuthorizer } from "@appport/authorization";
import { createRequest, type AppResponse, type Session } from "@appport/protocol";
import { createComputeApplication } from "../index.js";

const computeBinary = resolve(process.cwd(), "../../target/debug/compute");
const capabilities = [
  "compute.application.deploy", "compute.application.status", "compute.application.logs",
  "compute.application.history", "compute.application.rollback", "compute.application.stop",
];

function session(permissions: string[]): Session {
  return {
    id: "application-session",
    applicationId: "compute-appport-application-tests",
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
  assert.equal(
    response.ok, true,
    response.ok ? undefined : `${response.error.message} ${JSON.stringify(response.error.details ?? {})}`,
  );
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

function fetchText(url: string): Promise<string | undefined> {
  return new Promise((resolveText) => {
    const request = httpRequest(url, { timeout: 2000 }, (response) => {
      let body = "";
      response.setEncoding("utf8").on("data", (chunk: string) => { body += chunk; });
      response.on("end", () => resolveText(response.statusCode === 200 ? body.trim() : undefined));
    });
    request.on("error", () => resolveText(undefined));
    request.on("timeout", () => { request.destroy(); resolveText(undefined); });
    request.end();
  });
}

/**
 * A runtime catalog whose Python artifact is a script that reports the
 * pinned version and runs the host's `python3`: the provider acquires,
 * verifies, and prepares it without a network (as the Rust tests' fixture
 * catalog does).
 */
async function fixtureCatalog(directory: string): Promise<string | undefined> {
  const python = spawnSync("sh", ["-c", "command -v python3"], { encoding: "utf8" }).stdout.trim();
  if (!python) return undefined;
  const pinned = JSON.parse(await readFile(resolve(process.cwd(), "../../distribution/runtime-lock.json"), "utf8"));
  const locked = pinned.runtimes.python;
  const script = `#!/bin/sh\ncase "$1" in --version|--help) echo "Python ${locked.version}"; exit 0;; esac\nexec ${python} "$@"\n`;
  await mkdir(directory, { recursive: true });
  const artifact = join(directory, "python-fixture");
  await writeFile(artifact, script);
  await chmod(artifact, 0o755);
  const architecture = process.arch === "arm64" ? "aarch64" : "x86_64";
  const catalog = join(directory, "runtime-catalog.json");
  await writeFile(catalog, JSON.stringify({
    schema_version: 2,
    runtimes: {
      wasm: pinned.runtimes.wasm,
      native: pinned.runtimes.native,
      python: {
        version: locked.version,
        executable: locked.executable,
        artifacts: {
          [`linux-${architecture}`]: {
            url: `file://${artifact}`,
            sha256: createHash("sha256").update(script).digest("hex"),
            format: "file",
            install: [{ source: "artifact", destination: locked.executable }],
          },
        },
      },
    },
  }));
  return catalog;
}

test("application capabilities are versioned, authorized, and carry no transport", () => {
  const manifest = createComputeApplication({ computeBinary }).manifest();
  const find = (name: string) => manifest.capabilities.find((item) => item.name === name)!;
  for (const name of capabilities) {
    assert.ok(find(name), name);
    assert.equal(find(name).definitions[0]?.version, 1, name);
    assert.notDeepEqual(find(name).authorization, [], name);
  }
  assert.equal(find("compute.application.deploy").definitions[0]?.effect, "consequential");
  assert.equal(find("compute.application.status").definitions[0]?.effect, "observation");
  assert.deepEqual(find("compute.application.status").authorization, ["compute.application.read"]);
  assert.deepEqual(find("compute.application.rollback").authorization, ["compute.application.deploy"]);
  // The contract speaks of applications, providers, and endpoints; never of
  // daemons, tokens, or Compute's own API.
  const contract = JSON.stringify(capabilities.map((name) => find(name).definitions));
  for (const word of ["daemon", "token", "/applications", "bearer", "compute.remote"]) {
    assert.ok(!contract.toLowerCase().includes(word), word);
  }
});

test("an agent deploys, observes, rolls back, and stops an application through AppPort", {
  skip: process.platform !== "linux" ? "the fixture runtime catalog is Linux-only" : false,
}, async (context) => {
  const root = await mkdtemp(join(tmpdir(), "compute-appport-application-"));
  const catalog = await fixtureCatalog(join(root, "catalog"));
  if (!catalog) {
    context.skip("python3 is not installed");
    return;
  }
  const [listen, endpoints, instances] = [await freePort(), 21000 + (process.pid % 2000) * 8, 38000 + (process.pid % 2000) * 8];
  const node = `http://127.0.0.1:${listen}`;
  const token = "appport-provider-operator";
  const started = spawnSync(computeBinary, [
    "start", "--detach", "--listen", `127.0.0.1:${listen}`, "--state-dir", join(root, "node"),
    "--port-range", `${endpoints}-${endpoints + 3}`,
    "--instance-port-range", `${instances}-${instances + 7}`,
    "--reconcile-interval-ms", "500", "--require-token-env", "NODE_TOKEN",
  ], {
    encoding: "utf8",
    env: {
      ...process.env,
      NODE_TOKEN: token,
      COMPUTE_RUNTIME_CATALOG: catalog,
      COMPUTE_RUNTIME_STORE: join(root, "store"),
    },
  });
  assert.equal(started.status, 0, started.stderr);
  context.after(async () => {
    spawnSync(computeBinary, ["stop", "--daemon", node], { env: { ...process.env, COMPUTE_DAEMON_TOKEN: token } });
    await rm(root, { recursive: true, force: true });
  });
  const poolConfig = join(root, "compute-pool.toml");
  await writeFile(poolConfig, `[providers.linux-worker]\nkind = "remote"\nendpoint = "${node}"\ntoken_env = "WORKER_TOKEN"\n`);

  const app = createComputeApplication({
    computeBinary,
    cwd: root,
    authorizer: permissionAuthorizer(),
    poolConfig,
    capabilityCache: join(root, "capabilities.json"),
    applicationEnvironment: { WORKER_TOKEN: token },
  });
  const agent = session([
    "compute.application.deploy", "compute.application.read", "compute.application.stop",
  ]);

  // An application artifact, packed by Compute.
  const source = join(root, "hello-api");
  const init = spawnSync(computeBinary, ["init", source, "--runtime", "python", "--json"], { encoding: "utf8" });
  assert.equal(init.status, 0, init.stderr);
  const packed = spawnSync(computeBinary, [
    "application", "pack", source, "--output", join(root, "hello.capp"), "--json",
  ], { encoding: "utf8" });
  assert.equal(packed.status, 0, packed.stderr);
  const artifact = [...await readFile(join(root, "hello.capp"))];

  // Without authority, nothing is deployed.
  const denied = await app.handleRequest(envelope("compute.application.deploy", { artifact: { data: artifact } }));
  assert.equal(denied.ok, false);

  const v1 = output<any>(await app.handleRequest(
    envelope("compute.application.deploy", { artifact: { data: artifact } }), { session: agent },
  ));
  assert.equal(v1.application, "hello-api");
  assert.match(v1.application_id, /^sha256:/);
  assert.equal(v1.version, 1);
  assert.equal(v1.provider, "linux-worker");
  assert.equal(v1.runtime, "python");
  assert.equal(v1.status, "running");
  assert.equal(v1.active, true);
  assert.match(v1.artifact.artifact_id, /^sha256:/);
  assert.equal(v1.placement.provider_id, "linux-worker");
  let body: string | undefined;
  for (let attempt = 0; attempt < 100 && body !== "Hello from Compute"; attempt += 1) {
    body = await fetchText(v1.endpoint);
    if (body !== "Hello from Compute") await new Promise((wait) => setTimeout(wait, 100));
  }
  assert.equal(body, "Hello from Compute");

  const status = output<any>(await app.handleRequest(
    envelope("compute.application.status", { application: "hello-api" }), { session: agent },
  ));
  assert.equal(status.application_id, v1.application_id);
  assert.equal(status.status, "running");
  assert.equal(status.endpoint, v1.endpoint);
  assert.equal(status.version, 1);

  // v2 from the directory with new code, then back to v1's code as v3.
  const main = join(source, "main.py");
  await writeFile(main, (await readFile(main, "utf8")).replace("Hello from Compute", "Hello v2"));
  const v2 = output<any>(await app.handleRequest(
    envelope("compute.application.deploy", { application: source }), { session: agent },
  ));
  assert.equal(v2.version, 2);
  assert.equal(v2.endpoint, v1.endpoint);
  const v3 = output<any>(await app.handleRequest(
    envelope("compute.application.rollback", { application: "hello-api", version: "v1" }), { session: agent },
  ));
  assert.equal(v3.version, 3);
  assert.equal(v3.rollback_of, 1);
  assert.equal(v3.artifact.artifact_id, v1.artifact.artifact_id);
  assert.equal(await fetchText(v1.endpoint), "Hello from Compute");

  const history = output<any[]>(await app.handleRequest(
    envelope("compute.application.history", { application: "hello-api" }), { session: agent },
  ));
  assert.deepEqual(history.map((item) => item.version), [3, 2, 1]);
  assert.equal(history[0].state, "active");

  const logs = output<any>(await app.handleRequest(
    envelope("compute.application.logs", { application: "hello-api" }), { session: agent },
  ));
  assert.equal(typeof logs.stdout, "string");

  const stopped = output<any>(await app.handleRequest(
    envelope("compute.application.stop", { application: "hello-api" }), { session: agent },
  ));
  assert.equal(stopped.status, "stopped");

  const missing = await app.handleRequest(
    envelope("compute.application.status", { application: "not-deployed" }), { session: agent },
  );
  assert.equal(missing.ok, false);
  assert.equal(missing.ok ? "" : missing.error.code, "NOT_FOUND");
});
