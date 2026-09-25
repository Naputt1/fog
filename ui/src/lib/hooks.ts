/**
 * React Query hooks that wrap the fog API client (`@/lib/api`). Keeping
 * data-fetching in hooks means pages stay thin and adding mutations later
 * only requires adding a `useMutation` — the api module isolates transport.
 *
 * Every control action (launch, kill instance, service start/stop/restart)
 * follows the same feedback contract:
 *   - it patches the `status` cache optimistically so badges move at once,
 *   - it surfaces a keyed Sonner toast through loading → success/error,
 *   - it rolls the optimistic patch back on refusal or failure,
 *   - and the derived `use*State` hooks below expose the in-flight state to
 *     every copy of a target on screen (desktop table, mobile card, drawer),
 *     so duplicate controls stay consistent and cannot double-fire.
 */
import {
  useMutation,
  useMutationState,
  useQuery,
  useQueryClient,
  type QueryClient,
} from "@tanstack/react-query";
import { toast } from "sonner";
import {
  fetchServices,
  fetchStatus,
  fetchHealth,
  fetchLaunchTargets,
  postLaunch,
  postKillInstance,
  postServiceAction,
  type ServiceAction,
  type StatusSnapshot,
} from "@/lib/api";
import {
  applyServiceAction,
  isInstanceKilling,
  type KillIntentState,
} from "@/lib/action-cache";

/** Poll cadence for status-ish endpoints. */
const POLL_MS = 5_000;

/** Stable mutation keys, used to derive shared in-flight state. */
const SERVICE_ACTION_KEY = ["service-action"] as const;
const KILL_INSTANCE_KEY = ["kill-instance"] as const;
const LAUNCH_KEY = ["launch"] as const;

/** Variables of a service control mutation. */
interface ServiceActionVars {
  pid: number;
  name: string;
  action: ServiceAction;
}

/** Variables of an instance kill mutation (`script` is UI-only, for labels). */
interface KillInstanceVars {
  pid: number;
  script?: string;
}

const SERVICE_VERB: Record<ServiceAction, { gerund: string; past: string }> = {
  start: { gerund: "Starting", past: "Started" },
  stop: { gerund: "Stopping", past: "Stopped" },
  restart: { gerund: "Restarting", past: "Restarted" },
};

const serviceToastId = (pid: number, name: string) =>
  `service-action:${pid}:${name}`;
const killToastId = (pid: number) => `kill-instance:${pid}`;
const LAUNCH_TOAST_ID = "launch";

/**
 * Whether a service control action is mid-flight. While one is, status-ish
 * polls are paused: otherwise the 5s refetch would clobber the optimistic
 * patch with the server's not-yet-updated value and the badge would flicker.
 * (Kill is deliberately excluded — polling must keep running to notice the
 * instance actually disappearing.)
 */
function serviceActionInFlight(queryClient: QueryClient): boolean {
  return queryClient.isMutating({ mutationKey: SERVICE_ACTION_KEY }) > 0;
}

/** Live services list + status. */
export function useServices(opts?: { withInternal?: boolean }) {
  const queryClient = useQueryClient();
  return useQuery({
    queryKey: ["services", opts?.withInternal ? "withInternal" : "traefikOnly"],
    queryFn: () => fetchServices(opts),
    refetchInterval: () =>
      serviceActionInFlight(queryClient) ? false : POLL_MS,
  });
}

/** IPC status snapshot. */
export function useStatus() {
  const queryClient = useQueryClient();
  return useQuery({
    queryKey: ["status"],
    queryFn: fetchStatus,
    refetchInterval: () =>
      serviceActionInFlight(queryClient) ? false : POLL_MS,
  });
}

/** Per-service health results. */
export function useHealth() {
  const queryClient = useQueryClient();
  return useQuery({
    queryKey: ["health"],
    queryFn: fetchHealth,
    refetchInterval: () =>
      serviceActionInFlight(queryClient) ? false : POLL_MS,
  });
}

/**
 * Service control mutation (start/stop/restart). Patches `status`
 * optimistically, toasts the outcome, and invalidates `status`/`health`/
 * `services` on settle. Callers read `mutation.error` (transport failure) and
 * `mutation.data.ok === false` (the instance refused) to surface inline state.
 */
