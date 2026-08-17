import { createFileRoute } from "@tanstack/react-router";
import { useHealth } from "@/lib/hooks";
import { ErrorState, LoadingState, PageHeader } from "@/components/page-state";
import { StatusBadge } from "@/components/status-badge";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { ScrollArea } from "@/components/ui/scroll-area";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { cn } from "@/lib/utils";

export const Route = createFileRoute("/health")({
  component: HealthPage,
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

function StatChip({
  label,
  value,
  tone,
}: {
  label: string;
  value: number;
  tone?: "healthy" | "unhealthy";
}) {
  return (
    <span className="border-border bg-card flex items-center gap-1.5 rounded-full border px-2.5 py-1 font-mono text-[11px]">
      <span
        className={cn(
          "size-1.5 rounded-full",
          tone === "healthy"
            ? "bg-success"
            : tone === "unhealthy"
              ? "bg-destructive"
              : "bg-primary"
        )}
      />
      <span className="text-muted-foreground">{label}</span>
      <span className="text-foreground">{value}</span>
    </span>
  );
}

function HealthPage() {
  const { data, isLoading, isError, error } = useHealth();

  const results = data?.health ?? [];
  const total = results.length;
  const running = results.filter((h) => h.running).length;
  const healthy = results.filter(
    (h) => h.health?.toLowerCase() === "healthy"
  ).length;
  const unhealthy = results.filter(
    (h) => h.health?.toLowerCase() === "unhealthy"
  ).length;
  const unknown = results.filter((h) => !h.health).length;
  const other = results.filter(
    (h) =>
      h.health &&
      h.health.toLowerCase() !== "healthy" &&
      h.health.toLowerCase() !== "unhealthy"
  ).length;

  return (
    <div className="space-y-6">
      <PageHeader
        title="Health"
        description="Per-service health check results across all fog instances."
        actions={
          <span className="border-border bg-card text-muted-foreground flex items-center gap-1.5 rounded-full border px-2.5 py-1 font-mono text-[11px]">
            <span className="bg-primary size-1.5 animate-pulse rounded-full" />
            poll 5s
          </span>
        }
      />

      {isLoading ? (
        <LoadingState label="Running health checks…" />
      ) : isError ? (
        <ErrorState message={error?.message} />
      ) : total === 0 ? (
        <Card>
          <CardContent className="text-muted-foreground py-8 text-center font-mono text-sm">
            No health results yet.
          </CardContent>
        </Card>
      ) : (
        <div className="space-y-6">
          <div className="flex flex-wrap gap-2">
            <StatChip label="instances" value={total} />
            <StatChip label="running" value={running} />
            <StatChip label="healthy" value={healthy} tone="healthy" />
            <StatChip label="unhealthy" value={unhealthy} tone="unhealthy" />
            <StatChip label="starting/other" value={other} />
            <StatChip label="unknown" value={unknown} />
          </div>

          <Card className="gap-3 py-4">
            <CardHeader className="gap-0.5 px-5">
              <CardTitle className="font-mono text-sm">results</CardTitle>
            </CardHeader>
            <CardContent className="px-5">
              <ScrollArea className="max-h-[70vh]">
                <Table>
                  <TableHeader>
                    <TableRow>
                      <TableHead>Script</TableHead>
                      <TableHead>Project</TableHead>
                      <TableHead>Branch</TableHead>
                      <TableHead>Service</TableHead>
                      <TableHead>Instance</TableHead>
                      <TableHead>Running</TableHead>
                      <TableHead>Health</TableHead>
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {results.map((h) => (
                      <TableRow key={`${h.pid}/${h.script}/${h.service}`}>
                        <TableCell className="text-muted-foreground font-mono">
                          {h.script}
                        </TableCell>
                        <TableCell className="text-muted-foreground font-mono">
                          {h.project ?? ""}
                        </TableCell>
                        <TableCell className="text-muted-foreground font-mono">
                          {h.branch ?? ""}
                        </TableCell>
                        <TableCell className="font-mono font-medium">
                          {h.service}
                        </TableCell>
                        <TableCell className="text-muted-foreground font-mono">
                          pid {h.pid}
                        </TableCell>
                        <TableCell>
                          <StatusBadge
                            status={h.running ? "running" : "stopped"}
                          />
                        </TableCell>
                        <TableCell>
                          <HealthBadge health={h.health} />
                        </TableCell>
                      </TableRow>
                    ))}
                  </TableBody>
                </Table>
              </ScrollArea>
            </CardContent>
          </Card>
        </div>
      )}
    </div>
  );
}
