/**
 * Typed API client for the fog Rust backend.
 *
 * The Rust hyper server exposes JSON endpoints and a live SSE log stream.
 * The client uses relative paths (`/api/...`) so it works both when the app
 * is embedded and served by the Rust server at the root of 127.0.0.1:18080,
 * and during `vite dev` (which proxies `/api` and `/logs/stream` to the Rust
 * server — see vite.config.ts).
 *
 * All types below mirror the exact JSON shapes emitted by the Rust worker
 * (source of truth). Field names are snake_case as serialized by serde.
 * Optional sections are `null` when unset — never absent, never undefined.
 * Unknown `/api/*` paths return 404 with `{"error":"not found"}`.
 *
 * This module is intentionally free of UI / React concerns so it can grow
 * mutation endpoints (POST/PATCH) later without churn. React Query hooks that
 * wrap these functions live in `@/lib/hooks`.
 */

/** Status as reported by docker for a listed service (always "running"). */
export type ServiceStatus = "running" | (string & {});

/** One entry of GET /api/services. */
export interface Service {
  /** Project / script name the service belongs to. */
  project: string;
  /** Git worktree the service was started in (empty string when none). */
  worktree: string;
  /** Service name. */
  service: string;
  /**
   * Docker container name (e.g. `redfox-main-api-1`). `/logs/stream` streams a
   * container's logs by this name, so the picker must pass `container` (not the
   * compose service name) when subscribing. For native fog services `container`
   * is `fog-<pid>-<service>` and `pid` is set.
   */
  container: string;
  /** Docker-reported status — always "running" for listed services. */
  status: ServiceStatus;
  /** Externally reachable URL (e.g. http://main.acme:8080), null when none. */
  url: string | null;
  /** Exposed ports (empty array when none). */
  ports: string[];
  /** Free-form health detail from docker ("unknown" until real health check). */
  health: string;
  /** Fog PID for native services; when present logs stream via `?pid=&service=` instead of docker. */
  pid?: number | null;
}

/** Per-service health inside a GET /api/status instance. */
export interface InstanceServiceStatus {
  /** Service name. */
  name: string;
  /** Whether the service process is running. */
  running: boolean;
  /** Health detail, null when unset. */
  health: string | null;
}

/** One IPC instance in the GET /api/status snapshot. */
export interface InstanceStatus {
  /** Process id of the instance. */
  pid: number;
  /** Script the instance was started with. */
  script: string;
  /** Git project identity (repo name) of the instance, if reported. */
  project?: string | null;
  /** Branch the instance serves, if reported. */
  branch?: string | null;
  /** Services spawned by this instance. */
  services: InstanceServiceStatus[];
}

/** IPC status snapshot returned by GET /api/status. */
export interface StatusSnapshot {
  instances: InstanceStatus[];
}

/** Sidebar layout section of the fog config (null when unset). */
export interface SidebarConfig {
  min_width: number;
  max_width: number;
}

/** dnsmasq section of the fog config (null when unset). */
export interface DnsmasqConfig {
  /** Domains resolved via the dnsmasq instance. */
  domains: string[];
  /** Address dnsmasq binds / resolves to. */
  address: string;
  /** Port dnsmasq listens on. */
  port: number;
}

/** router section of the fog config (null when unset). */
export interface RouterConfig {
  /** Name of the shared docker network. */
  shared_network: string;
  /** Port the index/landing page is served on. */
  index_port: number;
  /** Whether TLS is enabled for router-managed routes. */
  tls_enabled: boolean;
}

/** Fog config summary returned by GET /api/config. */
export interface FogConfig {
  /** Names of the available scripts. */
  scripts: string[];
  /** Max log scrollback lines, null when unset (unlimited?). */
  max_scrollback: number | null;
  /** Sidebar layout options, null when unset. */
  sidebar: SidebarConfig | null;
  /** Whether dark theme is enabled. */
  theme: boolean;
  /** dnsmasq integration options, null when unset. */
  dnsmasq: DnsmasqConfig | null;
  /** Router options, null when unset. */
  router: RouterConfig | null;
}

/** Response envelope of GET /api/config. */
export interface ConfigResponse {
  config: FogConfig;
}

/** One entry of GET /api/health. */
export interface HealthItem {
  /** Process id of the instance running the service. */
  pid: number;
  /** Script the instance was started with. */
  script: string;
  /** Git project identity (repo name) of the instance, if reported. */
  project?: string | null;
  /** Branch the instance serves, if reported. */
  branch?: string | null;
  /** Service name. */
  service: string;
  /** Whether the service process is running. */
  running: boolean;
  /** Health detail ("healthy"/"unhealthy"/…), null when unset. */
  health: string | null;
}

/** Response envelope of GET /api/health. */
export interface HealthResponse {
  health: HealthItem[];
}

/** Service control actions accepted by POST /api/instances/{pid}/services/{name}/action. */
export type ServiceAction = "start" | "stop" | "restart";

/** Result of a service control action (200 even when ok is false). */
export interface ServiceActionResult {
  ok: boolean;
  reason?: string;
}

/** Kill an entire fog instance (sends graceful shutdown over IPC). */
export async function postKillInstance(pid: number): Promise<{ ok: boolean }> {
  return postJson<{ ok: boolean }>(`/api/instances/${pid}/kill`, {});
}

export class ApiError extends Error {
  readonly status: number;
  constructor(status: number, message: string) {
    super(message);
    this.name = "ApiError";
    this.status = status;
  }
}

