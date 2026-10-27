import { useEffect, useId, useState } from "react";
import { ChevronDown } from "lucide-react";

import { useLaunch, useLaunchInspect, useLaunchTargets } from "@/lib/hooks";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import { cn } from "@/lib/utils";

/** Tailwind classes shared by the native selects in the launch form. */
const selectClass =
  "border-input dark:bg-input/30 h-9 w-full min-w-0 appearance-none rounded-md border bg-transparent py-1 pr-8 pl-3 text-sm shadow-xs outline-none focus-visible:border-ring focus-visible:ring-ring/50 focus-visible:ring-3 disabled:cursor-not-allowed disabled:opacity-50";

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

/** Native select with a chevron, since `appearance-none` removes the OS one. */
function SelectField({
  id,
  label,
  value,
  disabled,
  autoFocus,
  onChange,
  hint,
  children,
}: {
  id: string;
  label: string;
  value: string;
  disabled?: boolean;
  autoFocus?: boolean;
  onChange: (value: string) => void;
  hint?: React.ReactNode;
  children: React.ReactNode;
}) {
  return (
    <div className="min-w-0">
      <FieldLabel htmlFor={id}>{label}</FieldLabel>
      <div className="relative">
        <select
          id={id}
          className={selectClass}
          value={value}
          disabled={disabled}
          autoFocus={autoFocus}
          onChange={(e) => onChange(e.target.value)}
        >
          {children}
        </select>
        <ChevronDown
          aria-hidden
          className="text-muted-foreground pointer-events-none absolute top-1/2 right-2.5 size-4 -translate-y-1/2"
        />
      </div>
      {hint ? (
        <p className="text-muted-foreground text-2xs mt-1 font-mono">{hint}</p>
      ) : null}
    </div>
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

  // Known-project mode state.
  const [projectPath, setProjectPath] = useState("");
  const [worktreePath, setWorktreePath] = useState("");
  const [script, setScript] = useState("");

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
  const ready = mode === "known" ? knownReady : newReady;

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

  const failure = launchError?.message ?? result?.error ?? null;
  const status = isPending
    ? "Starting instance…"
    : failure
      ? failure
      : result?.ok && result.pid != null
        ? `Started pid ${result.pid}`
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
          ) : projects.length === 0 ? (
            <p className="text-muted-foreground font-mono text-xs">
              No known projects. Start an instance from a config dir below.
            </p>
          ) : (
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
              <SelectField
                id={`${fieldId}-project`}
                label="Project"
                value={projectPath}
                disabled={isPending}
                autoFocus={autoFocus}
                hint={
                  selectedProject && launchable.length === 0
                    ? "No worktree with scripts on this project."
                    : undefined
                }
                onChange={(value) => {
                  setProjectPath(value);
                  setWorktreePath("");
                  setScript("");
                }}
              >
                <option value="" disabled>
                  Select a project…
                </option>
                {projects.map((p) => (
                  <option key={p.path} value={p.path}>
                    {p.name}
                  </option>
                ))}
              </SelectField>

              <SelectField
                id={`${fieldId}-worktree`}
                label="Worktree / branch"
                value={worktreePath}
                disabled={isPending || launchable.length === 0}
                onChange={(value) => {
                  setWorktreePath(value);
                  setScript("");
                }}
              >
                <option value="" disabled>
                  Select a worktree…
                </option>
                {launchable.map((w) => (
                  <option key={w.path} value={w.path}>
                    {w.branch ?? basename(w.path)}
                  </option>
                ))}
              </SelectField>

              <SelectField
                id={`${fieldId}-known-script`}
                label="Script"
                value={script}
                disabled={isPending || !selectedWorktree}
                onChange={setScript}
              >
                <option value="" disabled>
                  Select a script…
                </option>
                {scripts.map((s) => (
                  <option key={s} value={s}>
                    {s}
                  </option>
                ))}
              </SelectField>
            </div>
          )
        ) : (
          <div className="space-y-3">
            <div>
              <FieldLabel htmlFor={`${fieldId}-new-path`}>
                Config dir (absolute)
              </FieldLabel>
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
              <SelectField
                id={`${fieldId}-new-script`}
                label="Script"
                value={newScript}
                disabled={isPending || newScripts.length === 0}
                onChange={setNewScript}
                hint={
                  detected && newScripts.length === 0
                    ? "No scripts in this config dir's fog.json."
                    : undefined
                }
              >
                <option value="" disabled>
                  {detected
                    ? "Select a script…"
                    : "Detected from the config dir…"}
                </option>
                {newScripts.map((s) => (
                  <option key={s} value={s}>
                    {s}
                  </option>
                ))}
              </SelectField>

              {hasBranches ? (
                <SelectField
                  id={`${fieldId}-new-branch`}
                  label="Branch"
                  value={newWorktreePath}
                  disabled={isPending}
                  onChange={setNewWorktreePath}
                >
                  <option value="">
                    {detected?.branch
                      ? `Current checkout (${detected.branch})`
                      : "Current checkout"}
                  </option>
                  {detectedWorktrees
                    .filter((w) => w.branch)
                    .map((w) => (
                      <option key={w.path} value={w.path}>
                        {w.branch}
                      </option>
                    ))}
                </SelectField>
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
