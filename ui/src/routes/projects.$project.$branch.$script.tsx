import { Fragment } from "react";
import { createFileRoute, Link } from "@tanstack/react-router";
import { ExternalLink, X } from "lucide-react";

import { useInstanceKillState, useServices, useStatus } from "@/lib/hooks";
import {
  DEFAULT_WORKTREE,
  branchInstances,
  buildInstanceViews,
  endpointViews,
  findBranch,
  findProject,
  findScriptInstances,
  findWorktree,
  groupByBranch,
  groupServices,
  type InstanceServiceView,
  type InstanceView,
} from "@/lib/services";
import { ErrorState, LoadingState, PageHeader } from "@/components/page-state";
import { StatusBadge } from "@/components/status-badge";
import { ServiceUrl } from "@/components/service-url";
import { ServiceActions } from "@/components/service-actions";
import {
  ServiceTerminal,
  type TerminalMode,
} from "@/components/terminal/ServiceTerminal";
import { buttonVariants } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import {
  Sheet,
  SheetContent,
  SheetHeader,
  SheetTitle,
} from "@/components/ui/sheet";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { cn, toDisplayEndpointUrl } from "@/lib/utils";

export const Route = createFileRoute("/projects/$project/$branch/$script")({
  validateSearch: (
    search: Record<string, unknown>
  ): { service?: string; pid?: number; view?: TerminalMode } => ({
    service: typeof search.service === "string" ? search.service : undefined,
    pid:
      typeof search.pid === "number"
        ? search.pid
        : typeof search.pid === "string" && search.pid !== ""
          ? Number(search.pid)
          : undefined,
    view:
      search.view === "terminal" || search.view === "logs"
        ? (search.view as TerminalMode)
        : undefined,
  }),
  component: ScriptServicesPage,
});

/** One service row's action controls (disabled for the synthetic instance). */
function RowActions({
  inst,
  svc,
}: {
  inst: InstanceView;
  svc: InstanceServiceView;
}) {
  const { killing } = useInstanceKillState(inst.pid, inst.script);
  if (inst.pid <= 0) return null;
  return (
    <ServiceActions
      pid={inst.pid}
      name={svc.name}
      running={svc.running}
      killing={killing}
      className="flex flex-col items-start gap-1"
    />
  );
}

/**
 * Declared endpoints of a parent service, rendered as a nested
 * list with per-endpoint health and links. Renders nothing for the common case
 * of a service with a single implicit endpoint.
 */
function EndpointList({ svc }: { svc: InstanceServiceView }) {
  const endpoints = endpointViews(svc);
  if (endpoints.length === 0) return null;
  return (
    <ul className="space-y-1.5">
      {endpoints.map((endpoint) => {
        const displayUrl = toDisplayEndpointUrl(endpoint.url, endpoint.port);
        return (
          <li
            key={endpoint.name}
            className="flex flex-wrap items-center gap-x-2 gap-y-1 font-mono text-xs"
          >
            <span className="text-muted-foreground">↳</span>
            <span className="text-foreground">{endpoint.name}</span>
            <StatusBadge status={endpoint.health} />
            {displayUrl ? (
              <a
                href={displayUrl}
                target="_blank"
                rel="noreferrer"
                title={displayUrl}
                onClick={(e) => e.stopPropagation()}
                className="text-primary min-w-0 truncate underline-offset-4 hover:underline"
              >
                {displayUrl}
              </a>
            ) : endpoint.port ? (
              <span className="text-muted-foreground">:{endpoint.port}</span>
            ) : null}
          </li>
        );
      })}
    </ul>
  );
}

