import { s } from "@appport/schema";

const runtime = s.enum([
  "wasm", "node", "bun", "deno", "python", "ruby", "php", "jvm", "dotnet", "native", "shell",
] as const);
const network = s.enum(["none", "localhost", "network"] as const);
const isolationProfile = s.enum(["process", "sandboxed", "strict"] as const);
const isolationRequirement = s.object({ profile: isolationProfile });
const boundaryStatus = s.enum(["enforced", "disabled", "unavailable", "not_requested"] as const);
const isolationEvidence = s.object({
  profile: isolationProfile,
  requested: isolationProfile,
  effective: isolationProfile,
  filesystem: boundaryStatus,
  network: boundaryStatus,
  environment: boundaryStatus,
  resources: boundaryStatus,
});
const bytes = s.union([s.string(), s.array(s.integer({ minimum: 0, maximum: 255 }))] as const);
const positiveInteger = () => s.integer({ minimum: 1 });

export const resourceLimitsSchema = s.object({
  memory_bytes: s.optional(positiveInteger()),
  cpu_time_ms: s.optional(positiveInteger()),
  timeout_ms: s.optional(positiveInteger()),
  process_count: s.optional(positiveInteger()),
  stdout_bytes: s.optional(positiveInteger()),
  stderr_bytes: s.optional(positiveInteger()),
});

export const inputSourceSchema = s.union([
  s.object({ type: s.literal("inline"), data: bytes }),
  s.object({ type: s.literal("file"), path: s.string({ minLength: 1 }) }),
] as const);

export const workloadInputSchema = s.object({
  path: s.string({ minLength: 1 }),
  source: inputSourceSchema,
});

export const workloadOutputSchema = s.object({
  path: s.string({ minLength: 1 }),
  required: s.optional(s.boolean()),
});

export const workloadSpecSchema = s.object({
  version: s.union([s.literal("1"), s.literal(1)] as const),
  runtime,
  runtime_version: s.optional(s.string()),
  entrypoint: s.string({ minLength: 1 }),
  args: s.optional(s.array(s.string())),
  env: s.optional(s.record(s.string())),
  inputs: s.optional(s.array(workloadInputSchema)),
  outputs: s.optional(s.array(workloadOutputSchema)),
  resources: s.optional(resourceLimitsSchema),
  network: s.optional(network),
  isolation: s.optional(isolationRequirement),
});

const workloadExecutionRequestSchema = s.object({
  workload: workloadSpecSchema,
  execution_capabilities: s.optional(s.object({
    network: s.optional(network),
    resources: s.optional(resourceLimitsSchema),
    isolation: s.optional(isolationRequirement),
  })),
  environment: s.optional(s.record(s.string())),
  invocation: s.object({
    workload_path: s.string({ minLength: 1 }),
    metadata: s.optional(s.record(s.unknown())),
  }),
});

const bundleExecutionRequestSchema = s.object({
  bundle: s.object({
    data: s.array(s.integer({ minimum: 0, maximum: 255 })),
    expected_workload_id: s.optional(s.string()),
    expected_bundle_id: s.optional(s.string()),
  }),
  invocation: s.optional(s.object({ metadata: s.optional(s.record(s.unknown())) })),
});

export const executionRequestSchema = s.union([
  workloadExecutionRequestSchema,
  bundleExecutionRequestSchema,
] as const);

const failureKind = s.enum([
  "invalid_workload",
  "invalid_bundle",
  "identity_mismatch",
  "invalid_execution_request",
  "capability_denied",
  "runtime_failure",
  "input_materialization_failure",
  "output_contract",
] as const);

const failure = s.object({ kind: failureKind, message: s.string() });
const output = s.object({ text: s.string(), truncated: s.boolean(), bytes: s.integer({ minimum: 0 }) });
const outputArtifact = s.object({
  path: s.string(),
  data: bytes,
  size: s.integer({ minimum: 0 }),
});
const missingOutput = s.object({ path: s.string(), required: s.boolean() });
const executionError = s.object({
  execution_id: s.string(),
  phase: s.string(),
  kind: s.string(),
  message: s.string(),
  runtime: s.nullable(runtime),
  exit_code: s.nullable(s.integer()),
  started: s.boolean(),
});

const digest = s.string({ pattern: "^sha256:[0-9a-f]{64}$" });
const receipt = s.object({
  receipt_version: s.literal("compute.receipt@1"),
  execution_id: s.string(),
  workload: digest,
  bundle: s.nullable(digest),
  distribution: s.object({ id: digest, platform: s.string(), manifest_version: s.string() }),
  runtime: s.object({
    declared: runtime, selected: runtime, observed: runtime, version: s.string(),
    distribution_runtime_id: digest, executable_identity: digest,
  }),
  request: s.object({
    entrypoint: s.string(), argument_count: s.integer({ minimum: 0 }),
    stdin_size: s.integer({ minimum: 0 }), stdin_sha256: digest,
  }),
  policy: s.object({
    network, filesystem: s.string(), timeout_ms: s.nullable(s.integer({ minimum: 0 })),
    memory_bytes: s.nullable(s.integer({ minimum: 0 })), environment: s.literal("cleared"),
    environment_names: s.array(s.string()),
  }),
  isolation: isolationEvidence,
  inputs: s.array(s.object({
    path: s.string(), size: s.integer({ minimum: 0 }), sha256: digest, required: s.boolean(),
  })),
  outputs: s.array(s.object({
    path: s.string(), size: s.nullable(s.integer({ minimum: 0 })), sha256: s.nullable(digest),
    collection_status: s.enum(["collected", "missing_required", "missing_optional"] as const),
  })),
  execution: s.object({
    status: s.string(), exit_code: s.nullable(s.integer()),
    error: s.nullable(s.object({ kind: s.string(), code: s.string() })),
  }),
  started_at: s.nullable(s.string()),
  finished_at: s.nullable(s.string()),
  provenance: s.object({ distribution_id: digest, runtime_lock_id: digest, manifest_id: digest }),
  receipt_hash: digest,
});

