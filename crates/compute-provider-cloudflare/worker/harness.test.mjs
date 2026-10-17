// Drives harness.mjs under Node with real WASI modules, the way a Worker
// would: a Request in, a Response out. Used by tests/harness.rs, which
// builds the modules and checks the output.
//
//   node harness.test.mjs <cases.json>
//
// Each case: { module: path, token, header?, method?, body? }. Prints one
// JSON line per case: { status, body }.

import { readFile } from "node:fs/promises";
import { handle } from "./harness.mjs";

const cases = JSON.parse(await readFile(process.argv[2], "utf8"));
const compiled = new Map();
for (const item of cases) {
  if (!compiled.has(item.module)) {
    compiled.set(item.module, await WebAssembly.compile(await readFile(item.module)));
  }
  const headers = { "content-type": "application/json" };
  if (item.header !== undefined) headers.authorization = item.header;
  const request = new Request("https://worker.example/", {
    method: item.method || "POST",
    headers,
    body: item.method === "GET" ? undefined : item.raw ?? JSON.stringify(item.body ?? {}),
  });
  const env = { COMPUTE_INVOKE_TOKEN: item.token, COMPUTE_WORKLOAD_DIGEST: "sha256:test" };
  const response = await handle(request, env, compiled.get(item.module));
  console.log(JSON.stringify({ status: response.status, body: await response.json() }));
}
