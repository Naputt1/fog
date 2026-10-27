import { useEffect, useId, useState } from "react";
import { FolderOpen } from "lucide-react";

import {
  useForgetProject,
  useLaunch,
  useLaunchInspect,
  useLaunchTargets,
  useRegisterProject,
  useRegisteredProjects,
} from "@/lib/hooks";
import { DirectoryBrowser } from "@/components/directory-browser";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { ComboboxField } from "@/components/ui/combobox";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import { cn } from "@/lib/utils";

/** Tiny stacked field label above a select/input, wired to its control id. */
function FieldLabel({
  htmlFor,
  children,
}: {
  htmlFor: string;
  children: React.ReactNode;
}) {
  return (
    <label
      htmlFor={htmlFor}
      className="text-muted-foreground text-2xs mb-1 block font-mono tracking-wide uppercase"
    >
      {children}
    </label>
  );
}

type Mode = "known" | "new";

/** Last path segment, used when a worktree has no branch name. */
function basename(p: string) {
  return p.split("/").filter(Boolean).pop() ?? p;
}

/** Trailing-edge debounce, so inspecting a typed path doesn't run per key. */
function useDebounced<T>(value: T, ms: number): T {
  const [debounced, setDebounced] = useState(value);
  useEffect(() => {
    const timer = setTimeout(() => setDebounced(value), ms);
    return () => clearTimeout(timer);
  }, [value, ms]);
  return debounced;
}

/**
 * Launch form: a fog instance on a known project/worktree, or on an entirely
 * new config dir. Shared by the `/status` card and the services index's popup
 * (dialog on desktop, bottom sheet on mobile).
 *
 * One mode is shown at a time; both modes keep their own state, so switching
 * back and forth never loses what was typed. The form is a flex column whose
 * body scrolls and whose actions stay put, which is what keeps the popup's
 * Start reachable on a short window or a phone.
 */
