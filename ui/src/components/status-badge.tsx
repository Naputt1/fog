import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";
import type { ServiceStatus } from "@/lib/api";

const STATUS_STYLE: Record<string, string> = {
  running: "bg-primary/15 text-primary border-primary/30",
  healthy: "bg-success/15 text-success border-success/30",
  starting: "bg-info/15 text-info border-info/30",
  stopped: "bg-muted text-muted-foreground border-border",
  unhealthy: "bg-destructive/15 text-destructive border-destructive/30",
};

const STATUS_DOT: Record<string, string> = {
  running: "bg-primary",
  healthy: "bg-success",
  starting: "bg-info",
  stopped: "bg-muted-foreground",
  unhealthy: "bg-destructive",
};

export function StatusBadge({ status }: { status: ServiceStatus }) {
  const dot = STATUS_DOT[status] ?? "bg-muted-foreground";
  return (
    <Badge
      variant="outline"
      className={cn(
        "gap-1.5 min-w-0 max-w-full font-mono capitalize",
        STATUS_STYLE[status] ?? "border-border text-muted-foreground"
      )}
    >
      <span className={cn("size-1.5 shrink-0 rounded-full", dot)} />
      <span className="truncate">{status}</span>
    </Badge>
  );
}
