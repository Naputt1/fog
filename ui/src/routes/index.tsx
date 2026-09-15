import { createFileRoute, Link } from "@tanstack/react-router";
import { Boxes, ChevronRight } from "lucide-react";
import { useState } from "react";

import { useServices } from "@/lib/hooks";
import {
  groupServices,
  projectStats,
  type ProjectBucket,
} from "@/lib/services";
import { PageHeader, LoadingState, ErrorState } from "@/components/page-state";
import { Card, CardContent } from "@/components/ui/card";

export const Route = createFileRoute("/")({
  component: ServicesPage,
});

/**
 * Project icon from config (`project.icon`). Renders the configured image and
 * falls back to the default glyph when unset or if the image fails to load.
 */
function ProjectIcon({ icon }: { icon: string | null }) {
  const [failed, setFailed] = useState(false);
  if (icon && !failed) {
    return (
      <img
        src={icon}
        alt=""
        aria-hidden
        onError={() => setFailed(true)}
        className="size-4 shrink-0 rounded-sm object-contain"
      />
    );
  }
  return <Boxes className="text-primary size-4 shrink-0" aria-hidden />;
}

/** Miniature counts + ports card that links into a project's branches. */
function ProjectCard({ project }: { project: ProjectBucket }) {
  const stats = projectStats(project);
  const branches = project.worktrees.length;

  return (
    <Link
      to="/projects/$project"
      params={{ project: project.project }}
      className="focus-visible:ring-ring/60 block min-w-0 rounded-xl outline-none focus-visible:ring-2"
    >
      <Card className="hover:border-primary/40 h-full gap-0 py-0 transition-colors">
        <CardContent className="flex flex-col gap-3 p-4">
          <div className="flex items-center justify-between gap-2">
            <span className="flex min-w-0 items-center gap-2">
              <ProjectIcon icon={project.icon} />
              <span className="truncate font-mono text-sm font-semibold">
                {project.project}
              </span>
            </span>
            <ChevronRight
              className="text-muted-foreground size-4 shrink-0"
              aria-hidden
            />
          </div>
          <div className="text-muted-foreground flex flex-wrap items-center gap-x-3 gap-y-1 font-mono text-[11px]">
            <span>
              {branches} {branches === 1 ? "branch" : "branches"}
            </span>
            <span>{stats.total} services</span>
            <span className="text-primary">{stats.running} running</span>
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

function ServicesPage() {
  const { data, isLoading, isError, error } = useServices({
    withInternal: true,
  });
  const groups = groupServices(data ?? []);
  const running = data?.filter((s) => s.status === "running").length ?? 0;

  return (
    <div className="min-w-0 space-y-6">
      <PageHeader
        title="Services"
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
      ) : groups.length === 0 ? (
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
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3">
          {groups.map((project) => (
            <ProjectCard key={project.project} project={project} />
          ))}
        </div>
      )}
    </div>
  );
}
