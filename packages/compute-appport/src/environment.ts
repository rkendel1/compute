import { errors } from "@appport/protocol";
import type {
  DeploymentCreateInput,
  DeploymentPromoteInput,
  DeploymentView,
  ProjectDetail,
  ProjectSummary,
  EnvironmentCreateInput,
  EnvironmentSummary,
  EnvironmentView,
  ProjectAddInput,
  ProjectView,
} from "./types.js";

/**
 * The environment operations AppPort exposes. The default implementation
 * is a client of the Compute API served by `compute start`: the same API
 * the CLI and the UI use.
 */
export interface EnvironmentApi {
  listEnvironments(): Promise<EnvironmentSummary[]>;
  inspectEnvironment(environment: string): Promise<EnvironmentView>;
  environmentStatus(environment: string): Promise<EnvironmentView>;
  createEnvironment(definition: EnvironmentCreateInput): Promise<EnvironmentView>;
  environmentLifecycle(environment: string, action: "start" | "stop" | "restart"): Promise<EnvironmentView>;
  addProject(environment: string, project: ProjectAddInput["project"]): Promise<ProjectView>;
  removeProject(environment: string, project: string): Promise<{ removed: string }>;
  listProjects(): Promise<ProjectSummary[]>;
  /** A project in one environment, or across every environment. */
  inspectProject(project: string, environment?: string): Promise<ProjectView | ProjectDetail>;
  projectLifecycle(environment: string, project: string, action: "start" | "stop" | "restart"): Promise<ProjectView>;
  inspectDeployment(deployment: string): Promise<DeploymentView>;
  createDeployment(request: DeploymentCreateInput): Promise<DeploymentView>;
  promoteDeployment(request: DeploymentPromoteInput): Promise<DeploymentView>;
}

export const DEFAULT_DAEMON_ENDPOINT = "http://127.0.0.1:8787";

/** A Compute API error: `kind` is the API's error kind, such as `not_found`. */
export class ComputeApiError extends Error {
  constructor(readonly kind: string, message: string, readonly status: number) {
    super(message);
    this.name = "ComputeApiError";
  }
}

export interface ComputeDaemonClientOptions {
  /** `http://host:port`. Defaults to $COMPUTE_DAEMON, then 127.0.0.1:8787. */
  endpoint?: string;
  /** Bearer token for daemons started with `--require-token-env`. */
  token?: string;
}

export class ComputeDaemonClient implements EnvironmentApi {
  readonly endpoint: string;
  readonly #token: string | undefined;

  constructor(options: ComputeDaemonClientOptions = {}) {
    this.endpoint = (options.endpoint ?? process.env.COMPUTE_DAEMON ?? DEFAULT_DAEMON_ENDPOINT).replace(/\/+$/, "");
    this.#token = options.token;
  }

  listEnvironments(): Promise<EnvironmentSummary[]> {
    return this.request("GET", "/environments");
  }

  inspectEnvironment(environment: string): Promise<EnvironmentView> {
    return this.request("GET", `/environments/${segment(environment)}`);
  }

  environmentStatus(environment: string): Promise<EnvironmentView> {
    return this.request("GET", `/environments/${segment(environment)}/status`);
  }

  createEnvironment(definition: EnvironmentCreateInput): Promise<EnvironmentView> {
    return this.request("POST", "/environments", definition);
  }

  environmentLifecycle(environment: string, action: "start" | "stop" | "restart"): Promise<EnvironmentView> {
    return this.request("POST", `/environments/${segment(environment)}/${action}`);
  }

  addProject(environment: string, project: ProjectAddInput["project"]): Promise<ProjectView> {
    return this.request("POST", `/environments/${segment(environment)}/projects`, project);
  }

  removeProject(environment: string, project: string): Promise<{ removed: string }> {
    return this.request("DELETE", `/environments/${segment(environment)}/projects/${segment(project)}`);
  }

  listProjects(): Promise<ProjectSummary[]> {
    return this.request("GET", "/projects");
  }

  inspectProject(project: string, environment?: string): Promise<ProjectView | ProjectDetail> {
    return environment === undefined
      ? this.request("GET", `/projects/${segment(project)}`)
      : this.request("GET", `/environments/${segment(environment)}/projects/${segment(project)}`);
  }

  projectLifecycle(environment: string, project: string, action: "start" | "stop" | "restart"): Promise<ProjectView> {
    return this.request("POST", `/environments/${segment(environment)}/projects/${segment(project)}/${action}`);
  }

  inspectDeployment(deployment: string): Promise<DeploymentView> {
    return this.request("GET", `/deployments/${segment(deployment)}`);
  }

  createDeployment(request: DeploymentCreateInput): Promise<DeploymentView> {
    return this.request("POST", "/deployments", request);
  }

  promoteDeployment(request: DeploymentPromoteInput): Promise<DeploymentView> {
    return this.request("POST", "/deployments/promote", request);
  }

  async request<T>(method: string, path: string, body?: unknown): Promise<T> {
    const headers: Record<string, string> = { "content-type": "application/json" };
    if (this.#token) headers.authorization = `Bearer ${this.#token}`;
    let response: Response;
    try {
      response = await fetch(`${this.endpoint}${path}`, {
        method,
        headers,
        ...(body === undefined ? {} : { body: JSON.stringify(body) }),
      });
    } catch (error) {
      throw new ComputeApiError(
        "daemon_unavailable",
        `cannot reach the Compute daemon at ${this.endpoint}: ${(error as Error).message}; start it with \`compute start\``,
        0,
      );
    }
    const text = await response.text();
    const value = text ? JSON.parse(text) as unknown : undefined;
    if (!response.ok) {
      const detail = (value ?? {}) as { kind?: string; message?: string };
      throw new ComputeApiError(detail.kind ?? "invalid", detail.message ?? `HTTP ${response.status}`, response.status);
    }
    return value as T;
  }
}

function segment(value: string): string {
  return encodeURIComponent(value);
}

/**
 * Carry a Compute API refusal across AppPort with its meaning intact:
 * `not_found`, `conflict`, `unauthorized`, and `admission_denied` keep
 * distinct AppPort codes instead of collapsing into an internal error.
 */
export function toAppPortError(error: unknown): unknown {
  if (!(error instanceof ComputeApiError)) return error;
  const details = { compute_kind: error.kind };
  switch (error.kind) {
    case "not_found": return errors.notFound(error.message, details);
    case "conflict": return errors.conflict(error.message, details);
    case "unauthorized": return errors.forbidden(error.message, details);
    case "admission_denied": return errors.forbidden(error.message, details);
    case "invalid": return errors.invalidInput(error.message, details);
    case "daemon_unavailable": return errors.internal(error.message, { cause: error });
    default: return errors.internal(error.message, { cause: error });
  }
}
