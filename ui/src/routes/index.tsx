import { Fragment, useEffect, useRef, useState } from "react";
import { createFileRoute } from "@tanstack/react-router";
import { Check, Copy, GitBranch } from "lucide-react";

import { useServices } from "@/lib/hooks";
import type { Service } from "@/lib/api";
import { PageHeader, LoadingState, ErrorState } from "@/components/page-state";
import { StatusBadge } from "@/components/status-badge";
import { BrandCloud } from "@/components/brand-cloud";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { toDisplayUrl, isDnsOnly, isLocalHost, getRequestHostname } from "@/lib/utils";

export const Route = createFileRoute("/")({
  component: ServicesPage,
});

/** Label used when a service has no git worktree (started from the main checkout). */
const DEFAULT_WORKTREE = "default";

interface WorktreeBucket {
  /** Git worktree name; "" means services started in the default checkout. */
  worktree: string;
  services: Service[];
}

interface ProjectBucket {
  project: string;
  worktrees: WorktreeBucket[];
  total: number;
}

/**
 * Group services by project (git-derived from the repo) and within each
 * project by worktree. The default checkout ("") sorts first, remaining
 * worktrees alphabetically, and services by name within a worktree.
 */
function groupByProjectAndWorktree(services: Service[]): ProjectBucket[] {
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
    let total = 0;
    for (const [worktree, list] of byWorktree) {
      list.sort((a, b) => a.service.localeCompare(b.service));
      worktrees.push({ worktree, services: list });
      total += list.length;
    }
    worktrees.sort((a, b) => {
      if (a.worktree === "") return -1;
      if (b.worktree === "") return 1;
      return a.worktree.localeCompare(b.worktree);
    });
    projects.push({ project, worktrees, total });
  }
  projects.sort((a, b) => a.project.localeCompare(b.project));
  return projects;
}

/** Copy text to the clipboard, falling back to a hidden textarea (non-secure contexts). */
async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    try {
      const ta = document.createElement("textarea");
      ta.value = text;
      ta.style.position = "fixed";
      ta.style.opacity = "0";
      document.body.appendChild(ta);
      ta.select();
      const ok = document.execCommand("copy");
      document.body.removeChild(ta);
      return ok;
    } catch {
      return false;
    }
  }
}

/** Icon button that copies a URL and shows a checkmark for a moment. */
function CopyUrlButton({ url }: { url: string }) {
  const [copied, setCopied] = useState(false);
  const timer = useRef<number | null>(null);

  useEffect(
    () => () => {
      if (timer.current !== null) window.clearTimeout(timer.current);
    },
    []
  );

  const onCopy = async () => {
    if (await copyText(url)) {
      setCopied(true);
      if (timer.current !== null) window.clearTimeout(timer.current);
      timer.current = window.setTimeout(() => setCopied(false), 1_500);
    }
  };

  return (
    <Button
      variant="ghost"
      size="icon-xs"
      type="button"
      onClick={onCopy}
      aria-label={copied ? "Copied" : `Copy ${url}`}
      title={copied ? "Copied" : "Copy URL"}
      className="text-muted-foreground hover:text-foreground shrink-0"
    >
      {copied ? <Check className="text-primary" /> : <Copy />}
    </Button>
  );
}

/** Data-dense summary strip of live numbers above the table. */
function StatsStrip({ services }: { services: Service[] }) {
  const projects = new Set(services.map((s) => s.project));
  const worktrees = new Set(
    services.map((s) => s.worktree || DEFAULT_WORKTREE)
  );
  const ports = new Set(services.flatMap((s) => s.ports));
  const stats = [
    { label: "services", value: services.length },
    { label: "projects", value: projects.size },
    { label: "worktrees", value: worktrees.size },
    { label: "ports", value: ports.size },
  ];

  return (
    <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-4">
      {stats.map((stat) => (
        <div
          key={stat.label}
          className="terminal-surface rounded-lg px-3 py-2.5"
        >
          <div className="text-primary font-mono text-lg font-semibold">
            {stat.value}
          </div>
          <div className="text-muted-foreground font-mono text-[10px] tracking-wider uppercase">
            {stat.label}
          </div>
        </div>
      ))}
    </div>
  );
}