export function useServiceAction() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationKey: SERVICE_ACTION_KEY,
    mutationFn: ({ pid, name, action }: ServiceActionVars) =>
      postServiceAction(pid, name, action),
    onMutate: async ({ pid, name, action }) => {
      toast.loading(`${SERVICE_VERB[action].gerund} "${name}"…`, {
        id: serviceToastId(pid, name),
      });
      await queryClient.cancelQueries({ queryKey: ["status"] });
      const previous = queryClient.getQueryData<StatusSnapshot>(["status"]);
      queryClient.setQueryData<StatusSnapshot>(["status"], (old) =>
        applyServiceAction(old, pid, name, action)
      );
      return { previous };
    },
    onSuccess: (result, { pid, name, action }, context) => {
      const id = serviceToastId(pid, name);
      if (result.ok) {
        toast.success(`${SERVICE_VERB[action].past} "${name}"`, { id });
        return;
      }
      // The instance refused the action: drop the optimistic patch and say why.
      if (context?.previous !== undefined) {
        queryClient.setQueryData(["status"], context.previous);
      }
      toast.warning(`Could not ${action} "${name}"`, {
        id,
        description: result.reason ?? "The instance refused the action.",
      });
    },
    onError: (error, { pid, name, action }, context) => {
      if (context?.previous !== undefined) {
        queryClient.setQueryData(["status"], context.previous);
      }
      toast.error(`Could not ${action} "${name}"`, {
        id: serviceToastId(pid, name),
        description: error.message,
      });
    },
    onSettled: () => {
      void queryClient.invalidateQueries({ queryKey: ["status"] });
      void queryClient.invalidateQueries({ queryKey: ["health"] });
      void queryClient.invalidateQueries({ queryKey: ["services"] });
    },
  });
}

/**
 * Kill an entire fog instance. The POST resolves as soon as the signal is
 * delivered, but the instance keeps appearing in `status` until it exits; the
 * `useInstanceKillState` hook reports that transitional window so the row can
 * show a "Killing…" state until the instance is actually gone.
 */
export function useKillInstance() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationKey: KILL_INSTANCE_KEY,
    mutationFn: ({ pid }: KillInstanceVars) => postKillInstance(pid),
    onMutate: async ({ pid, script }) => {
      toast.loading(`Killing ${instanceLabel(pid, script)}…`, {
        id: killToastId(pid),
      });
      await queryClient.cancelQueries({ queryKey: ["status"] });
    },
    onSuccess: (_result, { pid, script }) => {
      toast.info(`Killing ${instanceLabel(pid, script)}`, {
        id: killToastId(pid),
        description: "Waiting for its services to stop gracefully…",
      });
    },
    onError: (error, { pid, script }) => {
      toast.error(`Could not kill ${instanceLabel(pid, script)}`, {
        id: killToastId(pid),
        description: error.message,
      });
    },
    onSettled: () => {
      void queryClient.invalidateQueries({ queryKey: ["status"] });
      void queryClient.invalidateQueries({ queryKey: ["health"] });
      void queryClient.invalidateQueries({ queryKey: ["services"] });
    },
  });
}

function instanceLabel(pid: number, script?: string): string {
  return script ? `"${script}" (pid ${pid})` : `pid ${pid}`;
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
 * Launch mutation. Toasts through loading → success/error and invalidates
 * `status`/`health` on settle so the new instance appears. Callers read
 * `mutation.error` / `mutation.isPending` / `mutation.data` for inline state.
 */
export function useLaunch() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationKey: LAUNCH_KEY,
    mutationFn: ({
      configDir,
      script,
      branch,
    }: {
      configDir: string;
      script: string;
      branch?: string | null;
    }) => postLaunch(configDir, script, branch),
    onMutate: async () => {
      toast.loading("Starting instance…", { id: LAUNCH_TOAST_ID });
    },
    onSuccess: (result) => {
      if (result.ok && result.pid != null) {
        toast.success(`Started pid ${result.pid}`, {
          id: LAUNCH_TOAST_ID,
          description: "The instance is booting its services.",
        });
        return;
      }
      toast.error(result.error ?? "Instance did not start", {
        id: LAUNCH_TOAST_ID,
      });
    },
    onError: (error) => {
      toast.error("Could not start instance", {
        id: LAUNCH_TOAST_ID,
        description: error.message,
      });
    },
    onSettled: () => {
      void queryClient.invalidateQueries({ queryKey: ["status"] });
      void queryClient.invalidateQueries({ queryKey: ["health"] });
    },
  });
}

/**
 * In-flight service action for one target, shared across every component that
 * renders it. `pendingAction` drives which button shows a spinner + gerund
 * label; `isPending` disables the rest.
 */
export function useServiceActionState(
  pid: number,
  name: string
): { pendingAction: ServiceAction | null; isPending: boolean } {
  const pending = useMutationState({
    filters: { mutationKey: SERVICE_ACTION_KEY, status: "pending" },
    select: (mutation) =>
      mutation.state.variables as ServiceActionVars | undefined,
  });
  const match = pending.find((vars) => vars?.pid === pid && vars.name === name);
  return {
    pendingAction: match?.action ?? null,
    isPending: match != null,
  };
}

/**
 * Whether an instance is being killed and has not failed. Stays true through
 * the graceful-shutdown window (while the instance still appears in `status`)
 * and drops on failure or after {@link KILL_GRACE_MS}.
 */
export function useInstanceKillState(
  pid: number,
  script?: string
): { killing: boolean } {
  const intents = useMutationState({
    filters: { mutationKey: KILL_INSTANCE_KEY },
    select: (mutation): KillIntentState => {
      const vars = mutation.state.variables as KillInstanceVars | undefined;
      return {
        pid: vars?.pid,
        script: vars?.script,
        status: mutation.state.status,
        submittedAt: mutation.state.submittedAt,
      };
    },
  });
  return { killing: isInstanceKilling(intents, pid, script) };
}
