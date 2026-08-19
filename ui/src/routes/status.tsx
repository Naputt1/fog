import { useState } from "react";
import { createFileRoute } from "@tanstack/react-router";
import {
  useKillInstance,
  useLaunch,
  useLaunchTargets,
  useServiceAction,
  useStatus,
} from "@/lib/hooks";
import { ErrorState, LoadingState, PageHeader } from "@/components/page-state";
import { StatusBadge } from "@/components/status-badge";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import type {
  InstanceServiceStatus,
  InstanceStatus,
  ServiceAction,
} from "@/lib/api";

export const Route = createFileRoute("/status")({
  component: StatusPage,
});

const KNOWN_HEALTH = ["running", "healthy", "starting", "stopped", "unhealthy"];

/** Health detail badge: maps known states to styled badges, null → "unknown". */
function HealthBadge({ health }: { health: string | null }) {
  if (!health) {
    return (
      <Badge
        variant="outline"
        className="border-border text-muted-foreground gap-1.5 font-mono capitalize"
      >
        <span className="bg-muted-foreground/70 size-1.5 rounded-full" />
        unknown
      </Badge>
    );
  }
  const key = health.toLowerCase();
  return <StatusBadge status={KNOWN_HEALTH.includes(key) ? key : health} />;
}

/** Start/Stop/Restart controls for a single service row. */
function ServiceActions({
  pid,
  svc,
}: {
  pid: number;
  svc: InstanceServiceStatus;
}) {
  const [confirmAction, setConfirmAction] = useState<ServiceAction | null>(
    null
  );
  const { mutate, isPending, error } = useServiceAction();

  const running = svc.running;

  const run = (action: ServiceAction) => {
    if (action === "stop" || action === "restart") {
      setConfirmAction(action);
      return;
    }
    mutate({ pid, name: svc.name, action });
  };

  const confirm = () => {
    if (!confirmAction) return;
    const action = confirmAction;
    setConfirmAction(null);
    mutate({ pid, name: svc.name, action });
  };

  return (
    <div className="flex flex-col items-start gap-1.5">
      <div className="flex items-center gap-1.5">
        <Button
          size="xs"
          variant="outline"
          disabled={running || isPending}
          onClick={() => run("start")}
        >
          Start
        </Button>
        <Button
          size="xs"
          variant="outline"
          disabled={!running || isPending}
          onClick={() => run("stop")}
        >
          Stop
        </Button>
        <Button
          size="xs"
          variant="outline"
          disabled={isPending}
          onClick={() => run("restart")}
        >
          Restart
        </Button>
      </div>
      {error ? (
        <span className="text-destructive font-mono text-[11px]">
          {error.message}
        </span>
      ) : null}

      <AlertDialog
        open={confirmAction !== null}
        onOpenChange={(open) => {
          if (!open) setConfirmAction(null);
        }}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>
              {confirmAction === "stop" ? "Stop" : "Restart"} service
            </AlertDialogTitle>
            <AlertDialogDescription>
              {confirmAction === "stop"
                ? `Stop "${svc.name}" (pid ${pid})?`
                : `Restart "${svc.name}" (pid ${pid})?`}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>Cancel</AlertDialogCancel>
            <AlertDialogAction
              variant={confirmAction === "stop" ? "destructive" : "default"}
              onClick={confirm}
            >
              {confirmAction === "stop" ? "Stop" : "Restart"}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </div>
  );
}

/**
 * Nested table of the services spawned by one IPC instance.
 *
 * Responsive: on `sm+` renders the scrollable table (the shared `Table`
 * primitive already wraps itself in an `overflow-x-auto` container, so we
 * only pin a `min-w` here to keep the columns from squishing on narrow-but-not-
 * mobile widths). On `<sm` we swap to a card list so the Start/Stop/Restart
 * actions don't force horizontal scrolling on phones.
 */