function ServicesTable({ services }: { services: Service[] }) {
  const groups = groupByProjectAndWorktree(services);

  return (
    <Card className="gap-0 overflow-x-auto py-0">
      <div className="max-h-[70vh] overflow-auto">
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
            {groups.map((project) => (
              <Fragment key={project.project}>
                {/* Project group row */}
                <TableRow className="border-primary/20 bg-secondary/40 hover:bg-secondary/40 border-t">
                  <TableCell
                    colSpan={4}
                    className="border-primary/50 border-l-2 px-3 py-1.5"
                  >
                    <div className="flex items-center justify-between gap-2">
                      <div className="flex min-w-0 items-center gap-2 font-mono text-sm font-semibold">
                        <BrandCloud
                          variant="mono"
                          className="text-primary/70 size-3.5 shrink-0"
                        />
                        <span className="truncate">{project.project}</span>
                      </div>
                      <span className="border-primary/30 bg-primary/10 text-primary shrink-0 rounded-full border px-2 font-mono text-[10px]">
                        {project.total}
                      </span>
                    </div>
                  </TableCell>
                </TableRow>

                {project.worktrees.map((worktree) => (
                  <Fragment key={`${project.project}:${worktree.worktree}`}>
                    {/* Worktree sub-group row */}
                    <TableRow className="hover:bg-transparent">
                      <TableCell colSpan={4} className="px-3 py-1 pl-8">
                        <div className="text-muted-foreground flex items-center gap-1.5 font-mono text-[11px] tracking-wider uppercase">
                          <GitBranch className="size-3 shrink-0" />
                          <span className="truncate">
                            {worktree.worktree || DEFAULT_WORKTREE}
                          </span>
                          <span className="text-muted-foreground/60">
                            {worktree.services.length}
                          </span>
                        </div>
                      </TableCell>
                    </TableRow>

                    {/* Service rows */}
                    {worktree.services.map((svc) => (
                      <TableRow
                        key={`${svc.project}/${svc.worktree}/${svc.service}`}
                      >
                        <TableCell className="font-mono font-medium">
                          {svc.service}
                        </TableCell>
                        <TableCell>
                          <StatusBadge status={svc.status} />
                        </TableCell>
                        <TableCell>
                          {svc.url
                            ? (() => {
                                const displayUrl = toDisplayUrl(svc.url, svc.ports);
                                const dnsOnly =
                                  isDnsOnly(svc.url, svc.ports) &&
                                  !isLocalHost(getRequestHostname());
                                return (
                                  <div className="flex min-w-0 items-center gap-1">
                                    <a
                                      href={displayUrl}
                                      target="_blank"
                                      rel="noreferrer"
                                      title={displayUrl}
                                      className="text-primary max-w-[320px] min-w-0 truncate font-mono underline-offset-4 hover:underline"
                                    >
                                      {displayUrl}
                                    </a>
                                    <CopyUrlButton url={displayUrl} />
                                    {dnsOnly ? (
                                      <span
                                        title="Traefik-only, no host port published — reachable via DNS (*.gems/*.red-fox) or add ports: [8080] in compose"
                                        className="border-warning/30 bg-warning/10 text-warning shrink-0 rounded-full border px-1.5 py-0.5 font-mono text-[9px] tracking-wide uppercase"
                                      >
                                        DNS
                                      </span>
                                    ) : null}
                                  </div>
                                );
                              })()
                            : (
                            <span className="text-muted-foreground font-mono">
                              —
                            </span>
                          )}
                        </TableCell>
                        <TableCell className="text-muted-foreground font-mono">
                          {svc.ports.length ? svc.ports.join(", ") : "—"}
                        </TableCell>
                      </TableRow>
                    ))}
                  </Fragment>
                ))}
              </Fragment>
            ))}
          </TableBody>
        </Table>
      </div>
    </Card>
  );
}

/**
 * Mobile (<lg) fallback for the services table. The wide table only fits once
 * the sidebar has room, so below that we render grouped cards instead — the
 * same project/worktree grouping as the table, without forcing horizontal
 * scrolling on narrow viewports.
 */
