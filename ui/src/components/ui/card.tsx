import * as React from "react";
import { cva, type VariantProps } from "class-variance-authority";

import { cn } from "@/lib/utils";

const cardVariants = cva(
  "bg-card text-card-foreground flex min-w-0 flex-col rounded-xl border shadow-sm",
  {
    variants: {
      spacing: {
        default: "gap-6",
        none: "gap-0",
        compact: "gap-3",
      },
      padding: {
        default: "py-6",
        none: "py-0",
        compact: "py-4",
      },
      /** Clickable card: hover border response. */
      interactive: {
        true: "transition-colors hover:border-primary/40",
        false: "",
      },
      /** De-emphasised card (e.g. a row being removed). */
      dimmed: {
        true: "opacity-60",
        false: "",
      },
      tone: {
        default: "",
        destructive: "border-destructive/40",
      },
    },
    defaultVariants: {
      spacing: "default",
      padding: "default",
      interactive: false,
      dimmed: false,
      tone: "default",
    },
  }
);

function Card({
  className,
  spacing = "default",
  padding = "default",
  interactive = false,
  dimmed = false,
  tone = "default",
  ...props
}: React.ComponentProps<"div"> & VariantProps<typeof cardVariants>) {
  return (
    <div
      data-slot="card"
      className={cn(
        cardVariants({ spacing, padding, interactive, dimmed, tone }),
        className
      )}
      {...props}
    />
  );
}

const cardHeaderVariants = cva(
  "@container/card-header grid auto-rows-min grid-rows-[auto_auto] items-start has-data-[slot=card-action]:grid-cols-[1fr_auto] [.border-b]:pb-6",
  {
    variants: {
      spacing: {
        default: "gap-2",
        tight: "gap-0.5",
      },
      padding: {
        default: "px-4 sm:px-6",
        narrow: "px-5",
      },
    },
    defaultVariants: { spacing: "default", padding: "default" },
  }
);

function CardHeader({
  className,
  spacing = "default",
  padding = "default",
  ...props
}: React.ComponentProps<"div"> & VariantProps<typeof cardHeaderVariants>) {
  return (
    <div
      data-slot="card-header"
      className={cn(cardHeaderVariants({ spacing, padding }), className)}
      {...props}
    />
  );
}

const cardTitleVariants = cva("leading-none font-semibold", {
  variants: {
    variant: {
      default: "",
      mono: "font-mono text-sm",
    },
    spacing: {
      default: "",
      inline: "gap-x-2",
    },
  },
  defaultVariants: { variant: "default", spacing: "default" },
});

function CardTitle({
  className,
  variant = "default",
  spacing = "default",
  ...props
}: React.ComponentProps<"div"> & VariantProps<typeof cardTitleVariants>) {
  return (
    <div
      data-slot="card-title"
      className={cn(cardTitleVariants({ variant, spacing }), className)}
      {...props}
    />
  );
}

const cardContentVariants = cva("min-w-0", {
  variants: {
    variant: {
      default: "",
      /** Vertical list rows inside a card. */
      stack: "gap-3 p-4",
      /** Single inline row (icon + message). */
      inline: "gap-3 py-4",
      /** Centered empty-state body. */
      empty: "py-10",
      /** Larger muted message body (errors, empty results). */
      message: "py-8 text-sm text-muted-foreground font-mono",
      /** Compact muted note. */
      note: "py-6 text-xs text-muted-foreground font-mono",
    },
    padding: {
      default: "px-4 sm:px-6",
      narrow: "px-5",
    },
    spacing: {
      default: "",
      loose: "space-y-6",
    },
  },
  defaultVariants: { variant: "default", padding: "default", spacing: "default" },
});

function CardDescription({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="card-description"
      className={cn("text-muted-foreground text-sm", className)}
      {...props}
    />
  );
}

function CardAction({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="card-action"
      className={cn(
        "col-start-2 row-span-2 row-start-1 self-start justify-self-end",
        className
      )}
      {...props}
    />
  );
}

function CardContent({
  className,
  variant = "default",
  padding = "default",
  spacing = "default",
  ...props
}: React.ComponentProps<"div"> & VariantProps<typeof cardContentVariants>) {
  return (
    <div
      data-slot="card-content"
      className={cn(cardContentVariants({ variant, padding, spacing }), className)}
      {...props}
    />
  );
}

function CardFooter({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="card-footer"
      className={cn(
        "flex items-center px-4 sm:px-6 [.border-t]:pt-6",
        className
      )}
      {...props}
    />
  );
}

export {
  Card,
  CardHeader,
  CardFooter,
  CardTitle,
  CardAction,
  CardDescription,
  CardContent,
};
