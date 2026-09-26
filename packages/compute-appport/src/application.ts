import { spawn } from "node:child_process";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { errors } from "@appport/protocol";
import { s } from "@appport/schema";
import { applicationArtifactEvidence, poolPlacement } from "./schemas.js";

/**
 * Applications as AppPort capabilities: deploy, status, logs, history,
 * rollback, and stop. The contract is the application's: its identity,
 * version, provider, runtime, endpoint, status, and evidence. Where
 * Compute runs it, and how it reaches that provider, stay in Compute:
 * placement chooses among the caller's pool, exactly as `compute deploy`
 * does.
 */

const applicationName = s.string({ pattern: "^[A-Za-z0-9._-]{1,64}$" });
const digest = s.string({ pattern: "^sha256:[0-9a-f]{64}$" });
const deploymentId = s.string({ pattern: "^dep_[0-9a-f]+$" });
const providerId = s.string({ pattern: "^[A-Za-z0-9_-]{1,64}$" });
const timestamp = s.string();
const versionState = s.enum(["active", "stopped", "superseded", "deploying", "failed", "rolled_back"] as const);

export const applicationDeployInputSchema = s.object({
  /**
   * An application directory or artifact file on the AppPort host, or a
   * `file://` or `http(s)://` artifact URL, which the selected provider
   * fetches and verifies.
   */
  application: s.optional(s.string({ minLength: 1 })),
  /** Or: the artifact (`compute.application-artifact@1`) itself. */
  artifact: s.optional(s.object({ data: s.array(s.integer({ minimum: 0, maximum: 255 })) })),
  /** A pool provider to deploy to. Placement chooses when omitted. */
  provider: s.optional(providerId),
  /** The application's configuration, replacing the current one. */
  env: s.optional(s.record(s.string())),
});

export const applicationSelectorSchema = s.object({ application: applicationName });

export const applicationRollbackInputSchema = s.object({
  application: applicationName,
  /** A version (`3`, `v3`) or a deployment ID. */
  version: s.string({ pattern: "^(v?[0-9]+|dep_[0-9a-f]+)$" }),
});

/** One version of an application. */
export const applicationVersionSchema = s.object({
  application: applicationName,
  version: s.integer({ minimum: 1 }),
  deployment_id: deploymentId,
  state: versionState,
  active: s.boolean(),
  rollback_of: s.optional(s.integer({ minimum: 1 })),
  endpoint: s.optional(s.string()),
  runtime: s.optional(s.string()),
  runtime_version: s.optional(s.string()),
  placement: s.optional(poolPlacement),
  artifact: s.optional(applicationArtifactEvidence),
  failure: s.optional(s.string()),
  receipt: s.optional(digest),
  execution_receipts: s.array(digest),
  created_at: timestamp,
  completed_at: s.optional(timestamp),
});

/** An application as it stands, and the version it serves. */
export const applicationStatusSchema = s.object({
  application_id: digest,
  application: applicationName,
  status: s.string(),
  provider: providerId,
  endpoint: s.optional(s.string()),
  version: s.optional(s.integer({ minimum: 1 })),
  deployment_id: s.optional(deploymentId),
  runtime: s.optional(s.string()),
  receipt: s.optional(digest),
  artifact: s.optional(applicationArtifactEvidence),
  deploying: s.optional(s.integer({ minimum: 1 })),
});

/** A release: the version it created, where, and its evidence. */
export const applicationReleaseSchema = s.object({
  application_id: digest,
  application: applicationName,
  deployment_id: deploymentId,
  version: s.integer({ minimum: 1 }),
  provider: providerId,
  runtime: s.optional(s.string()),
  runtime_version: s.optional(s.string()),
  endpoint: s.optional(s.string()),
  status: s.string(),
  active: s.boolean(),
  rollback_of: s.optional(s.integer({ minimum: 1 })),
  receipt: s.optional(digest),
  artifact: s.optional(applicationArtifactEvidence),
  placement: s.optional(poolPlacement),
});

