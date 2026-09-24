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
  dependencies: s.optional(s.object({ capsule: s.string({ pattern: "^sha256:[0-9a-f]{64}$" }) })),
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
  "placement_failed",
  "provider_unavailable",
  "provider_rejected",
  "evidence_invalid",
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
const providerIdentity = s.union([
  s.object({ kind: s.literal("local"), id: s.string() }),
  s.object({ kind: s.literal("remote"), id: s.string(), endpoint: s.string() }),
] as const);
const selectionMode = s.enum(["explicit", "pool"] as const);
const receiptPlacement = s.object({
  placement_id: digest,
  provider_id: s.string({ pattern: "^[A-Za-z0-9_-]{1,64}$" }),
  provider_protocol: s.string(),
  selection_mode: selectionMode,
  selection_reason: s.object({
    compatibility_result: s.literal("compatible"),
    selection_priority: s.integer(),
    ordering: s.string(),
    compatible_candidates: s.integer({ minimum: 1 }),
  }),
});
const receipt = s.object({
  receipt_version: s.literal("compute.receipt@1"),
  execution_id: s.string(),
  workload: digest,
  bundle: s.nullable(digest),
  provider: s.optional(providerIdentity),
  provider_protocol: s.optional(s.string()),
  placement: s.optional(receiptPlacement),
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
  dependencies: s.optional(s.object({ capsule_id: digest, verified: s.boolean() })),
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
  dependencies: s.optional(s.object({
    capsule_id: digest, file_count: s.integer({ minimum: 0 }), verified: s.boolean(),
  })),
  provider: s.optional(providerIdentity),
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
  dependencies: s.object({
    required: s.boolean(), capsule_id: s.optional(digest), available: s.boolean(),
  }),
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

export const providerSelectorSchema = s.object({
  provider: s.optional(s.string()),
});

export const providerCapabilitiesSchema = s.object({
  protocol: s.string(),
  provider: providerIdentity,
  artifact_modes: s.array(s.string()),
  isolation_profiles: s.array(isolationProfile),
  network_policies: s.array(network),
  dependency_capsule_formats: s.array(s.string()),
  max_request_bytes: s.integer({ minimum: 0 }),
  max_output_bytes: s.integer({ minimum: 0 }),
  distribution_id: s.optional(s.string()),
  max_concurrent_jobs: s.optional(s.integer({ minimum: 1 })),
  job_retention_seconds: s.optional(s.integer({ minimum: 0 })),
  dependency_capsules: s.optional(s.array(digest)),
  runtime_artifacts: s.optional(s.record(digest)),
  max_timeout_ms: s.optional(s.integer({ minimum: 1 })),
  max_memory_bytes: s.optional(s.integer({ minimum: 1 })),
  inventory: s.unknown(),
});

const providerId = s.string({ pattern: "^[A-Za-z0-9_-]{1,64}$" });
const discoveryStatus = s.enum(["discovered", "cached", "stale", "invalid", "unavailable"] as const);
const discoveryError = s.object({ code: s.string(), message: s.string() });

/** A pool member's validated descriptor (or why it has none). */
export const discoveryRecordSchema = s.object({
  provider_id: providerId,
  status: discoveryStatus,
  descriptor: s.optional(s.object({}, { additionalProperties: true })),
  error: s.optional(discoveryError),
});

export const providerInspectResultSchema = s.union([
  providerCapabilitiesSchema,
  discoveryRecordSchema,
] as const);

export const providerListInputSchema = s.object({ refresh: s.optional(s.boolean()) });

export const providerListResultSchema = s.object({
  providers: s.array(s.object({
    provider_id: providerId,
    kind: s.enum(["local", "remote"] as const),
    endpoint: s.nullable(s.string()),
    priority: s.integer(),
    discovery: s.nullable(discoveryStatus),
    health: s.nullable(s.enum(["healthy", "unhealthy", "unknown"] as const)),
    capability_version: s.nullable(digest),
    runtimes: s.nullable(s.record(s.string())),
    error: s.nullable(discoveryError),
  })),
});

/** Selection options shared by placement inspection and pool execution. */
const placementOptions = {
  provider: s.optional(providerId),
  refresh: s.optional(s.boolean()),
  distribution_id: s.optional(digest),
  isolation: s.optional(isolationProfile),
};

export const placementInspectInputSchema = s.object({
  request: executionRequestSchema,
  submit: s.optional(s.boolean()),
  ...placementOptions,
});

export const placementReportSchema = s.object({
  placement_version: s.literal("compute.placement@1"),
  placement_id: digest,
  outcome: s.enum(["placed", "placement_failed"] as const),
  selection_mode: selectionMode,
  requested_provider: s.optional(providerId),
  requirements: s.object({}, { additionalProperties: true }),
  selection_policy: s.object({
    ordering: s.array(s.string()),
    require_healthy: s.boolean(),
    allow_stale_capabilities: s.boolean(),
  }),
  providers: s.array(s.object({}, { additionalProperties: true })),
  compatible_providers: s.array(providerId),
  incompatible_providers: s.array(providerId),
  excluded_providers: s.array(providerId),
  selected: s.optional(s.object({}, { additionalProperties: true })),
  failure: s.optional(s.object({ code: s.string(), message: s.string() })),
  explanation: s.object({
    requires: s.array(s.string()),
    considered: s.array(s.string()),
    selection: s.string(),
  }),
});

export const poolRunInputSchema = s.object({ request: executionRequestSchema, ...placementOptions });
export const poolSubmitInputSchema = s.object({
  request: executionRequestSchema,
  idempotency_key: s.optional(s.string({ minLength: 1, maxLength: 256 })),
  ...placementOptions,
});

export const jobSubmissionInputSchema = s.object({ request: executionRequestSchema });
export const jobAccessInputSchema = s.object({ job_id: s.string({ pattern: "^job_[0-9a-f]{64}$" }) });
export const jobValueSchema = s.unknown();

/** `compute.pool.run` adds the placement decision to every outcome. */
export const poolRunResultSchema = s.union([
  s.object({
    kind: s.literal("execution"),
    workload_id: s.string(),
    bundle_id: s.optional(s.string()),
    result: executionResultSchema,
    receipt: receipt,
    isolation: s.optional(isolationEvidence),
    placement: placementReportSchema,
  }),
  s.object({
    kind: s.literal("failure"),
    workload_id: s.optional(s.string()),
    bundle_id: s.optional(s.string()),
    failure,
    result: s.optional(executionResultSchema),
    receipt: s.optional(receipt),
    isolation: s.optional(isolationEvidence),
    placement: s.optional(placementReportSchema),
  }),
] as const);

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
