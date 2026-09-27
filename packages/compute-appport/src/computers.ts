import { ComputeDaemonClient, type ComputeDaemonClientOptions } from "./environment.js";

/**
 * Environments on a computer, for programs, agents, and embedding
 * applications (Attn, Factory, a desktop shell).
 *
 * ```ts
 * const compute = new ComputeDaemonClient({ token });
 * await createComputeEnvironment(compute, {
 *   name: "my-app",
 *   computer: { lifecycle: "persistent", requirements: { cpu_count: 4, memory_bytes: 8 * 2 ** 30 } },
 * });
 * await updateComputeEnvironment(compute, "my-app", (contents) => {
 *   contents.repositories = [{ name: "app", url: "https://…/app.git", revision: "main" }];
 *   contents.processes = [{ name: "api", command: ["npm", "start"], repository: "app" }];
 * });
 * const result = await executeComputeEnvironment(compute, "my-app", ["npm", "test"]);
 * ```
 *
 * Every function is one request to the Compute daemon's API. The daemon
 * authorizes it, records it, and reconciles the computer; nothing here
 * keeps state or decides anything.
 */

export type ComputerLifecycle = "persistent" | "ephemeral";

export type ComputerStatus =
  | "pending"
  | "provisioning"
  | "running"
  | "stopping"
  | "stopped"
  | "resuming"
  | "failed"
  | "destroying"
  | "destroyed"
  | "expired";

export interface ComputerRequirements {
  cpu_count?: number;
  memory_bytes?: number;
  disk_bytes?: number;
  architecture?: string;
  network?: "none" | "localhost" | "network";
  isolation?: "process" | "sandboxed" | "strict";
  /** Session capabilities: `persistent_storage`, `public_endpoint`, `terminal`, … */
  capabilities?: string[];
  /** Target features: `kvm`, `firecracker`, `containers`, `gpu`, `virtualization`. */
  features?: string[];
}

export interface RepositorySpec {
  name: string;
  url: string;
  revision: string;
}

export interface PackageSpec {
  name: string;
  install: string[];
  repository?: string;
}

export interface ProcessSpec {
  name: string;
  kind?: "application" | "service" | "agent" | "process";
  command: string[];
  repository?: string;
  env?: Record<string, string>;
  desired?: "running" | "stopped";
}

/** What belongs in the computer: desired state. */
export interface EnvironmentContents {
  repositories?: RepositorySpec[];
  packages?: PackageSpec[];
  processes?: ProcessSpec[];
  generation?: number;
}

export interface OperationEvidence {
  job_id: string;
  execution_id: string;
  outcome: string;
  at: string;
  error?: string;
}

export interface ObservedContents {
  repositories?: Record<string, { revision: string; commit?: string; fingerprint: string; evidence: OperationEvidence }>;
  packages?: Record<string, { fingerprint: string; evidence: OperationEvidence }>;
  processes?: Record<string, { state: "running" | "stopped" | "exited" | "failed"; fingerprint: string; pid?: number; evidence: OperationEvidence }>;
  converged_generation: number;
  observed_at?: string;
}

export interface ComputerView {
  environment: string;
  environment_id: string;
  owner: string;
  lifecycle: ComputerLifecycle;
  status: ComputerStatus;
  requirements: ComputerRequirements;
  spec_generation: number;
  running_generation: number;
  target?: string;
  requested_target?: string;
  placement_id?: string;
  session_id?: string;
  provider_kind?: string;
  capabilities?: Record<string, boolean>;
  connection?: { mode: string; address?: string; port?: number; details?: Record<string, string> };
  desired: EnvironmentContents;
  observed: ObservedContents;
  converged: boolean;
  failure?: { phase: string; code: string; message: string; retryable: boolean; target?: string; at: string };
  generation: number;
  created_at: string;
  ready_at?: string;
  expires_at?: string;
  ended_at?: string;
}

export interface ComputeEnvironmentDefinition {
  name: string;
  env?: Record<string, string>;
  desired_state?: "running" | "stopped";
  computer: {
    lifecycle?: ComputerLifecycle;
    requirements?: ComputerRequirements;
    /** Constrain placement to one target; placement chooses otherwise. */
    target?: string;
    ttl_seconds?: number;
  };
  contents?: EnvironmentContents;
}

export interface ComputeTarget {
  target_id: string;
  kind: "local" | "remote";
  health: string;
  hosts_computers: boolean;
  platform?: { os: string; architecture: string };
  capabilities?: Record<string, boolean>;
  features?: string[];
  error?: string;
}

