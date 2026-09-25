/**
 * Pure helpers backing the optimistic / in-flight UI for control actions.
 *
 * The action hooks (`@/lib/hooks`) call these to patch the React Query cache
 * the moment a request starts, so status badges change immediately instead of
 * waiting for the next poll. Keeping the transforms pure makes them easy to
 * unit-test and keeps the hooks thin.
 *
 * Server contract reminder: `POST /api/instances/{pid}/kill` resolves as soon
 * as the shutdown signal is delivered; the instance then takes time to exit
 * and keeps appearing in `/api/status`. The kill UI therefore needs a
 * "transitional" state that outlives the request. {@link KILL_GRACE_MS} bounds
 * how long a recorded kill intent is trusted, so a wedge cannot leave an
 * instance stuck in "killing" forever.
 */
import type {
  InstanceServiceStatus,
  ServiceAction,
  StatusSnapshot,
} from "@/lib/api";

/** How long a recorded kill intent keeps an instance in its "killing" state. */
export const KILL_GRACE_MS = 90_000;

/**
 * The running/health values a service should show optimistically while an
 * action is in flight. Shared by the cache patch and (indirectly) the badges.
 */
export function optimisticServicePatch(
  action: ServiceAction
): Pick<InstanceServiceStatus, "running" | "health"> {
  switch (action) {
    case "start":
      return { running: true, health: "starting" };
    case "restart":
      return { running: true, health: "starting" };
    case "stop":
      return { running: false, health: null };
  }
}

/**
 * Returns a copy of the status snapshot with one service's `running`/`health`
 * optimistically patched for `action`. Unknown pid/name returns the input
 * unchanged so a rollback can never invent an instance.
 */
export function applyServiceAction(
  snapshot: StatusSnapshot | undefined,
  pid: number,
  name: string,
  action: ServiceAction
): StatusSnapshot | undefined {
  if (!snapshot) return snapshot;

  let changed = false;
  const instances = snapshot.instances.map((inst) => {
    if (inst.pid !== pid) return inst;
    let instanceChanged = false;
    const services = inst.services.map((svc) => {
      if (svc.name !== name) return svc;
      instanceChanged = true;
      return { ...svc, ...optimisticServicePatch(action) };
    });
    if (!instanceChanged) return inst;
    changed = true;
    return { ...inst, services };
  });

  return changed ? { ...snapshot, instances } : snapshot;
}

/** One kill mutation's projected variables/status, as read from the cache. */
export interface KillIntentState {
  pid?: number;
  script?: string;
  status: "idle" | "pending" | "error" | "success";
  submittedAt: number;
}

/**
 * Whether `pid` currently has a kill that was requested and not failed,
 * within {@link KILL_GRACE_MS}. When `script` is given it must match too, so a
 * recycled pid from an unrelated instance is not mistaken for the killed one.
 */
export function isInstanceKilling(
  intents: readonly KillIntentState[],
  pid: number,
  script?: string,
  now: number = Date.now()
): boolean {
  return intents.some(
    (intent) =>
      intent.pid === pid &&
      (intent.status === "pending" || intent.status === "success") &&
      (script == null || intent.script == null || intent.script === script) &&
      now - intent.submittedAt < KILL_GRACE_MS
  );
}
