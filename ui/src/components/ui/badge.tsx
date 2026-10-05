import * as React from "react";
import { cva, type VariantProps } from "class-variance-authority";

import { cn } from "@/lib/utils";

const badgeVariants = cva(
  "inline-flex w-fit shrink-0 items-center justify-center gap-1 truncate rounded-full border border-transparent px-2 py-0.5 text-xs font-medium whitespace-nowrap transition-[color,box-shadow] focus-visible:border-ring focus-visible:ring-[3px] focus-visible:ring-ring/50 aria-invalid:border-destructive aria-invalid:ring-destructive/20 dark:aria-invalid:ring-destructive/40 [&>svg]:pointer-events-none [&>svg]:size-3",
  {
    variants: {
      variant: {
        default: "bg-primary text-primary-foreground [a&]:hover:bg-primary/90",
        secondary:
          "bg-secondary text-secondary-foreground [a&]:hover:bg-secondary/90",
        destructive:
          "bg-destructive text-white focus-visible:ring-destructive/20 dark:bg-destructive/60 dark:focus-visible:ring-destructive/40 [a&]:hover:bg-destructive/90",
        outline:
          "border-border text-foreground [a&]:hover:bg-accent [a&]:hover:text-accent-foreground",
        ghost: "[a&]:hover:bg-accent [a&]:hover:text-accent-foreground",
        link: "text-primary underline-offset-4 [a&]:hover:underline",
      },
      /** Service/lifecycle tones. Pair with `variant="outline"`. */
      tone: {
        default: "",
        running: "bg-primary/15 text-primary border-primary/30",
        healthy: "bg-success/15 text-success border-success/30",
        starting: "bg-info/15 text-info border-info/30",
        stopped: "bg-muted text-muted-foreground border-border",
        stopping: "bg-warning/15 text-warning border-warning/30",
        killing: "bg-warning/15 text-warning border-warning/30",
        unhealthy: "bg-destructive/15 text-destructive border-destructive/30",
        unknown: "border-border text-muted-foreground",
      },
      size: {
        default: "",
        /** Status pill: mono, capitalized label with roomier gap. */
        status: "gap-1.5 font-mono capitalize",
        /** Monospace label only. */
        mono: "font-mono",
      },
    },
    defaultVariants: {
      variant: "default",
      tone: "default",
      size: "default",
    },
  }
);

function Badge({
  className,
  variant = "default",
  tone = "default",
  size = "default",
  ...props
}: React.ComponentProps<"span"> & VariantProps<typeof badgeVariants>) {
  return (
    <span
      data-slot="badge"
      data-variant={variant}
      data-tone={tone}
      className={cn(badgeVariants({ variant, tone, size }), className)}
      {...props}
    />
  );
}

export { Badge, badgeVariants };
