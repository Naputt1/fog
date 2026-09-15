import { Fragment, useEffect, useId } from "react";
import type { Service } from "@/lib/api";
import type { ProjectBucket } from "@/lib/services";
import {
  Sheet,
  SheetContent,
  SheetHeader,
  SheetTitle,
} from "@/components/ui/sheet";
import { cn } from "@/lib/utils";
import { useRightSidebar } from "@/lib/right-sidebar-context";

interface ServiceSidebarProps {
  groups: ProjectBucket[];
  allGroups?: ProjectBucket[];
  selectedProject?: string;
  onSelectProject?: (project: string) => void;
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
      aria-current={active ? "true" : undefined}
      className={cn(
        "focus-visible:ring-ring/60 flex min-h-9 w-full items-center gap-2 rounded-md px-2.5 py-1.5 text-left font-mono text-xs transition-colors outline-none focus-visible:ring-2",
        active
          ? "bg-accent text-accent-foreground shadow-[inset_0_0_0_1px_var(--color-primary)/20]"
          : "text-muted-foreground hover:bg-accent/60 hover:text-foreground"
      )}
    >
      <span
        aria-hidden="true"
        className={cn(
          "size-1.5 shrink-0 rounded-full",
          running ? "bg-emerald-500" : "bg-muted-foreground/50"
        )}
      />
      <span className="min-w-0 flex-1 truncate">{svc.service}</span>
      {active && (
        <span
          aria-hidden="true"
          className="bg-primary h-1 w-1 shrink-0 rounded-full"
        />
      )}
    </button>
  );
}

function SidebarContent({
  groups,
  activeContainer,
  onSelect,
  isLoading,
  isError,
}: ServiceSidebarProps) {
  if (isError) {
    return (
      <div className="text-destructive p-3 font-mono text-xs">
        Could not load services.
      </div>
    );
  }
  if (isLoading) {
    return (
      <div className="text-muted-foreground p-3 font-mono text-xs">
        loading…
      </div>
    );
  }
  if (groups.length === 0) {
    return (
      <div className="text-muted-foreground p-3 font-mono text-xs">
        no services running
      </div>
    );
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
  const {
    groups,
    allGroups,
    selectedProject,
    onSelectProject,
    activeContainer,
    onSelect,
    isLoading,
    isError,
  } = props;
  const displayGroups = groups;
  const projectOptions = (allGroups ?? groups).map((g) => ({
    name: g.project,
    count: g.worktrees.reduce((a, w) => a + w.services.length, 0),
  }));
  const count = displayGroups.reduce(
    (acc, p) => acc + p.worktrees.reduce((a, w) => a + w.services.length, 0),
    0
  );
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
          <span className="text-muted-foreground font-mono text-xs tracking-wider uppercase">
            Services
          </span>
          <span className="bg-muted text-muted-foreground ml-auto rounded-full px-1.5 py-0.5 font-mono text-[10px]">
            {isLoading ? "…" : count}
          </span>
        </div>
        <ProjectDropdown
          projectOptions={projectOptions}
          selectedProject={selectedProject}
          onSelectProject={onSelectProject}
        />
        <div className="min-h-0 flex-1 overflow-y-auto overscroll-contain">
          <SidebarContent
            groups={displayGroups}
            activeContainer={activeContainer}
            onSelect={handleSelect}
            isLoading={isLoading}
            isError={isError}
          />
        </div>
      </aside>

      {/* Mobile: sheet controlled by header button */}
      <Sheet open={ctx?.open ?? false} onOpenChange={(o) => ctx?.setOpen(o)}>
        <SheetContent side="right" className="w-72 gap-0 p-0">
          <SheetHeader className="border-border pt-safe border-b px-4 py-3">
            <SheetTitle className="font-mono text-xs tracking-wider uppercase">
              Services
            </SheetTitle>
          </SheetHeader>
          <ProjectDropdown
            projectOptions={projectOptions}
            selectedProject={selectedProject}
            onSelectProject={onSelectProject}
          />
          <div className="pb-safe min-h-0 flex-1 overflow-y-auto overscroll-contain">
            <SidebarContent
              groups={displayGroups}
              activeContainer={activeContainer}
              onSelect={handleSelect}
              isLoading={isLoading}
              isError={isError}
            />
          </div>
        </SheetContent>
      </Sheet>
    </>
  );
}

function ProjectDropdown({
  projectOptions,
  selectedProject,
  onSelectProject,
}: {
  projectOptions: { name: string; count: number }[];
  selectedProject?: string;
  onSelectProject?: (project: string) => void;
}) {
  const selectId = useId();
  if (!projectOptions.length || !onSelectProject) return null;
  return (
    <div className="border-b px-2 py-2">
      <label
        htmlFor={selectId}
        className="text-muted-foreground mb-1 block font-mono text-[10px] tracking-wider uppercase"
      >
        Project
      </label>
      <select
        id={selectId}
        value={selectedProject ?? ""}
        onChange={(e) => onSelectProject(e.target.value)}
        className="bg-background border-input focus:ring-ring h-9 w-full rounded-md border px-2 py-1.5 font-mono text-xs focus:ring-1 focus:outline-none"
      >
        {projectOptions.map((p) => (
          <option key={p.name} value={p.name}>
            {p.name} ({p.count})
          </option>
        ))}
      </select>
    </div>
  );
}
