import { useEffect, useRef, useState } from "react";
import { GitBranch, TriangleAlert } from "lucide-react";

import { useKillInstance, useKillingPids } from "@/lib/hooks";
import {
  DEFAULT_WORKTREE,
  groupByBranch,
  type InstanceView,
} from "@/lib/services";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Sheet, SheetContent } from "@/components/ui/sheet";
import { useMediaQuery } from "@/lib/use-media-query";
import { cn } from "@/lib/utils";

/** How many of an instance's services are running. */
function runningServices(inst: InstanceView): number {
  return inst.services.filter((svc) => svc.running).length;
}

/** One selectable instance in the picker. */
function KillInstanceRow({
  inst,
  checked,
  disabled,
  killing,
  onCheckedChange,
}: {
  inst: InstanceView;
  checked: boolean;
  disabled: boolean;
  killing: boolean;
  onCheckedChange: (checked: boolean) => void;
}) {
  return (
    <label
      className={cn(
        "hover:bg-muted/50 flex cursor-pointer items-center gap-3 px-4 py-2 transition-colors",
        disabled && "cursor-not-allowed opacity-60 hover:bg-transparent"
      )}
    >
      <Checkbox
        checked={checked}
        disabled={disabled}
        onCheckedChange={onCheckedChange}
      />
      <span className="flex min-w-0 flex-1 items-baseline gap-2">
        <span className="min-w-0 truncate font-mono text-xs font-medium">
          {inst.script}
        </span>
        <span className="text-muted-foreground text-2xs shrink-0 font-mono">
          pid {inst.pid}
        </span>
      </span>
      <span
        className={cn(
          "text-2xs shrink-0 font-mono",
          killing ? "text-muted-foreground" : "text-muted-foreground/80"
        )}
      >
        {killing
          ? "killing…"
          : `${runningServices(inst)}/${inst.services.length}`}
      </span>
    </label>
  );
}

/**
 * The picker body. Mounted only while its popup surface is open, so a fresh
 * open always starts with nothing selected and no pending confirmation.
 */
