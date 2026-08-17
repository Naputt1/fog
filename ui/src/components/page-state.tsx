import { AlertCircle, Loader2 } from "lucide-react";
import { cn } from "@/lib/utils";
import { Card, CardContent } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";

export function PageHeader({
  title,
  description,
  actions,
}: {
  title: string;
  description?: string;
  actions?: React.ReactNode;
}) {
  return (
    <div className="flex flex-wrap items-start justify-between gap-3">
      <div className="min-w-0">
        <h1 className="font-mono text-xl font-semibold tracking-tight">
          {title}
        </h1>
        {description ? (
          <p className="text-muted-foreground mt-1 max-w-2xl text-sm">
            {description}
          </p>
        ) : null}
      </div>
      {actions ? (
        <div className="flex flex-wrap items-center gap-2">{actions}</div>
      ) : null}
    </div>
  );
}

export function LoadingState({ label = "Loading…" }: { label?: string }) {
  return (
    <div className="space-y-3 py-2">
      <Skeleton className="h-8 w-48" />
      <Skeleton className="h-4 w-full" />
      <Skeleton className="h-4 w-3/4" />
      <p className="text-muted-foreground flex items-center gap-2 font-mono text-xs">
        <Loader2 className="size-3.5 animate-spin" />
        {label}
      </p>
    </div>
  );
}

export function ErrorState({
  message,
  className,
}: {
  message?: string;
  className?: string;
}) {
  return (
    <Card className={cn("border-destructive/40", className)}>
      <CardContent className="flex min-w-0 items-center gap-3 py-4">
        <AlertCircle className="text-destructive size-4 shrink-0" />
        <div className="text-muted-foreground min-w-0 font-mono text-sm">
          {message ?? "Failed to load data."}
        </div>
      </CardContent>
    </Card>
  );
}
