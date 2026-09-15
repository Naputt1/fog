import { useState } from "react";

import { useServiceAction } from "@/lib/hooks";
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

/**
 * Start/Stop/Restart controls for a single instance service.
 *
 * Shared by the `/status` table, the branch service list and the branch
 * service drawer. Stop and Restart open a confirm dialog (they interrupt a
 * running process); Start is immediate. The instance `pid` is the fog
 * process owning the service, not a service pid.
 */
export function ServiceActions({
  pid,
  name,
  running,
  size = "xs",
  className,
}: {
  pid: number;
  name: string;
  running: boolean;
  size?: ButtonSize;
  className?: string;
}) {
  const [confirmAction, setConfirmAction] = useState<ServiceAction | null>(
    null
  );
  const { mutate, isPending, error } = useServiceAction();

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

  return (
    <div className={className}>
      <div className="flex items-center gap-1.5">
        <Button
          size={size}
          variant="outline"
          disabled={running || isPending}
          onClick={() => run("start")}
        >
          Start
        </Button>
        <Button
          size={size}
          variant="outline"
          disabled={!running || isPending}
          onClick={() => run("stop")}
        >
          Stop
        </Button>
        <Button
          size={size}
          variant="outline"
          disabled={isPending}
          onClick={() => run("restart")}
        >
          Restart
        </Button>
      </div>
      {error ? (
        <span className="text-destructive mt-1 block font-mono text-[11px]">
          {error.message}
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
