/**
 * Browser-local dashboard preferences.
 *
 * These tune the UI (polling, terminals, navigation) — never the fog instance.
 * One validated object lives in localStorage and is exposed through
 * `useSyncExternalStore`, so a change in Settings reaches polling, terminals
 * and the app shell without a provider.
 */
import { useSyncExternalStore } from "react";

/** Which pane a selected service opens. */
export type LogViewMode = "logs" | "terminal";

/** Poll intervals offered in Settings; `0` turns polling off. */
export const POLL_MS_OPTIONS = [2_000, 5_000, 10_000, 30_000, 0] as const;

/** Log backfill sizes offered in Settings. */
export const LOG_LINES_OPTIONS = [100, 250, 500, 1_000, 2_000] as const;

export const FONT_SIZE_MIN = 11;
export const FONT_SIZE_MAX = 18;

const STORAGE_KEY = "fog:ui:preferences";

export interface Preferences {
  /** Refetch cadence for services, status and health; `0` = off. */
  pollMs: number;
  /** Pane opened when a service is selected. */
  defaultLogView: LogViewMode;
  /** Lines backfilled before the live log stream starts. */
  initialLogLines: number;
  /** xterm font size in px. */
  terminalFontSize: number;
  /** Swipe between primary destinations on touch devices. */
  swipeNavigation: boolean;
}

export const DEFAULT_PREFERENCES: Preferences = {
  pollMs: 5_000,
  defaultLogView: "logs",
  initialLogLines: 500,
  terminalFontSize: 13,
  swipeNavigation: true,
};

function isOneOf<T>(values: readonly T[], value: unknown): value is T {
  return values.includes(value as T);
}

/** Coerce untrusted stored data (older versions, hand edits) to valid values. */
export function normalizePreferences(raw: unknown): Preferences {
  const input: Record<string, unknown> =
    typeof raw === "object" && raw !== null
      ? (raw as Record<string, unknown>)
      : {};
  const fontSize =
    typeof input.terminalFontSize === "number"
      ? Math.round(input.terminalFontSize)
      : NaN;
  return {
    pollMs: isOneOf(POLL_MS_OPTIONS, input.pollMs)
      ? input.pollMs
      : DEFAULT_PREFERENCES.pollMs,
    defaultLogView:
      input.defaultLogView === "terminal" || input.defaultLogView === "logs"
        ? input.defaultLogView
        : DEFAULT_PREFERENCES.defaultLogView,
    initialLogLines: isOneOf(LOG_LINES_OPTIONS, input.initialLogLines)
      ? input.initialLogLines
      : DEFAULT_PREFERENCES.initialLogLines,
    terminalFontSize: Number.isFinite(fontSize)
      ? Math.min(FONT_SIZE_MAX, Math.max(FONT_SIZE_MIN, fontSize))
      : DEFAULT_PREFERENCES.terminalFontSize,
    swipeNavigation:
      typeof input.swipeNavigation === "boolean"
        ? input.swipeNavigation
        : DEFAULT_PREFERENCES.swipeNavigation,
  };
}

/** True when every preference still matches its default. */
export function isDefaultPreferences(preferences: Preferences): boolean {
  return (Object.keys(DEFAULT_PREFERENCES) as (keyof Preferences)[]).every(
    (key) => preferences[key] === DEFAULT_PREFERENCES[key]
  );
}

function readStored(): Preferences {
  if (typeof window === "undefined") return DEFAULT_PREFERENCES;
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    return raw ? normalizePreferences(JSON.parse(raw)) : DEFAULT_PREFERENCES;
  } catch {
    return DEFAULT_PREFERENCES;
  }
}

function writeStored(preferences: Preferences) {
  if (typeof window === "undefined") return;
  try {
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(preferences));
  } catch {
    // Private mode or a full quota: this tab keeps the in-memory value.
  }
}

let current: Preferences | null = null;
const listeners = new Set<() => void>();

function getSnapshot(): Preferences {
  current ??= readStored();
  return current;
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

function emit() {
  for (const listener of listeners) listener();
}

/** Replace one preference, normalizing it like a stored read. */
export function setPreference<K extends keyof Preferences>(
  key: K,
  value: Preferences[K]
) {
  const previous = getSnapshot();
  const next = normalizePreferences({ ...previous, [key]: value });
  if (next[key] === previous[key]) return;
  current = next;
  writeStored(next);
  emit();
}

/** Restore every default and clear the stored entry. */
export function resetPreferences() {
  current = { ...DEFAULT_PREFERENCES };
  if (typeof window !== "undefined") {
    try {
      window.localStorage.removeItem(STORAGE_KEY);
    } catch {
      // ignore
    }
  }
  emit();
}

/** All preferences; re-renders when any of them changes. */
export function usePreferences(): Preferences {
  return useSyncExternalStore(
    subscribe,
    getSnapshot,
    () => DEFAULT_PREFERENCES
  );
}

/** One preference value. */
export function usePreference<K extends keyof Preferences>(
  key: K
): Preferences[K] {
  return usePreferences()[key];
}