export interface ComputerExecResult {
  environment: string;
  target: string;
  session_id: string;
  job_id: string;
  execution_id: string;
  status: string;
  exit_code: number | null;
  stdout: string;
  stderr: string;
  /** The job and its full result, including the verifiable receipt. */
  job: { job: Record<string, unknown>; result?: Record<string, unknown> };
}

function path(environment: string, rest = ""): string {
  return `/environments/${encodeURIComponent(environment)}${rest}`;
}

export function computeClient(options: ComputeDaemonClientOptions = {}): ComputeDaemonClient {
  return new ComputeDaemonClient(options);
}

/** Create an environment on a computer. Placement chooses the target. */
export function createComputeEnvironment(client: ComputeDaemonClient, definition: ComputeEnvironmentDefinition): Promise<unknown> {
  return client.request("POST", "/environments", {
    ...definition,
    computer: { lifecycle: "persistent", ...definition.computer },
  });
}

export function getComputeEnvironment(client: ComputeDaemonClient, environment: string): Promise<ComputerView> {
  return client.request("GET", path(environment, "/computer"));
}

/** Every environment that has a computer. */
export async function listComputeEnvironments(client: ComputeDaemonClient): Promise<ComputerView[]> {
  const environments = await client.listEnvironments();
  const views: ComputerView[] = [];
  for (const environment of environments) {
    if (!environment.computer) continue;
    views.push(await getComputeEnvironment(client, environment.name));
  }
  return views;
}

/**
 * Change what belongs in the computer. `change` edits a copy of the current
 * desired contents; the result is submitted only if nobody changed them in
 * between (what a UI's GO does), and the computer is changed in place.
 */
export async function updateComputeEnvironment(
  client: ComputeDaemonClient,
  environment: string,
  change: EnvironmentContents | ((contents: EnvironmentContents) => void | EnvironmentContents),
): Promise<ComputerView> {
  const current = await getComputeEnvironment(client, environment);
  let contents: EnvironmentContents;
  if (typeof change === "function") {
    const draft = structuredClone(current.desired);
    contents = change(draft) ?? draft;
  } else {
    contents = change;
  }
  return client.request("POST", path(environment, "/contents"), {
    contents,
    expected_generation: current.desired.generation ?? 0,
  });
}

/** Run a command in the computer as a durable job, and wait for it. */
export async function executeComputeEnvironment(
  client: ComputeDaemonClient,
  environment: string,
  command: string[],
  options: { env?: Record<string, string>; timeoutMs?: number; pollMs?: number } = {},
): Promise<ComputerExecResult> {
  const submitted = await client.request<{ environment: string; target: string; session_id: string; job_id: string; execution_id: string; status: string }>(
    "POST",
    path(environment, "/exec"),
    { command, ...(options.env ? { env: options.env } : {}), ...(options.timeoutMs ? { timeout: options.timeoutMs } : {}) },
  );
  for (;;) {
    const job = await client.request<{ job: { status: string; failure?: string }; result?: { result: { exit_code?: number; stdout: { text: string }; stderr: { text: string }; status: string } } }>(
      "GET",
      path(environment, `/jobs/${encodeURIComponent(submitted.job_id)}`),
    );
    if (job.result || ["succeeded", "failed", "cancelled", "timed_out", "rejected"].includes(job.job.status)) {
      const result = job.result?.result;
      return {
        ...submitted,
        status: job.job.status,
        exit_code: result?.exit_code ?? null,
        stdout: result?.stdout.text ?? "",
        stderr: result?.stderr.text ?? job.job.failure ?? "",
        job: job as unknown as ComputerExecResult["job"],
      };
    }
    await new Promise((resolve) => setTimeout(resolve, options.pollMs ?? 100));
  }
}

export function connectComputeEnvironment(client: ComputeDaemonClient, environment: string): Promise<{
  session_id: string;
  connection: { mode: string; address?: string; port?: number };
  command?: string[];
  credentials?: Record<string, string>;
  expires_at?: string;
}> {
  return client.request("POST", path(environment, "/connect"));
}

/** Destroy the computer. The environment's record remains as evidence. */
export function destroyComputeEnvironment(client: ComputeDaemonClient, environment: string): Promise<ComputerView> {
  return client.request("DELETE", path(environment));
}

export function reconcileComputeEnvironment(client: ComputeDaemonClient, environment: string): Promise<ComputerView> {
  return client.request("POST", path(environment, "/reconcile"));
}

/** Replace the computer to meet new requirements: the one change that provisions a new machine. */
export function replaceComputeEnvironment(client: ComputeDaemonClient, environment: string, requirements: ComputerRequirements): Promise<ComputerView> {
  return client.request("POST", path(environment, "/replace"), requirements);
}

export function listComputeTargets(client: ComputeDaemonClient): Promise<ComputeTarget[]> {
  return client.request("GET", "/targets");
}
