import { useMemo, useState } from "react";
import { createFileRoute, useNavigate } from "@tanstack/react-router";

import type { Service } from "@/lib/api";
import { useServices } from "@/lib/hooks";
import { TerminalView } from "@/components/terminal/TerminalView";
import { ServiceSidebar } from "@/components/terminal/ServiceSidebar";

export const Route = createFileRoute("/logs")({
  validateSearch: (search: Record<string, unknown>) => ({
    service: typeof search.service === "string" ? search.service : undefined,
  }),
  component: LogsPage,
});

/* ------------------------------------------------------------------ */
/* Service grouping (project → worktree → services)                    */
/* ------------------------------------------------------------------ */

interface WorktreeBucket {
  worktree: string;
  services: Service[];
}

interface ProjectBucket {
  project: string;
  worktrees: WorktreeBucket[];
}

/** Group services by project, then worktree ("" = default checkout first). */
function groupServices(services: Service[]): ProjectBucket[] {
  const byProject = new Map<string, Map<string, Service[]>>();
  for (const svc of services) {
    let byWorktree = byProject.get(svc.project);
    if (!byWorktree) {
      byWorktree = new Map();
      byProject.set(svc.project, byWorktree);
    }
    const bucket = byWorktree.get(svc.worktree);
    if (bucket) bucket.push(svc);
    else byWorktree.set(svc.worktree, [svc]);
  }

  const projects: ProjectBucket[] = [];
  for (const [project, byWorktree] of byProject) {
    const worktrees: WorktreeBucket[] = [];
    for (const [worktree, list] of byWorktree) {
      list.sort((a, b) => a.service.localeCompare(b.service));
      worktrees.push({ worktree, services: list });
    }
    worktrees.sort((a, b) => {
      if (a.worktree === "") return -1;
      if (b.worktree === "") return 1;
      return a.worktree.localeCompare(b.worktree);
    });
    projects.push({ project, worktrees });
  }
  projects.sort((a, b) => a.project.localeCompare(b.project));
  return projects;
}

function LogsPage() {
  const navigate = useNavigate();
  const search = Route.useSearch();
  const { data: services, isLoading, isError } = useServices();
  const [live, setLive] = useState(true);

  const active = useMemo(() => {
    const list = services ?? [];
    if (list.length === 0) return null;
    const byContainer = new Map(list.map((s) => [s.container, s]));
    const byService = new Map(list.map((s) => [s.service, s]));
    const toActive = (svc: (typeof list)[number]) => ({
      container: svc.container,
      service: svc.service,
      pid: svc.pid ?? null,
      label: svc.service,
    });
    if (search.service) {
      const svc = byContainer.get(search.service) ?? byService.get(search.service);
      if (svc) return toActive(svc);
    }
    return toActive(list[0]);
  }, [services, search.service]);

  const groups = useMemo(() => groupServices(services ?? []), [services]);

  return (
    <div className="flex min-w-0 flex-col gap-3 lg:h-[calc(100dvh-8rem)] lg:flex-row lg:gap-6">
      {/* Main terminal */}
      <div className="flex min-w-0 flex-1 flex-col gap-3 lg:min-h-0">
        <label className="flex items-center gap-2 text-sm">
          <input
            type="checkbox"
            checked={live}
            onChange={(e) => setLive(e.target.checked)}
            disabled={!active}
            className="rounded"
          />
          <span className={active ? "" : "text-muted-foreground"}>
            Live — same PTY as TUI (mirror service, bidirectional). Unchecked = fresh shell in service workdir.
          </span>
        </label>
        <TerminalView
          key={`${active?.service ?? "__shell__"}:${live ? "live" : "cwd"}`}
          service={active?.service}
          live={live && !!active}
          className="flex min-h-[320px] flex-1"
        />
      </div>

      {/* Right sidebar */}
      <ServiceSidebar
        groups={groups}
        activeContainer={active?.container}
        onSelect={(container) =>
          void navigate({
            to: "/logs",
            search: { service: container },
          })
        }
        isLoading={isLoading}
        isError={isError}
      />
    </div>
  );
}
