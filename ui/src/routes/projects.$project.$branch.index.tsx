import { createFileRoute, Link, Navigate } from "@tanstack/react-router";
import { ChevronRight, Terminal } from "lucide-react";

import { useServices, useStatus } from "@/lib/hooks";
import {
  DEFAULT_WORKTREE,
  branchInstances,
  buildInstanceViews,
  findBranch,
  findProject,
  findWorktree,
  groupByBranch,
  groupByScript,
  groupServices,
  scriptStats,
  type InstanceView,
} from "@/lib/services";
import { ErrorState, LoadingState, PageHeader } from "@/components/page-state";
import { InstanceKillButton } from "@/components/instance-kill-button";
import { InstanceKillingBadge } from "@/components/instance-killing-badge";
import { useInstanceKillState } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import { Card, CardContent } from "@/components/ui/card";

export const Route = createFileRoute("/projects/$project/$branch/")({
  component: BranchScriptsPage,
});

/** One script card: header link plus a row per instance (pid + Kill). */
function ScriptCard({
  project,
  branch,
  script,
  instances,
}: {
  project: string;
  branch: string;
  script: string;
  instances: InstanceView[];
}) {
  const stats = scriptStats(instances);

  return (
    <Card interactive spacing="none" padding="none">
      <CardContent variant="stack" className="flex flex-col">
        <Link
          to="/projects/$project/$branch/$script"
          params={{ project, branch, script }}
          className="focus-visible:ring-ring/60 flex min-w-0 items-center gap-2 rounded outline-none focus-visible:ring-2"
        >
          <Terminal className="text-primary size-4 shrink-0" aria-hidden />
          <span className="min-w-0 truncate font-mono text-sm font-semibold">
            {script}
          </span>
          <ChevronRight
            className="text-muted-foreground ml-auto size-4 shrink-0"
            aria-hidden
          />
        </Link>

        <div className="text-muted-foreground text-2xs flex flex-wrap items-center gap-x-3 gap-y-1 font-mono">
          <span>{stats.total} services</span>
          <span className="text-primary">{stats.running} running</span>
          {instances.length > 1 ? (
            <span>{instances.length} instances</span>
          ) : null}
        </div>

        <div className="border-border space-y-1.5 border-t pt-3">
          {instances.map((inst) => (
            <ScriptInstanceRow
              key={inst.pid}
              project={project}
              branch={branch}
              script={script}
              inst={inst}
            />
          ))}
        </div>
      </CardContent>
    </Card>
  );
}

/** One instance row inside a script card: pid + counts + Kill. */
function ScriptInstanceRow({
  project,
  branch,
  script,
  inst,
}: {
  project: string;
  branch: string;
  script: string;
  inst: InstanceView;
}) {
  const { killing } = useInstanceKillState(inst.pid, inst.script);
  const running = inst.services.filter((s) => s.running).length;

  return (
    <div
      className={cn(
        "flex items-center gap-2 transition-opacity",
        killing && "opacity-60"
      )}
    >
      <Link
        to="/projects/$project/$branch/$script"
        params={{ project, branch, script }}
        search={{ pid: inst.pid }}
        className="focus-visible:ring-ring/60 flex min-w-0 flex-1 items-center gap-2 rounded outline-none focus-visible:ring-2"
      >
        <span className="text-muted-foreground text-2xs shrink-0 font-mono">
          pid {inst.pid}
        </span>
        <span className="text-muted-foreground text-2xs ml-auto shrink-0 font-mono">
          {killing ? "killing…" : `${running}/${inst.services.length}`}
        </span>
      </Link>
      <InstanceKillingBadge pid={inst.pid} script={inst.script} />
      {inst.pid > 0 ? (
        <InstanceKillButton
          pid={inst.pid}
          script={inst.script}
          project={inst.project}
          branch={inst.branch}
        />
      ) : null}
    </div>
  );
}

function BranchScriptsPage() {
  const { project, branch } = Route.useParams();
  const {
    data: services,
    isLoading,
    isError,
    error,
  } = useServices({
    withInternal: true,
  });
  const { data: status, isLoading: statusLoading } = useStatus();

  const allBranches = groupByBranch(
    buildInstanceViews(status?.instances ?? [], services ?? [])
  );
  const bucket = findBranch(allBranches, project, branch);
  const projectBucket = findProject(groupServices(services ?? []), project);
  const legacy =
    bucket || !projectBucket ? null : findWorktree(projectBucket, branch);

  const instances = branchInstances(
    bucket,
    legacy,
    projectBucket?.project ?? project
  );
  const scripts = groupByScript(instances);
  const label = (bucket?.worktree ?? legacy?.worktree) || DEFAULT_WORKTREE;

  if (isLoading || statusLoading) {
    return <LoadingState label="Loading branch…" />;
  }
  if (isError) {
    return <ErrorState message={error?.message} />;
  }

  if (instances.length === 0) {
    return (
      <Card>
        <CardContent variant="empty" className="text-center">
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

  // A single script has no list worth showing: go straight to it.
  if (scripts.length === 1) {
    return (
      <Navigate
        replace
        to="/projects/$project/$branch/$script"
        params={{ project, branch, script: scripts[0][0] }}
      />
    );
  }

  return (
    <div className="space-y-4">
      <PageHeader title={label} />
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        {scripts.map(([script, list]) => (
          <ScriptCard
            key={script}
            project={project}
            branch={branch}
            script={script}
            instances={list}
          />
        ))}
      </div>
    </div>
  );
}
