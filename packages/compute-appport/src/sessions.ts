import { spawn } from "node:child_process";

/**
 * Compute sessions for programs and agents: a temporary, authorized,
 * durable computer.
 *
 * ```ts
 * const compute = createComputeSessions({ poolConfig: "compute-pool.toml" });
 * const session = await compute.create({ resources: { cpu: 2, memory: "2Gi" }, ttl: "1h" });
 * await session.exec(["make", "test"]);
 * await session.logs();
 * await session.destroy();
 * ```
 *
 * This is a thin client of `compute session`: placement, authorization,
 * the durable lifecycle, and execution all happen in Compute. Every `exec`
 * is a durable job with its own job and execution IDs and a verifiable
 * receipt; this module keeps no state of its own.
 */

export type SessionStatus =
  | "requested"
  | "provisioning"
  | "ready"
  | "running"
  | "stopping"
  | "stopped"
  | "resuming"
  | "expiring"
  | "destroying"
  | "destroyed"
  | "expired"
  | "failed";

export interface SessionCapabilities {
  exec: boolean;
  terminal: boolean;
  filesystem: boolean;
  network: boolean;
  public_endpoint: boolean;
  persistent_storage: boolean;
  suspend: boolean;
  resume: boolean;
  claim: boolean;
}

export interface SessionFailure {
  phase: string;
  provider?: string;
  code: string;
  message: string;
  retryable: boolean;
  at: string;
}

/** The authoritative session record, as Compute returns it. */
export interface SessionRecord {
  version: string;
  session_id: string;
  status: SessionStatus;
  node_id: string;
  provider_kind: string;
  job_id: string;
  execution_id: string;
  ownership: "ephemeral" | "claimed";
  resources: { cpu_count?: number; memory_bytes?: number; disk_bytes?: number };
  network: "none" | "localhost" | "network";
  capabilities: SessionCapabilities;
  connection?: { mode: string; address?: string; port?: number; details?: Record<string, string> };
  endpoints?: { id: string; protocol: string; address: string; port: number; public: boolean; expires_at?: string }[];
  placement_id?: string;
  created_at: string;
  updated_at: string;
  expires_at?: string;
  ready_at?: string;
  ended_at?: string;
  generation: number;
  failure?: SessionFailure;
  executions?: { job_id: string; execution_id: string; purpose: string; command?: string[]; status: string }[];
}

export interface SessionCreateOptions {
  resources?: { cpu?: number; memory?: string; disk?: string };
  /** `30m`, `1h`, `2d`. Defaults to an hour. */
  ttl?: string;
  network?: "none" | "localhost" | "network";
  isolation?: "process" | "sandboxed" | "strict";
  /** Capabilities the environment must offer. */
  require?: (keyof SessionCapabilities)[];
  /** `PORT[/PROTOCOL][:public]`; each one is authorized separately. */
  expose?: string[];
  /** Name the provider; placement chooses when omitted. */
  provider?: string;
  /** Wait until the session is ready. Defaults to true. */
  wait?: boolean;
}

export interface SessionExecOptions {
  env?: Record<string, string>;
  /** `10s`, `1m`. */
  timeout?: string;
}

/** The outcome of one command: a durable job and its execution. */
export interface SessionExecResult {
  jobId: string;
  executionId: string;
  status: string;
  exitCode: number | null;
  stdout: string;
  stderr: string;
  /** The full execution result, including its receipt. */
  result: Record<string, unknown>;
}

export interface SessionLogs {
  session_id: string;
  executions: {
    job_id: string;
    execution_id: string;
    purpose: string;
    command?: string[];
    status: string;
    stdout: string;
    stderr: string;
    complete: boolean;
  }[];
  environment?: string;
}

export interface SessionConnection {
  session_id: string;
  connection: { mode: string; address?: string; port?: number; details?: Record<string, string> };
  command?: string[];
  /** Short-lived, connection-scoped material. Never stored by Compute. */
  credentials?: Record<string, string>;
  expires_at?: string;
}

export interface ComputeSessionsOptions {
  /** The `compute` executable. Defaults to `compute` on PATH. */
  computeBinary?: string;
  poolConfig?: string;
  capabilityCache?: string;
  cwd?: string;
  /** Extra environment for the `compute` process (for example, the token variables the pool names). */
  env?: Record<string, string>;
}

/** A session failure reported by Compute: `code` is its error kind. */
export class ComputeSessionError extends Error {
  constructor(readonly code: string, message: string, readonly status: number | null) {
    super(message);
    this.name = "ComputeSessionError";
  }
}

interface CommandResult {
  status: number | null;
  stdout: string;
  stderr: string;
}

export class ComputeSessions {
  readonly #options: ComputeSessionsOptions;

  constructor(options: ComputeSessionsOptions = {}) {
    this.#options = options;
  }