function ScriptServicesPage() {
  const { project, branch, script } = Route.useParams();
  const search = Route.useSearch();
  const navigate = Route.useNavigate();
  const {
    data: services,
    isLoading,
    isError,
    error,
  } = useServices({ withInternal: true });
  const { data: status, isLoading: statusLoading } = useStatus();

  const allBranches = groupByBranch(
    buildInstanceViews(status?.instances ?? [], services ?? [])
  );
  const bucket = findBranch(allBranches, project, branch);
  const projectBucket = findProject(groupServices(services ?? []), project);
  const legacy =
    bucket || !projectBucket ? null : findWorktree(projectBucket, branch);

  const scriptInstances = findScriptInstances(
    branchInstances(bucket, legacy, projectBucket?.project ?? project),
    script
  );

  const scoped =
    search.pid != null
      ? scriptInstances.filter((i) => i.pid === search.pid)
      : scriptInstances;
  const instances: InstanceView[] =
    scoped.length > 0 ? scoped : scriptInstances;

  const label = (bucket?.worktree ?? legacy?.worktree) || DEFAULT_WORKTREE;
  const projectName = bucket?.project ?? projectBucket?.project ?? project;
  const multi = scriptInstances.length > 1 || search.pid != null;

  const selected = search.service
    ? (instances
        .flatMap((inst) => inst.services.map((svc) => ({ inst, svc })))
        .find(
          (x) =>
            x.svc.name === search.service &&
            (search.pid == null || x.inst.pid === search.pid)
        ) ?? null)
    : null;

  const mode: TerminalMode = search.view ?? "logs";

  const openService = (pid: number, name: string) => {
    void navigate({
      search: (prev) => ({ ...prev, service: name, pid }),
      replace: true,
    });
  };

  const closeDrawer = () => {
    void navigate({
      search: (prev) => ({ ...prev, service: undefined }),
      replace: true,
    });
  };

  const setMode = (next: TerminalMode) => {
    void navigate({
      search: (prev) => ({
        ...prev,
        view: next === "terminal" ? "terminal" : undefined,
      }),
      replace: true,
    });
  };

  if (isLoading || statusLoading) {
    return <LoadingState label="Loading script…" />;
  }
  if (isError) {
    return <ErrorState message={error?.message} />;
  }

  if (scriptInstances.length === 0) {
    return (
      <Card>
        <CardContent className="py-10 text-center">
          <p className="font-mono text-sm">
            <span className="text-muted-foreground">script </span>
            <span className="text-foreground">{script}</span>
            <span className="text-muted-foreground">
              {" "}
              has no running services.
            </span>
          </p>
          <Link
            to="/projects/$project/$branch"
            params={{ project, branch }}
            className="text-primary mt-3 inline-block font-mono text-xs underline-offset-4 hover:underline"
          >
            ← back to scripts
          </Link>
        </CardContent>
      </Card>
    );
  }

  const containerOf = (svc: InstanceServiceView, pid: number) =>
    svc.service?.container ?? `fog-${pid}-${svc.name}`;

  return (
    <div className="min-w-0 space-y-4">
      <PageHeader title={script} />

      {multi ? (
        <div className="flex flex-wrap gap-1.5">
          <button
            type="button"
            onClick={() =>
              void navigate({
                search: (prev) => ({
                  ...prev,
                  pid: undefined,
                  service: undefined,
                }),
                replace: true,
              })
            }
            className={cn(
              buttonVariants({
                variant: search.pid == null ? "default" : "outline",
                size: "xs",
              }),
              "font-mono"
            )}
          >
            All instances
          </button>
          {scriptInstances.map((inst) => (
            <button
              key={inst.pid}
              type="button"
              onClick={() =>
                void navigate({
                  search: (prev) => ({
                    ...prev,
                    pid: inst.pid,
                    service: undefined,
                  }),
                  replace: true,
                })
              }
              className={cn(
                buttonVariants({
                  variant: search.pid === inst.pid ? "default" : "outline",
                  size: "xs",
                }),
                "font-mono"
              )}
            >
              pid {inst.pid}
            </button>
          ))}
        </div>
      ) : null}

      {instances.map((inst) => (
        <section key={inst.pid} className="space-y-2">
          {multi ? (
            <div className="text-muted-foreground flex items-center gap-2 font-mono text-[11px]">
              <span className="text-foreground font-semibold">
                pid {inst.pid}
              </span>
              <span className="text-muted-foreground/60">
                {inst.services.filter((s) => s.running).length}/
                {inst.services.length} running
              </span>
            </div>
          ) : null}

          {inst.services.length === 0 ? (
            <Card>
              <CardContent className="text-muted-foreground py-6 text-center font-mono text-xs">
                No services in this instance.
              </CardContent>
            </Card>
          ) : (
            <>
              {/* Mobile: tap-friendly cards */}
              <div className="space-y-2 lg:hidden">
                {inst.services.map((svc) => (
                  <div
                    key={svc.name}
                    onClick={() => openService(inst.pid, svc.name)}
                    className={cn(
                      "border-border cursor-pointer rounded-lg border p-3 transition-colors",
                      selected?.svc.name === svc.name &&
                        selected?.inst.pid === inst.pid &&
                        "border-primary/50"
                    )}
                  >
                    <div className="flex items-center justify-between gap-2">
                      <span className="min-w-0 truncate font-mono text-sm font-medium">
                        {svc.name}
                      </span>
                      <StatusBadge
                        status={svc.running ? "running" : "stopped"}
                      />
                    </div>
                    {svc.service ? (
                      <div className="mt-2">
                        <ServiceUrl
                          svc={svc.service}
                          linkClassName="text-xs break-all"
                        />
                      </div>
                    ) : null}
                    {svc.service && svc.service.ports.length > 0 ? (
                      <div className="text-muted-foreground mt-1 font-mono text-xs break-all">
                        {svc.service.ports.join(", ")}
                      </div>
                    ) : null}
                    <EndpointList svc={svc} />
                    <div
                      onClick={(e) => e.stopPropagation()}
                      className="mt-3 flex flex-wrap items-center gap-2 border-t pt-3"
                    >
                      <RowActions inst={inst} svc={svc} />
                      <button
                        type="button"
                        onClick={() => openService(inst.pid, svc.name)}
                        className={cn(
                          buttonVariants({ variant: "outline", size: "sm" }),
                          "font-mono"
                        )}
                      >
                        Logs / Terminal
                      </button>
                    </div>
                  </div>
                ))}
              </div>

              {/* Desktop: dense, clickable table */}
              <Card className="hidden overflow-hidden py-0 lg:block">
                <div className="overflow-auto">
                  <Table className="min-w-[820px]">
                    <TableHeader className="[&_th]:bg-card sticky top-0 z-10 [&_th]:shadow-[inset_0_-1px_0_var(--color-border)]">
                      <TableRow>
                        <TableHead className="w-44">Service</TableHead>
                        <TableHead className="w-24">Status</TableHead>
                        <TableHead className="w-[30%]">URL</TableHead>
                        <TableHead>Ports</TableHead>
                        <TableHead className="text-right">Actions</TableHead>
                      </TableRow>
                    </TableHeader>
                    <TableBody>
                      {inst.services.map((svc) => {
                        const endpoints = endpointViews(svc);
                        return (
                          <Fragment key={svc.name}>
                            <TableRow
                              onClick={() => openService(inst.pid, svc.name)}
                              aria-selected={
                                selected?.svc.name === svc.name &&
                                selected?.inst.pid === inst.pid
                              }
                              className={cn(
                                "cursor-pointer",
                                selected?.svc.name === svc.name &&
                                  selected?.inst.pid === inst.pid &&
                                  "bg-accent/60 hover:bg-accent/60"
                              )}
                            >
                              <TableCell className="font-mono font-medium">
                                {svc.name}
                              </TableCell>
                              <TableCell>
                                <StatusBadge
                                  status={svc.running ? "running" : "stopped"}
                                />
                              </TableCell>
                              <TableCell>
                                {svc.service ? (
                                  <ServiceUrl svc={svc.service} />
                                ) : (
                                  "—"
                                )}
                              </TableCell>
                              <TableCell className="text-muted-foreground font-mono">
                                {svc.service && svc.service.ports.length
                                  ? svc.service.ports.join(", ")
                                  : "—"}
                              </TableCell>
                              <TableCell
                                onClick={(e) => e.stopPropagation()}
                                className="text-right"
                              >
                                <div className="flex justify-end">
                                  <RowActions inst={inst} svc={svc} />
                                </div>
                              </TableCell>
                            </TableRow>
                            {endpoints.length > 0 ? (
                              <TableRow className="hover:bg-transparent">
                                <TableCell colSpan={5} className="pt-0">
                                  <EndpointList svc={svc} />
                                </TableCell>
                              </TableRow>
                            ) : null}
                          </Fragment>
                        );
                      })}
                    </TableBody>
                  </Table>
                </div>
              </Card>
            </>
          )}
        </section>
      ))}

      {/* Bottom drawer: logs (SSE) with a PTY toggle */}
      <Sheet
        open={!!selected}
        onOpenChange={(open) => {
          if (!open) closeDrawer();
        }}
      >
        <SheetContent
          side="bottom"
          showCloseButton={false}
          className="h-[85dvh] max-h-[85dvh] gap-0 rounded-t-2xl p-0"
        >
          {selected ? (
            <>
              <div
                aria-hidden
                className="bg-muted-foreground/30 mx-auto mt-2 h-1 w-10 shrink-0 rounded-full"
              />
              <SheetHeader className="border-border flex-row items-center gap-2 border-b px-4 py-3">
                <div className="flex min-w-0 flex-col">
                  <div className="flex min-w-0 items-center gap-2">
                    <SheetTitle className="truncate font-mono text-sm">
                      {selected.svc.name}
                    </SheetTitle>
                    <StatusBadge
                      status={selected.svc.running ? "running" : "stopped"}
                    />
                  </div>
                  <span className="text-muted-foreground truncate font-mono text-[11px]">
                    {projectName} @ {label} · {script} pid {selected.inst.pid}
                  </span>
                </div>
                <div className="ml-auto flex shrink-0 flex-wrap items-center justify-end gap-1">
                  <RowActions inst={selected.inst} svc={selected.svc} />
                  <Link
                    to="/logs"
                    search={{
                      project: projectName,
                      service:
                        selected.svc.service?.container ?? selected.svc.name,
                      view: search.view,
                    }}
                    title="Open full-page terminal"
                    className={cn(
                      buttonVariants({ variant: "ghost", size: "sm" }),
                      "font-mono"
                    )}
                  >
                    <ExternalLink className="size-4" />
                    <span className="hidden sm:inline">Full page</span>
                  </Link>
                  <button
                    type="button"
                    onClick={closeDrawer}
                    aria-label="Close"
                    className={cn(
                      buttonVariants({ variant: "ghost", size: "icon-sm" })
                    )}
                  >
                    <X className="size-4" />
                  </button>
                </div>
              </SheetHeader>
              <div className="flex min-h-0 flex-1 flex-col p-3">
                <ServiceTerminal
                  key={`${selected.inst.pid}:${selected.svc.name}`}
                  active={{
                    container: containerOf(selected.svc, selected.inst.pid),
                    service: selected.svc.name,
                    pid: selected.svc.service?.pid ?? null,
                  }}
                  mode={mode}
                  onModeChange={setMode}
                  showModeToggle={selected.svc.service?.pid != null}
                />
              </div>
            </>
          ) : null}
        </SheetContent>
      </Sheet>
    </div>
  );
}
