import { createFileRoute } from "@tanstack/react-router";
import { useStatus, useInstanceKillState } from "@/lib/hooks";
import { ErrorState, LoadingState, PageHeader } from "@/components/page-state";
import { StatusBadge } from "@/components/status-badge";
import { ServiceActions } from "@/components/service-actions";
import { InstanceKillButton } from "@/components/instance-kill-button";
import { LaunchCard } from "@/components/launch-instance";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import type { InstanceServiceStatus, InstanceStatus } from "@/lib/api";

export const Route = createFileRoute("/status")({
  component: StatusPage,
});

const KNOWN_HEALTH = ["running", "healthy", "starting", "stopped", "unhealthy"];

/** Health detail badge: maps known states to styled badges, null → "unknown". */
function HealthBadge({ health }: { health: string | null }) {
  if (!health) {
    return (
      <Badge variant="outline" tone="unknown" size="status">
        <span className="bg-muted-foreground/70 size-1.5 rounded-full" />
        unknown
      </Badge>
    );
  }
  const key = health.toLowerCase();
  return <StatusBadge status={KNOWN_HEALTH.includes(key) ? key : health} />;
}

/** Declared endpoints of one service, as a compact health list. */
function Endpoints({ svc }: { svc: InstanceServiceStatus }) {
  const endpoints = svc.endpoints ?? [];
  if (endpoints.length === 0) return null;
  return (
    <ul className="mt-1 space-y-0.5">
      {endpoints.map((endpoint) => (
        <li
          key={endpoint.name}
          className="flex items-center gap-1.5 font-mono text-xs"
        >
          <span className="text-muted-foreground">↳</span>
          <span>{endpoint.name}</span>
          <StatusBadge status={endpoint.health} />
        </li>
      ))}
    </ul>
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
  killing = false,
}: {
  pid: number;
  services: InstanceServiceStatus[];
  killing?: boolean;
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
                <TableCell variant="monoStrong">
                  {svc.name}
                  <Endpoints svc={svc} />
                </TableCell>
                <TableCell>
                  <StatusBadge status={svc.running ? "running" : "stopped"} />
                </TableCell>
                <TableCell>
                  <HealthBadge health={svc.health} />
                </TableCell>
                <TableCell className="text-right">
                  <div className="flex justify-end">
                    <ServiceActions
                      pid={pid}
                      name={svc.name}
                      running={svc.running}
                      killing={killing}
                    />
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
            <Endpoints svc={svc} />
            <div className="mt-2">
              <HealthBadge health={svc.health} />
            </div>
            <div className="mt-3 border-t pt-3">
              <ServiceActions
                pid={pid}
                name={svc.name}
                running={svc.running}
                killing={killing}
              />
            </div>
          </div>
        ))}
      </div>
    </>
  );
}

function InstanceCard({
  inst,
  running,
}: {
  inst: InstanceStatus;
  running: number;
}) {
  const { killing } = useInstanceKillState(inst.pid, inst.script);

  return (
    <Card spacing="compact" padding="compact" dimmed={killing}>
      <CardHeader
        padding="narrow"
        className="flex-row flex-wrap items-center justify-between"
      >
        <CardTitle
          variant="mono"
          spacing="inline"
          className="flex flex-wrap items-baseline"
        >
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
          {killing ? (
            <StatusBadge status="killing" />
          ) : (
            <Badge variant="outline" size="mono">
              {running}/{inst.services.length} running
            </Badge>
          )}
          <InstanceKillButton
            pid={inst.pid}
            script={inst.script}
            project={inst.project}
            branch={inst.branch}
          />
        </div>
      </CardHeader>
      <CardContent padding="narrow">
        {inst.services.length === 0 ? (
          <p className="text-muted-foreground font-mono text-xs">
            No services in this instance.
          </p>
        ) : (
          <ServiceTable
            pid={inst.pid}
            services={inst.services}
            killing={killing}
          />
        )}
      </CardContent>
    </Card>
  );
}

function StatusPage() {
  const { data, isLoading, isError, error } = useStatus();

  const instances = data?.instances ?? [];

  return (
    <div className="space-y-6">
      <PageHeader title="Status" />

      <LaunchCard />

      {isLoading ? (
        <LoadingState label="Fetching status…" />
      ) : isError ? (
        <ErrorState message={error?.message} />
      ) : instances.length === 0 ? (
        <Card>
          <CardContent variant="message" className="text-center">
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
