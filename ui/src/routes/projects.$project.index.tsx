import { createFileRoute, Link } from "@tanstack/react-router";
import { ChevronRight, GitBranch } from "lucide-react";

import { useServices } from "@/lib/hooks";
import {
  DEFAULT_WORKTREE,
  findProject,
  groupServices,
  worktreeParam,
  worktreeStats,
  type ProjectBucket,
  type WorktreeBucket,
} from "@/lib/services";
import { PageHeader } from "@/components/page-state";
import { Card, CardContent } from "@/components/ui/card";

export const Route = createFileRoute("/projects/$project/")({
  component: BranchesPage,
});

/** Miniature branch card that drills into the branch's services. */
function BranchCard({
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
              <GitBranch
                className="text-primary size-4 shrink-0"
                aria-hidden
              />
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
  const { data } = useServices({ withInternal: true });
  const bucket = findProject(groupServices(data ?? []), project);

  // The parent layout guards loading/error/not-found before rendering us.
  if (!bucket) return null;

  return (
    <div className="space-y-4">
      <PageHeader
        title={bucket.project}
        description={`${bucket.worktrees.length} ${bucket.worktrees.length === 1 ? "branch" : "branches"} · ${bucket.total} services. Pick a branch to see its services and open a terminal.`}
      />
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        {bucket.worktrees.map((wt) => (
          <BranchCard
            key={`${bucket.project}:${wt.worktree}`}
            project={bucket}
            worktree={wt}
          />
        ))}
      </div>
    </div>
  );
}
