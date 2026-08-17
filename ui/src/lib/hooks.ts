/**
 * React Query hooks + a small SSE hook that wrap the fog API client
 * (`@/lib/api`). Keeping data-fetching in hooks means pages stay thin and
 * adding mutations later (post/restart/control) only requires adding a
 * `useMutation` — the api module already isolates the transport layer.
 */
import { useCallback, useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import {
  fetchServices,
  fetchStatus,
  fetchScripts,
  fetchConfig,
  fetchHealth,
  subscribeLogs,
  type LogLine,
} from "@/lib/api";

/** Poll cadence for status-ish endpoints. */
const POLL_MS = 5_000;

/** Live services list + status. */
export function useServices() {
  return useQuery({
    queryKey: ["services"],
    queryFn: fetchServices,
    refetchInterval: POLL_MS,
  });
}

/** IPC status snapshot. */
export function useStatus() {
  return useQuery({
    queryKey: ["status"],
    queryFn: fetchStatus,
    refetchInterval: POLL_MS,
  });
}

/** Scripts configuration. */
export function useScripts() {
  return useQuery({
    queryKey: ["scripts"],
    queryFn: fetchScripts,
    staleTime: 30_000,
  });
}

/** Fog config summary. */
export function useConfig() {
  return useQuery({
    queryKey: ["config"],
    queryFn: fetchConfig,
    staleTime: 30_000,
  });
}

/** Per-service health results. */
export function useHealth() {
  return useQuery({
    queryKey: ["health"],
    queryFn: fetchHealth,
    refetchInterval: POLL_MS,
  });
}

const MAX_LOG_LINES = 2_000;

/**
 * Subscribe to the live SSE log stream for a service.
 *
 * Keeps the last `MAX_LOG_LINES` lines in memory and clears the buffer when
 * the service changes. Callers get `lines` (append-only) and `clear()`.
 */
export function useLogStream(service: string | null | undefined) {
  const [lines, setLines] = useState<LogLine[]>([]);
  const [connected, setConnected] = useState(false);

  useEffect(() => {
    if (!service) {
      setLines([]);
      setConnected(false);
      return;
    }
    setLines([]);
    setConnected(false);

    const unsubscribe = subscribeLogs(service, {
      onOpen: () => setConnected(true),
      onLine: (line) =>
        setLines((prev) =>
          prev.length >= MAX_LOG_LINES
            ? [...prev.slice(prev.length - MAX_LOG_LINES + 1), line]
            : [...prev, line]
        ),
      // EventSource fires `error` on transient reconnects too; keep connected
      // until we actually open again — the browser manages retries.
    });
    return unsubscribe;
  }, [service]);

  const clear = useCallback(() => setLines([]), []);

  return { lines, connected, clear };
}
