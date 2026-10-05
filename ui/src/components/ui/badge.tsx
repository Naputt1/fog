import * as React from "react";
import type { VariantProps } from "class-variance-authority";

import { cn } from "@/lib/utils";
import { badgeVariants } from "@/components/ui/badge-variants";

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

export { Badge };
