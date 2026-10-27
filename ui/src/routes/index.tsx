import { createFileRoute, Link } from "@tanstack/react-router";
import { Boxes, ChevronRight, Plus } from "lucide-react";
import { useState } from "react";

import { useServices } from "@/lib/hooks";
import {
  groupServices,
  projectStats,
  type ProjectBucket,
} from "@/lib/services";
import { PageHeader, LoadingState, ErrorState } from "@/components/page-state";
import { LaunchForm } from "@/components/launch-instance";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Sheet, SheetContent } from "@/components/ui/sheet";
import { useMediaQuery } from "@/lib/use-media-query";

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
      <Card interactive spacing="none" padding="none" className="h-full">
        <CardContent variant="stack" className="flex flex-col">
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
          <div className="text-muted-foreground text-2xs flex flex-wrap items-center gap-x-3 gap-y-1 font-mono">
            <span>
              {branches} {branches === 1 ? "branch" : "branches"}
            </span>
            <span>{stats.total} services</span>
            <span className="text-primary">{stats.running} running</span>
          </div>
          {stats.ports.length > 0 ? (
            <div className="text-muted-foreground/80 text-2xs truncate font-mono">
              {stats.ports.join("  ")}
            </div>
          ) : null}
        </CardContent>
      </Card>
    </Link>
  );
}

/**
 * Launch form in a popup: a centered dialog on desktop and a bottom sheet on
 * mobile, the same split the branch script page uses for its service panel.
 * Only one surface is ever mounted, so there is no duplicate focus trap.
 */
function LaunchPopup({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const isDesktop = useMediaQuery("(min-width: 1024px)");
  const close = () => onOpenChange(false);

  if (isDesktop) {
    return (
      <Dialog open={open} onOpenChange={onOpenChange}>
        <DialogContent spacing="none" className="max-h-[85dvh] max-w-xl">
          <DialogHeader>
            <div className="min-w-0">
              <DialogTitle>Start instance</DialogTitle>
              <p className="text-muted-foreground mt-0.5 text-xs">
                Launch a detached fog instance
              </p>
            </div>
          </DialogHeader>
          <div className="flex min-h-0 flex-1 flex-col p-4">
            <LaunchForm onLaunched={close} onCancel={close} autoFocus />
          </div>
        </DialogContent>
      </Dialog>
    );
  }

  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent
        side="bottom"
        shape="roundedTop"
        spacing="none"
        className="max-h-[85dvh]"
      >
        <div
          aria-hidden
          className="bg-muted-foreground/30 mx-auto mt-2 h-1 w-10 shrink-0 rounded-full"
        />
        <DialogHeader>
          <div className="min-w-0">
            <DialogTitle>Start instance</DialogTitle>
            <p className="text-muted-foreground mt-0.5 text-xs">
              Launch a detached fog instance
            </p>
          </div>
        </DialogHeader>
        <div className="flex min-h-0 flex-1 flex-col px-4 pt-4 pb-4">
          <LaunchForm onLaunched={close} onCancel={close} autoFocus />
        </div>
        <div aria-hidden className="h-safe-bottom shrink-0" />
      </SheetContent>
    </Sheet>
  );
}

function ServicesPage() {
  const { data, isLoading, isError, error } = useServices({
    withInternal: true,
  });
  const [launchOpen, setLaunchOpen] = useState(false);
  const groups = groupServices(data ?? []);
  const running = data?.filter((s) => s.status === "running").length ?? 0;

  return (
    <div className="min-w-0 space-y-6">
      <PageHeader
        title="Services"
        actions={
          <>
            {data && data.length > 0 ? (
              <div className="border-primary/30 bg-primary/10 text-primary flex items-center gap-1.5 rounded-full border px-3 py-1 font-mono text-xs whitespace-nowrap">
                <span className="bg-primary size-1.5 shrink-0 animate-pulse rounded-full" />
                {running}/{data.length} running
              </div>
            ) : null}
            <Button size="sm" onClick={() => setLaunchOpen(true)}>
              <Plus aria-hidden />
              Start instance
            </Button>
          </>
        }
      />

      {isLoading ? (
        <LoadingState label="Loading services…" />
      ) : isError ? (
        <ErrorState message={error?.message} />
      ) : groups.length === 0 ? (
        <Card>
          <CardContent variant="empty" className="text-center">
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
            <Button
              variant="outline"
              size="sm"
              className="mt-4"
              onClick={() => setLaunchOpen(true)}
            >
              <Plus aria-hidden />
              Start instance
            </Button>
          </CardContent>
        </Card>
      ) : (
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3">
          {groups.map((project) => (
            <ProjectCard key={project.project} project={project} />
          ))}
        </div>
      )}

      <LaunchPopup open={launchOpen} onOpenChange={setLaunchOpen} />
    </div>
  );
}
