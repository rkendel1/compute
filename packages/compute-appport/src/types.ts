export const COMPUTE_CAPABILITY_VERSION = 1 as const;

export type RuntimeKind =
  | "wasm"
  | "node"
  | "bun"
  | "deno"
  | "python"
  | "ruby"
  | "php"
  | "jvm"
  | "dotnet"
  | "native"
  | "shell";
export type NetworkPolicy = "none" | "localhost" | "network";
export type IsolationProfile = "process" | "sandboxed" | "strict";
export type BoundaryStatus = "enforced" | "disabled" | "unavailable" | "not_requested";

export interface IsolationEvidence {
  profile: IsolationProfile;
  requested: IsolationProfile;
  effective: IsolationProfile;
  filesystem: BoundaryStatus;
  network: BoundaryStatus;
  environment: BoundaryStatus;
  resources: BoundaryStatus;
}
export type JsonBytes = string | number[];

export interface ResourceLimits {
  memory_bytes?: number;
  cpu_time_ms?: number;
  timeout_ms?: number;
  process_count?: number;
  stdout_bytes?: number;
  stderr_bytes?: number;
}

export type InputSource =
  | { type: "inline"; data: JsonBytes }
  | { type: "file"; path: string };

export interface WorkloadInput {
  path: string;
  source: InputSource;
}

export interface WorkloadOutput {
  path: string;
  required?: boolean;
}

export interface WorkloadSpec {
  version: "1" | 1;
  runtime: RuntimeKind;
  runtime_version?: string;
  entrypoint: string;
  args?: string[];
  env?: Record<string, string>;
  inputs?: WorkloadInput[];
  outputs?: WorkloadOutput[];
  resources?: ResourceLimits;
  network?: NetworkPolicy;
  isolation?: { profile: IsolationProfile };
  dependencies?: { capsule: string };
}

/**
 * Invocation policy and local resolution metadata stay separate from the
 * portable workload. A remote provider may use different metadata without
 * changing WorkloadSpec or its identity.
 */
export interface WorkloadExecutionRequest {
  workload: WorkloadSpec;
  execution_capabilities?: {
    network?: NetworkPolicy;
    resources?: ResourceLimits;
    isolation?: { profile: IsolationProfile };
  };
  environment?: Record<string, string>;
  invocation: {
    workload_path: string;
    metadata?: Record<string, unknown>;
  };
}

export interface BundleExecutionRequest {
  bundle: {
    data: number[];
    expected_workload_id?: string;
    expected_bundle_id?: string;
  };
  invocation?: {
    metadata?: Record<string, unknown>;
  };
}

export type ExecutionRequest = WorkloadExecutionRequest | BundleExecutionRequest;

export type JobStatus =
  | "created" | "accepted" | "queued" | "preparing" | "running"
  | "succeeded" | "failed" | "cancelled" | "timed_out" | "rejected";

export interface JobSubmission {
  job_id: string;
  request_id?: string;
  status: JobStatus;
}

export interface ExecutionJob {
  version: "compute.job@1";
  job_id: string;
  status: JobStatus;
  execution_id?: string;
  result_digest?: string;
  cancellation: { requested: boolean; effective: boolean; phase?: string };
  failure?: string;
  created_at: string;
  updated_at: string;
}

export interface OutputArtifact {
  path: string;
  data: JsonBytes;
  size: number;
}

export interface ExecutionReceipt {
  receipt_version: "compute.receipt@1";
  execution_id: string;
  workload: string;
  bundle: string | null;
  provider?: { kind: "local"; id: string } | { kind: "remote"; id: string; endpoint: string };
  provider_protocol?: string;
  placement?: ReceiptPlacement;
  policy_id?: string;
  admission_id?: string;
  admission_status?: "admitted";
  distribution: { id: string; platform: string; manifest_version: string };
  runtime: {
    declared: RuntimeKind;
    selected: RuntimeKind;
    observed: RuntimeKind;
    version: string;
    distribution_runtime_id: string;
    executable_identity: string;
  };
  request: { entrypoint: string; argument_count: number; stdin_size: number; stdin_sha256: string };
  policy: {
    network: NetworkPolicy;
    filesystem: string;
    timeout_ms: number | null;
    memory_bytes: number | null;
    environment: "cleared";
    environment_names: string[];
  };
  isolation: IsolationEvidence;
  dependencies?: { capsule_id: string; verified: boolean };
  inputs: Array<{ path: string; size: number; sha256: string; required: boolean }>;
  outputs: Array<{
    path: string;
    size: number | null;
    sha256: string | null;
    collection_status: "collected" | "missing_required" | "missing_optional";
  }>;
  execution: {
    status: string;
    exit_code: number | null;
    error: { kind: string; code: string } | null;
  };
  started_at: string | null;
  finished_at: string | null;
  provenance: { distribution_id: string; runtime_lock_id: string; manifest_id: string };
  receipt_hash: string;
}