function ServiceCardList({ services }: { services: Service[] }) {
  const groups = groupByProjectAndWorktree(services);

  return (
    <div className="space-y-4">
      {groups.map((project) => (
        <Card key={project.project} className="gap-0 py-0">
          <div className="border-primary/50 bg-secondary/40 border-b border-l-2 px-3 py-1.5">
            <div className="flex items-center justify-between gap-2">
              <div className="flex min-w-0 items-center gap-2 font-mono text-sm font-semibold">
                <BrandCloud
                  variant="mono"
                  className="text-primary/70 size-3.5 shrink-0"
                />
                <span className="truncate">{project.project}</span>
              </div>
              <span className="border-primary/30 bg-primary/10 text-primary shrink-0 rounded-full border px-2 font-mono text-[10px]">
                {project.total}
              </span>
            </div>
          </div>

          <div className="flex flex-col gap-4 px-3 py-3">
            {project.worktrees.map((worktree) => (
              <div key={`${project.project}:${worktree.worktree}`}>
                <div className="text-muted-foreground flex items-center gap-1.5 font-mono text-[11px] tracking-wider uppercase">
                  <GitBranch className="size-3 shrink-0" />
                  <span className="truncate">
                    {worktree.worktree || DEFAULT_WORKTREE}
                  </span>
                  <span className="text-muted-foreground/60">
                    {worktree.services.length}
                  </span>
                </div>

                <div className="mt-1.5 space-y-2">
                  {worktree.services.map((svc) => (
                    <div
                      key={`${svc.project}/${svc.worktree}/${svc.service}`}
                      className="border-border rounded-md border px-3 py-2"
                    >
                      <div className="flex items-center justify-between gap-2">
                        <span className="min-w-0 truncate font-mono text-sm font-medium">
                          {svc.service}
                        </span>
                        <StatusBadge status={svc.status} />
                      </div>
                      {svc.url
                        ? (() => {
                            const displayUrl = toDisplayUrl(svc.url, svc.ports);
                            const dnsOnly =
                              isDnsOnly(svc.url, svc.ports) &&
                              !isLocalHost(getRequestHostname());
                            return (
                              <div className="mt-1.5 flex items-center gap-1">
                                <a
                                  href={displayUrl}
                                  target="_blank"
                                  rel="noreferrer"
                                  title={displayUrl}
                                  className="text-primary min-w-0 font-mono text-xs break-all underline-offset-4 hover:underline"
                                >
                                  {displayUrl}
                                </a>
                                <CopyUrlButton url={displayUrl} />
                                {dnsOnly ? (
                                  <span
                                    title="Traefik-only, no host port published — reachable via DNS (*.gems/*.red-fox) or add ports: [8080] in compose"
                                    className="border-warning/30 bg-warning/10 text-warning shrink-0 rounded-full border px-1.5 py-0.5 font-mono text-[9px] tracking-wide uppercase"
                                  >
                                    DNS
                                  </span>
                                ) : null}
                              </div>
                            );
                          })()
                        : (
                        <div className="text-muted-foreground mt-1.5 font-mono text-xs">
                          —
                        </div>
                      )}
                      {svc.ports.length > 0 ? (
                        <div className="text-muted-foreground mt-1.5 font-mono text-xs break-all">
                          {svc.ports.join(", ")}
                        </div>
                      ) : null}
                    </div>
                  ))}
                </div>
              </div>
            ))}
          </div>
        </Card>
      ))}
    </div>
  );
}

function ServicesPage() {
  const { data, isLoading, isError, error } = useServices();
  const running = data?.filter((s) => s.status === "running").length ?? 0;

  return (
    <div className="min-w-0 space-y-6">
      <PageHeader
        title="Services"
        description="Docker-discovered containers managed by fog, grouped by project and worktree."
        actions={
          data && data.length > 0 ? (
            <div className="border-primary/30 bg-primary/10 text-primary flex items-center gap-1.5 rounded-full border px-3 py-1 font-mono text-xs whitespace-nowrap">
              <span className="bg-primary size-1.5 shrink-0 animate-pulse rounded-full" />
              {running}/{data.length} running
            </div>
          ) : undefined
        }
      />

      {isLoading ? (
        <LoadingState label="Loading services…" />
      ) : isError ? (
        <ErrorState message={error?.message} />
      ) : !data || data.length === 0 ? (
        <Card>
          <CardContent className="py-10 text-center">
            <div className="text-muted-foreground font-mono text-sm">
              <span className="text-primary">$</span> docker ps
              <span className="text-muted-foreground/60">
                {" "}
                # no services running
              </span>
            </div>
            <p className="text-muted-foreground/70 mt-2 text-xs">
              Start a script to see its services appear here.
            </p>
          </CardContent>
        </Card>
      ) : (
        <>
          <StatsStrip services={data} />
          <div className="hidden lg:block">
            <ServicesTable services={data} />
          </div>
          <div className="lg:hidden">
            <ServiceCardList services={data} />
          </div>
        </>
      )}
    </div>
  );
}