export const applicationHistorySchema = s.array(applicationVersionSchema);
export const applicationLogsSchema = s.object({ stdout: s.string(), stderr: s.string() });

export type ApplicationDeployInput = {
  application?: string | undefined;
  artifact?: { data: number[] } | undefined;
  provider?: string | undefined;
  env?: Record<string, string> | undefined;
};
export type ApplicationVersion = Record<string, unknown>;
export type ApplicationStatus = Record<string, unknown>;
export type ApplicationRelease = Record<string, unknown>;
export type ApplicationLogs = { stdout: string; stderr: string };

/** The application operations AppPort exposes. */
export interface ApplicationApi {
  deploy(input: ApplicationDeployInput): Promise<ApplicationRelease>;
  status(application: string): Promise<ApplicationStatus>;
  logs(application: string): Promise<ApplicationLogs>;
  history(application: string): Promise<ApplicationVersion[]>;
  rollback(application: string, version: string): Promise<ApplicationRelease>;
  stop(application: string): Promise<ApplicationStatus>;
}

export interface ComputeApplicationsOptions {
  computeBinary?: string;
  cwd?: string;
  /** Caller-owned provider pool configuration (TOML). */
  poolConfig?: string;
  /** Capability cache used by pool discovery. */
  capabilityCache?: string;
  /**
   * Variables the Compute CLI may read, such as the pool's `token_env`
   * credentials. Nothing else from this process's environment is passed.
   */
  environment?: Record<string, string>;
}

/** A refusal from Compute, with its meaning. */
export class ApplicationError extends Error {
  constructor(readonly kind: "not_found" | "no_compatible_provider" | "invalid" | "failed", message: string) {
    super(message);
    this.name = "ApplicationError";
  }
}

/**
 * The default implementation: `compute application … --json`, so placement,
 * provider discovery, and credentials work exactly as they do for
 * `compute deploy`.
 */
export class ComputeApplications implements ApplicationApi {
  readonly computeBinary: string;
  readonly cwd: string | undefined;
  readonly #options: ComputeApplicationsOptions;

  constructor(options: ComputeApplicationsOptions = {}) {
    this.computeBinary = options.computeBinary ?? "compute";
    this.cwd = options.cwd;
    this.#options = options;
  }

