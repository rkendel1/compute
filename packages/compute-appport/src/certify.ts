import { readFile } from "node:fs/promises";
import { permissionAuthorizer } from "@appport/authorization";
import { createRequest, type AppResponse, type Session } from "@appport/protocol";
import { createComputeApplication } from "./index.js";
import type { BundleExecutionRequest } from "./types.js";

function option(name: string): string {
  const index = process.argv.indexOf(name);
  if (index < 0 || !process.argv[index + 1]) throw new Error(`missing ${name}`);
  return process.argv[index + 1]!;
}

function envelope(input: BundleExecutionRequest) {
  return createRequest({
    requestId: "compute-distribution-certification",
    capability: { name: "compute.run", version: 1 },
    input: { request: input },
  });
}

const computeBinary = option("--compute");
const bundlePath = option("--bundle");
const workloadId = option("--workload-id");
const bundleId = option("--bundle-id");
const request: BundleExecutionRequest = {
  bundle: {
    data: [...await readFile(bundlePath)],
    expected_workload_id: workloadId,
    expected_bundle_id: bundleId,
  },
};
const application = createComputeApplication({
  computeBinary,
  authorizer: permissionAuthorizer(),
  mode: "production",
});

const unauthorizedResponse: AppResponse = await application.handleRequest(envelope(request));
const session: Session = {
  id: "compute-certification",
  applicationId: "compute-distribution-certification",
  createdAt: new Date(0).toISOString(),
  permissions: ["compute.run"],
};
const authorizedResponse: AppResponse = await application.handleRequest(
  envelope(request),
  { session },
);
const unauthorized = !unauthorizedResponse.ok && unauthorizedResponse.error.code === "UNAUTHORIZED";
const output = authorizedResponse.ok ? authorizedResponse.output as { kind?: string } : undefined;
const authorized = authorizedResponse.ok && output?.kind === "execution";
console.log(JSON.stringify({ authorized, unauthorized }));
if (!authorized || !unauthorized) process.exit(1);
