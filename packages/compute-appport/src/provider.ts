import { spawn } from "node:child_process";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import type { Authorizer } from "@appport/authorization";
import { createApplication, defineCapability, type AppPortApplication } from "@appport/core";
import {
  inspectInputSchema,
  inspectResultSchema,
  runInputSchema,
  runResultSchema,
} from "./schemas.js";
import type {
  ExecutionFailureKind,
  ExecutionRequest,
  ExecutionResult,
  InspectResult,
  RunResult,
  WorkloadPlan,
  WorkloadSpec,
  BundleExecutionRequest,
  BundleWorkloadPlan,
  WorkloadExecutionRequest,
} from "./types.js";

export interface LocalComputeProviderOptions {
  computeBinary?: string;
  cwd?: string;
  authorizer?: Authorizer;
  mode?: "development" | "production";
}

interface CommandResult {
  status: number | null;
  stdout: string;
  stderr: string;
}

export class LocalComputeProvider {
  readonly computeBinary: string;
  readonly cwd: string | undefined;

  constructor(options: Pick<LocalComputeProviderOptions, "computeBinary" | "cwd"> = {}) {
    this.computeBinary = options.computeBinary ?? "compute";
    this.cwd = options.cwd;
  }

  async inspect(request: ExecutionRequest, mode: "inspect" | "dry_run" = "inspect"): Promise<InspectResult> {
    if ("bundle" in request) return this.inspectBundle(request, mode);
    const invalid = validateExecutionRequest(request);
    if (invalid) return failure("invalid_execution_request", invalid);

    const command = await this.invoke([
      mode === "dry_run" ? "run" : "inspect",
      "--workload",
      resolve(this.cwd ?? process.cwd(), request.invocation.workload_path),
      ...(mode === "dry_run" ? ["--dry-run"] : []),
      "--json",
    ]);
    const plan = parseJson<WorkloadPlan>(command.stdout);
    if (!plan || command.status !== 0) {
      return failure(classifyPreExecutionFailure(command.stderr), command.stderr.trim() || "compute inspection failed");
    }
    if (!sameWorkload(plan.workload, request.workload)) {
      return failure("invalid_execution_request", "invocation workload does not match the portable WorkloadSpec");
    }
    return { kind: "inspection", mode, plan };
  }

  async run(request: ExecutionRequest): Promise<RunResult> {
    if ("bundle" in request) return this.runBundle(request);
    const inspected = await this.inspect(request);
    if (inspected.kind === "failure") return inspected;
    const plan = inspected.plan as WorkloadPlan;

    const command = await this.invoke([
      "run",
      "--workload",
      resolve(this.cwd ?? process.cwd(), request.invocation.workload_path),
      "--expected-workload-id",
      plan.workload_id,
      "--json",
    ]);
    const result = parseJson<ExecutionResult>(command.stdout);
    if (!result) {
      return {
        kind: "failure",
        workload_id: plan.workload_id,
        failure: {
          kind: classifyPreExecutionFailure(command.stderr),
          message: command.stderr.trim() || "compute execution failed",
        },
      };
    }
    if (result.status === "completed" && (result.exit_code === null || result.exit_code === 0)) {
      return { kind: "execution", workload_id: plan.workload_id, result };
    }
    const kind = classifyExecutionFailure(result);
    return {
      kind: "failure",
      workload_id: plan.workload_id,
      failure: { kind, message: result.error?.message ?? `execution ended with status ${result.status}` },
      result,
    };
  }

  private async inspectBundle(
    request: BundleExecutionRequest,
    mode: "inspect" | "dry_run",
  ): Promise<InspectResult> {
    return this.withBundle(request, async (path) => {
      const command = await this.invoke([
        "run", "--bundle", path, "--dry-run", "--json",
        ...(request.bundle.expected_workload_id
          ? ["--expected-workload-id", request.bundle.expected_workload_id]
          : []),
        ...(request.bundle.expected_bundle_id
          ? ["--expected-bundle-id", request.bundle.expected_bundle_id]
          : []),
      ]);
      const plan = parseJson<BundleWorkloadPlan>(command.stdout);
      if (!plan || command.status !== 0) {
        return failure(classifyBundleFailure(command.stderr), command.stderr.trim() || "bundle inspection failed");
      }
      return { kind: "inspection", mode, plan };
    });
  }

