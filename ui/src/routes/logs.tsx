import { Suspense, lazy, useEffect, useMemo, useRef, useState } from "react";
import { createFileRoute, useNavigate } from "@tanstack/react-router";

import type { Service } from "@/lib/api";
import { useServices } from "@/lib/hooks";
import { Button } from "@/components/ui/button";
import { LoadingState } from "@/components/page-state";
// xterm (~300K+) is the heaviest dependency of the dashboard. Both views that
// use it are split into on-demand chunks so the entry bundle (index, status,
// health pages) stays lean; the chunk loads when /logs renders, behind a
// skeleton. `ref` passes straight through the lazy wrapper (React 19
// ref-as-prop; both views declare it in their props).
const TerminalView = lazy(() =>
  import("@/components/terminal/TerminalView").then((m) => ({
    default: m.TerminalView,
  }))
);
const LogView = lazy(() =>
  import("@/components/terminal/LogView").then((m) => ({ default: m.LogView }))
);
import { TerminalKeypad } from "@/components/terminal/TerminalKeypad";
import { ServiceSidebar } from "@/components/terminal/ServiceSidebar";
import type { TerminalHandle } from "@/components/terminal/terminal-handle";
import { cn } from "@/lib/utils";

export const Route = createFileRoute("/logs")({
  validateSearch: (search: Record<string, unknown>) => ({
    project: typeof search.project === "string" ? search.project : undefined,
    service: typeof search.service === "string" ? search.service : undefined,
    view:
      search.view === "terminal" || search.view === "logs"
        ? (search.view as "terminal" | "logs")
        : undefined,
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

const LS_KEY = "fog:terminal:last";
const LS_MODE_KEY = "fog:logs:mode";

type ViewMode = "logs" | "terminal";

function readMode(): ViewMode {
  if (typeof window === "undefined") return "logs";
  try {
    return window.localStorage.getItem(LS_MODE_KEY) === "terminal"
      ? "terminal"
      : "logs";
  } catch {
    return "logs";
  }
}

function writeMode(mode: ViewMode) {
  if (typeof window === "undefined") return;
  try {
    window.localStorage.setItem(LS_MODE_KEY, mode);
  } catch {
    // ignore
  }
}

function readStored(): { project?: string; service?: string } {
  if (typeof window === "undefined") return {};
  try {
    const raw = window.localStorage.getItem(LS_KEY);
    if (!raw) return {};
    const parsed = JSON.parse(raw);
    return {
      project: typeof parsed.project === "string" ? parsed.project : undefined,
      service: typeof parsed.service === "string" ? parsed.service : undefined,
    };
  } catch {
    return {};
  }
}

function writeStored(project: string, service: string) {
  if (typeof window === "undefined") return;
  try {
    window.localStorage.setItem(LS_KEY, JSON.stringify({ project, service }));
  } catch {
    // ignore
  }
}

function LogsPage() {
  const navigate = useNavigate();
  const search = Route.useSearch();
  const termApiRef = useRef<TerminalHandle | null>(null);
  // Logs picker needs non-Traefik services too (e.g. postgres) so opt-in
  const {
    data: services,
    isLoading,
    isError,
  } = useServices({ withInternal: true });
  const [live, setLive] = useState(true);
  const handleCopy = () => {
    const text = termApiRef.current?.copyText() ?? "";
    if (!text) return;
    if (navigator.clipboard?.writeText) {
      return navigator.clipboard.writeText(text);
    }
    // Fallback for contexts without the async clipboard API.
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.style.position = "fixed";
    ta.style.opacity = "0";
    document.body.appendChild(ta);
    ta.select();
    try {
      document.execCommand("copy");
    } finally {
      document.body.removeChild(ta);
    }
  };

  const groups = useMemo(() => groupServices(services ?? []), [services]);
  const projectNames = useMemo(() => groups.map((g) => g.project), [groups]);

  // Effective project: URL > localStorage > first project
  const effectiveProject = useMemo(() => {
    if (search.project && projectNames.includes(search.project))
      return search.project;
    const stored = readStored();
    if (stored.project && projectNames.includes(stored.project))
      return stored.project;
    return projectNames[0];
  }, [search.project, projectNames]);

  const filteredGroups = useMemo(() => {
    if (!effectiveProject) return groups;
    const found = groups.filter((g) => g.project === effectiveProject);
    return found.length > 0 ? found : groups;
  }, [groups, effectiveProject]);

  // All services in the effective project (flattened)
  const projectServices = useMemo(() => {
    if (!filteredGroups.length) return [];
    return filteredGroups.flatMap((g) =>
      g.worktrees.flatMap((w) => w.services)
    );
  }, [filteredGroups]);

  const active = useMemo(() => {
    const list =
      projectServices.length > 0 ? projectServices : (services ?? []);
    if (list.length === 0) return null;
    const byContainer = new Map(list.map((s) => [s.container, s]));
    const byService = new Map(list.map((s) => [s.service, s]));
    const toActive = (svc: (typeof list)[number]) => ({
      container: svc.container,
      service: svc.service,
      pid: svc.pid ?? null,
      label: svc.service,
      project: svc.project,
    });
    // Prefer URL service if it belongs to this project
    if (search.service) {
      const svc =
        byContainer.get(search.service) ?? byService.get(search.service);
      if (svc) return toActive(svc);
      // If service not in this project, try global list (maybe project param stale)
      const global = services ?? [];
      const gByContainer = new Map(global.map((s) => [s.container, s]));
      const gByService = new Map(global.map((s) => [s.service, s]));
      const gs =
        gByContainer.get(search.service) ?? gByService.get(search.service);
      if (gs) return toActive(gs as (typeof list)[number]);
    }
    // Try stored service if it belongs to effective project
    const stored = readStored();
    if (stored.service) {
      const svc =
        byContainer.get(stored.service) ?? byService.get(stored.service);
      if (svc) return toActive(svc);
    }
    return toActive(list[0]);
  }, [projectServices, services, search.service]);

  // Sync URL and localStorage when effective selection is known
  useEffect(() => {
    if (!groups.length || !active) return;
    const targetProject = active.project;
    const targetService = active.container;
    const stored = readStored();
    // Write to storage if changed
    if (stored.project !== targetProject || stored.service !== targetService) {
      writeStored(targetProject, targetService);
    }
    // Sync URL if missing or mismatched project/service (preserve view)
    if (search.project !== targetProject || search.service !== targetService) {
      // Avoid navigating if both are already matching after we just wrote
      // Use replace to not pollute history when restoring from storage
      void navigate({
        to: "/logs",
        search: {
          project: targetProject,
          service: targetService,
          view: search.view,
        },
        replace: true,
      });
    }
  }, [groups, active, search.project, search.service, search.view, navigate]);

  const handleSelectService = (container: string) => {
    const svc = (services ?? []).find((s) => s.container === container);
    const project = svc?.project ?? effectiveProject;
    if (project && container) writeStored(project, container);
    void navigate({
      to: "/logs",
      search: {
        project: project ?? effectiveProject,
        service: container,
        view: search.view,
      },
    });
  };

  const handleSelectProject = (project: string) => {
    const g = groups.find((x) => x.project === project);
    const first = g?.worktrees.flatMap((w) => w.services)[0];
    const svcContainer = first?.container ?? "";
    if (project && svcContainer) writeStored(project, svcContainer);
    void navigate({
      to: "/logs",
      search: {
        project,
        service: svcContainer || undefined,
        view: search.view,
      },
    });
  };

  // Logs (SSE) is the default: fast on slow networks, read-only. PTY is
  // opt-in via the toggle (and unavailable for docker containers, which have
  // no fog PTY — they always use `docker logs`).
  const viewMode: ViewMode = search.view ?? readMode();
  const isDocker = active?.pid == null;
  const showTerminal = viewMode === "terminal" && !isDocker;

  const handleViewMode = (mode: ViewMode) => {
    writeMode(mode);
    void navigate({
      to: "/logs",
      search: {
        project: search.project ?? effectiveProject,
        service: search.service ?? active?.container,
        // Keep shared URLs clean: the default (logs) omits the param.
        view: mode === "terminal" ? "terminal" : undefined,
      },
    });
  };

  return (
    <div className="flex min-w-0 flex-col gap-3 lg:h-[calc(100dvh-8rem)] lg:flex-row lg:gap-6">
      {/* Main terminal / logs */}
      <div className="flex min-w-0 flex-1 flex-col gap-3 lg:min-h-0">
        <div className="flex flex-wrap items-center gap-2">
          <div className="inline-flex rounded-md border p-1">
            <Button
              variant={showTerminal ? "ghost" : "default"}
              size="sm"
              className="h-7 font-mono text-xs"
              onClick={() => handleViewMode("logs")}
            >
              Logs (SSE)
            </Button>
            <Button
              variant={showTerminal ? "default" : "ghost"}
              size="sm"
              className={cn("h-7 font-mono text-xs", isDocker && "opacity-50")}
              onClick={() => handleViewMode("terminal")}
              disabled={isDocker}
              title={
                isDocker
                  ? "PTY not available for docker containers"
                  : "Interactive PTY shell (bidirectional) via WebSocket"
              }
            >
              Terminal (PTY)
            </Button>
          </div>
          <span className="text-muted-foreground font-mono text-xs">
            {showTerminal
              ? `interactive shell${active ? ` — ${active.service} workdir` : " — ephemeral"}`
              : "read-only stream from docker/fog"}
          </span>
        </div>
        {isDocker ? (
          <div className="text-muted-foreground flex items-center gap-2 font-mono text-xs">
            <span className="rounded-full bg-amber-500/20 px-2 py-0.5 text-amber-600">
              docker logs — read-only
            </span>
            <span>
              PTY not available for container — streaming `docker logs`
            </span>
          </div>
        ) : (
          showTerminal && (
            <label className="flex items-center gap-2 text-sm">
              <input
                type="checkbox"
                checked={live}
                onChange={(e) => setLive(e.target.checked)}
                disabled={!active}
                className="rounded"
              />
              <span className={active ? "" : "text-muted-foreground"}>
                Live — same PTY as TUI (mirror service, bidirectional).
                Unchecked = fresh shell in service workdir.
              </span>
            </label>
          )
        )}
        {showTerminal ? (
          <Suspense fallback={<LoadingState label="Loading terminal…" />}>
            <TerminalView
              key={`${active?.service ?? "__shell__"}:${live ? "live" : "cwd"}`}
              ref={termApiRef}
              service={active?.service}
              live={live && !!active}
              className="flex min-h-[320px] flex-1"
            />
          </Suspense>
        ) : (
          <Suspense fallback={<LoadingState label="Loading logs…" />}>
            <LogView
              key={`${active?.container ?? "__none__"}`}
              ref={termApiRef}
              container={active?.container ?? null}
              pid={active?.pid ?? null}
              service={active?.service ?? null}
              className="flex min-h-[320px] flex-1"
            />
          </Suspense>
        )}
        <TerminalKeypad
          input={showTerminal}
          enabled={!!active}
          onDispatch={(init, keyCode) =>
            termApiRef.current?.dispatchKey(init, keyCode)
          }
          onRaw={(data) => termApiRef.current?.sendRaw(data)}
          onScroll={(amount) => termApiRef.current?.scroll(amount)}
          onCopy={handleCopy}
          className="lg:hidden"
        />
      </div>

      {/* Right sidebar */}
      <ServiceSidebar
        groups={filteredGroups}
        allGroups={groups}
        selectedProject={effectiveProject}
        onSelectProject={handleSelectProject}
        activeContainer={active?.container}
        onSelect={handleSelectService}
        isLoading={isLoading}
        isError={isError}
      />
    </div>
  );
}
