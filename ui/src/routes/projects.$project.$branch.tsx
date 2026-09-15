import { createFileRoute, Link } from "@tanstack/react-router";
import { ExternalLink, X } from "lucide-react";

import { useServices } from "@/lib/hooks";
import {
  DEFAULT_WORKTREE,
  findProject,
  findWorktree,
  groupServices,
} from "@/lib/services";
import { PageHeader } from "@/components/page-state";
import { StatusBadge } from "@/components/status-badge";
import { ServiceUrl } from "@/components/service-url";
import { ServiceTerminal, type TerminalMode } from "@/components/terminal/ServiceTerminal";
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
import { cn } from "@/lib/utils";

export const Route = createFileRoute("/projects/$project/$branch")({
  validateSearch: (
    search: Record<string, unknown>
  ): { service?: string; view?: TerminalMode } => ({
    service: typeof search.service === "string" ? search.service : undefined,
    view:
      search.view === "terminal" || search.view === "logs"
        ? (search.view as TerminalMode)
        : undefined,
  }),
  component: BranchServicesPage,
});

function BranchServicesPage() {
  const { project, branch } = Route.useParams();
  const search = Route.useSearch();
  const navigate = Route.useNavigate();
  const { data } = useServices({ withInternal: true });

  const bucket = findProject(groupServices(data ?? []), project);
  const worktree = bucket ? findWorktree(bucket, branch) : null;

  if (!bucket || !worktree) {
    return (
      <Card>
        <CardContent className="py-10 text-center">
          <p className="font-mono text-sm">
            <span className="text-muted-foreground">branch </span>
            <span className="text-foreground">{branch}</span>
            <span className="text-muted-foreground">
              {" "}
              has no running services.
            </span>
          </p>
          <Link
            to="/projects/$project"
            params={{ project }}
            className="text-primary mt-3 inline-block font-mono text-xs underline-offset-4 hover:underline"
          >
            ← back to branches
          </Link>
        </CardContent>
      </Card>
    );
  }

  const services = worktree.services;
  const label = worktree.worktree || DEFAULT_WORKTREE;
  const selected = search.service
    ? (services.find((s) => s.container === search.service) ?? null)
    : null;
  const mode: TerminalMode = search.view ?? "logs";

  const openService = (container: string) => {
    void navigate({
      search: (prev) => ({ ...prev, service: container }),
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

  return (
    <div className="min-w-0 space-y-4">
      <PageHeader
        title={label}
        description={`${services.length} ${services.length === 1 ? "service" : "services"} on ${bucket.project}. Tap a service to stream its logs or open a PTY.`}
      />

      {/* Mobile: tap-friendly cards */}
      <div className="space-y-2 lg:hidden">
        {services.map((svc) => (
          <div
            key={svc.container}
            onClick={() => openService(svc.container)}
            className={cn(
              "border-border cursor-pointer rounded-lg border p-3 transition-colors",
              selected?.container === svc.container && "border-primary/50"
            )}
          >
            <div className="flex items-center justify-between gap-2">
              <span className="min-w-0 truncate font-mono text-sm font-medium">
                {svc.service}
              </span>
              <StatusBadge status={svc.status} />
            </div>
            <div className="mt-2">
              <ServiceUrl svc={svc} linkClassName="text-xs break-all" />
            </div>
            {svc.ports.length > 0 ? (
              <div className="text-muted-foreground mt-1 font-mono text-xs break-all">
                {svc.ports.join(", ")}
              </div>
            ) : null}
            <button
              type="button"
              onClick={() => openService(svc.container)}
              className={cn(
                buttonVariants({ variant: "outline", size: "sm" }),
                "mt-3 w-full font-mono"
              )}
            >
              Logs / Terminal
            </button>
          </div>
        ))}
      </div>

      {/* Desktop: dense, clickable table */}
      <Card className="hidden overflow-hidden py-0 lg:block">
        <div className="overflow-auto">
          <Table className="min-w-[720px]">
            <TableHeader className="[&_th]:bg-card sticky top-0 z-10 [&_th]:shadow-[inset_0_-1px_0_var(--color-border)]">
              <TableRow>
                <TableHead className="w-48">Service</TableHead>
                <TableHead className="w-28">Status</TableHead>
                <TableHead className="w-[38%]">URL</TableHead>
                <TableHead>Ports</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {services.map((svc) => (
                <TableRow
                  key={svc.container}
                  onClick={() => openService(svc.container)}
                  aria-selected={selected?.container === svc.container}
                  className={cn(
                    "cursor-pointer",
                    selected?.container === svc.container &&
                      "bg-accent/60 hover:bg-accent/60"
                  )}
                >
                  <TableCell className="font-mono font-medium">
                    {svc.service}
                  </TableCell>
                  <TableCell>
                    <StatusBadge status={svc.status} />
                  </TableCell>
                  <TableCell>
                    <ServiceUrl svc={svc} />
                  </TableCell>
                  <TableCell className="text-muted-foreground font-mono">
                    {svc.ports.length ? svc.ports.join(", ") : "—"}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </div>
      </Card>

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
                      {selected.service}
                    </SheetTitle>
                    <StatusBadge status={selected.status} />
                  </div>
                  <span className="text-muted-foreground truncate font-mono text-[11px]">
                    {bucket.project} @ {label}
                  </span>
                </div>
                <div className="ml-auto flex shrink-0 items-center gap-1">
                  <Link
                    to="/logs"
                    search={{
                      project: bucket.project,
                      service: selected.container,
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
                  key={selected.container}
                  active={{
                    container: selected.container,
                    service: selected.service,
                    pid: selected.pid ?? null,
                  }}
                  mode={mode}
                  onModeChange={setMode}
                  showModeToggle={selected.pid != null}
                />
              </div>
            </>
          ) : null}
        </SheetContent>
      </Sheet>
    </div>
  );
}