function KillInstancesPicker({
  instances,
  onClose,
}: {
  instances: readonly InstanceView[];
  onClose: () => void;
}) {
  const { mutate } = useKillInstance();
  const killingPids = useKillingPids(instances);
  const [selected, setSelected] = useState<ReadonlySet<number>>(new Set());
  const [confirming, setConfirming] = useState(false);
  // A double-click on "Kill selected" would otherwise land on the confirmation
  // button that replaces it in the same spot. Arm the destructive action only
  // after the new footer has settled, and dispatch at most once.
  const [confirmReady, setConfirmReady] = useState(false);
  const submitted = useRef(false);

  useEffect(() => {
    if (!confirming) return;
    const timer = window.setTimeout(() => setConfirmReady(true), 400);
    return () => window.clearTimeout(timer);
  }, [confirming]);

  // Polling can remove an instance while the popup is open. Keep the selection
  // derived from what is still selectable so a stale pid is never killed.
  const selectablePids = new Set(
    instances
      .filter((inst) => inst.pid > 0 && !killingPids.has(inst.pid))
      .map((inst) => inst.pid)
  );
  const selectedPids = new Set(
    [...selected].filter((pid) => selectablePids.has(pid))
  );
  const selectedCount = selectedPids.size;
  const allSelected =
    selectablePids.size > 0 && selectedCount === selectablePids.size;
  const someSelected = selectedCount > 0 && !allSelected;
  const confirmingSelection = confirming && selectedCount > 0;
  const branches = groupByBranch([...instances]);

  const toggle = (pid: number, checked: boolean) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (checked) next.add(pid);
      else next.delete(pid);
      return next;
    });
  };

  const beginConfirm = () => {
    if (selectedCount === 0) return;
    submitted.current = false;
    setConfirmReady(false);
    setConfirming(true);
  };

  const cancelConfirm = () => {
    setConfirmReady(false);
    setConfirming(false);
  };

  const killSelected = () => {
    if (!confirmReady || submitted.current) return;
    submitted.current = true;
    for (const pid of selectedPids) {
      const inst = instances.find((candidate) => candidate.pid === pid);
      if (inst) mutate({ pid, script: inst.script });
    }
    onClose();
  };

  const selectAllDisabled = confirmingSelection || selectablePids.size === 0;

  return (
    <>
      <div className="border-border flex shrink-0 flex-wrap items-center justify-between gap-2 border-b px-4 py-2.5">
        <label
          className={cn(
            "flex items-center gap-2 text-xs",
            selectAllDisabled
              ? "cursor-not-allowed opacity-50"
              : "cursor-pointer"
          )}
        >
          <Checkbox
            checked={allSelected}
            indeterminate={someSelected}
            disabled={selectAllDisabled}
            onCheckedChange={(checked) =>
              setSelected(checked ? new Set(selectablePids) : new Set())
            }
          />
          <span>Select all</span>
        </label>
        <span className="text-muted-foreground text-2xs font-mono">
          {selectedCount} of {selectablePids.size} selected
        </span>
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto">
        {branches.length === 0 ? (
          <p className="text-muted-foreground p-4 font-mono text-xs">
            No running instances.
          </p>
        ) : (
          branches.map((branch) => {
            const label = branch.worktree || DEFAULT_WORKTREE;
            return (
              <section key={branch.worktree || DEFAULT_WORKTREE}>
                <h3 className="text-muted-foreground bg-muted/80 text-3xs sticky top-0 z-10 flex items-center gap-1.5 border-b px-4 py-1.5 font-mono tracking-wider uppercase backdrop-blur">
                  <GitBranch className="size-3 shrink-0" aria-hidden />
                  <span className="min-w-0 truncate">{label}</span>
                  <span className="ml-auto shrink-0">
                    {branch.instances.length}
                  </span>
                </h3>
                <div className="divide-border divide-y">
                  {branch.instances.map((inst) => {
                    const killing = killingPids.has(inst.pid);
                    return (
                      <KillInstanceRow
                        key={inst.pid}
                        inst={inst}
                        checked={selectedPids.has(inst.pid)}
                        disabled={confirmingSelection || killing}
                        killing={killing}
                        onCheckedChange={(checked) => toggle(inst.pid, checked)}
                      />
                    );
                  })}
                </div>
              </section>
            );
          })
        )}
      </div>

      <div className="border-border flex shrink-0 flex-col gap-2 border-t px-4 py-3">
        {confirmingSelection ? (
          <>
            <p
              role="alert"
              className="text-muted-foreground text-2xs flex items-start gap-1.5 font-mono"
            >
              <TriangleAlert
                className="text-destructive mt-0.5 size-3.5 shrink-0"
                aria-hidden
              />
              <span>
                Shut down {selectedCount}{" "}
                {selectedCount === 1 ? "instance" : "instances"}? Their services
                stop gracefully.
              </span>
            </p>
            <div className="flex items-center justify-end gap-2">
              <Button
                variant="ghost"
                size="sm"
                onClick={cancelConfirm}
                autoFocus
              >
                Back
              </Button>
              <Button
                variant="destructive"
                size="sm"
                onClick={killSelected}
                disabled={!confirmReady}
              >
                Kill {selectedCount}{" "}
                {selectedCount === 1 ? "instance" : "instances"}
              </Button>
            </div>
          </>
        ) : (
          <div className="flex flex-wrap items-center justify-between gap-2">
            <p className="text-muted-foreground text-2xs min-w-0 flex-1 font-mono">
              {selectedCount > 0
                ? `${selectedCount} selected`
                : "Select instances to kill."}
            </p>
            <div className="flex shrink-0 items-center gap-2">
              <Button variant="ghost" size="sm" onClick={onClose}>
                Cancel
              </Button>
              <Button
                variant="destructive"
                size="sm"
                disabled={selectedCount === 0}
                onClick={beginConfirm}
              >
                Kill selected{selectedCount > 0 ? ` (${selectedCount})` : ""}
              </Button>
            </div>
          </div>
        )}
      </div>
    </>
  );
}

/**
 * Kill one or more of a project's instances. A centered dialog on desktop and
 * a bottom sheet on mobile, matching the launch popup's split. Only one
 * surface is mounted, so there is no duplicate focus trap.
 */
export function KillInstancesPopup({
  open,
  onOpenChange,
  project,
  instances,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  project: string;
  instances: readonly InstanceView[];
}) {
  const isDesktop = useMediaQuery("(min-width: 1024px)");
  const close = () => onOpenChange(false);
  const header = (
    <DialogHeader>
      <div className="min-w-0">
        <DialogTitle>Kill instances</DialogTitle>
        <p className="text-muted-foreground mt-0.5 text-xs">
          {project} · choose the instances to shut down
        </p>
      </div>
    </DialogHeader>
  );
  const picker = <KillInstancesPicker instances={instances} onClose={close} />;

  if (isDesktop) {
    return (
      <Dialog open={open} onOpenChange={onOpenChange}>
        <DialogContent spacing="none" className="max-h-[85dvh] max-w-lg">
          {header}
          {picker}
        </DialogContent>
      </Dialog>
    );
  }

  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent
        side="bottom"
        shape="roundedTop"
        spacing="none"
        className="max-h-[85dvh]"
      >
        <div
          aria-hidden
          className="bg-muted-foreground/30 mx-auto mt-2 h-1 w-10 shrink-0 rounded-full"
        />
        {header}
        {picker}
        <div aria-hidden className="h-safe-bottom shrink-0" />
      </SheetContent>
    </Sheet>
  );
}
