import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";
import type { ServiceStatus } from "@/lib/api";

const STATUS_DOT: Record<string, string> = {
  running: "bg-primary",
  healthy: "bg-success",
  starting: "bg-info",
  stopped: "bg-muted-foreground",
  stopping: "bg-warning",
  killing: "bg-warning",
  unhealthy: "bg-destructive",
};

const STATUS_TONES = [
  "running",
  "healthy",
  "starting",
  "stopped",
  "stopping",
  "killing",
  "unhealthy",
] as const;
type StatusTone = (typeof STATUS_TONES)[number] | "unknown";

export function StatusBadge({ status }: { status: ServiceStatus }) {
  const dot = STATUS_DOT[status] ?? "bg-muted-foreground";
  const tone: StatusTone = (STATUS_TONES as readonly string[]).includes(status)
    ? (status as (typeof STATUS_TONES)[number])
    : "unknown";
  return (
    <Badge
      variant="outline"
      tone={tone}
      size="status"
      className="max-w-full min-w-0"
    >
      <span className={cn("size-1.5 shrink-0 rounded-full", dot)} />
      <span className="truncate">{status}</span>
    </Badge>
  );
}
