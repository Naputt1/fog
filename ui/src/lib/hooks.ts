/**
 * React Query hooks + a small SSE hook that wrap the fog API client
 * (`@/lib/api`). Keeping data-fetching in hooks means pages stay thin and
 * adding mutations later (post/restart/control) only requires adding a
 * `useMutation` — the api module already isolates the transport layer.
 */
import { useCallback, useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  fetchServices,
  fetchStatus,
  fetchScripts,
  fetchConfig,
  fetchHealth,
  fetchLaunchTargets,
  postLaunch,
  postKillInstance,
  postServiceAction,
  subscribeLogs,
  type LogLine,
  type ServiceAction,
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

/**
 * Service control mutation (start/stop/restart). On success invalidates the
 * `status` and `health` queries so the status table refetches the new running
 * state. Callers read `mutation.error` / `mutation.isPending` to surface state.
 */
export function useServiceAction() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({
      pid,
      name,
      action,
    }: {
      pid: number;
      name: string;
      action: ServiceAction;
    }) => postServiceAction(pid, name, action),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["status"] });
      queryClient.invalidateQueries({ queryKey: ["health"] });
    },
  });
}

/**
 * Kill an entire fog instance. On success invalidates `status` and `health`
 * so the table drops the terminated instance.
 */
export function useKillInstance() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ pid }: { pid: number }) => postKillInstance(pid),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["status"] });
      queryClient.invalidateQueries({ queryKey: ["health"] });
    },
  });
}

const MAX_LOG_LINES = 2_000;

/** Launchable projects/worktrees/scripts (cached briefly, not polled). */
export function useLaunchTargets() {
  return useQuery({
    queryKey: ["launch-targets"],
    queryFn: fetchLaunchTargets,
    staleTime: 30_000,
  });
}

/**
 * Launch mutation. On success invalidates `status` and `health` so the status
 * table refetches the new running instance. Callers read `mutation.error` /
 * `mutation.isPending` / `mutation.data` to surface state.
 */
export function useLaunch() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({
      configDir,
      script,
      branch,
    }: {
      configDir: string;
      script: string;
      branch?: string | null;
    }) => postLaunch(configDir, script, branch),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["status"] });
      queryClient.invalidateQueries({ queryKey: ["health"] });
    },
  });
}

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
