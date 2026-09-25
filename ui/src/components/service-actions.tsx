import { useState } from "react";

import { useServiceAction, useServiceActionState } from "@/lib/hooks";
import type { ServiceAction } from "@/lib/api";
import type { VariantProps } from "class-variance-authority";
import { Button, buttonVariants } from "@/components/ui/button";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";

type ButtonSize = VariantProps<typeof buttonVariants>["size"];

const GERUND: Record<ServiceAction, string> = {
  start: "Starting…",
  stop: "Stopping…",
  restart: "Restarting…",
};

/**
 * Start/Stop/Restart controls for a single instance service.
 *
 * Shared by the `/status` table, the branch service list and the branch
 * service drawer. Stop and Restart open a confirm dialog (they interrupt a
 * running process); Start is immediate. The instance `pid` is the fog
 * process owning the service, not a service pid.
 *
 * Feedback: the active action's button shows a spinner + gerund label while
 * the request is in flight, the status badge flips immediately via the hook's
 * optimistic patch, and the other controls stay disabled until it settles.
 * `killing` disables everything once the owning instance is shutting down.
 * The pending state is read from the shared mutation cache, so a second copy
 * of the same service (desktop table vs. drawer) stays in lockstep.
 */
export function ServiceActions({
  pid,
  name,
  running,
  killing = false,
  size = "xs",
  className,
}: {
  pid: number;
  name: string;
  running: boolean;
  killing?: boolean;
  size?: ButtonSize;
  className?: string;
}) {
  const [confirmAction, setConfirmAction] = useState<ServiceAction | null>(
    null
  );
  const { mutate, error, data } = useServiceAction();
  const { pendingAction, isPending } = useServiceActionState(pid, name);

  const run = (action: ServiceAction) => {
    if (action === "stop" || action === "restart") {
      setConfirmAction(action);
      return;
    }
    mutate({ pid, name, action });
  };

  const confirm = () => {
    if (!confirmAction) return;
    const action = confirmAction;
    setConfirmAction(null);
    mutate({ pid, name, action });
  };

  const disabled = killing || isPending;
  const refused = !error && data && !data.ok ? data.reason : null;

  return (
    <div className={className}>
      <div className="flex items-center gap-1.5">
        <Button
          size={size}
          variant="outline"
          loading={pendingAction === "start"}
          loadingLabel={GERUND.start}
          disabled={disabled || running}
          onClick={() => run("start")}
        >
          Start
        </Button>
        <Button
          size={size}
          variant="outline"
          loading={pendingAction === "stop"}
          loadingLabel={GERUND.stop}
          disabled={disabled || !running}
          onClick={() => run("stop")}
        >
          Stop
        </Button>
        <Button
          size={size}
          variant="outline"
          loading={pendingAction === "restart"}
          loadingLabel={GERUND.restart}
          disabled={disabled}
          onClick={() => run("restart")}
        >
          Restart
        </Button>
      </div>
      {killing ? (
        <span className="text-warning mt-1 block font-mono text-[11px]">
          instance is shutting down…
        </span>
      ) : null}
      {error || refused ? (
        <span className="text-destructive mt-1 block font-mono text-[11px]">
          {error?.message ?? refused}
        </span>
      ) : null}

      <AlertDialog
        open={confirmAction !== null}
        onOpenChange={(open) => {
          if (!open) setConfirmAction(null);
        }}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>
              {confirmAction === "stop" ? "Stop" : "Restart"} service
            </AlertDialogTitle>
            <AlertDialogDescription>
              {confirmAction === "stop"
                ? `Stop "${name}" (pid ${pid})?`
                : `Restart "${name}" (pid ${pid})?`}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>Cancel</AlertDialogCancel>
            <AlertDialogAction
              variant={confirmAction === "stop" ? "destructive" : "default"}
              onClick={confirm}
            >
              {confirmAction === "stop" ? "Stop" : "Restart"}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </div>
  );
}
