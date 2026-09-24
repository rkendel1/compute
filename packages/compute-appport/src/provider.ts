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
  providerCapabilitiesSchema,
  providerSelectorSchema,
  jobAccessInputSchema,
  jobSubmissionInputSchema,
  jobValueSchema,
  providerInspectResultSchema,
  providerListInputSchema,
  providerListResultSchema,
  placementInspectInputSchema,
  placementReportSchema,
  poolRunInputSchema,
  poolRunResultSchema,
  poolSubmitInputSchema,
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
  PlacementOptions,
  PlacementReport,
  PoolRunResult,
} from "./types.js";

export interface LocalComputeProviderOptions {
  computeBinary?: string;
  cwd?: string;
  authorizer?: Authorizer;
  mode?: "development" | "production";
  provider?: ComputeProvider;
  remoteProvider?: string;
  /** Caller-owned provider pool configuration (TOML) for pool capabilities. */
  poolConfig?: string;
  /** Capability cache used by pool discovery. */
  capabilityCache?: string;
}

/** Transport-neutral provider selected by the AppPort application boundary. */
export interface ComputeProvider {
  inspect(request: ExecutionRequest, mode?: "inspect" | "dry_run"): Promise<InspectResult>;
  run(request: ExecutionRequest): Promise<RunResult>;
  capabilities(): Promise<unknown>;
  submit(request: ExecutionRequest): Promise<unknown>;
  status(jobId: string): Promise<unknown>;
  cancel(jobId: string): Promise<unknown>;
  result(jobId: string): Promise<unknown>;
  receipt(jobId: string): Promise<unknown>;
  /** Pool capabilities. Providers without a pool reject these operations. */
  listProviders?(refresh?: boolean): Promise<unknown>;
  inspectProvider?(provider?: string): Promise<unknown>;
  providerCapabilities?(provider?: string): Promise<unknown>;
  inspectPlacement?(request: ExecutionRequest, options: PlacementOptions & { submit?: boolean | undefined }): Promise<PlacementReport>;
  poolRun?(request: ExecutionRequest, options: PlacementOptions): Promise<PoolRunResult>;
  poolSubmit?(request: ExecutionRequest, options: PlacementOptions & { idempotency_key?: string | undefined }): Promise<unknown>;
}

interface CommandResult {
  status: number | null;
  stdout: string;
  stderr: string;
}

export class LocalComputeProvider implements ComputeProvider {
  readonly computeBinary: string;
  readonly cwd: string | undefined;
  readonly remoteProvider: string | undefined;
  readonly poolConfig: string | undefined;
  readonly capabilityCache: string | undefined;

  constructor(
    options: Pick<
      LocalComputeProviderOptions,
      "computeBinary" | "cwd" | "remoteProvider" | "poolConfig" | "capabilityCache"
    > = {},
  ) {
    this.computeBinary = options.computeBinary ?? "compute";
    this.cwd = options.cwd;
    this.remoteProvider = options.remoteProvider;
    this.poolConfig = options.poolConfig;
    this.capabilityCache = options.capabilityCache;
  }

  private poolArgs(): string[] {
    return [
      ...(this.poolConfig ? ["--pool-config", resolve(this.cwd ?? process.cwd(), this.poolConfig)] : []),
      ...(this.capabilityCache
        ? ["--capability-cache", resolve(this.cwd ?? process.cwd(), this.capabilityCache)]
        : []),
    ];
  }

  async listProviders(refresh = false): Promise<unknown> {
    return this.invokeJson(
      ["provider", "list", ...(refresh ? ["--refresh"] : []), ...this.poolArgs(), "--json"],
      "provider listing failed",
    );
  }

  async inspectProvider(provider = "local"): Promise<unknown> {
    return this.invokeJson(
      ["provider", "inspect", provider, ...this.poolArgs(), "--json"],
      "provider inspection failed",
    );
  }

  async providerCapabilities(provider = "local"): Promise<unknown> {
    return this.invokeJson(
      ["provider", "capabilities", provider, ...this.poolArgs(), "--json"],
      "provider capability discovery failed",
    );
  }

