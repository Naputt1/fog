import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

/**
 * Host-aware URL helpers.
 *
 * When the UI is opened on localhost / 127.0.0.1 / ::1 we keep the DNS name
 * (e.g. `ui.red-fox`) so `.red-fox` dnsmasq resolution works.
 * When opened on any other host (Tailnet IP, LAN IP, public hostname) we
 * rewrite the service URL's hostname to the request host so links and redirects
 * stay on the same host and remain reachable (remote peers cannot resolve
 * `*.red-fox`).
 *
 * The port from the original service URL is preserved (e.g. raw-TCP
 * `postgres.red-fox:55274` → `192.168.1.10:55274`). Port-less router URLs
 * (`https://ui.red-fox/`) become `https://<request-host>/`.
 */
export function isLocalHost(hostname: string): boolean {
  const h = hostname.toLowerCase();
  return (
    h === "localhost" ||
    h === "127.0.0.1" ||
    h === "::1" ||
    h === "[::1]" ||
    h === "0.0.0.0"
  );
}

export function getRequestHostname(): string {
  if (typeof window === "undefined") return "";
  return window.location.hostname;
}

export function getRequestHost(): string {
  if (typeof window === "undefined") return "127.0.0.1:18080";
  return window.location.host || `${window.location.hostname}:${window.location.port}`;
}

/**
 * Extract the host-published port from a docker/native `ports` entry.
 * Handles both "0.0.0.0:HOST->CONTAINER/tcp" and "CONTAINER->0.0.0.0:HOST" shapes.
 * Returns null when no host port is found.
 */
function extractHostPort(ports: string[]): string | null {
  for (const entry of ports) {
    // Try "0.0.0.0:5432->5432/tcp" (host first) or "5432/tcp->0.0.0.0:5432" (host second)
    // The host side always contains a colon with a numeric port.
    const parts = entry.split("->");
    for (const part of parts) {
      const m = part.trim().match(/:(\d+)(?:\/|$)/);
      if (m) return m[1];
    }
    // Fallback: standalone "0.0.0.0:HOST" without arrow (native list uses "0.0.0.0:62175->62175/tcp")
    const fallback = entry.match(/:(\d+)\b/);
    if (fallback) return fallback[1];
  }
  return null;
}

/**
 * Rewrite a service URL to be host-reliant.
 * - localhost → return `url` unchanged (DNS)
 * - other host + URL has explicit port → replace hostname, keep port (raw-TCP)
 * - other host + URL has no port but service publishes a host port → use http://<host>:<publishedPort>/
 * - other host + no published port → keep DNS (Traefik-only, host:port would 404), caller can show DNS badge
 */
export function toDisplayUrl(url: string, ports: string[] = []): string {
  if (!url) return url;
  if (typeof window === "undefined") return url;
  const reqHost = window.location.hostname;
  if (!reqHost || isLocalHost(reqHost)) return url;
  try {
    const u = new URL(url);
    // Already on same host: keep original (DNS case like ui.red-fox)
    if (u.hostname === reqHost) return url;
    // Raw-TCP / explicit port: keep port, just swap hostname
    if (u.port) {
      u.hostname = reqHost;
      return u.toString();
    }
    // Port-less Traefik URL: try to use published host port so remote can reach without DNS
    const hostPort = extractHostPort(ports);
    if (hostPort) {
      // Use http since published port is plain http (container)
      return `http://${reqHost}:${hostPort}${u.pathname}${u.search}${u.hash}`;
    }
    // No published port: Traefik-only. Swapping hostname would hit catch-all index (404) because
    // Traefik routes by Host(`*.gems`). Keep DNS so at least localhost/dnsmasq works, and
    // remote shows DNS with warning. Returning DNS avoids broken host-only 404.
    return url;
  } catch {
    return url;
  }
}

/** Whether a Traefik-only service has no host port and will stay DNS-only on remote. */
export function isDnsOnly(url: string, ports: string[]): boolean {
  if (!url) return false;
  try {
    const u = new URL(url);
    if (u.port) return false;
    return extractHostPort(ports) == null;
  } catch {
    return false;
  }
}

/** Host label for the sidebar/footer (host:port of the UI itself). */
export function getHostLabel(): string {
  if (typeof window === "undefined") return "127.0.0.1:18080";
  return window.location.host || "127.0.0.1:18080";
}
