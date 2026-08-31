import { Fragment, useEffect } from "react";
import type { Service } from "@/lib/api";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Sheet, SheetContent, SheetHeader, SheetTitle } from "@/components/ui/sheet";
import { cn } from "@/lib/utils";
import { useRightSidebar } from "@/lib/right-sidebar-context";

interface WorktreeBucket {
  worktree: string;
  services: Service[];
}

interface ProjectBucket {
  project: string;
  worktrees: WorktreeBucket[];
}

interface ServiceSidebarProps {
  groups: ProjectBucket[];
  activeContainer?: string;
  onSelect: (container: string) => void;
  isLoading?: boolean;
  isError?: boolean;
}

function ServiceButton({
  svc,
  active,
  onSelect,
}: {
  svc: Service;
  active: boolean;
  onSelect: (c: string) => void;
}) {
  const running = svc.status === "running";
  return (
    <button
      type="button"
      onClick={() => onSelect(svc.container)}
      className={cn(
        "flex w-full items-center gap-2 rounded-md px-2.5 py-1.5 text-left font-mono text-xs transition-colors",
        active
          ? "bg-accent text-accent-foreground shadow-[inset_0_0_0_1px_var(--color-primary)/20]"
          : "text-muted-foreground hover:bg-accent/60 hover:text-foreground"
      )}
    >
      <span
        className={cn(
          "size-1.5 shrink-0 rounded-full",
          running ? "bg-emerald-500" : "bg-muted-foreground/50"
        )}
      />
      <span className="min-w-0 flex-1 truncate">{svc.service}</span>
      {active && <span className="bg-primary h-1 w-1 shrink-0 rounded-full" />}
    </button>
  );
}

function SidebarContent({ groups, activeContainer, onSelect, isLoading, isError }: ServiceSidebarProps) {
  if (isError) {
    return <div className="text-destructive p-3 font-mono text-xs">Could not load services.</div>;
  }
  if (isLoading) {
    return <div className="text-muted-foreground p-3 font-mono text-xs">loading…</div>;
  }
  if (groups.length === 0) {
    return <div className="text-muted-foreground p-3 font-mono text-xs">no services running</div>;
  }
  return (
    <div className="space-y-4 p-2">
      {groups.map((project) => (
        <div key={project.project}>
          <div className="text-primary/80 px-2 py-1 font-mono text-[11px] tracking-wider uppercase">
            {project.project}
          </div>
          <div className="space-y-2">
            {project.worktrees.map((wt) => (
              <Fragment key={`${project.project}:${wt.worktree}`}>
                <div className="text-muted-foreground px-2 pt-1 font-mono text-[10px] tracking-wider uppercase">
                  {wt.worktree || "default"} · {wt.services.length}
                </div>
                <div className="space-y-0.5">
                  {wt.services.map((svc) => (
                    <ServiceButton
                      key={svc.container}
                      svc={svc}
                      active={svc.container === activeContainer}
                      onSelect={onSelect}
                    />
                  ))}
                </div>
              </Fragment>
            ))}
          </div>
        </div>
      ))}
    </div>
  );
}

export function ServiceSidebar(props: ServiceSidebarProps) {
  const { groups, activeContainer, onSelect, isLoading, isError } = props;
  const count = groups.reduce((acc, p) => acc + p.worktrees.reduce((a, w) => a + w.services.length, 0), 0);
  const ctx = useRightSidebar();

  useEffect(() => {
    ctx?.setEnabled(true);
    return () => ctx?.setEnabled(false);
  }, [ctx]);

  const handleSelect = (container: string) => {
    onSelect(container);
    ctx?.setOpen(false);
  };

  return (
    <>
      {/* Desktop: always visible right sidebar */}
      <aside className="hidden w-72 shrink-0 flex-col border-l pl-0 lg:flex">
        <div className="flex h-10 shrink-0 items-center gap-2 border-b px-3">
          <span className="text-muted-foreground font-mono text-xs tracking-wider uppercase">Services</span>
          <span className="bg-muted text-muted-foreground ml-auto rounded-full px-1.5 py-0.5 font-mono text-[10px]">
            {isLoading ? "…" : count}
          </span>
        </div>
        <ScrollArea className="flex-1 min-h-0">
          <SidebarContent
            groups={groups}
            activeContainer={activeContainer}
            onSelect={handleSelect}
            isLoading={isLoading}
            isError={isError}
          />
        </ScrollArea>
      </aside>

      {/* Mobile: sheet controlled by header button */}
      <Sheet open={ctx?.open ?? false} onOpenChange={(o) => ctx?.setOpen(o)}>
        <SheetContent side="right" className="w-72 p-0">
          <SheetHeader className="border-b px-4 py-3">
            <SheetTitle className="font-mono text-xs tracking-wider uppercase">Services</SheetTitle>
          </SheetHeader>
          <ScrollArea className="h-[calc(100dvh-56px)]">
            <SidebarContent
              groups={groups}
              activeContainer={activeContainer}
              onSelect={handleSelect}
              isLoading={isLoading}
              isError={isError}
            />
          </ScrollArea>
        </SheetContent>
      </Sheet>
    </>
  );
}

export type { ProjectBucket, WorktreeBucket };
