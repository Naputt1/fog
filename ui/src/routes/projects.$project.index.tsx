import { createFileRoute, Link } from "@tanstack/react-router";
import { ChevronRight, GitBranch, Terminal } from "lucide-react";

import { useServices, useStatus } from "@/lib/hooks";
import {
  DEFAULT_WORKTREE,
  branchStats,
  buildInstanceViews,
  findProject,
  groupByBranch,
  groupServices,
  worktreeParam,
  worktreeStats,
  type BranchBucket,
  type InstanceView,
  type ProjectBucket,
  type WorktreeBucket,
} from "@/lib/services";
import { PageHeader } from "@/components/page-state";
import { InstanceKillButton } from "@/components/instance-kill-button";
import { Card, CardContent } from "@/components/ui/card";
import { cn } from "@/lib/utils";

export const Route = createFileRoute("/projects/$project/")({
  component: BranchesPage,
});

/** Unique service names across a branch's instances, with an aggregate state. */
function branchServiceNames(
  bucket: BranchBucket
): { name: string; running: boolean }[] {
  const byName = new Map<string, boolean>();
  for (const inst of bucket.instances) {
    for (const svc of inst.services) {
      byName.set(svc.name, (byName.get(svc.name) ?? false) || svc.running);
    }
  }
  return [...byName.entries()]
    .map(([name, running]) => ({ name, running }))
    .sort((a, b) => a.name.localeCompare(b.name));
}

/** Groups a branch's instances by script, preserving script (and pid) order. */
function groupByScript(instances: InstanceView[]): [string, InstanceView[]][] {
  const map = new Map<string, InstanceView[]>();
  for (const inst of instances) {
    const list = map.get(inst.script);
    if (list) list.push(inst);
    else map.set(inst.script, [inst]);
  }
  return [...map.entries()].sort((a, b) => a[0].localeCompare(b[0]));
}

/** One instance row: link into the branch scoped to this pid, plus its Kill. */
function InstanceRow({
  project,
  branch,
  inst,
  running,
}: {
  project: string;
  branch: string;
  inst: InstanceView;
  running: number;
}) {
  return (
    <div className="border-border flex items-center gap-2 rounded-md border px-2.5 py-1.5">
      <Link
        to="/projects/$project/$branch"
        params={{ project, branch }}
        search={{ pid: inst.pid }}
        className="focus-visible:ring-ring/60 flex min-w-0 flex-1 items-center gap-2 rounded outline-none focus-visible:ring-2"
      >
        <Terminal
          className="text-muted-foreground size-3.5 shrink-0"
          aria-hidden
        />
        <span className="min-w-0 truncate font-mono text-xs font-medium">
          {inst.script}
        </span>
        <span className="text-muted-foreground shrink-0 font-mono text-[11px]">
          pid {inst.pid}
        </span>
        <span className="text-muted-foreground ml-auto shrink-0 font-mono text-[11px]">
          {running}/{inst.services.length}
        </span>
      </Link>
      <InstanceKillButton
        pid={inst.pid}
        script={inst.script}
        project={inst.project}
        branch={inst.branch}
      />
    </div>
  );
}

/** Instance-driven branch card: per-script/per-instance rows with Kill. */
function BranchCard({ bucket }: { bucket: BranchBucket }) {
  const stats = branchStats(bucket);
  const param = worktreeParam(bucket.worktree);
  const label = bucket.worktree || DEFAULT_WORKTREE;
  const services = branchServiceNames(bucket);
  const multi = bucket.instances.length > 1;
  const only = bucket.instances[0];

  return (
    <Card className="hover:border-primary/40 gap-0 py-0 transition-colors">
      <CardContent className="flex flex-col gap-3 p-4">
        <div className="flex items-center justify-between gap-2">
          <Link
            to="/projects/$project/$branch"
            params={{ project: bucket.project, branch: param }}
            search={{}}
            className="focus-visible:ring-ring/60 flex min-w-0 flex-1 items-center gap-2 rounded outline-none focus-visible:ring-2"
          >
            <GitBranch className="text-primary size-4 shrink-0" aria-hidden />
            <span className="min-w-0 truncate font-mono text-sm font-semibold">
              {label}
            </span>
            <ChevronRight
              className="text-muted-foreground ml-auto size-4 shrink-0"
              aria-hidden
            />
          </Link>
          {!multi && only ? (
            <InstanceKillButton
              pid={only.pid}
              script={only.script}
              project={only.project}
              branch={only.branch}
            />
          ) : null}
        </div>

        <div className="text-muted-foreground flex flex-wrap items-center gap-x-3 gap-y-1 font-mono text-[11px]">
          <span>{stats.total} services</span>
          <span className="text-primary">{stats.running} running</span>
          {multi ? <span>{bucket.instances.length} instances</span> : null}
        </div>

        {services.length > 0 ? (
          <div className="flex flex-wrap gap-1.5">
            {services.map((svc) => (
              <span
                key={svc.name}
                className={cn(
                  "border-border rounded-full border px-2 py-0.5 font-mono text-[10px]",
                  svc.running
                    ? "text-muted-foreground"
                    : "text-muted-foreground/50 border-dashed"
                )}
              >
                {svc.name}
              </span>
            ))}
          </div>
        ) : null}

        {stats.ports.length > 0 ? (
          <div className="text-muted-foreground/80 truncate font-mono text-[11px]">
            {stats.ports.join("  ")}
          </div>
        ) : null}

        {multi ? (
          <div className="border-border space-y-2 border-t pt-3">
            <div className="text-muted-foreground font-mono text-[10px] tracking-wider uppercase">
              Instances
            </div>
            {groupByScript(bucket.instances).map(([script, list]) => (
              <div key={script} className="space-y-1.5">
                {list.map((inst) => (
                  <InstanceRow
                    key={inst.pid}
                    project={bucket.project}
                    branch={param}
                    inst={inst}
                    running={inst.services.filter((s) => s.running).length}
                  />
                ))}
              </div>
            ))}
          </div>
        ) : null}
      </CardContent>
    </Card>
  );
}