  async inspectPlacement(
    request: ExecutionRequest,
    options: PlacementOptions & { submit?: boolean | undefined },
  ): Promise<PlacementReport> {
    return this.withRequestBundle(request, async (path) => {
      const command = await this.invoke([
        "placement", "inspect", "--bundle", path,
        ...placementArgs(options),
        ...(options.submit ? ["--submit"] : []),
        ...this.poolArgs(), "--json",
      ]);
      const report = parseJson<PlacementReport>(command.stdout);
      // Exit status 2 is a completed placement that selected no provider.
      if (!report || (command.status !== 0 && command.status !== 2)) {
        throw new Error(command.stderr.trim() || "placement inspection failed");
      }
      return report;
    });
  }

  async poolRun(request: ExecutionRequest, options: PlacementOptions): Promise<PoolRunResult> {
    return this.withRequestBundle(request, async (path) => {
      const command = await this.invoke([
        "pool", "run", "--bundle", path, ...placementArgs(options), ...this.poolArgs(), "--json",
      ]);
      const value = parseJson<Record<string, unknown>>(command.stdout);
      // Dispatch failures have no execution; results carry `error: null`.
      if (value && !("execution_id" in value) && value.error) {
        const error = value.error as { code: string; message: string };
        return { kind: "failure", failure: { kind: dispatchFailureKind(error.code), message: error.message } };
      }
      if (value && !("execution_id" in value) && "placement" in value) {
        const placement = value.placement as PlacementReport;
        return {
          kind: "failure",
          failure: {
            kind: "placement_failed",
            message: placement.failure
              ? `${placement.failure.code}: ${placement.failure.message}`
              : "placement selected no provider",
          },
          placement,
        };
      }
      if (!value) {
        return {
          kind: "failure",
          failure: { kind: classifyBundleFailure(command.stderr), message: command.stderr.trim() || "pool execution failed" },
        };
      }
      const { placement, ...rest } = value as unknown as ExecutionResult & { placement: PlacementReport };
      const result = rest as ExecutionResult;
      const receipt = result.receipt;
      const identity = {
        workload_id: receipt?.workload ?? "",
        ...(receipt?.bundle ? { bundle_id: receipt.bundle } : {}),
      };
      if (receipt && result.status === "completed" && (result.exit_code === null || result.exit_code === 0)) {
        return {
          kind: "execution",
          ...identity,
          result,
          receipt,
          ...(result.isolation ? { isolation: result.isolation } : {}),
          placement,
        };
      }
      return {
        kind: "failure",
        ...identity,
        failure: {
          kind: classifyExecutionFailure(result),
          message: result.error?.message ?? `execution ended with status ${result.status}`,
        },
        result,
        ...(receipt ? { receipt } : {}),
        ...(result.isolation ? { isolation: result.isolation } : {}),
        placement,
      };
    });
  }

  async poolSubmit(
    request: ExecutionRequest,
    options: PlacementOptions & { idempotency_key?: string | undefined },
  ): Promise<unknown> {
    return this.withRequestBundle(request, async (path) => {
      const command = await this.invoke([
        "pool", "submit", "--bundle", path, ...placementArgs(options),
        ...(options.idempotency_key ? ["--idempotency-key", options.idempotency_key] : []),
        ...this.poolArgs(), "--json",
      ]);
      const value = parseJson<Record<string, unknown>>(command.stdout);
      if (!value || (command.status !== 0 && !("placement" in value))) {
        throw new Error(command.stderr.trim() || "pool submission failed");
      }
      return value;
    });
  }

  private async invokeJson(args: string[], message: string): Promise<unknown> {
    const command = await this.invoke(args);
    const value = parseJson<unknown>(command.stdout);
    if (command.status !== 0 || !value) throw new Error(command.stderr.trim() || message);
    return value;
  }

