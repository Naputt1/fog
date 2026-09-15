/**
 * React Query hooks that wrap the fog API client (`@/lib/api`). Keeping
 * data-fetching in hooks means pages stay thin and adding mutations later
 * only requires adding a `useMutation` — the api module isolates transport.
 */
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  fetchServices,
  fetchStatus,
  fetchHealth,
  fetchLaunchTargets,
  postLaunch,
  postKillInstance,
  postServiceAction,
  type ServiceAction,
} from "@/lib/api";

/** Poll cadence for status-ish endpoints. */
const POLL_MS = 5_000;

/** Live services list + status. */
export function useServices(opts?: { withInternal?: boolean }) {
  return useQuery({
    queryKey: ["services", opts?.withInternal ? "withInternal" : "traefikOnly"],
    queryFn: () => fetchServices(opts),
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