export function LaunchForm({
  onLaunched,
  onCancel,
  autoFocus = false,
}: {
  /** Called with the new pid when a launch succeeds, so popups can close. */
  onLaunched?: (pid: number) => void;
  /** Rendered as a Cancel button when the form lives in a dismissible popup. */
  onCancel?: () => void;
  /** Focus the first control on mount (popups; the status card leaves focus). */
  autoFocus?: boolean;
}) {
  const { data, isLoading, isError, error } = useLaunchTargets();
  const { mutate, isPending, data: result, error: launchError } = useLaunch();
  const fieldId = useId();

  const [mode, setMode] = useState<Mode>("known");
  /** Which mode's folder browser is open, if any. */
  const [browse, setBrowse] = useState<Mode | null>(null);

  // Known-project mode state.
  const [projectPath, setProjectPath] = useState("");
  const [worktreePath, setWorktreePath] = useState("");
  const [script, setScript] = useState("");
  /** A browsed dir that launch targets don't list (yet). */
  const [pendingPath, setPendingPath] = useState<string | null>(null);

  // New-project mode state.
  const [newPath, setNewPath] = useState("");
  const [newScript, setNewScript] = useState("");
  /** Worktree picked for a new-project launch; "" means the typed dir itself. */
  const [newWorktreePath, setNewWorktreePath] = useState("");

  const projects = data?.projects ?? [];
  const selectedProject = projects.find((p) => p.path === projectPath);
  const launchable =
    selectedProject?.worktrees.filter((w) => w.scripts.length > 0) ?? [];
  const selectedWorktree = launchable.find((w) => w.path === worktreePath);
  const scripts = selectedWorktree?.scripts ?? [];

  const registration = useRegisterProject();
  const { mutate: forgetProject, isPending: isForgetting } = useForgetProject();
  const registered = useRegisteredProjects();

  // A browsed folder becomes selectable once the launch targets list it, which
  // remembering it does; until then the form waits on that refetch.
  useEffect(() => {
    if (!pendingPath) return;
    const match = projects.find(
      (p) =>
        p.path === pendingPath ||
        p.worktrees.some((w) => w.path === pendingPath)
    );
    if (!match) return;
    setProjectPath(match.path);
    setWorktreePath("");
    setScript("");
    setPendingPath(null);
  }, [pendingPath, projects]);

  /** Remembered config dir behind the current selection, if any. */
  const registeredPath = registered.data?.projects.find(
    (p) =>
      p.path === projectPath ||
      selectedProject?.worktrees.some((w) => w.path === p.path)
  )?.path;

  // New-project mode: the backend inspects the typed dir for its scripts, its
  // branch and its repo's worktrees, so nothing has to be typed blind.
  const debouncedPath = useDebounced(newPath.trim(), 300);
  const inspect = useLaunchInspect(debouncedPath);
  const detected = inspect.data;
  const detectedWorktrees = detected?.worktrees ?? [];
  const pickedWorktree = detectedWorktrees.find(
    (w) => w.path === newWorktreePath
  );
  const newScripts = pickedWorktree?.scripts ?? detected?.scripts ?? [];
  const hasBranches = detectedWorktrees.some((w) => w.branch);
  /** Branch the launch resolves to: the picked worktree, else the checkout. */
  const branchLabel = pickedWorktree?.branch ?? detected?.branch ?? null;
  /** Branch picker options: the dir's own checkout first, then its worktrees. */
  const branchOptions = [
    {
      value: "",
      label: detected?.branch
        ? `Current checkout (${detected.branch})`
        : "Current checkout",
    },
    ...detectedWorktrees
      .filter((w) => w.branch)
      .map((w) => ({
        value: w.path,
        label: w.branch ?? w.path,
        detail: w.path,
      })),
  ];
  const scriptKey = newScripts.join(",");

  // A fresh inspection decides the valid scripts; pick one as soon as it
  // arrives and keep the choice valid when the branch changes.
  useEffect(() => {
    if (!detected) return;
    setNewScript((current) =>
      newScripts.includes(current) ? current : (newScripts[0] ?? "")
    );
    // newScripts is derived from detected + newWorktreePath (see scriptKey).
  }, [detected, scriptKey]);

  // A different dir means a different repo: drop a worktree picked for the old
  // one so its scripts can't leak into the new launch.
  useEffect(() => {
    setNewWorktreePath("");
  }, [detected?.path]);

  const knownReady = Boolean(projectPath && worktreePath && script);
  const newReady = Boolean(detected?.path && newScript);
  const ready = mode === "known" ? knownReady && !pendingPath : newReady;

  const startKnown = () => {
    if (!selectedWorktree || !script) return;
    mutate(
      {
        configDir: selectedWorktree.path,
        script,
        branch: selectedWorktree.branch,
      },
      { onSuccess: (r) => onSettled(r.ok, r.pid) }
    );
  };

  const startNew = () => {
    if (!newReady) return;
    mutate(
      {
        configDir: detected?.path ?? newPath.trim(),
        script: newScript,
        branch: pickedWorktree?.branch ?? null,
      },
      { onSuccess: (r) => onSettled(r.ok, r.pid) }
    );
  };

  /** Close popup surfaces once the instance actually started. */
  const onSettled = (ok: boolean, pid?: number) => {
    if (ok && pid != null) onLaunched?.(pid);
  };

  /**
   * A browsed folder: a new project takes the path as-is (the inspect endpoint
   * fills in its scripts and branch), a known one is remembered so discovery
   * lists it, then picked once the refetch brings it in.
   */
  const pickDirectory = (path: string) => {
    const target = browse;
    setBrowse(null);
    if (target === "new") {
      setNewPath(path);
      return;
    }
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
              : mode === "known"
                ? "Pick a project, worktree and script"
                : !newPath.trim()
                  ? "Enter a config dir to detect its scripts"
                  : inspect.isFetching
                    ? "Inspecting config dir…"
                    : detected && !newScript
                      ? "That config dir has no scripts"
                      : "Pick a script";

  if (browse) {
    return (
      <DirectoryBrowser
        startDir={
          browse === "known" ? (selectedProject?.path ?? "") : newPath.trim()
        }
        onPick={pickDirectory}
        onCancel={() => setBrowse(null)}
      />
    );
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-3">
      <div className="min-h-0 flex-1 space-y-4 overflow-y-auto">
        <div
          role="group"
          aria-label="Launch mode"
          className="bg-background border-border flex gap-0.5 rounded-md border p-0.5"
        >
          {(["known", "new"] as const).map((value) => (
            <button
              key={value}
              type="button"
              aria-pressed={mode === value}
              disabled={isPending}
              onClick={() => setMode(value)}
              className={cn(
                "flex-1 rounded-sm border px-3 py-1.5 text-xs font-medium transition-colors disabled:opacity-50",
                mode === value
                  ? "border-border bg-muted text-foreground shadow-sm"
                  : "text-muted-foreground hover:text-foreground border-transparent"
              )}
            >
              {value === "known" ? "Known project" : "New project"}
            </button>
          ))}
        </div>

        {mode === "known" ? (
          isLoading ? (
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
              {[0, 1, 2].map((i) => (
                <div key={i} className="space-y-1.5">
                  <Skeleton className="h-2.5 w-16" />
                  <Skeleton className="h-9 w-full" />
                </div>
              ))}
            </div>
          ) : isError ? (
            <p className="text-destructive font-mono text-xs">
              {error?.message ?? "Failed to load launch targets."}
            </p>
          ) : (
            <div className="space-y-3">
              {projects.length === 0 ? (
                <p className="text-muted-foreground font-mono text-xs">
                  No known projects yet. Browse to a folder with a fog.json.
                </p>
              ) : (
                <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
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
                      setProjectPath(p?.path ?? "");
                      setWorktreePath("");
                      setScript("");
                    }}
                  />

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
                    id={`${fieldId}-known-script`}
                    label="Script"
                    value={script || null}
                    options={scripts.map((s) => ({ value: s, label: s }))}
                    placeholder="Select a script…"
                    disabled={isPending || !selectedWorktree}
                    onChange={(s) => setScript(s ?? "")}
                  />
                </div>
              )}

              <div className="flex flex-wrap items-center gap-2">
                <Button
                  variant="outline"
                  size="sm"
                  disabled={isPending}
                  onClick={() => setBrowse("known")}
                >
                  <FolderOpen aria-hidden />
                  Browse folders
                </Button>
                {registeredPath ? (
                  <Button
                    variant="muted"
                    size="sm"
                    loading={isForgetting}
                    disabled={isPending}
                    onClick={() =>
                      forgetProject(registeredPath, {
                        onSuccess: () => {
                          setProjectPath("");
                          setWorktreePath("");
                          setScript("");
                        },
                      })
                    }
                  >
                    Forget this project
                  </Button>
                ) : null}
              </div>
            </div>
          )
        ) : (
          <div className="space-y-3">
            <div>
              <FieldLabel htmlFor={`${fieldId}-new-path`}>
                Config dir (absolute)
              </FieldLabel>
              <div className="flex items-center gap-2">
                <div className="min-w-0 flex-1">
                  <Input
                    id={`${fieldId}-new-path`}
                    value={newPath}
                    placeholder="/abs/path/to/project"
                    font="mono"
                    autoFocus={autoFocus}
                    disabled={isPending}
                    aria-invalid={inspect.isError}
                    onChange={(e) => setNewPath(e.target.value)}
                  />
                </div>
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  className="shrink-0"
                  disabled={isPending}
                  onClick={() => setBrowse("new")}
                >
                  <FolderOpen aria-hidden />
                  Browse
                </Button>
              </div>
              <p
                className={cn(
                  "text-2xs mt-1 truncate font-mono",
                  inspect.isError ? "text-destructive" : "text-muted-foreground"
                )}
                title={detected?.path}
              >
                {inspect.isError
                  ? (inspect.error?.message ??
                    "Could not inspect this config dir.")
                  : inspect.isFetching
                    ? "Inspecting…"
                    : detected
                      ? `${detected.path} · ${newScripts.length} ${
                          newScripts.length === 1 ? "script" : "scripts"
                        }${branchLabel ? ` · on ${branchLabel}` : ""}`
                      : "A directory with a fog.json, anywhere on this machine."}
              </p>
            </div>

            <div
              className={cn(
                "grid grid-cols-1 gap-3",
                hasBranches && "sm:grid-cols-2"
              )}
            >
              <ComboboxField
                id={`${fieldId}-new-script`}
                label="Script"
                value={newScript || null}
                options={newScripts.map((s) => ({ value: s, label: s }))}
                placeholder={
                  detected
                    ? "Select a script…"
                    : "Detected from the config dir…"
                }
                emptyLabel="No matching script"
                disabled={isPending || newScripts.length === 0}
                onChange={(s) => setNewScript(s ?? "")}
                hint={
                  detected && newScripts.length === 0
                    ? "No scripts in this config dir's fog.json."
                    : undefined
                }
              />

              {hasBranches ? (
                <ComboboxField
                  id={`${fieldId}-new-branch`}
                  label="Branch"
                  value={newWorktreePath}
                  options={branchOptions}
                  emptyLabel="No matching branch"
                  disabled={isPending}
                  onChange={(path) => setNewWorktreePath(path ?? "")}
                />
              ) : null}
            </div>
          </div>
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
            onClick={() => (mode === "known" ? startKnown() : startNew())}
          >
            Start instance
          </Button>
        </div>
      </div>
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
