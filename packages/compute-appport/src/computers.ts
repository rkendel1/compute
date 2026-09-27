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
  | "expired"
  /** Its target did not answer, or refused this control plane. */
  | "unreachable"
  /** Its target answered without the machine. Wanted still; replace it. */
  | "lost";

/**
 * Desired and observed state, told apart: the one account every surface
 * (API, CLI, UI, AppPort) gives of a computer.
 */
export interface ComputerReality {
  desired: "running" | "stopped" | "destroyed";
  observed:
    | "starting"
    | "running"
    | "unverified"
    | "reconciling"
    | "unreachable"
    | "lost"
    | "stopping"
    | "stopped"
    | "failed"
    | "destroyed"
    | "expired";
  /** When the target last confirmed the machine, while it runs. */
  confirmed_at?: string;
  /** When it became unreachable or lost. */
  since?: string;
  explanation: string;
}

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
  /** Published as an endpoint, and given to the process as `$PORT`. */
  port?: number;
}

/** Software in one of the environment's repositories, built and operated inside the computer. */
export interface ProjectSpec {
  name: string;
  repository: string;
  /** Runs whenever the checkout, the command, or the configuration changes. */
  build?: string[];
  test?: string[];
  commands?: Record<string, string[]>;
}

/** What belongs in the computer: desired state. */
export interface EnvironmentContents {
  repositories?: RepositorySpec[];
  packages?: PackageSpec[];
  processes?: ProcessSpec[];
  projects?: ProjectSpec[];
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
  builds?: Record<string, { commit?: string; fingerprint: string; evidence: OperationEvidence }>;
  converged_generation: number;
  observed_at?: string;
}

