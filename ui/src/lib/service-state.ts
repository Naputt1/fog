/**
 * Service state vocabulary, mirroring `src/service_state.rs`.
 *
 * These are wire values: `/api/services`, `/api/status`, and the IPC socket
 * emit them, and the legal vocabulary lives in the Rust enum. The table below
 * is the only place a state name becomes a badge tone or dot, so adding a state
 * means adding it to both sides and letting the compiler point at the gap.
 */
export const SERVICE_STATES = [
  "pending",
  "starting",
  "running",
  "healthy",
  "unhealthy",
  "stopped",
  "failed",
  "unknown",
] as const;

export type ServiceState = (typeof SERVICE_STATES)[number];

/** Transitional states the UI shows locally while a request is in flight. */
export type LocalServiceState = "stopping" | "killing";

export type BadgeState = ServiceState | LocalServiceState;

/** Tones understood by `Badge`, matching `src/service_state.rs`'s semantic set. */
export type ServiceTone =
  | "running"
  | "healthy"
  | "starting"
  | "stopped"
  | "stopping"
  | "killing"
  | "unhealthy"
  | "unknown";

export interface ServiceStateStyle {
  tone: ServiceTone;
  /** Tailwind class for the badge's status dot. */
  dot: string;
}

/**
 * One table for both the tone and the dot, so the two cannot drift. The values
 * reproduce the palette the badge used before the table existed.
 */
export const SERVICE_STATE_STYLE: Record<BadgeState, ServiceStateStyle> = {
  running: { tone: "running", dot: "bg-primary" },
  healthy: { tone: "healthy", dot: "bg-success" },
  starting: { tone: "starting", dot: "bg-info" },
  stopped: { tone: "stopped", dot: "bg-muted-foreground" },
  stopping: { tone: "stopping", dot: "bg-warning" },
  killing: { tone: "killing", dot: "bg-warning" },
  unhealthy: { tone: "unhealthy", dot: "bg-destructive" },
  // States without a dedicated palette entry share the neutral fallback, as
  // they did before the table existed.
  pending: { tone: "unknown", dot: "bg-muted-foreground" },
  failed: { tone: "unknown", dot: "bg-muted-foreground" },
  unknown: { tone: "unknown", dot: "bg-muted-foreground" },
};

/** Style for a string that names no known state. */
export const UNKNOWN_STATE_STYLE: ServiceStateStyle =
  SERVICE_STATE_STYLE.unknown;

/** Whether a wire string names one of the Rust states. */
export function isServiceState(value: string): value is ServiceState {
  return (SERVICE_STATES as readonly string[]).includes(value);
}

/** Whether a string names a badge state, including the UI-only ones. */
export function isBadgeState(value: string): value is BadgeState {
  return Object.hasOwn(SERVICE_STATE_STYLE, value);
}

/**
 * Parses a wire string, case-insensitively. Returns `null` for anything unknown
 * so a caller must decide what an unrecognized state means instead of silently
 * rendering it as healthy.
 */
export function parseServiceState(raw: string): ServiceState | null {
  const key = raw.toLowerCase();
  return isServiceState(key) ? key : null;
}

/**
 * Normalizes a wire string for display and lookup: a known state becomes its
 * lowercase name, an unknown string is kept verbatim so the badge can still
 * show it.
 */
export function serviceStateKey(raw: string): string {
  return parseServiceState(raw) ?? raw;
}

/** The badge style for a wire string, falling back to the neutral style. */
export function serviceStateStyle(raw: string): ServiceStateStyle {
  const key = serviceStateKey(raw).toLowerCase();
  return isBadgeState(key) ? SERVICE_STATE_STYLE[key] : UNKNOWN_STATE_STYLE;
}