  async create(options: SessionCreateOptions = {}): Promise<ComputeSessionHandle> {
    const args = ["session", "create", "--json"];
    if (options.resources?.cpu !== undefined) args.push("--cpu", String(options.resources.cpu));
    if (options.resources?.memory) args.push("--memory", options.resources.memory);
    if (options.resources?.disk) args.push("--disk", options.resources.disk);
    if (options.ttl) args.push("--ttl", options.ttl);
    if (options.network) args.push("--network", options.network);
    if (options.isolation) args.push("--isolation", options.isolation);
    for (const capability of options.require ?? []) args.push("--require", capability);
    for (const endpoint of options.expose ?? []) args.push("--expose", endpoint);
    if (options.provider) args.push("--provider", options.provider);
    if (options.wait ?? true) args.push("--wait");
    const created = await this.json<{ provider_id: string; session: SessionRecord }>(args);
    return new ComputeSessionHandle(this, created.session, created.provider_id);
  }

  async get(sessionId: string): Promise<ComputeSessionHandle> {
    return new ComputeSessionHandle(this, await this.json<SessionRecord>(["session", "info", sessionId, "--json"]));
  }

  async list(): Promise<{ provider_id: string; session: SessionRecord }[]> {
    return this.json(["session", "list", "--json"]);
  }

  /** @internal */
  async json<T>(args: string[]): Promise<T> {
    const result = await this.invoke(args);
    const value = parseJson<T>(result.stdout);
    if (result.status !== 0 || value === undefined) {
      throw commandError(result);
    }
    return value;
  }

  /** @internal */
  async invoke(args: string[]): Promise<CommandResult> {
    const options = this.#options;
    const [scope = "", command = ""] = args;
    const location: string[] = [];
    if (options.poolConfig) location.push("--pool-config", options.poolConfig);
    if (options.capabilityCache) location.push("--capability-cache", options.capabilityCache);
    // Pool options go right after the subcommand, never after `--`.
    const full = [scope, command, ...location, ...args.slice(2)];
    return new Promise((resolvePromise, reject) => {
      const child = spawn(options.computeBinary ?? "compute", full, {
        cwd: options.cwd,
        env: { ...process.env, ...options.env },
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

/** One session. Every method asks Compute; the record is refreshed from its answers. */
export class ComputeSessionHandle {
  #record: SessionRecord;

  constructor(
    readonly sessions: ComputeSessions,
    record: SessionRecord,
    readonly providerId?: string,
  ) {
    this.#record = record;
  }

  get id(): string { return this.#record.session_id; }
  get record(): SessionRecord { return this.#record; }

  async info(): Promise<SessionRecord> {
    this.#record = await this.sessions.json<SessionRecord>(["session", "info", this.id, "--json"]);
    return this.#record;
  }

  /** Run a command as a durable job and wait for it. A non-zero exit is a result, not an exception. */
  async exec(command: string[], options: SessionExecOptions = {}): Promise<SessionExecResult> {
    if (command.length === 0) throw new ComputeSessionError("invalid_command", "a command needs a program", null);
    const args = ["session", "exec", this.id, "--json"];
    for (const [key, value] of Object.entries(options.env ?? {})) args.push("--env", `${key}=${value}`);
    if (options.timeout) args.push("--timeout", options.timeout);
    const outcome = await this.sessions.invoke([...args, "--", ...command]);
    const result = parseJson<Record<string, unknown>>(outcome.stdout);
    if (!result || typeof result.execution_id !== "string") {
      throw commandError(outcome);
    }
    const stream = (name: "stdout" | "stderr") => {
      const value = result[name] as { text?: string } | undefined;
      return value?.text ?? "";
    };
    return {
      jobId: String(result.job_id),
      executionId: result.execution_id,
      status: String(result.status),
      exitCode: typeof result.exit_code === "number" ? result.exit_code : null,
      stdout: stream("stdout"),
      stderr: stream("stderr"),
      result,
    };
  }

  logs(): Promise<SessionLogs> {
    return this.sessions.json(["session", "logs", this.id, "--json"]);
  }

  connect(): Promise<SessionConnection> {
    return this.sessions.json(["session", "connect", this.id, "--json"]);
  }

  stop(): Promise<SessionRecord> { return this.lifecycle("stop"); }
  resume(): Promise<SessionRecord> { return this.lifecycle("resume"); }
  claim(): Promise<SessionRecord> { return this.lifecycle("claim"); }
  destroy(): Promise<SessionRecord> { return this.lifecycle("destroy"); }

  private async lifecycle(action: "stop" | "resume" | "claim" | "destroy"): Promise<SessionRecord> {
    this.#record = await this.sessions.json<SessionRecord>(["session", action, this.id, "--json"]);
    return this.#record;
  }
}

export function createComputeSessions(options: ComputeSessionsOptions = {}): ComputeSessions {
  return new ComputeSessions(options);
}

function commandError(result: CommandResult): ComputeSessionError {
  const message = result.stderr.trim() || result.stdout.trim() || `compute exited with ${result.status}`;
  // `compute session` reports `invalid workload: <kind>: …` or
  // `runtime error: <kind>: …`.
  const code = /^(?:invalid workload|runtime error): ([a-z_]+): /m.exec(message)?.[1] ?? "compute_failed";
  return new ComputeSessionError(code, message, result.status);
}

function parseJson<T>(text: string): T | undefined {
  try { return JSON.parse(text) as T; } catch { return undefined; }
}
