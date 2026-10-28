import { useEffect, useId, useState } from "react";
import { FolderOpen } from "lucide-react";

import {
  useLaunch,
  useLaunchTargets,
  useRegisterProject,
  useRegisteredProjects,
} from "@/lib/hooks";
import { DirectoryBrowser } from "@/components/directory-browser";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { ComboboxField } from "@/components/ui/combobox";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Sheet, SheetContent } from "@/components/ui/sheet";
import { Skeleton } from "@/components/ui/skeleton";
import { useMediaQuery } from "@/lib/use-media-query";
import { cn } from "@/lib/utils";

/** Last path segment, used when a worktree has no branch name. */
function basename(p: string) {
  return p.split("/").filter(Boolean).pop() ?? p;
}

/**
 * The folder browser on its own surface, layered over the form it was opened
 * from: a centered dialog on desktop, a bottom sheet on mobile. Picking a
 * folder therefore never hides the project/worktree/script choices, and
 * dismissing it leaves the form exactly as it was.
 */
function BrowsePopup({
  open,
  startDir,
  added,
  onPick,
  onOpenChange,
}: {
  open: boolean;
  /** Folder to open in; the current selection's dir, when there is one. */
  startDir: string;
  /** Folders the project list already covers, marked and disabled. */
  added: readonly string[];
  onPick: (path: string) => void;
  onOpenChange: (open: boolean) => void;
}) {
  const isDesktop = useMediaQuery("(min-width: 1024px)");
  const browser = (
    <DirectoryBrowser
      startDir={startDir}
      added={added}
      onPick={onPick}
      onCancel={() => onOpenChange(false)}
    />
  );
  const header = (
    <DialogHeader>
      <div className="min-w-0">
        <DialogTitle>Browse folders</DialogTitle>
        <p className="text-muted-foreground mt-0.5 text-xs">
          Pick a folder holding a fog.json. Type a name to filter, or a path to
          complete it.
        </p>
      </div>
    </DialogHeader>
  );

  if (isDesktop) {
    return (
      <Dialog open={open} onOpenChange={onOpenChange}>
        <DialogContent spacing="none" className="max-h-[80dvh] max-w-lg">
          {header}
          <div className="flex min-h-0 flex-1 flex-col p-4">{browser}</div>
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
        {header}
        <div className="flex min-h-0 flex-1 flex-col px-4 pt-4 pb-4">
          {browser}
        </div>
        <div aria-hidden className="h-safe-bottom shrink-0" />
      </SheetContent>
    </Sheet>
  );
}

/**
 * The launch form in a popup: a centered dialog on desktop and a bottom sheet
 * on mobile, the same split the branch script page uses for its service panel.
 * Only one surface is ever mounted, so there is no duplicate focus trap.
 */
export function LaunchPopup({
  open,
  onOpenChange,
  scope,
  subtitle = "Launch a detached fog instance",
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Pin the form to one project, e.g. on that project's script page. */
  scope?: LaunchScope;
  /** Second line under the title, e.g. which project the popup is scoped to. */
  subtitle?: string;
}) {
  const isDesktop = useMediaQuery("(min-width: 1024px)");
  const close = () => onOpenChange(false);
  const header = (
    <DialogHeader>
      <div className="min-w-0">
        <DialogTitle>Start instance</DialogTitle>
        <p className="text-muted-foreground mt-0.5 text-xs">{subtitle}</p>
      </div>
    </DialogHeader>
  );
  const form = (
    <LaunchForm onLaunched={close} onCancel={close} autoFocus scope={scope} />
  );

  if (isDesktop) {
    return (
      <Dialog open={open} onOpenChange={onOpenChange}>
        <DialogContent spacing="none" className="max-h-[85dvh] max-w-xl">
          {header}
          <div className="flex min-h-0 flex-1 flex-col p-4">{form}</div>
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
        {header}
        <div className="flex min-h-0 flex-1 flex-col px-4 pt-4 pb-4">
          {form}
        </div>
        <div aria-hidden className="h-safe-bottom shrink-0" />
      </SheetContent>
    </Sheet>
  );
}

/** What a scoped launch form is pinned and preselected to. */
export type LaunchScope = {
  /** Project name to lock the form to; its picker and Browse are hidden. */
  project: string;
  /** Branch of the page's instance, preselected when a worktree has it. */
  branch?: string | null;
  /** Script the page shows, preselected when its worktree has it. */
  script?: string;
};

/**
 * Launch form: pick a project, then a worktree and a script. Shared by the
 * `/status` card, the services index's popup and a project's script page, which
 * passes a `scope` so the project is fixed to the page's own.
 *
 * There is one project list. Browsing to a folder with a `fog.json` adds it to
 * that list and selects it, so a config dir fog has never seen is two clicks
 * away instead of a second mode's path field. The form is a flex column whose
 * body scrolls and whose actions stay put, which keeps the popup's Start
 * reachable on a short window or a phone.
 */
export function LaunchForm({
  onLaunched,
  onCancel,
  autoFocus = false,
  scope,
}: {
  /** Called with the new pid when a launch succeeds, so popups can close. */
  onLaunched?: (pid: number) => void;
  /** Rendered as a Cancel button when the form lives in a dismissible popup. */
  onCancel?: () => void;
  /** Focus the first control on mount (popups; the status card leaves focus). */
  autoFocus?: boolean;
  /** Pin the form to one project, e.g. from that project's services page. */
  scope?: LaunchScope;
}) {
  const { data, isLoading, isError, error } = useLaunchTargets();
  const { mutate, isPending, data: result, error: launchError } = useLaunch();
  const fieldId = useId();

  const [browse, setBrowse] = useState(false);
  const [pickedPath, setPickedPath] = useState("");
  const [worktreePath, setWorktreePath] = useState("");
  const [script, setScript] = useState("");
  /** A browsed dir that launch targets don't list (yet). */
  const [pendingPath, setPendingPath] = useState<string | null>(null);

  const projects = data?.projects ?? [];
  /** The project a scope names, once the target list knows it. */
  const scopedProject = scope
    ? projects.find((p) => p.name.toLowerCase() === scope.project.toLowerCase())
    : undefined;
  const projectPath = scopedProject?.path ?? pickedPath;
  const selectedProject = projects.find((p) => p.path === projectPath);
  const launchable =
    selectedProject?.worktrees.filter((w) => w.scripts.length > 0) ?? [];
  /**
   * A scoped form arrives knowing its branch, so start there; a project with a
   * single worktree needs no choice at all.
   */
  const presetWorktree = !scope
    ? undefined
    : scope.branch
      ? launchable.find((w) => w.branch === scope.branch)
      : launchable.length === 1
        ? launchable[0]
        : undefined;
  const selectedWorktree =
    launchable.find((w) => w.path === worktreePath) ??
    (worktreePath ? undefined : presetWorktree);
  const scripts = selectedWorktree?.scripts ?? [];
  /** The picked script, or the page's own while it fits the worktree. */
  const activeScript =
    script && scripts.includes(script)
      ? script
      : scope?.script && scripts.includes(scope.script)
        ? scope.script
        : "";

  const registration = useRegisterProject();
  const registered = useRegisteredProjects();

  // A browsed folder becomes selectable once the launch targets list it, which
  // adding it to the registry does; until then the form waits on that refetch.
  useEffect(() => {
    if (!pendingPath) return;
    const match = projects.find(
      (p) =>
        p.path === pendingPath ||
        p.worktrees.some((w) => w.path === pendingPath)
    );
    if (!match) return;
    setPickedPath(match.path);
    setWorktreePath("");
    setScript("");
    setPendingPath(null);
  }, [pendingPath, projects]);

  /** Folders the project list already covers, so the browser can mark them. */
  const addedPaths = [
    ...(registered.data?.projects ?? []).map((p) => p.path),
    ...projects.map((p) => p.path),
    ...projects.flatMap((p) => p.worktrees.map((w) => w.path)),
  ];

  const ready =
    Boolean(projectPath && selectedWorktree && activeScript) && !pendingPath;

  const start = () => {
    if (!selectedWorktree || !activeScript) return;
    mutate(
      {
        configDir: selectedWorktree.path,
        script: activeScript,
        branch: selectedWorktree.branch,
      },
      { onSuccess: (r) => onSettled(r.ok, r.pid) }
    );
  };

  /** Close popup surfaces once the instance actually started. */
  const onSettled = (ok: boolean, pid?: number) => {
    if (ok && pid != null) onLaunched?.(pid);
  };

  /** Add a browsed folder to the project list and select it once it lands. */
  const pickDirectory = (path: string) => {
    setBrowse(false);
    registration.reset();
    setPendingPath(path);
    registration.mutate(path);
  };

  const failure = launchError?.message ?? result?.error ?? null;
  const status = isPending
    ? "Starting instance…"
    : failure
      ? failure
      : result?.ok && result.pid != null
        ? `Started pid ${result.pid}`
        : registration.isError
          ? (registration.error?.message ?? "Could not add this project.")
          : pendingPath
            ? `Adding ${basename(pendingPath)}…`
            : ready
              ? "Runs detached in the background"
              : scopedProject
                ? "Pick a worktree and script"
                : "Pick a project, worktree and script";

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-3">
      <div className="min-h-0 flex-1 space-y-3 overflow-y-auto">
        {isLoading ? (
          <div className="space-y-3">
            <div className="grid grid-cols-[minmax(0,1fr)_auto] items-end gap-2">
              <div className="space-y-1.5">
                <Skeleton className="h-2.5 w-16" />
                <Skeleton className="h-9 w-full" />
              </div>
              <Skeleton className="h-9 w-24" />
            </div>
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
              {[0, 1].map((i) => (
                <div key={i} className="space-y-1.5">
                  <Skeleton className="h-2.5 w-20" />
                  <Skeleton className="h-9 w-full" />
                </div>
              ))}
            </div>
          </div>
        ) : isError ? (
          <p className="text-destructive font-mono text-xs">
            {error?.message ?? "Failed to load launch targets."}
          </p>
        ) : (
          <>
            {projects.length === 0 ? (
              <p className="text-muted-foreground font-mono text-xs">
                No projects yet. Browse to a folder with a fog.json.
              </p>
            ) : null}

            {scopedProject ? null : (
              <div className="grid grid-cols-[minmax(0,1fr)_auto] items-end gap-2">
                <ComboboxField
                  id={`${fieldId}-project`}
                  label="Project"
                  value={selectedProject ?? null}
                  options={projects.map((p) => ({
                    value: p,
                    label: p.name,
                    detail: p.path,
                    keywords: p.path,
                  }))}
                  placeholder="Select a project…"
                  emptyLabel="No matching project"
                  disabled={isPending}
                  autoFocus={autoFocus}
                  hint={
                    selectedProject && launchable.length === 0
                      ? "No worktree with scripts on this project."
                      : undefined
                  }
                  onChange={(p) => {
                    setPickedPath(p?.path ?? "");
                    setWorktreePath("");
                    setScript("");
                  }}
                />
                <Button
                  type="button"
                  variant="outline"
                  className="shrink-0"
                  disabled={isPending}
                  onClick={() => setBrowse(true)}
                >
                  <FolderOpen aria-hidden />
                  Browse
                </Button>
              </div>
            )}

            {scopedProject && launchable.length === 0 ? (
              <p className="text-muted-foreground font-mono text-xs">
                No worktree with scripts in this project.
              </p>
            ) : null}

            <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
              <ComboboxField
                id={`${fieldId}-worktree`}
                label="Worktree / branch"
                value={selectedWorktree ?? null}
                options={launchable.map((w) => ({
                  value: w,
                  label: w.branch ?? basename(w.path),
                  detail: w.path,
                  keywords: w.path,
                }))}
                placeholder="Select a worktree…"
                emptyLabel="No matching worktree"
                disabled={isPending || launchable.length === 0}
                onChange={(w) => {
                  setWorktreePath(w?.path ?? "");
                  setScript("");
                }}
              />

              <ComboboxField
                id={`${fieldId}-script`}
                label="Script"
                value={activeScript || null}
                options={scripts.map((s) => ({ value: s, label: s }))}
                placeholder="Select a script…"
                emptyLabel="No matching script"
                disabled={isPending || !selectedWorktree}
                onChange={(s) => setScript(s ?? "")}
              />
            </div>
          </>
        )}
      </div>

      <div className="border-border flex flex-wrap items-center justify-between gap-2 border-t pt-3">
        <p
          className={cn(
            "text-2xs min-w-0 flex-1 truncate font-mono",
            failure ? "text-destructive" : "text-muted-foreground"
          )}
          aria-live="polite"
        >
          {status}
        </p>
        <div className="flex shrink-0 items-center gap-2">
          {onCancel ? (
            <Button
              variant="ghost"
              size="sm"
              disabled={isPending}
              onClick={onCancel}
            >
              Cancel
            </Button>
          ) : null}
          <Button
            size="sm"
            loading={isPending}
            loadingLabel="Starting…"
            disabled={!ready}
            onClick={start}
          >
            Start instance
          </Button>
        </div>
      </div>

      <BrowsePopup
        open={browse}
        startDir={selectedProject?.path ?? ""}
        added={addedPaths}
        onPick={pickDirectory}
        onOpenChange={setBrowse}
      />
    </div>
  );
}

/**
 * "Start instance" card: the launch form as a card, rendered at the top of the
 * status page.
 */
export function LaunchCard() {
  return (
    <Card>
      <CardHeader>
        <CardTitle variant="mono">Start instance</CardTitle>
      </CardHeader>
      <CardContent spacing="loose">
        <LaunchForm />
      </CardContent>
    </Card>
  );
}
