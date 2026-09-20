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

/** One declared endpoint of a service, from GET /api/services. */
export interface Endpoint {
  /** Sub-service display name. */
  name: string;
  /** Externally reachable URL, empty string when it declares no routable host. */
  url: string;
  /** Host-published port, empty string when not declared. */
  port: string;
  /** Optional PathPrefix combined with the host. */
  path_prefix?: string;
  /** Per-endpoint health state. */
  health: string;
}

/** Health reading of one endpoint, from GET /api/status. */
export interface EndpointStatus {
  /** Sub-service display name. */
  name: string;
  /** Health state (`healthy`/`unhealthy`/`starting`/...). */
  health: string;
}

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
  /**
   * Optional project icon sourced from the owning project's `fog.json`
   * (`project.icon`). Either the configured URL/data URI verbatim, or a
   * same-origin `/api/projects/{name}/icon` URL when the config points at a
   * filesystem path. Absent when unset; repeated on every service of a project.
   */
  icon?: string;
  /**
   * Declared endpoints of this service, omitted when the service
   * exposes a single implicit endpoint (the common case). Each carries its own
   * URL/port/health; the parent still has its own row.
   */
  endpoints?: Endpoint[];
}

/** Per-service health inside a GET /api/status instance. */
export interface InstanceServiceStatus {
  /** Service name. */
  name: string;
  /** Whether the service process is running. */
  running: boolean;
  /** Health detail, null when unset. */
  health: string | null;
  /** Declared endpoints and their health; omitted when none. */
  endpoints?: EndpointStatus[];
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
    const headers = new Headers(init?.headers);
    headers.set("Accept", "application/json");
    res = await fetch(path, { ...init, headers });
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
export function fetchServices(opts?: {
  withInternal?: boolean;
}): Promise<Service[]> {
  const qs = opts?.withInternal ? "?withInternal=1" : "";
  return fetchJson<Service[]>(`/api/services${qs}`);
}

/** IPC status snapshot. */
export function fetchStatus(): Promise<StatusSnapshot> {
  return fetchJson<StatusSnapshot>("/api/status");
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

/**
 * Normalizes one raw SSE `data:` payload into a {@link LogLine}.
 *
 * The fog server always sends raw log text, but a line may itself be JSON
 * (e.g. Go's `slog` JSON handler starts every line with `{`). Only treat the
 * payload as a structured envelope when it parses to an object carrying a
 * string `text` field; anything else — including JSON log lines — is kept as
 * raw text so it is displayed rather than dropped.
 */
export function parseLogLine(data: string): LogLine {
  const raw = data ?? "";
  if (raw.startsWith("{")) {
    try {
      const parsed = JSON.parse(raw) as { text?: unknown };
      if (parsed && typeof parsed.text === "string") {
        return parsed as LogLine;
      }
    } catch {
      // Not JSON after all: fall through and keep the raw text.
    }
  }
  return { text: raw };
}

export interface LogStreamOptions {
  onLine: (line: LogLine, raw: MessageEvent) => void;
  onOpen?: () => void;
  onError?: (event: Event) => void;
  /** Initial backfill size for the live stream (1..10000, default 500). */
  tail?: number;
}

/**
 * Subscribe to the live log stream for a service via EventSource (SSE) at
 * `/logs/stream?service=NAME[&tail=N]` (docker) or `?pid=PID&service=NAME`
 * (native fog). Returns an unsubscribe function. EventSource reconnects
 * automatically and the browser fires `error` while reconnecting — callers
 * should treat errors as transient and rely on `onOpen` / line events for
 * true data.
 */
export function subscribeLogs(
  service: string,
  {
    onLine,
    onOpen,
    onError,
    pid,
    tail,
  }: LogStreamOptions & { pid?: number | null }
): () => void {
  const params = new URLSearchParams({ service });
  if (pid != null) params.set("pid", String(pid));
  if (tail != null) params.set("tail", String(tail));
  const es = new EventSource(`/logs/stream?${params.toString()}`);

  es.addEventListener("message", (ev: MessageEvent) => {
    onLine(parseLogLine(ev.data ?? ""), ev);
  });
  if (onOpen) es.onopen = onOpen;
  if (onError) es.onerror = onError;

  return () => es.close();
}

/** One window of older log lines from GET /api/logs/history. */
export interface LogHistory {
  lines: string[];
  has_more: boolean;
}

/**
 * Fetch one window of older log lines (no follow) for scroll-up backfill.
 * `offset` skips that many newest lines (already shown); `tail` is how many
 * before that to return. Mirrors the SSE stream's service/pid addressing.
 */
export async function fetchLogHistory(
  service: string,
  opts?: { pid?: number | null; tail?: number; offset?: number }
): Promise<LogHistory> {
  const params = new URLSearchParams({ service });
  if (opts?.pid != null) params.set("pid", String(opts.pid));
  if (opts?.tail != null) params.set("tail", String(opts.tail));
  if (opts?.offset != null) params.set("offset", String(opts.offset));
  return fetchJson<LogHistory>(`/api/logs/history?${params.toString()}`);
}