function ServiceTable({
  pid,
  services,
}: {
  pid: number;
  services: InstanceServiceStatus[];
}) {
  return (
    <>
      {/* Desktop/tablet: horizontally scrollable table */}
      <div className="hidden sm:block">
        <Table className="min-w-[600px]">
          <TableHeader>
            <TableRow>
              <TableHead>Service</TableHead>
              <TableHead>Running</TableHead>
              <TableHead>Health</TableHead>
              <TableHead className="text-right">Actions</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {services.map((svc) => (
              <TableRow key={svc.name}>
                <TableCell className="font-mono font-medium">
                  {svc.name}
                </TableCell>
                <TableCell>
                  <StatusBadge status={svc.running ? "running" : "stopped"} />
                </TableCell>
                <TableCell>
                  <HealthBadge health={svc.health} />
                </TableCell>
                <TableCell className="text-right">
                  <div className="flex justify-end">
                    <ServiceActions pid={pid} svc={svc} />
                  </div>
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </div>

      {/* Mobile (<sm): card-based fallback so actions stay tap-friendly */}
      <div className="space-y-3 sm:hidden">
        {services.map((svc) => (
          <div key={svc.name} className="border-border rounded-lg border p-3">
            <div className="flex items-center justify-between gap-2">
              <span className="font-mono text-sm font-medium">{svc.name}</span>
              <StatusBadge status={svc.running ? "running" : "stopped"} />
            </div>
            <div className="mt-2">
              <HealthBadge health={svc.health} />
            </div>
            <div className="mt-3 border-t pt-3">
              <ServiceActions pid={pid} svc={svc} />
            </div>
          </div>
        ))}
      </div>
    </>
  );
}

/** Tailwind classes shared by the native selects in the launch card. */
const selectClass =
  "border-input dark:bg-input/30 h-9 w-full min-w-0 rounded-md border bg-transparent px-3 py-1 text-sm shadow-xs outline-none focus-visible:border-ring focus-visible:ring-ring/50 focus-visible:ring-[3px] disabled:cursor-not-allowed disabled:opacity-50";

/** Tiny stacked field label above a select/input. */
function FieldLabel({ children }: { children: React.ReactNode }) {
  return (
    <span className="text-muted-foreground mb-1 block font-mono text-[11px] tracking-wide uppercase">
      {children}
    </span>
  );
}

/**
 * "Start instance" card: launch a fog instance on a known project/worktree or
 * on an entirely new config dir. Rendered at the top of the status page.
 */
function LaunchCard() {
  const { data, isLoading, isError, error } = useLaunchTargets();
  const { mutate, isPending, data: result, error: launchError } = useLaunch();

  // Known-project mode state.
  const [projectPath, setProjectPath] = useState("");
  const [worktreePath, setWorktreePath] = useState("");
  const [script, setScript] = useState("");

  // New-project mode state.
  const [newPath, setNewPath] = useState("");
  const [newScript, setNewScript] = useState("");
  const [newBranch, setNewBranch] = useState("");

  const projects = data?.projects ?? [];
  const selectedProject = projects.find((p) => p.path === projectPath);
  const launchable =
    selectedProject?.worktrees.filter((w) => w.scripts.length > 0) ?? [];
  const selectedWorktree = launchable.find((w) => w.path === worktreePath);
  const scripts = selectedWorktree?.scripts ?? [];

  const basename = (p: string) => p.split("/").filter(Boolean).pop() ?? p;

  const knownReady = Boolean(projectPath && worktreePath && script);
  const newReady = Boolean(newPath.trim() && newScript.trim());

  const startKnown = () => {
    if (!selectedWorktree || !script) return;
    mutate({
      configDir: selectedWorktree.path,
      script,
      branch: selectedWorktree.branch,
    });
  };

  const startNew = () => {
    if (!newReady) return;
    mutate({
      configDir: newPath.trim(),
      script: newScript.trim(),
      branch: newBranch.trim() || null,
    });
  };

  return (
    <Card>
      <CardHeader>
        <CardTitle className="font-mono text-sm">Start instance</CardTitle>
      </CardHeader>
      <CardContent className="space-y-6">
        {/* Known project */}
        <div className="space-y-3">
          <div className="text-foreground font-mono text-xs font-semibold">
            Known project
          </div>
          {isLoading ? (
            <p className="text-muted-foreground font-mono text-xs">
              Loading projects…
            </p>
          ) : isError ? (
            <p className="text-destructive font-mono text-xs">
              {error?.message ?? "Failed to load launch targets."}
            </p>
          ) : projects.length === 0 ? (
            <p className="text-muted-foreground font-mono text-xs">
              No known projects.
            </p>
          ) : (
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-[1fr_1fr_1fr_auto] sm:items-end">
              <div>
                <FieldLabel>Project</FieldLabel>
                <select
                  className={selectClass}
                  value={projectPath}
                  disabled={isPending}
                  onChange={(e) => {
                    setProjectPath(e.target.value);
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
                </select>
                {selectedProject && launchable.length === 0 ? (
                  <p className="text-muted-foreground mt-1 font-mono text-[11px]">
                    No worktree with scripts on this project.
                  </p>
                ) : null}
              </div>
              <div>
                <FieldLabel>Worktree / branch</FieldLabel>
                <select
                  className={selectClass}
                  value={worktreePath}
                  disabled={isPending || launchable.length === 0}
                  onChange={(e) => {
                    setWorktreePath(e.target.value);
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
                </select>
              </div>
              <div>
                <FieldLabel>Script</FieldLabel>
                <select
                  className={selectClass}
                  value={script}
                  disabled={isPending || !selectedWorktree}
                  onChange={(e) => setScript(e.target.value)}
                >
                  <option value="" disabled>
                    Select a script…
                  </option>
                  {scripts.map((s) => (
                    <option key={s} value={s}>
                      {s}
                    </option>
                  ))}
                </select>
              </div>
              <Button
                variant="default"
                disabled={!knownReady || isPending}
                onClick={startKnown}
              >
                {isPending ? "Starting…" : "Start"}
              </Button>
            </div>
          )}
        </div>

        {/* New project */}
        <div className="space-y-3">
          <div className="text-foreground font-mono text-xs font-semibold">
            New project
          </div>
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-[2fr_1fr_1fr_auto] sm:items-end">
            <div>
              <FieldLabel>Config dir (absolute)</FieldLabel>
              <Input
                value={newPath}
                placeholder="/abs/path/to/project"
                className="font-mono"
                disabled={isPending}
                onChange={(e) => setNewPath(e.target.value)}
              />
            </div>
            <div>
              <FieldLabel>Script</FieldLabel>
              <Input
                value={newScript}
                placeholder="dev"
                className="font-mono"
                disabled={isPending}
                onChange={(e) => setNewScript(e.target.value)}
              />
            </div>
            <div>
              <FieldLabel>Branch (optional)</FieldLabel>
              <Input
                value={newBranch}
                placeholder="feature-x"
                className="font-mono"
                disabled={isPending}
                onChange={(e) => setNewBranch(e.target.value)}
              />
            </div>
            <Button
              variant="default"
              disabled={!newReady || isPending}
              onClick={startNew}
            >
              {isPending ? "Starting…" : "Start"}
            </Button>
          </div>
        </div>

        {/* Result feedback */}
        {isPending ? (
          <p className="text-muted-foreground font-mono text-xs">
            Starting instance…
          </p>
        ) : result?.ok && result.pid != null ? (
          <p className="font-mono text-xs text-emerald-500">
            Started pid {result.pid}
          </p>
        ) : launchError ? (
          <p className="text-destructive font-mono text-xs">
            {launchError.message}
          </p>
        ) : result?.error ? (
          <p className="text-destructive font-mono text-xs">{result.error}</p>
        ) : null}
      </CardContent>
    </Card>
  );
}

function InstanceCard({
  inst,
  running,
}: {
  inst: InstanceStatus;
  running: number;
}) {
  const [confirmKill, setConfirmKill] = useState(false);
  const {
    mutate: killInstance,
    isPending: killPending,
    error: killError,
  } = useKillInstance();

  return (
    <Card className="gap-3 py-4">
      <CardHeader className="flex-row flex-wrap items-center justify-between gap-2 px-5">
        <CardTitle className="flex flex-wrap items-baseline gap-x-2 font-mono text-sm">
          {inst.script}
          <span className="text-muted-foreground font-mono text-xs">
            pid {inst.pid}
          </span>
          {inst.project ? (
            <span className="text-muted-foreground font-mono text-xs">
              · {inst.project}
              {inst.branch ? `@${inst.branch}` : ""}
            </span>
          ) : null}
        </CardTitle>
        <div className="flex shrink-0 items-center gap-2">
          <Badge variant="outline" className="font-mono">
            {running}/{inst.services.length} running
          </Badge>
          <Button
            size="xs"
            variant="destructive"
            disabled={killPending}
            onClick={() => setConfirmKill(true)}
          >
            Kill
          </Button>
        </div>
      </CardHeader>
      <CardContent className="px-5">
        {inst.services.length === 0 ? (
          <p className="text-muted-foreground font-mono text-xs">
            No services in this instance.
          </p>
        ) : (
          <ServiceTable pid={inst.pid} services={inst.services} />
        )}
        {killError ? (
          <p className="text-destructive font-mono text-[11px]">
            {killError.message}
          </p>
        ) : null}
      </CardContent>

      <AlertDialog
        open={confirmKill}
        onOpenChange={(open) => {
          if (!open) setConfirmKill(false);
        }}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Kill instance</AlertDialogTitle>
            <AlertDialogDescription>
              Kill "{inst.script}" (pid {inst.pid})?
              {inst.project
                ? ` Project: ${inst.project}${inst.branch ? `@${inst.branch}` : ""}.`
                : ""}{" "}
              All services will be shut down gracefully.
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>Cancel</AlertDialogCancel>
            <AlertDialogAction
              variant="destructive"
              onClick={() => {
                setConfirmKill(false);
                killInstance({ pid: inst.pid });
              }}
            >
              Kill
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </Card>
  );
}

function StatusPage() {
  const { data, isLoading, isError, error } = useStatus();

  const instances = data?.instances ?? [];

  return (
    <div className="space-y-6">
      <PageHeader
        title="Status"
        description="IPC status snapshot of the running fog instances and their services."
        actions={
          <span className="border-border bg-card text-muted-foreground flex items-center gap-1.5 rounded-full border px-2.5 py-1 font-mono text-[11px]">
            <span className="bg-primary size-1.5 animate-pulse rounded-full" />
            poll 5s
          </span>
        }
      />

      <LaunchCard />

      {isLoading ? (
        <LoadingState label="Fetching status…" />
      ) : isError ? (
        <ErrorState message={error?.message} />
      ) : instances.length === 0 ? (
        <Card>
          <CardContent className="text-muted-foreground py-8 text-center font-mono text-sm">
            No instances reported.
          </CardContent>
        </Card>
      ) : (
        <div className="space-y-4">
          {instances.map((inst) => {
            const running = inst.services.filter((svc) => svc.running).length;
            return (
              <InstanceCard key={inst.pid} inst={inst} running={running} />
            );
          })}
        </div>
      )}
    </div>
  );
}