  private async runBundle(request: BundleExecutionRequest): Promise<RunResult> {
    return this.withBundle(request, async (path) => {
      const inspectCommand = await this.invoke([
        "run", "--bundle", path, "--dry-run", "--json",
        ...(request.bundle.expected_workload_id
          ? ["--expected-workload-id", request.bundle.expected_workload_id]
          : []),
        ...(request.bundle.expected_bundle_id
          ? ["--expected-bundle-id", request.bundle.expected_bundle_id]
          : []),
      ]);
      const plan = parseJson<BundleWorkloadPlan>(inspectCommand.stdout);
      if (!plan || inspectCommand.status !== 0) {
        return {
          kind: "failure",
          failure: {
            kind: classifyBundleFailure(inspectCommand.stderr),
            message: inspectCommand.stderr.trim() || "bundle verification failed",
          },
        };
      }
      const command = await this.invoke([
        "run", "--bundle", path,
        "--expected-workload-id", plan.bundle_verification.workload_id,
        "--expected-bundle-id", plan.bundle_verification.bundle_id,
        "--json",
      ]);
      const result = parseJson<ExecutionResult>(command.stdout);
      const identity = {
        workload_id: plan.bundle_verification.workload_id,
        bundle_id: plan.bundle_verification.bundle_id,
      };
      if (!result) {
        return {
          kind: "failure",
          ...identity,
          failure: {
            kind: classifyBundleFailure(command.stderr),
            message: command.stderr.trim() || "bundle execution failed",
          },
        };
      }
      if (result.status === "completed" && (result.exit_code === null || result.exit_code === 0)) {
        return { kind: "execution", ...identity, result };
      }
      return {
        kind: "failure",
        ...identity,
        failure: {
          kind: classifyExecutionFailure(result),
          message: result.error?.message ?? `execution ended with status ${result.status}`,
        },
        result,
      };
    });
  }

  private async withBundle<T>(
    request: BundleExecutionRequest,
    operation: (path: string) => Promise<T>,
  ): Promise<T> {
    const directory = await mkdtemp(join(tmpdir(), "compute-appport-bundle-"));
    const path = join(directory, "workload.compute");
    try {
      await writeFile(path, Uint8Array.from(request.bundle.data));
      return await operation(path);
    } finally {
      await rm(directory, { recursive: true, force: true });
    }
  }

  private invoke(args: string[]): Promise<CommandResult> {
    return new Promise((resolvePromise, reject) => {
      const child = spawn(this.computeBinary, args, {
        cwd: this.cwd,
        env: minimalEnvironment(),
        stdio: ["ignore", "pipe", "pipe"],
      });
      let stdout = "";
      let stderr = "";
      child.stdout.setEncoding("utf8").on("data", (chunk: string) => { stdout += chunk; });
      child.stderr.setEncoding("utf8").on("data", (chunk: string) => { stderr += chunk; });
      child.once("error", reject);
      child.once("close", (status) => resolvePromise({ status, stdout, stderr }));
    });
  }
}

export function createComputeApplication(options: LocalComputeProviderOptions = {}): AppPortApplication {
  const provider = new LocalComputeProvider(options);
  const inspect = defineCapability({
    name: "compute.inspect",
    version: 1,
    description: "Validate and plan a portable workload without executing it.",
    input: inspectInputSchema,
    output: inspectResultSchema,
    effect: "observation",
    authorizationContract: { required: false, public: true },
    attributes: { "compute.contract": "1", "compute.executes": false },
    handler: ({ request, mode }) => provider.inspect(request as ExecutionRequest, mode ?? "inspect"),
  });
  const run = defineCapability({
    name: "compute.run",
    version: 1,
    description: "Execute a portable workload in an isolated Compute workspace.",
    input: runInputSchema,
    output: runResultSchema,
    effect: "consequential",
    authorization: ["compute.run"],
    authorizationContract: { required: true, scopes: ["compute.run"] },
    attributes: { "compute.contract": "1", "compute.executes": true },
    handler: ({ request }) => provider.run(request as ExecutionRequest),
  });
  return createApplication({
    application: {
      id: "dev.compute.provider.local",
      name: "Compute Local Provider",
      version: "0.1.0",
      description: "Local AppPort provider for portable Compute workloads",
    },
    capabilities: [inspect, run],
    ...(options.authorizer ? { authorizer: options.authorizer } : {}),
    mode: options.mode ?? "development",
    transports: [{ kind: "inprocess" }],
    extensions: ["dev.compute.workload.v1"],
  });
}