export type ExecutionFailureKind =
  | "invalid_workload"
  | "invalid_bundle"
  | "identity_mismatch"
  | "invalid_execution_request"
  | "capability_denied"
  | "runtime_failure"
  | "input_materialization_failure"
  | "output_contract"
  | "placement_failed"
  | "provider_unavailable"
  | "provider_rejected"
  | "evidence_invalid"
  | "admission_denied";

export interface OutputContractFailure {
  kind: "output_contract";
  message: string;
  missing_outputs: Array<{ path: string; required: boolean }>;
}

export interface ExecutionResult {
  execution_id: string;
  runtime: RuntimeKind;
  network: NetworkPolicy;
  lifecycle: string[];
  status: string;
  exit_code: number | null;
  stdout: { text: string; truncated: boolean; bytes: number };
  stderr: { text: string; truncated: boolean; bytes: number };
  duration: number;
  resource_usage: { max_memory_bytes: number | null };
  artifacts: Array<{ name: string; path: string; size: number }>;
  outputs: OutputArtifact[];
  missing_outputs: Array<{ path: string; required: boolean }>;
  error: {
    execution_id: string;
    phase: string;
    kind: string;
    message: string;
    runtime: RuntimeKind | null;
    exit_code: number | null;
    started: boolean;
  } | null;
  isolation?: IsolationEvidence;
  dependencies?: { capsule_id: string; file_count: number; verified: boolean };
  provider?: { kind: "local"; id: string } | { kind: "remote"; id: string; endpoint: string };
  admission?: { policy_id: string; admission_id: string; admission_status: "admitted" };
  receipt?: ExecutionReceipt;
}

export interface RuntimeCapabilities {
  stdin: { supported: boolean };
  stdout: { supported: boolean };
  stderr: { supported: boolean };
  environment: { supported: boolean };
  filesystem_isolation: { supported: boolean };
  artifacts: { supported: boolean };
  timeout: { supported: boolean };
  cancellation: { supported: boolean };
  stdout_limit: { supported: boolean };
  stderr_limit: { supported: boolean };
  memory_limit: { supported: boolean };
  cpu_limit: { supported: boolean };
  process_limit: { supported: boolean };
  network: Record<NetworkPolicy, { supported: boolean }>;
  isolation: {
    process_boundary: boolean;
    filesystem_boundary: boolean;
    network_boundary: boolean;
    environment_boundary: boolean;
    timeout_enforcement: boolean;
    memory_enforcement: boolean;
    cpu_enforcement: boolean;
    process_enforcement: boolean;
  };
}

export interface WorkloadPlan {
  valid: boolean;
  workload_id: string;
  capability: { name: "compute"; version: 1; operations: string[] };
  workload: WorkloadSpec;
  validation: "valid";
  resolved_runtime: { kind: RuntimeKind; version: string | null };
  backend_capabilities: RuntimeCapabilities;
  capability_compatible: boolean;
  capability_error?: string;
  dependencies: { required: boolean; capsule_id?: string; available: boolean };
  isolation: {
    requested: IsolationProfile;
    effective: IsolationProfile | null;
    compatible: boolean;
    evidence?: IsolationEvidence;
    reason?: { code: string; message: string };
  };
  input_preparation: WorkloadInput[];
  output_root: string;
  data_flow: {
    steps: string[];
    input_root: string;
    entrypoint: string;
    runtime: RuntimeKind;
    declared_outputs: WorkloadOutput[];
    output_root: string;
  };
}

export interface BundleWorkloadPlan {
  bundle_verification: {
    valid: true;
    format: "compute.bundle";
    version: 1;
    workload_id: string;
    bundle_id: string;
  };
  plan: WorkloadPlan;
}

export type InspectResult =
  | { kind: "inspection"; mode: "inspect" | "dry_run"; plan: WorkloadPlan | BundleWorkloadPlan }
  | { kind: "failure"; failure: { kind: ExecutionFailureKind; message: string } };

export type RunResult =
  | { kind: "execution"; workload_id: string; bundle_id?: string; result: ExecutionResult; receipt?: ExecutionReceipt; isolation?: IsolationEvidence }
  | {
      kind: "failure";
      workload_id?: string;
      bundle_id?: string;
      failure: { kind: ExecutionFailureKind; message: string };
      result?: ExecutionResult;
      receipt?: ExecutionReceipt;
      isolation?: IsolationEvidence;
    };

export interface ReceiptPlacement {
  placement_id: string;
  provider_id: string;
  provider_protocol: string;
  selection_mode: "explicit" | "pool";
  selection_reason: {
    compatibility_result: "compatible";
    selection_priority: number;
    ordering: string;
    compatible_candidates: number;
  };
}

