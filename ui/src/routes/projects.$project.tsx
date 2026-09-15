import { createFileRoute, Link, Outlet } from "@tanstack/react-router";

import { useServices, useStatus } from "@/lib/hooks";
import { findProject, groupServices } from "@/lib/services";
import { ErrorState, LoadingState } from "@/components/page-state";
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

  return (
    <div className="flex min-w-0 flex-col gap-4">
      {isLoading ? (
        <LoadingState label="Loading project…" />
      ) : isError ? (
        <ErrorState message={error?.message} />
      ) : !exists ? (
        <Card>
          <CardContent className="py-10 text-center">
            <p className="font-mono text-sm">
              <span className="text-muted-foreground">project </span>
              <span className="text-foreground">{project}</span>
              <span className="text-muted-foreground">
                {" "}
                has no running services.
              </span>
            </p>
            <Link
              to="/"
              className="text-primary mt-3 inline-block font-mono text-xs underline-offset-4 hover:underline"
            >
              ← back to all projects
            </Link>
          </CardContent>
        </Card>
      ) : (
        <Outlet />
      )}
    </div>
  );
}