/** Parse a non-2xx response into a descriptive ApiError (reads {"error"} body). */
async function parseErrorResponse(
  res: Response,
  path: string
): Promise<ApiError> {
  let detail = res.statusText;
  try {
    const body = await res.json();
    if (typeof body?.error === "string") detail = body.error;
    else if (typeof body?.message === "string") detail = body.message;
  } catch {
    // ignore non-JSON error bodies
  }
  return new ApiError(res.status, detail || `Request to ${path} failed`);
}

/** Shared GET helper: resolves JSON or throws a descriptive ApiError. */
async function fetchJson<T>(path: string, init?: RequestInit): Promise<T> {
  let res: Response;
  try {
    res = await fetch(path, {
      headers: { Accept: "application/json", ...init?.headers },
      ...init,
    });
  } catch (cause) {
    throw new Error(`Network error fetching ${path}: ${String(cause)}`);
  }
  if (!res.ok) throw await parseErrorResponse(res, path);
  return (await res.json()) as T;
}

/** Shared POST helper: sends a JSON body and resolves JSON or throws a descriptive ApiError. */
async function postJson<T>(path: string, body: unknown): Promise<T> {
  let res: Response;
  try {
    res = await fetch(path, {
      method: "POST",
      headers: {
        Accept: "application/json",
        "Content-Type": "application/json",
      },
      body: JSON.stringify(body),
    });
  } catch (cause) {
    throw new Error(`Network error posting ${path}: ${String(cause)}`);
  }
  if (!res.ok) throw await parseErrorResponse(res, path);
  return (await res.json()) as T;
}

/** List of running services and their status. */
export function fetchServices(): Promise<Service[]> {
  return fetchJson<Service[]>("/api/services");
}

/** IPC status snapshot. */
export function fetchStatus(): Promise<StatusSnapshot> {
  return fetchJson<StatusSnapshot>("/api/status");
}

/** Fog config summary. */
export function fetchConfig(): Promise<ConfigResponse> {
  return fetchJson<ConfigResponse>("/api/config");
}

/** Per-service health results. */
export function fetchHealth(): Promise<HealthResponse> {
  return fetchJson<HealthResponse>("/api/health");
}

/**
 * Send a service control action (start/stop/restart) to a running fog instance.
 *
 * The backend responds 200 with `{ok:false, reason}` even when the action
 * could not be applied, and throws 400/404 with `{"error":...}` for invalid
 * actions / unknown instances — those surface as ApiError via `postJson`.
 */
export function postServiceAction(
  pid: number,
  name: string,
  action: ServiceAction
): Promise<ServiceActionResult> {
  return postJson<ServiceActionResult>(
    `/api/instances/${pid}/services/${encodeURIComponent(name)}/action`,
    { action }
  );
}

/** One git worktree (or the main checkout) of a launchable project. */
export interface LaunchWorktree {
  /** Absolute path of the worktree. */
  path: string;
  /** Git branch name, null for the main checkout. */
  branch: string | null;
  /** Script names available to launch in this worktree. */
  scripts: string[];
}

/** A known project with launchable worktrees. */
export interface LaunchProject {
  /** Absolute path of the project root. */
  path: string;
  /** Basename of the project. */
  name: string;
  worktrees: LaunchWorktree[];
}

/** Response envelope of GET /api/launch/targets. */
export interface LaunchTargets {
  projects: LaunchProject[];
}

/** Result of POST /api/launch (200 success, or ApiError for 400/404/500). */
export interface LaunchResult {
  ok: boolean;
  /** Process id of the started instance, present when ok. */
  pid?: number;
  /** Error detail, present when the backend returned a non-ok body. */
  error?: string;
}

/** List the projects/worktrees/scripts a fog instance can be launched on. */
export function fetchLaunchTargets(): Promise<LaunchTargets> {
  return fetchJson<LaunchTargets>("/api/launch/targets");
}

/**
 * Launch a fog instance on a config dir.
 *
 * `branch` is optional: null/undefined launches the main checkout, otherwise a
 * named worktree. The backend responds 200 `{ok,pid}`, or throws 400/404/500
 * with `{"error":...}` — those surface as ApiError via `postJson`.
 */
export function postLaunch(
  configDir: string,
  script: string,
  branch?: string | null
): Promise<LaunchResult> {
  return postJson<LaunchResult>("/api/launch", {
    config_dir: configDir,
    script,
    branch: branch ?? null,
  });
}

/** Live log line delivered via the SSE stream. */
export interface LogLine {
  /** Raw text (ANSI sequences may be present). */
  text: string;
  /** Optional metadata the server may include. */
  [key: string]: unknown;
}

export interface LogStreamOptions {
  onLine: (line: LogLine, raw: MessageEvent) => void;
  onOpen?: () => void;
  onError?: (event: Event) => void;
}

/**
 * Subscribe to the live log stream for a service via EventSource (SSE) at
 * `/logs/stream?service=NAME` (docker) or `?pid=PID&service=NAME` (native fog).
 * Returns an unsubscribe function. EventSource reconnects automatically and the
 * browser fires `error` while reconnecting — callers should treat errors as
 * transient and rely on `onOpen` / line events for true data.
 */
export function subscribeLogs(
  service: string,
  { onLine, onOpen, onError, pid }: LogStreamOptions & { pid?: number | null }
): () => void {
  const params = new URLSearchParams({ service });
  if (pid != null) params.set("pid", String(pid));
  const es = new EventSource(`/logs/stream?${params.toString()}`);

  es.addEventListener("message", (ev: MessageEvent) => {
    let line: LogLine = { text: ev.data ?? "" };
    if (ev.data && ev.data.startsWith("{")) {
      try {
        line = JSON.parse(ev.data) as LogLine;
      } catch {
        line = { text: ev.data };
      }
    }
    onLine(line, ev);
  });
  if (onOpen) es.onopen = onOpen;
  if (onError) es.onerror = onError;

  return () => es.close();
}
