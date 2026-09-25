import { useState } from "react";
import { Trash2 } from "lucide-react";

import { useKillInstance, useInstanceKillState } from "@/lib/hooks";
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
 * Kill a whole fog instance (its services are shut down gracefully). Shared by
 * the `/status` page and the branch list page. Renders the confirm dialog
 * itself; keep it a sibling of any surrounding link so the click never
 * navigates.
 *
 * The kill POST resolves as soon as the signal is delivered, but the instance
 * lingers in `status` while it shuts down. The button therefore stays in a
 * spinner + "Killing…" state for that whole window (see `useInstanceKillState`)
 * rather than only while the request is in flight, so the click never looks
 * like a no-op.
 */
export function InstanceKillButton({
  pid,
  script,
  project,
  branch,
  size = "xs",
  withIcon = false,
  className,
}: {
  pid: number;
  script: string;
  project?: string | null;
  branch?: string | null;
  size?: ButtonSize;
  withIcon?: boolean;
  className?: string;
}) {
  const [confirmKill, setConfirmKill] = useState(false);
  const { mutate, error } = useKillInstance();
  const { killing } = useInstanceKillState(pid, script);

  return (
    <div className={className}>
      <Button
        size={size}
        variant="destructive"
        loading={killing}
        loadingLabel="Killing…"
        disabled={killing}
        onClick={() => setConfirmKill(true)}
      >
        {withIcon ? <Trash2 aria-hidden /> : null}
        Kill
      </Button>
      {error ? (
        <span className="text-destructive mt-1 block font-mono text-[11px]">
          {error.message}
        </span>
      ) : null}

      <AlertDialog
        open={confirmKill}
        onOpenChange={(open) => {
          if (!open) setConfirmKill(false);
        }}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Kill instance</AlertDialogTitle>
            <AlertDialogDescription>
              Kill "{script}" (pid {pid})?
              {project
                ? ` Project: ${project}${branch ? `@${branch}` : ""}.`
                : ""}{" "}
              All services will be shut down gracefully.
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>Cancel</AlertDialogCancel>
            <AlertDialogAction
              variant="destructive"
              onClick={() => {
                setConfirmKill(false);
                mutate({ pid, script });
              }}
            >
              Kill
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </div>
  );
}
