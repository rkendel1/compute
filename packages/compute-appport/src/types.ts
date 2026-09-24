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
  | "output_contract";

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