  /** Run `operation` against a `.compute` bundle for either request form. */
  private async withRequestBundle<T>(
    request: ExecutionRequest,
    operation: (path: string) => Promise<T>,
  ): Promise<T> {
    if ("bundle" in request) return this.withBundle(request, operation);
    const invalid = validateExecutionRequest(request);
    if (invalid) throw new Error(invalid);
    const directory = await mkdtemp(join(tmpdir(), "compute-appport-pool-"));
    const bundle = join(directory, "workload.compute");
    try {
      const create = await this.invoke([
        "bundle", "create", "--workload",
        resolve(this.cwd ?? process.cwd(), request.invocation.workload_path),
        "--output", bundle, "--json",
      ]);
      if (create.status !== 0) throw new Error(create.stderr.trim() || "bundle creation failed");
      const inspected = await this.inspect(request);
      if (inspected.kind === "failure") throw new Error(inspected.failure.message);
      return await operation(bundle);
    } finally {
      await rm(directory, { recursive: true, force: true });
    }
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
      ...(request.execution_capabilities?.isolation
        ? ["--isolation", request.execution_capabilities.isolation.profile]
        : []),
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
      ...(request.execution_capabilities?.isolation
        ? ["--isolation", request.execution_capabilities.isolation.profile]
        : []),
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
      return {
        kind: "execution",
        workload_id: plan.workload_id,
        result,
        ...(result.receipt ? { receipt: result.receipt } : {}),
        ...(result.isolation ? { isolation: result.isolation } : {}),
      };
    }
    const kind = classifyExecutionFailure(result);
    return {
      kind: "failure",
      workload_id: plan.workload_id,
      failure: { kind, message: result.error?.message ?? `execution ended with status ${result.status}` },
      result,
      ...(result.receipt ? { receipt: result.receipt } : {}),
      ...(result.isolation ? { isolation: result.isolation } : {}),
    };
  }

  async capabilities(): Promise<unknown> {
    return this.providerCapabilities("local");
  }

  async submit(request: ExecutionRequest): Promise<unknown> {
    const provider = this.requireRemoteProvider();
    if ("bundle" in request) {
      return this.withBundle(request, async (path) => this.invokeRemoteJson([
        "remote", "submit", "--provider", provider, "--bundle", path, "--json",
      ]));
    }
    const directory = await mkdtemp(join(tmpdir(), "compute-appport-submit-"));
    const bundle = join(directory, "workload.compute");
    try {
      const create = await this.invoke([
        "bundle", "create", "--workload",
        resolve(this.cwd ?? process.cwd(), request.invocation.workload_path),
        "--output", bundle, "--json",
      ]);
      if (create.status !== 0) throw new Error(create.stderr.trim() || "bundle creation failed");
      return await this.invokeRemoteJson([
        "remote", "submit", "--provider", provider, "--bundle", bundle, "--json",
      ]);
    } finally {
      await rm(directory, { recursive: true, force: true });
    }
  }

  status(jobId: string): Promise<unknown> { return this.jobCommand("status", jobId); }
  cancel(jobId: string): Promise<unknown> { return this.jobCommand("cancel", jobId); }
  result(jobId: string): Promise<unknown> { return this.jobCommand("result", jobId); }
  receipt(jobId: string): Promise<unknown> { return this.jobCommand("receipt", jobId); }

  private async jobCommand(operation: string, jobId: string): Promise<unknown> {
    return this.invokeRemoteJson([
      "remote", operation, "--provider", this.requireRemoteProvider(), jobId, "--json",
    ]);
  }

  private requireRemoteProvider(): string {
    if (!this.remoteProvider) throw new Error("remoteProvider is required for asynchronous capabilities");
    return this.remoteProvider;
  }

  private async invokeRemoteJson(args: string[]): Promise<unknown> {
    const command = await this.invoke(args);
    const value = parseJson<unknown>(command.stdout);
    if (command.status !== 0 || !value) throw new Error(command.stderr.trim() || "remote provider operation failed");
    return value;
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
        return {
          kind: "execution",
          ...identity,
          result,
          ...(result.receipt ? { receipt: result.receipt } : {}),
          ...(result.isolation ? { isolation: result.isolation } : {}),
        };
      }
      return {
        kind: "failure",
        ...identity,
        failure: {
          kind: classifyExecutionFailure(result),
          message: result.error?.message ?? `execution ended with status ${result.status}`,
        },
        result,
        ...(result.receipt ? { receipt: result.receipt } : {}),
        ...(result.isolation ? { isolation: result.isolation } : {}),
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
  const provider = options.provider ?? new LocalComputeProvider(options);
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
  const pool = <K extends keyof ComputeProvider>(name: K): NonNullable<ComputeProvider[K]> => {
    const operation = provider[name];
    if (typeof operation !== "function") {
      throw new Error(`the configured Compute provider does not support ${String(name)}`);
    }
    return (operation as (...args: never[]) => unknown).bind(provider) as NonNullable<ComputeProvider[K]>;
  };
  const providerList = defineCapability({
    name: "compute.provider.list",
    version: 1,
    description: "List the caller-owned provider pool with discovery status and health.",
    input: providerListInputSchema,
    output: providerListResultSchema,
    effect: "observation",
    authorizationContract: { required: false, public: true },
    attributes: { "compute.contract": "1", "compute.executes": false },
    handler: async ({ refresh }) =>
      (await pool("listProviders")(refresh ?? false)) as never,
  });
  const providerInspect = defineCapability({
    name: "compute.provider.inspect",
    version: 1,
    description: "Inspect a provider: a pool member's validated descriptor, or local capabilities.",
    input: providerSelectorSchema,
    output: providerInspectResultSchema,
    effect: "observation",
    authorizationContract: { required: false, public: true },
    attributes: { "compute.contract": "1", "compute.executes": false },
    handler: async ({ provider: selected }) =>
      (selected === undefined || typeof provider.inspectProvider !== "function"
        ? await provider.capabilities()
        : await pool("inspectProvider")(selected)) as never,
  });
  const providerCapabilities = defineCapability({
    name: "compute.provider.capabilities",
    version: 1,
    description: "Describe a provider's raw capability response.",
    input: providerSelectorSchema,
    output: providerCapabilitiesSchema,
    effect: "observation",
    authorizationContract: { required: false, public: true },
    attributes: { "compute.contract": "1", "compute.executes": false },
    handler: async ({ provider: selected }) =>
      (selected === undefined || typeof provider.providerCapabilities !== "function"
        ? await provider.capabilities()
        : await pool("providerCapabilities")(selected)) as never,
  });
  const placementInspect = defineCapability({
    name: "compute.placement.inspect",
    version: 1,
    description: "Evaluate which provider can satisfy a workload contract. Never executes.",
    input: placementInspectInputSchema,
    output: placementReportSchema,
    effect: "observation",
    authorizationContract: { required: false, public: true },
    attributes: { "compute.contract": "1", "compute.executes": false },
    handler: async ({ request, ...options }) =>
      (await pool("inspectPlacement")(request as ExecutionRequest, options)) as never,
  });
  const poolRun = defineCapability({
    name: "compute.pool.run",
    version: 1,
    description: "Place a workload on a compatible provider and execute it there, once.",
    input: poolRunInputSchema,
    output: poolRunResultSchema,
    effect: "consequential",
    authorization: ["compute.run"],
    authorizationContract: { required: true, scopes: ["compute.run"] },
    attributes: { "compute.contract": "1", "compute.executes": true },
    handler: async ({ request, ...options }) =>
      (await pool("poolRun")(request as ExecutionRequest, options)) as never,
  });
  const poolSubmit = defineCapability({
    name: "compute.pool.submit",
    version: 1,
    description: "Place a workload on a compatible job-capable provider and submit a durable job.",
    input: poolSubmitInputSchema,
    output: jobValueSchema,
    effect: "consequential",
    authorization: ["compute.submit"],
    authorizationContract: { required: true, scopes: ["compute.submit"] },
    attributes: { "compute.contract": "1", "compute.executes": true },
    handler: ({ request, ...options }) =>
      pool("poolSubmit")(request as ExecutionRequest, options),
  });
  const providerRun = defineCapability({
    name: "compute.provider.run",
    version: 1,
    description: "Execute through the selected Compute provider.",
    input: runInputSchema,
    output: runResultSchema,
    effect: "consequential",
    authorization: ["compute.run"],
    authorizationContract: { required: true, scopes: ["compute.run"] },
    attributes: { "compute.contract": "1", "compute.executes": true },
    handler: ({ request }) => provider.run(request as ExecutionRequest),
  });
  const submit = defineCapability({
    name: "compute.submit", version: 1,
    description: "Submit a durable asynchronous Compute execution job.",
    input: jobSubmissionInputSchema, output: jobValueSchema, effect: "consequential",
    authorization: ["compute.submit"],
    authorizationContract: { required: true, scopes: ["compute.submit"] },
    handler: ({ request }) => provider.submit(request as ExecutionRequest),
  });
  const status = defineCapability({
    name: "compute.status", version: 1,
    description: "Observe a durable Compute job.", input: jobAccessInputSchema,
    output: jobValueSchema, effect: "observation", authorization: ["compute.status"],
    authorizationContract: { required: true, scopes: ["compute.status"] },
    handler: ({ job_id }) => provider.status(job_id),
  });
  const cancel = defineCapability({
    name: "compute.cancel", version: 1,
    description: "Request cancellation of a durable Compute job.", input: jobAccessInputSchema,
    output: jobValueSchema, effect: "consequential", authorization: ["compute.cancel"],
    authorizationContract: { required: true, scopes: ["compute.cancel"] },
    handler: ({ job_id }) => provider.cancel(job_id),
  });
  const result = defineCapability({
    name: "compute.result", version: 1,
    description: "Retrieve a terminal Compute job result.", input: jobAccessInputSchema,
    output: jobValueSchema, effect: "observation", authorization: ["compute.result"],
    authorizationContract: { required: true, scopes: ["compute.result"] },
    handler: ({ job_id }) => provider.result(job_id),
  });
  const jobReceipt = defineCapability({
    name: "compute.receipt", version: 1,
    description: "Retrieve independently verifiable job evidence.", input: jobAccessInputSchema,
    output: jobValueSchema, effect: "observation", authorization: ["compute.receipt"],
    authorizationContract: { required: true, scopes: ["compute.receipt"] },
    handler: ({ job_id }) => provider.receipt(job_id),
  });
  return createApplication({
    application: {
      id: "dev.compute.provider.local",
      name: "Compute Local Provider",
      version: "0.1.0",
      description: "Local AppPort provider for portable Compute workloads",
    },
    capabilities: [
      inspect, run, providerList, providerInspect, providerCapabilities, providerRun,
      placementInspect, poolRun, poolSubmit,
      submit, status, cancel, result, jobReceipt,
    ],
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
  if (requested?.isolation) {
    const declared = request.workload.isolation?.profile ?? "process";
    const rank = { process: 0, sandboxed: 1, strict: 2 } as const;
    if (rank[requested.isolation.profile] < rank[declared]) {
      return "requested isolation cannot weaken the workload isolation requirement";
    }
  }
  if (requested?.network && requested.network !== (request.workload.network ?? "none")) {
    return "requested network capability must match the workload requirement";
  }
  if (requested?.resources && canonical(requested.resources) !== canonical(request.workload.resources ?? {})) {
    return "requested resources must match the workload requirements";
  }
  return undefined;
}

function placementArgs(options: PlacementOptions): string[] {
  return [
    ...(options.provider ? ["--provider", options.provider] : []),
    ...(options.refresh ? ["--refresh"] : []),
    ...(options.distribution_id ? ["--distribution", options.distribution_id] : []),
    ...(options.isolation ? ["--isolation", options.isolation] : []),
  ];
}

function dispatchFailureKind(code: string): ExecutionFailureKind {
  switch (code) {
    case "placement_failed": return "placement_failed";
    case "provider_unavailable": return "provider_unavailable";
    case "evidence_invalid": return "evidence_invalid";
    default: return "provider_rejected";
  }
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
    isolation: workload.isolation ?? { profile: "process" },
    ...(workload.dependencies === undefined ? {} : { dependencies: workload.dependencies }),
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
  if (normalized.includes("unsupported capability") || normalized.includes("isolation profile")) {
    return "capability_denied";
  }
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