/** Docker-directory fallback card, used when no instances are reported. */
function LegacyBranchCard({
  project,
  worktree,
}: {
  project: ProjectBucket;
  worktree: WorktreeBucket;
}) {
  const stats = worktreeStats(worktree);
  const label = worktree.worktree || DEFAULT_WORKTREE;

  return (
    <Link
      to="/projects/$project/$branch"
      params={{
        project: project.project,
        branch: worktreeParam(worktree.worktree),
      }}
      search={{}}
      className="focus-visible:ring-ring/60 block min-w-0 rounded-xl outline-none focus-visible:ring-2"
    >
      <Card className="hover:border-primary/40 h-full gap-0 py-0 transition-colors">
        <CardContent className="flex flex-col gap-3 p-4">
          <div className="flex items-center justify-between gap-2">
            <span className="flex min-w-0 items-center gap-2">
              <GitBranch className="text-primary size-4 shrink-0" aria-hidden />
              <span className="truncate font-mono text-sm font-semibold">
                {label}
              </span>
            </span>
            <ChevronRight
              className="text-muted-foreground size-4 shrink-0"
              aria-hidden
            />
          </div>
          <div className="text-muted-foreground flex flex-wrap items-center gap-x-3 gap-y-1 font-mono text-[11px]">
            <span>{stats.total} services</span>
            <span className="text-primary">{stats.running} running</span>
          </div>
          <div className="flex flex-wrap gap-1.5">
            {worktree.services.map((svc) => (
              <span
                key={svc.container}
                className="border-border text-muted-foreground rounded-full border px-2 py-0.5 font-mono text-[10px]"
              >
                {svc.service}
              </span>
            ))}
          </div>
          {stats.ports.length > 0 ? (
            <div className="text-muted-foreground/80 truncate font-mono text-[11px]">
              {stats.ports.join("  ")}
            </div>
          ) : null}
        </CardContent>
      </Card>
    </Link>
  );
}

function BranchesPage() {
  const { project } = Route.useParams();
  const { data: services } = useServices({ withInternal: true });
  const { data: status } = useStatus();

  const needle = project.toLowerCase();
  const projectBuckets = groupByBranch(
    buildInstanceViews(status?.instances ?? [], services ?? [])
  ).filter((b) => b.project.toLowerCase() === needle);

  // Fallback to the docker directory when no instance reports this project
  // (e.g. the index server could not query the IPC sockets).
  const legacy =
    projectBuckets.length === 0
      ? findProject(groupServices(services ?? []), project)
      : null;

  if (projectBuckets.length === 0 && !legacy) return null;

  const instanceCount = projectBuckets.reduce(
    (n, b) => n + b.instances.length,
    0
  );
  const description =
    projectBuckets.length > 0
      ? `${projectBuckets.length} ${projectBuckets.length === 1 ? "branch" : "branches"} · ${instanceCount} instances. Pick a branch to control its services.`
      : `${legacy!.worktrees.length} ${legacy!.worktrees.length === 1 ? "branch" : "branches"} · ${legacy!.total} services. Pick a branch to see its services and open a terminal.`;

  return (
    <div className="space-y-4">
      <PageHeader title={project} description={description} />
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        {projectBuckets.length > 0
          ? projectBuckets.map((b) => (
              <BranchCard key={`${b.project}:${b.worktree}`} bucket={b} />
            ))
          : legacy?.worktrees.map((wt) => (
              <LegacyBranchCard
                key={`${legacy.project}:${wt.worktree}`}
                project={legacy}
                worktree={wt}
              />
            ))}
      </div>
    </div>
  );
}
