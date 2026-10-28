import { useState } from "react";
import { createFileRoute, Link, Outlet } from "@tanstack/react-router";
import { Plus } from "lucide-react";

import { useServices, useStatus } from "@/lib/hooks";
import { findProject, groupServices } from "@/lib/services";
import { ErrorState, LoadingState } from "@/components/page-state";
import { LaunchPopup } from "@/components/launch-instance";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";

export const Route = createFileRoute("/projects/$project")({
  component: ProjectLayout,
});

/**
 * Pass-through layout for a project. Guards the route, then hands off to the
 * branches index, branch script list, or script services page via `<Outlet />`.
 * The breadcrumb lives in the app-shell top bar.
 */
function ProjectLayout() {
  const { project } = Route.useParams();

  // A project with nothing running is exactly where starting one belongs.
  const [launchOpen, setLaunchOpen] = useState(false);

  const { data, isLoading, isError, error } = useServices({
    withInternal: true,
  });
  const { data: status } = useStatus();
  const bucket = findProject(groupServices(data ?? []), project);
  // A project also exists when a fog instance runs there but every service is
  // stopped (docker discovery lists only running containers).
  const hasInstances = (status?.instances ?? []).some(
    (inst) =>
      (inst.project ?? inst.script).toLowerCase() === project.toLowerCase()
  );
  const exists = bucket !== null || hasInstances;

  // Scoped to this project: its worktrees and scripts are all the form needs.
  const launchPopup = (
    <LaunchPopup
      open={launchOpen}
      onOpenChange={setLaunchOpen}
      scope={{ project }}
      subtitle={`Another instance of ${project}`}
    />
  );

  return (
    <div className="flex min-w-0 flex-col gap-4">
      {isLoading ? (
        <LoadingState label="Loading project…" />
      ) : isError ? (
        <ErrorState message={error?.message} />
      ) : !exists ? (
        <div className="min-w-0 space-y-4">
          <Card>
            <CardContent variant="empty" className="text-center">
              <p className="font-mono text-sm">
                <span className="text-muted-foreground">project </span>
                <span className="text-foreground">{project}</span>
                <span className="text-muted-foreground">
                  {" "}
                  has no running services.
                </span>
              </p>
              <div className="mt-3 flex flex-wrap items-center justify-center gap-3">
                <Button size="sm" onClick={() => setLaunchOpen(true)}>
                  <Plus aria-hidden />
                  Start instance
                </Button>
                <Link
                  to="/"
                  className="text-primary font-mono text-xs underline-offset-4 hover:underline"
                >
                  ← back to all projects
                </Link>
              </div>
            </CardContent>
          </Card>
          {launchPopup}
        </div>
      ) : (
        <Outlet />
      )}
    </div>
  );
}
