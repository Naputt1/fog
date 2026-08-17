import { createFileRoute } from "@tanstack/react-router";
import { useStatus } from "@/lib/hooks";
import { ErrorState, LoadingState, PageHeader } from "@/components/page-state";
import { StatusBadge } from "@/components/status-badge";
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
import type { InstanceServiceStatus } from "@/lib/api";

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

/** Nested table of the services spawned by one IPC instance. */
function ServiceTable({ services }: { services: InstanceServiceStatus[] }) {
  return (
    <Table>
      <TableHeader>
        <TableRow>
          <TableHead>Service</TableHead>
          <TableHead>Running</TableHead>
          <TableHead>Health</TableHead>
        </TableRow>
      </TableHeader>
      <TableBody>
        {services.map((svc) => (
          <TableRow key={svc.name}>
            <TableCell className="font-mono font-medium">{svc.name}</TableCell>
            <TableCell>
              <StatusBadge status={svc.running ? "running" : "stopped"} />
            </TableCell>
            <TableCell>
              <HealthBadge health={svc.health} />
            </TableCell>
          </TableRow>
        ))}
      </TableBody>
    </Table>
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
              <Card key={inst.pid} className="gap-3 py-4">
                <CardHeader className="flex-row items-center justify-between gap-2 px-5">
                  <CardTitle className="flex flex-wrap items-baseline gap-x-2 font-mono text-sm">
                    {inst.script}
                    <span className="text-muted-foreground font-mono text-xs">
                      pid {inst.pid}
                    </span>
                  </CardTitle>
                  <Badge variant="outline" className="shrink-0 font-mono">
                    {running}/{inst.services.length} running
                  </Badge>
                </CardHeader>
                <CardContent className="px-5">
                  {inst.services.length === 0 ? (
                    <p className="text-muted-foreground font-mono text-xs">
                      No services in this instance.
                    </p>
                  ) : (
                    <ServiceTable services={inst.services} />
                  )}
                </CardContent>
              </Card>
            );
          })}
        </div>
      )}
    </div>
  );
}
