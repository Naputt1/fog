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
   * compose service name) when subscribing.
   */
  container: string;
  /** Docker-reported status — always "running" for listed services. */
  status: ServiceStatus;
  /** Externally reachable URL (e.g. http://main.acme:8080), null when none. */
  url: string | null;
  /** Exposed ports (empty array when none). */
  ports: number[];
  /** Free-form health detail from docker ("unknown" until real health check). */
  health: string;
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
  /** Services spawned by this instance. */
  services: InstanceServiceStatus[];
}

/** IPC status snapshot returned by GET /api/status. */
export interface StatusSnapshot {
  instances: InstanceStatus[];
}

/** One route inside a script's proxy block. */
export interface ProxyRoute {
  /** Path prefix the route matches. */
  path: string;
  /** Host header to match, or null when the route matches any host. */
  host: string | null;
  /** Upstream target (host:port or URL). */
  upstream: string;
  /** Whether the route upgrades to WebSocket, null when unspecified. */
  ws: boolean | null;
}

/** Proxy block of a script (null when the script exposes no proxy). */
export interface ProxyConfig {
  /** Port the proxy listens on. */
  port: number;
  routes: ProxyRoute[];
}

/** Script definition returned by GET /api/scripts. */
export interface ScriptConfig {
  /** Whether services run concurrently or sequentially. */
  concurrent: boolean;
  /** Services this script starts. */
  services: string[];
  /** Proxy block, or null when the script exposes no proxy. */
  proxy: ProxyConfig | null;
}

/** Response envelope of GET /api/scripts keyed by script name. */
export interface ScriptsResponse {
  scripts: Record<string, ScriptConfig>;
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

export class ApiError extends Error {
  readonly status: number;
  constructor(status: number, message: string) {
    super(message);
    this.name = "ApiError";
    this.status = status;
  }
}

/** Shared fetch helper: resolves JSON or throws a descriptive ApiError. */
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
  if (!res.ok) {
    let detail = res.statusText;
    try {
      const body = await res.json();
      if (typeof body?.error === "string") detail = body.error;
      else if (typeof body?.message === "string") detail = body.message;
    } catch {
      // ignore non-JSON error bodies
    }
    throw new ApiError(res.status, detail || `Request to ${path} failed`);
  }
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

/** Scripts configuration keyed by script name. */
export function fetchScripts(): Promise<ScriptsResponse> {
  return fetchJson<ScriptsResponse>("/api/scripts");
}

/** Fog config summary. */
export function fetchConfig(): Promise<ConfigResponse> {
  return fetchJson<ConfigResponse>("/api/config");
}

/** Per-service health results. */
export function fetchHealth(): Promise<HealthResponse> {
  return fetchJson<HealthResponse>("/api/health");
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
 * `/logs/stream?service=NAME`. Returns an unsubscribe function. EventSource
 * reconnects automatically and the browser fires `error` while reconnecting —
 * callers should treat errors as transient and rely on `onOpen` / line events
 * for true data.
 */
export function subscribeLogs(
  service: string,
  { onLine, onOpen, onError }: LogStreamOptions
): () => void {
  const params = new URLSearchParams({ service });
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
