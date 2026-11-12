import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";
import { serviceStateStyle } from "@/lib/service-state";

/**
 * A service-state pill. Known states get their tone and dot from the single
 * state table; an unrecognized wire string keeps its own text with the neutral
 * style, so a newer backend state is never hidden behind a generic label.
 */
export function StatusBadge({ status }: { status: string }) {
  const style = serviceStateStyle(status);
  return (
    <Badge
      variant="outline"
      tone={style.tone}
      size="status"
      className="max-w-full min-w-0"
    >
      <span className={cn("size-1.5 shrink-0 rounded-full", style.dot)} />
      <span className="truncate">{status}</span>
    </Badge>
  );
}