export interface ComputerView {
  environment: string;
  environment_id: string;
  owner: string;
  lifecycle: ComputerLifecycle;
  requested_lifecycle: ComputerLifecycle;
  status: ComputerStatus;
  /** The machine behind the environment. Ordinary changes never replace it. */
  machine?: { target: string; session_id: string; provider_kind?: string; resource?: string };
  config: Record<string, string>;
  endpoints?: { process: string; port: number; url?: string; serving: boolean }[];
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
  reality: ComputerReality;
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
  return waitForJob(client, environment, submitted, options.pollMs);
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

export interface LifecycleChange {
  lifecycle: ComputerLifecycle;
  ttl_seconds?: number;
}

export interface UpdateOptions {
  /** Replace the configuration in the same change. */
  config?: Record<string, string>;
  /** Change how long the environment lives, in the same change. */
  lifecycle?: LifecycleChange;
}

/**
 * GO: submit contents, configuration, and lifetime as one change, only if
 * the environment is still at `expectedGeneration`. A stale view is refused
 * with `conflict` ("the environment changed since you loaded it").
 */
export function submitComputeEnvironment(
  client: ComputeDaemonClient,
  environment: string,
  contents: EnvironmentContents,
  expectedGeneration: number,
  options: UpdateOptions = {},
): Promise<ComputerView> {
  return client.request("POST", path(environment, "/contents"), {
    contents,
    expected_generation: expectedGeneration,
    ...(options.config ? { config: options.config } : {}),
    ...(options.lifecycle ? { lifecycle: options.lifecycle } : {}),
  });
}

/** Release a revision of a project: the same computer checks it out, builds it, and restarts what runs from it. */
export function releaseComputeEnvironment(
  client: ComputeDaemonClient,
  environment: string,
  project: string,
  revision: string,
  expectedGeneration?: number,
): Promise<ComputerView> {
  return client.request("POST", path(environment, "/release"), {
    project,
    revision,
    ...(expectedGeneration === undefined ? {} : { expected_generation: expectedGeneration }),
  });
}

/** Wait until the computer holds generation `generation` of the contents (or failed trying). */
export async function waitForComputeEnvironment(
  client: ComputeDaemonClient,
  environment: string,
  generation: number,
  options: { timeoutMs?: number; pollMs?: number } = {},
): Promise<ComputerView> {
  const deadline = Date.now() + (options.timeoutMs ?? 10 * 60_000);
  for (;;) {
    const view = await getComputeEnvironment(client, environment);
    const settled = view.converged && view.observed.converged_generation >= generation;
    if (settled || view.failure?.phase === "reconciliation" || ["failed", "destroyed", "expired"].includes(view.status)) {
      return view;
    }
    if (Date.now() > deadline) throw new Error(`${environment} did not reach generation ${generation}`);
    await new Promise((resolve) => setTimeout(resolve, options.pollMs ?? 250));
  }
}

/** Run a project's `build`, `test`, or named command inside the environment's computer, and wait for it. */
export async function runComputeProjectCommand(
  client: ComputeDaemonClient,
  environment: string,
  project: string,
  command: string,
  options: { env?: Record<string, string>; pollMs?: number } = {},
): Promise<ComputerExecResult> {
  const submitted = await client.request<{ environment: string; target: string; session_id: string; job_id: string; execution_id: string; status: string }>(
    "POST",
    path(environment, "/run"),
    { project, command, ...(options.env ? { env: options.env } : {}) },
  );
  return waitForJob(client, environment, submitted, options.pollMs);
}

async function waitForJob(
  client: ComputeDaemonClient,
  environment: string,
  submitted: { environment: string; target: string; session_id: string; job_id: string; execution_id: string; status: string },
  pollMs = 100,
): Promise<ComputerExecResult> {
  for (;;) {
    const job = await client.request<{ job: { status: string; failure?: string }; result?: { result: { exit_code?: number; stdout: { text: string }; stderr: { text: string } } } }>(
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
    await new Promise((resolve) => setTimeout(resolve, pollMs));
  }
}

export function setComputeEnvironmentLifetime(client: ComputeDaemonClient, environment: string, lifecycle: LifecycleChange): Promise<ComputerView> {
  return client.request("POST", path(environment, "/lifecycle"), lifecycle);
}

export function setComputeEnvironmentConfig(client: ComputeDaemonClient, environment: string, config: Record<string, string>): Promise<ComputerView> {
  return client.request("POST", path(environment, "/config"), config);
}

export interface WorkSession {
  session_id: string;
  environment: string;
  environment_id: string;
  owner: string;
  /** `attached`: the environment outlives the session. `ephemeral`: the session's own temporary environment ends with it. */
  kind: "attached" | "ephemeral";
  status: "open" | "closed";
  opened_at: string;
  closed_at?: string;
  expires_at?: string;
  close_reason?: string;
  connection?: { connection: { mode: string; address?: string; port?: number }; command?: string[] };
}

/**
 * Open a work session: `{ environment }` enters one you have (closing it
 * leaves the environment running); `{ computer }` makes a temporary
 * environment for the session (closing it, or its TTL, destroys it).
 */
export function openWorkSession(
  client: ComputeDaemonClient,
  request: { environment: string } | { computer?: Omit<ComputeEnvironmentDefinition["computer"], "lifecycle">; contents?: EnvironmentContents; env?: Record<string, string> },
): Promise<WorkSession> {
  const body = "environment" in request
    ? request
    : { ...request, computer: { ...(request.computer ?? {}), lifecycle: "ephemeral" } };
  return client.request("POST", "/sessions", body);
}

export function closeWorkSession(client: ComputeDaemonClient, session: string): Promise<WorkSession> {
  return client.request("DELETE", `/sessions/${encodeURIComponent(session)}`);
}

export function listWorkSessions(client: ComputeDaemonClient, environment?: string): Promise<WorkSession[]> {
  return client.request("GET", environment ? `/sessions?environment=${encodeURIComponent(environment)}` : "/sessions");
}

/** The address of the Compute control plane in Work mode for an environment: what Attn opens for "Work on this". */
export function workModeUrl(endpoint: string, environment: string): string {
  return `${endpoint.replace(/\/$/, "")}/#/work/${encodeURIComponent(environment)}`;
}

// ---- Software: proposals, versions, and rollouts ------------------------------

export interface OperationStep {
  name: string;
  status: "pending" | "running" | "succeeded" | "failed" | "skipped";
  detail?: string;
  job_id?: string;
  execution_id?: string;
  at?: string;
}

export interface ProjectAssembly {
  repository?: RepositorySpec;
  project?: ProjectSpec & { checks?: string[] };
  packages?: PackageSpec[];
  processes?: ProcessSpec[];
}

/** What Compute proposes after inspecting a project's source in a computer. */
export interface ProjectProposal {
  name: string;
  runtime?: string;
  assembly: ProjectAssembly;
  services?: ProcessSpec[];
  config?: Record<string, string>;
  notes?: string[];
  evidence?: string[];
}

export interface VersionRecord {
  version_id: string;
  project: string;
  version: string;
  environment: string;
  environment_id: string;
  commit?: string;
  package_digest?: string;
  assembly: ProjectAssembly;
  config_keys?: string[];
  status: "publishing" | "published" | "failed";
  steps: OperationStep[];
  created_by: string;
  created_at: string;
  completed_at?: string;
  failure?: string;
}

export interface RolloutRecord {
  rollout_id: string;
  kind: "deploy" | "promote" | "rollback";
  project: string;
  environment: string;
  environment_id: string;
  version_id: string;
  version: string;
  previous_version?: string;
  from_environment?: string;
  status: "applying" | "active" | "failed" | "superseded";
  steps: OperationStep[];
  contents_generation: number;
  created_by: string;
  created_at: string;
  completed_at?: string;
  failure?: string;
}

export interface SoftwareSummary {
  project: string;
  latest_version?: string;
  latest_status?: VersionRecord["status"];
  environments: {
    environment: string;
    environment_id: string;
    computer: ComputerStatus;
    target?: string;
    revision: string;
    commit?: string;
    version?: string;
    rollout?: RolloutRecord["status"];
    converged: boolean;
    processes: [string, string][];
  }[];
}

export interface PromotionPlan {
  project: string;
  from: string;
  to: string;
  version: string;
  version_id: string;
  from_healthy: boolean;
  to_current?: string;
  changes: string[];
  config_only_in_from: string[];
  config_only_in_to: string[];
  config_different: string[];
  authority: string;
  approvals: string[];
  expected_generation: number;
}

const software = (project: string, rest = "") => `/software/${encodeURIComponent(project)}${rest}`;

/** Inspect a project's source inside the environment's computer and propose how to run it. Nothing changes. */
export function proposeProject(client: ComputeDaemonClient, environment: string, url: string, options: { revision?: string; name?: string } = {}): Promise<ProjectProposal> {
  return client.request("POST", path(environment, "/propose"), { url, ...options });
}

/** Every project, where it runs, and at which version. */
export function listSoftware(client: ComputeDaemonClient): Promise<SoftwareSummary[]> {
  return client.request("GET", "/software");
}

export function getSoftware(client: ComputeDaemonClient, project: string): Promise<SoftwareSummary & { versions: VersionRecord[]; rollouts: RolloutRecord[]; next_version: string }> {
  return client.request("GET", software(project));
}

/** Publish a version: build, tests, checks, and a source package, in the environment it is developed in. */
export function publishVersion(client: ComputeDaemonClient, project: string, environment: string, version?: string): Promise<VersionRecord> {
  return client.request("POST", software(project, "/versions"), { environment, ...(version ? { version } : {}) });
}

export function getVersion(client: ComputeDaemonClient, project: string, version: string): Promise<VersionRecord> {
  return client.request("GET", software(project, `/versions/${encodeURIComponent(version)}`));
}

export function deployVersion(client: ComputeDaemonClient, project: string, environment: string, version: string, expectedGeneration?: number): Promise<RolloutRecord> {
  return client.request("POST", software(project, "/deploy"), {
    environment, version, ...(expectedGeneration === undefined ? {} : { expected_generation: expectedGeneration }),
  });
}

/** What promoting would change, for review before GO. */
export function promotionPlan(client: ComputeDaemonClient, project: string, from: string, to: string): Promise<PromotionPlan> {
  return client.request("GET", software(project, `/promotion?from=${encodeURIComponent(from)}&to=${encodeURIComponent(to)}`));
}

export function promoteVersion(client: ComputeDaemonClient, project: string, from: string, to: string, expectedGeneration?: number): Promise<RolloutRecord> {
  return client.request("POST", software(project, "/promote"), {
    from, to, ...(expectedGeneration === undefined ? {} : { expected_generation: expectedGeneration }),
  });
}

export function rollbackVersion(client: ComputeDaemonClient, project: string, environment: string, version?: string): Promise<RolloutRecord> {
  return client.request("POST", software(project, "/rollback"), { environment, ...(version ? { version } : {}) });
}

export function getRollout(client: ComputeDaemonClient, rollout: string): Promise<RolloutRecord> {
  return client.request("GET", `/rollouts/${encodeURIComponent(rollout)}`);
}

export function restartProcess(client: ComputeDaemonClient, environment: string, process: string): Promise<ComputerView> {
  return client.request("POST", path(environment, `/processes/${encodeURIComponent(process)}/restart`));
}

/** Follow a publish or a rollout until it is done; `onStep` sees each change. */
export async function waitForOperation<T extends VersionRecord | RolloutRecord>(
  read: () => Promise<T>,
  options: { pollMs?: number; onUpdate?: (operation: T) => void } = {},
): Promise<T> {
  let last = "";
  for (;;) {
    const operation = await read();
    const seen = JSON.stringify(operation.steps);
    if (seen !== last) { last = seen; options.onUpdate?.(operation); }
    if (!["publishing", "applying"].includes(operation.status)) return operation;
    await new Promise((resolve) => setTimeout(resolve, options.pollMs ?? 250));
  }
}
