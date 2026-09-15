import {
  createFileRoute,
  Link,
  Outlet,
  useParams,
} from "@tanstack/react-router";
import { ChevronRight } from "lucide-react";

import { useServices } from "@/lib/hooks";
import { findProject, groupServices } from "@/lib/services";
import { ErrorState, LoadingState } from "@/components/page-state";
import { Card, CardContent } from "@/components/ui/card";

export const Route = createFileRoute("/projects/$project")({
  component: ProjectLayout,
});

/**
 * Pass-through layout for a project. Renders the breadcrumb and guards the
 * route, then hands off to the branches index or the branch services page via
 * `<Outlet />`.
 */
function ProjectLayout() {
  const { project } = Route.useParams();
  // `branch` only exists while a child branch route is active.
  const params = useParams({ strict: false }) as {
    project: string;
    branch?: string;
  };
  const branch = params.branch;

  const { data, isLoading, isError, error } = useServices({
    withInternal: true,
  });
  const bucket = findProject(groupServices(data ?? []), project);

  return (
    <div className="flex min-w-0 flex-col gap-4">
      <nav
        aria-label="Breadcrumb"
        className="flex min-w-0 items-center gap-1.5 font-mono text-xs"
      >
        <Link
          to="/"
          className="text-muted-foreground hover:text-foreground shrink-0 transition-colors"
        >
          Services
        </Link>
        <ChevronRight
          className="text-muted-foreground/50 size-3 shrink-0"
          aria-hidden
        />
        {branch ? (
          <>
            <Link
              to="/projects/$project"
              params={{ project }}
              className="text-muted-foreground hover:text-foreground max-w-[40%] truncate transition-colors"
            >
              {bucket?.project ?? project}
            </Link>
            <ChevronRight
              className="text-muted-foreground/50 size-3 shrink-0"
              aria-hidden
            />
            <span
              className="text-foreground min-w-0 truncate font-semibold"
              aria-current="page"
            >
              {branch}
            </span>
          </>
        ) : (
          <span
            className="text-foreground min-w-0 truncate font-semibold"
            aria-current="page"
          >
            {bucket?.project ?? project}
          </span>
        )}
      </nav>

      {isLoading ? (
        <LoadingState label="Loading project…" />
      ) : isError ? (
        <ErrorState message={error?.message} />
      ) : !bucket ? (
        <Card>
          <CardContent className="py-10 text-center">
            <p className="font-mono text-sm">
              <span className="text-muted-foreground">project </span>
              <span className="text-foreground">{project}</span>
              <span className="text-muted-foreground"> has no running services.</span>
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