  async deploy(input: ApplicationDeployInput): Promise<ApplicationRelease> {
    if ((input.application === undefined) === (input.artifact === undefined)) {
      throw new ApplicationError("invalid", "give exactly one of application or artifact");
    }
    const options = [
      ...(input.provider ? ["--provider", input.provider] : []),
      ...Object.entries(input.env ?? {}).map(([name, value]) => `--set=${name}=${value}`),
    ];
    const deploy = async (target: string) =>
      release(await this.invoke(["application", "deploy", target, ...options, ...this.#pool(), "--json"]));
    if (input.application !== undefined) return deploy(input.application);
    const directory = await mkdtemp(join(tmpdir(), "compute-appport-application-"));
    try {
      const path = join(directory, "application.capp");
      await writeFile(path, Uint8Array.from(input.artifact!.data));
      return await deploy(path);
    } finally {
      await rm(directory, { recursive: true, force: true });
    }
  }

  async status(application: string): Promise<ApplicationStatus> {
    return status(await this.invoke(["application", "status", application, ...this.#pool(), "--json"]));
  }

  async logs(application: string): Promise<ApplicationLogs> {
    const value = await this.invoke(["application", "logs", application, ...this.#pool(), "--json"]);
    return { stdout: String(value.stdout ?? ""), stderr: String(value.stderr ?? "") };
  }

  async history(application: string): Promise<ApplicationVersion[]> {
    const value = await this.invoke(["application", "history", application, ...this.#pool(), "--json"]);
    return (value as unknown as Record<string, unknown>[]).map(version);
  }

  async rollback(application: string, target: string): Promise<ApplicationRelease> {
    return release(await this.invoke(["application", "rollback", application, target, ...this.#pool(), "--json"]));
  }

  async stop(application: string): Promise<ApplicationStatus> {
    return status(await this.invoke(["application", "stop", application, ...this.#pool(), "--json"]));
  }

  #pool(): string[] {
    const base = this.cwd ?? process.cwd();
    return [
      ...(this.#options.poolConfig ? ["--pool-config", resolve(base, this.#options.poolConfig)] : []),
      ...(this.#options.capabilityCache ? ["--capability-cache", resolve(base, this.#options.capabilityCache)] : []),
    ];
  }

  invoke(args: string[]): Promise<Record<string, unknown>> {
    const env: NodeJS.ProcessEnv = {};
    for (const key of ["PATH", "HOME", "SystemRoot", "WINDIR", "PATHEXT"]) {
      if (process.env[key]) env[key] = process.env[key];
    }
    Object.assign(env, this.#options.environment ?? {});
    return new Promise((resolvePromise, reject) => {
      const child = spawn(this.computeBinary, args, { cwd: this.cwd, env, stdio: ["ignore", "pipe", "pipe"] });
      let stdout = "";
      let stderr = "";
      child.stdout.setEncoding("utf8").on("data", (chunk: string) => { stdout += chunk; });
      child.stderr.setEncoding("utf8").on("data", (chunk: string) => { stderr += chunk; });
      child.once("error", reject);
      child.once("close", (code) => {
        if (code === 0) {
          try {
            resolvePromise(JSON.parse(stdout) as Record<string, unknown>);
          } catch {
            reject(new ApplicationError("failed", "Compute returned no application result"));
          }
          return;
        }
        reject(classify(stderr.trim() || `compute exited with status ${code}`));
      });
    });
  }
}

function classify(message: string): ApplicationError {
  const text = message.replace(/^runtime error: /, "");
  if (/is not deployed on any provider|has no version|not found/i.test(text)) {
    return new ApplicationError("not_found", text);
  }
  if (/No compatible provider found/.test(text)) return new ApplicationError("no_compatible_provider", text);
  if (/requires configuration it was not given|is not an application|is deployed on|give exactly one/.test(text)) {
    return new ApplicationError("invalid", text);
  }
  return new ApplicationError("failed", text);
}

/** Carry an application refusal across AppPort with its meaning intact. */
export function applicationError(error: unknown): unknown {
  if (!(error instanceof ApplicationError)) return error;
  const details = { compute_kind: error.kind };
  switch (error.kind) {
    case "not_found": return errors.notFound(error.message, details);
    case "no_compatible_provider": return errors.conflict(error.message, details);
    case "invalid": return errors.invalidInput(error.message, details);
    default: return errors.internal(error.message, { cause: error });
  }
}

/** Keep the listed fields that are present. */
function pick(value: Record<string, unknown>, fields: string[]): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const field of fields) {
    if (value[field] !== undefined && value[field] !== null) out[field] = value[field];
  }
  return out;
}

const VERSION_FIELDS = [
  "application", "version", "deployment_id", "state", "active", "rollback_of", "endpoint", "runtime",
  "runtime_version", "placement", "artifact", "failure", "receipt", "execution_receipts", "created_at",
  "completed_at",
];

function version(value: Record<string, unknown>): ApplicationVersion {
  return pick(value, VERSION_FIELDS);
}

function status(value: Record<string, unknown>): ApplicationStatus {
  const identity = value.application as { id: string; name: string };
  const deploying = value.deploying as { version?: number } | undefined;
  return {
    ...pick(value, ["application_id", "status", "provider", "endpoint", "version", "deployment_id", "runtime", "receipt", "artifact"]),
    application_id: identity.id,
    application: identity.name,
    ...(deploying?.version !== undefined ? { deploying: deploying.version } : {}),
  };
}

function release(value: Record<string, unknown>): ApplicationRelease {
  const deployment = value.deployment as Record<string, unknown>;
  const identity = value.application as { id: string; name: string };
  return {
    ...pick(deployment, ["deployment_id", "version", "runtime", "runtime_version", "active", "rollback_of", "receipt", "artifact", "placement"]),
    ...pick(value, ["provider", "endpoint", "status"]),
    application_id: identity.id,
    application: identity.name,
  };
}