/** Deterministic placement decision produced by `compute placement inspect`. */
export interface PlacementReport {
  placement_version: "compute.placement@1";
  placement_id: string;
  outcome: "placed" | "placement_failed";
  selection_mode: "explicit" | "pool";
  requested_provider?: string;
  requirements: Record<string, unknown>;
  selection_policy: { ordering: string[]; require_healthy: boolean; allow_stale_capabilities: boolean };
  policy_id: string;
  admission: Record<string, unknown>;
  providers: Array<Record<string, unknown>>;
  compatible_providers: string[];
  incompatible_providers: string[];
  excluded_providers: string[];
  selected?: Record<string, unknown> & { provider_id: string };
  failure?: { code: string; message: string };
  explanation: { requires: string[]; considered: string[]; selection: string };
}

export interface PlacementOptions {
  provider?: string | undefined;
  refresh?: boolean | undefined;
  distribution_id?: string | undefined;
  isolation?: IsolationProfile | undefined;
}

export type PoolRunResult =
  | (Extract<RunResult, { kind: "execution" }> & { receipt: ExecutionReceipt; placement: PlacementReport })
  | (Extract<RunResult, { kind: "failure" }> & { placement?: PlacementReport });

export interface AdmissionReason {
  code: string;
  kind: "contract" | "capability" | "policy";
  dimension: string;
  requested: unknown;
  allowed: unknown;
  message: string;
}

/** compute.admission@1: whether an execution is permitted, and why. */
export interface AdmissionDecision {
  admission_version: "compute.admission@1";
  admission_id: string;
  status: "admitted" | "denied";
  admitted: boolean;
  policy_id: string;
  provider: Record<string, unknown>;
  capability: Record<string, unknown>;
  reasons: AdmissionReason[];
  contract: Record<string, unknown>;
}

export interface PolicyCheckResult {
  policy: Array<Record<string, unknown>>;
  policy_id: string;
  requirements: Record<string, unknown>;
  provider: Record<string, unknown>;
  admission: { admission_id: string; status: "admitted" | "denied"; admitted: boolean; capability: Record<string, unknown> };
  reasons: AdmissionReason[];
  effective_policy: Record<string, unknown>;
  decision: AdmissionDecision;
}

/** compute.environment@1 lifecycle states. */
export type DesiredState = "running" | "stopped";
export type ActualState =
  | "pending" | "starting" | "running" | "stopping" | "stopped"
  | "completed" | "failed" | "denied" | "degraded";
export type Health = "healthy" | "unhealthy" | "unknown";
export type EnvironmentWorkloadKind = "service" | "task";

export interface EnvironmentWorkloadView {
  workload_id: string;
  name: string;
  kind: EnvironmentWorkloadKind;
  desired_state: DesiredState;
  actual_state: ActualState;
  health: Health;
  runtime: RuntimeKind;
  bundle_id: string;
  execution_id?: string;
  /** Logical project ports and the host ports the environment bound them to. */
  ports: Array<{ name: string; logical: number; host: number }>;
  restarts: number;
  started_at?: string;
  finished_at?: string;
  exit_code?: number;
  error?: string;
  placement: { placement_id?: string; provider?: string; node?: string };
  evidence: { policy_id?: string; admission_id?: string; receipt_ids: string[] };
  /** CPU usage is reported as "not_measured". */
  resources: { cpu: string; memory_limit_bytes?: number; timeout_ms?: number; disk_bytes: number; network: NetworkPolicy };
  log_directory?: string;
}

export interface ProjectView {
  project_id: string;
  name: string;
  revision: string;
  revision_digest: string;
  source?: string;
  desired_state: DesiredState;
  actual_state: ActualState;
  health: Health;
  deployed_at: string;
  workloads: EnvironmentWorkloadView[];
  disk_bytes: number;
}

export interface EnvironmentView {
  version: "compute.environment@1";
  environment_id: string;
  name: string;
  desired_state: DesiredState;
  actual_state: ActualState;
  health: Health;
  created_at: string;
  /** The environment's effective policy: daemon ∩ environment ∩ baseline. */
  policy_id: string;
  provider?: string;
  project_count: number;
  projects: ProjectView[];
  disk_bytes: number;
}

export interface EnvironmentSummary {
  environment_id: string;
  name: string;
  desired_state: DesiredState;
  actual_state: ActualState;
  health: Health;
  project_count: number;
}

export interface EnvironmentCreateInput {
  name: string;
  desired_state?: DesiredState | undefined;
  env?: Record<string, string> | undefined;
  policy?: Record<string, unknown> | undefined;
  provider?: string | undefined;
}

export interface EnvironmentWorkloadDefinition {
  name: string;
  kind: EnvironmentWorkloadKind;
  /** The canonical `.compute` bundle bytes. */
  bundle: string | number[];
  ports?: Array<{ name: string; port: number }> | undefined;
  restart?: "never" | "on_failure" | undefined;
  desired_state?: DesiredState | undefined;
}

export interface ProjectAddInput {
  environment: string;
  project: {
    name: string;
    revision: string;
    source?: string | undefined;
    desired_state?: DesiredState | undefined;
    env?: Record<string, string> | undefined;
    workloads: EnvironmentWorkloadDefinition[];
  };
}