export const executionResultSchema = s.object({
  execution_id: s.string(),
  runtime,
  network,
  lifecycle: s.array(s.string()),
  status: s.string(),
  exit_code: s.nullable(s.integer()),
  stdout: output,
  stderr: output,
  duration: s.integer({ minimum: 0 }),
  resource_usage: s.object({ max_memory_bytes: s.nullable(s.integer({ minimum: 0 })) }),
  artifacts: s.array(s.object({ name: s.string(), path: s.string(), size: s.integer({ minimum: 0 }) })),
  outputs: s.array(outputArtifact),
  missing_outputs: s.array(missingOutput),
  error: s.nullable(executionError),
  isolation: s.optional(isolationEvidence),
  receipt: s.optional(receipt),
});

export const inspectInputSchema = s.object({
  request: executionRequestSchema,
  mode: s.optional(s.enum(["inspect", "dry_run"] as const)),
});

export const workloadPlanSchema = s.object({
  valid: s.boolean(),
  workload_id: s.string({ pattern: "^sha256:[0-9a-f]{64}$" }),
  capability: s.object({
    name: s.literal("compute"),
    version: s.literal(1),
    operations: s.array(s.string()),
  }),
  workload: workloadSpecSchema,
  validation: s.literal("valid"),
  resolved_runtime: s.object({ kind: runtime, version: s.nullable(s.string()) }),
  backend_capabilities: (() => {
    const capability = () => s.object({ supported: s.boolean() });
    return s.object({
      stdin: capability(),
      stdout: capability(),
      stderr: capability(),
      environment: capability(),
      filesystem_isolation: capability(),
      artifacts: capability(),
      timeout: capability(),
      cancellation: capability(),
      stdout_limit: capability(),
      stderr_limit: capability(),
      memory_limit: capability(),
      cpu_limit: capability(),
      process_limit: capability(),
      network: s.object({ none: capability(), localhost: capability(), network: capability() }),
      isolation: s.object({
        process_boundary: s.boolean(), filesystem_boundary: s.boolean(),
        network_boundary: s.boolean(), environment_boundary: s.boolean(),
        timeout_enforcement: s.boolean(), memory_enforcement: s.boolean(),
        cpu_enforcement: s.boolean(), process_enforcement: s.boolean(),
      }),
    });
  })(),
  capability_compatible: s.boolean(),
  capability_error: s.optional(s.string()),
  isolation: s.object({
    requested: isolationProfile,
    effective: s.nullable(isolationProfile),
    compatible: s.boolean(),
    evidence: s.optional(isolationEvidence),
    reason: s.optional(s.object({ code: s.string(), message: s.string() })),
  }),
  input_preparation: s.array(workloadInputSchema),
  output_root: s.string(),
  data_flow: s.object({
    steps: s.array(s.string()),
    input_root: s.string(),
    entrypoint: s.string(),
    runtime,
    declared_outputs: s.array(workloadOutputSchema),
    output_root: s.string(),
  }),
});

const bundleWorkloadPlanSchema = s.object({
  bundle_verification: s.object({
    valid: s.literal(true),
    format: s.literal("compute.bundle"),
    version: s.literal(1),
    workload_id: s.string(),
    bundle_id: s.string(),
  }),
  plan: workloadPlanSchema,
});

export const inspectResultSchema = s.union([
  s.object({
    kind: s.literal("inspection"),
    mode: s.enum(["inspect", "dry_run"] as const),
    plan: s.union([workloadPlanSchema, bundleWorkloadPlanSchema] as const),
  }),
  s.object({ kind: s.literal("failure"), failure }),
] as const);

export const runInputSchema = s.object({ request: executionRequestSchema });

export const runResultSchema = s.union([
  s.object({
    kind: s.literal("execution"),
    workload_id: s.string(),
    bundle_id: s.optional(s.string()),
    result: executionResultSchema,
    receipt: s.optional(receipt),
    isolation: s.optional(isolationEvidence),
  }),
  s.object({
    kind: s.literal("failure"),
    workload_id: s.optional(s.string()),
    bundle_id: s.optional(s.string()),
    failure,
    result: s.optional(executionResultSchema),
    receipt: s.optional(receipt),
    isolation: s.optional(isolationEvidence),
  }),
] as const);