function validateExecutionRequest(request: WorkloadExecutionRequest): string | undefined {
  const env = request.environment;
  if (env && canonical(env) !== canonical(request.workload.env ?? {})) {
    return "execution environment must match the workload environment";
  }
  const requested = request.execution_capabilities;
  if (requested?.network && requested.network !== (request.workload.network ?? "none")) {
    return "requested network capability must match the workload requirement";
  }
  if (requested?.resources && canonical(requested.resources) !== canonical(request.workload.resources ?? {})) {
    return "requested resources must match the workload requirements";
  }
  return undefined;
}

function sameWorkload(left: WorkloadSpec, right: WorkloadSpec): boolean {
  return canonical(normalizeWorkload(left)) === canonical(normalizeWorkload(right));
}

function normalizeWorkload(workload: WorkloadSpec): unknown {
  return {
    version: String(workload.version),
    runtime: workload.runtime,
    ...(workload.runtime_version === undefined ? {} : { runtime_version: workload.runtime_version }),
    entrypoint: workload.entrypoint,
    args: workload.args ?? [],
    env: workload.env ?? {},
    inputs: [...(workload.inputs ?? [])].sort((a, b) => a.path.localeCompare(b.path)),
    outputs: [...(workload.outputs ?? [])]
      .map((output) => ({ path: output.path, required: output.required ?? false }))
      .sort((a, b) => a.path.localeCompare(b.path)),
    resources: Object.fromEntries(Object.entries(workload.resources ?? {}).filter(([, value]) => value != null)),
    network: workload.network ?? "none",
  };
}

function canonical(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  if (value && typeof value === "object") {
    return `{${Object.entries(value).sort(([a], [b]) => a.localeCompare(b)).map(([key, child]) => `${JSON.stringify(key)}:${canonical(child)}`).join(",")}}`;
  }
  return JSON.stringify(value);
}

function minimalEnvironment(): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = {};
  for (const key of ["PATH", "SystemRoot", "WINDIR", "PATHEXT"]) {
    if (process.env[key]) env[key] = process.env[key];
  }
  return env;
}

function parseJson<T>(text: string): T | undefined {
  try { return JSON.parse(text) as T; } catch { return undefined; }
}

function failure(kind: ExecutionFailureKind, message: string): InspectResult {
  return { kind: "failure", failure: { kind, message } };
}

function classifyPreExecutionFailure(message: string): ExecutionFailureKind {
  const normalized = message.toLowerCase();
  if (normalized.includes("unsupported capability")) return "capability_denied";
  if (
    normalized.includes("does not resolve") ||
    normalized.includes("no such file") ||
    normalized.includes("not a regular file")
  ) {
    return "input_materialization_failure";
  }
  return "invalid_workload";
}

function classifyBundleFailure(message: string): ExecutionFailureKind {
  const normalized = message.toLowerCase();
  if (normalized.includes("identity mismatch")) return "identity_mismatch";
  if (normalized.includes("bundle")) return "invalid_bundle";
  return classifyPreExecutionFailure(message);
}

function classifyExecutionFailure(result: ExecutionResult): ExecutionFailureKind {
  switch (result.error?.kind) {
    case "output_contract": return "output_contract";
    case "unsupported_capability": return "capability_denied";
    case "preparation": return "input_materialization_failure";
    default: return "runtime_failure";
  }
}
