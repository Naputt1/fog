import * as React from "react";
import type { VariantProps } from "class-variance-authority";
import { Loader2 } from "lucide-react";

import { cn } from "@/lib/utils";
import { buttonVariants } from "@/components/ui/button-variants";

/**
 * Button with an optional in-flight state. `loading` swaps in a spinner (and
 * `loadingLabel` for the label, falling back to the normal children) and
 * disables the control, so action buttons render consistent feedback.
 */
function Button({
  className,
  variant = "default",
  size = "default",
  font = "default",
  text = "default",
  dimmed = false,
  loading = false,
  loadingLabel,
  disabled,
  children,
  ...props
}: React.ComponentProps<"button"> &
  VariantProps<typeof buttonVariants> & {
    loading?: boolean;
    loadingLabel?: React.ReactNode;
  }) {
  return (
    <button
      data-slot="button"
      data-variant={variant}
      data-size={size}
      disabled={disabled || loading}
      aria-busy={loading || undefined}
      className={cn(
        buttonVariants({ variant, size, font, text, dimmed, className })
      )}
      {...props}
    >
      {loading ? <Loader2 className="animate-spin" aria-hidden /> : null}
      {loading ? (loadingLabel ?? children) : children}
    </button>
  );
}

export { Button };
