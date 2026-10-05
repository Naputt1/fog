import * as React from "react";
import { cva, type VariantProps } from "class-variance-authority";

import { cn } from "@/lib/utils";

function Table({ className, ...props }: React.ComponentProps<"table">) {
  return (
    <div
      data-slot="table-container"
      className="relative w-full overflow-x-auto"
    >
      <table
        data-slot="table"
        className={cn("w-full caption-bottom text-sm", className)}
        {...props}
      />
    </div>
  );
}

const tableHeaderVariants = cva("[&_tr]:border-b", {
  variants: {
    variant: {
      default: "",
      /** Sticky header that repaints cells with the card background. */
      card: "[&_th]:bg-card [&_th]:border-b [&_th]:border-border",
    },
  },
  defaultVariants: { variant: "default" },
});

function TableHeader({
  className,
  variant = "default",
  ...props
}: React.ComponentProps<"thead"> & VariantProps<typeof tableHeaderVariants>) {
  return (
    <thead
      data-slot="table-header"
      className={cn(tableHeaderVariants({ variant }), className)}
      {...props}
    />
  );
}

function TableBody({ className, ...props }: React.ComponentProps<"tbody">) {
  return (
    <tbody
      data-slot="table-body"
      className={cn("[&_tr:last-child]:border-0", className)}
      {...props}
    />
  );
}

function TableFooter({ className, ...props }: React.ComponentProps<"tfoot">) {
  return (
    <tfoot
      data-slot="table-footer"
      className={cn(
        "bg-muted/50 border-t font-medium [&>tr]:last:border-b-0",
        className
      )}
      {...props}
    />
  );
}

const tableRowVariants = cva(
  "has-aria-expanded:bg-muted/50 data-[state=selected]:bg-muted border-b transition-colors",
  {
    variants: {
      variant: {
        default: "hover:bg-muted/50",
        /** Persistent highlight (e.g. the running service). */
        accent: "bg-accent/60 hover:bg-accent/60",
        /** No hover response (nested/detail rows). */
        static: "hover:bg-transparent",
      },
    },
    defaultVariants: { variant: "default" },
  }
);

function TableRow({
  className,
  variant = "default",
  ...props
}: React.ComponentProps<"tr"> & VariantProps<typeof tableRowVariants>) {
  return (
    <tr
      data-slot="table-row"
      className={cn(tableRowVariants({ variant }), className)}
      {...props}
    />
  );
}

function TableHead({ className, ...props }: React.ComponentProps<"th">) {
  return (
    <th
      data-slot="table-head"
      className={cn(
        "text-foreground h-10 px-2 text-left align-middle font-medium whitespace-nowrap [&:has([role=checkbox])]:pr-0 [&>[role=checkbox]]:translate-y-[2px]",
        className
      )}
      {...props}
    />
  );
}

const tableCellVariants = cva(
  "align-middle whitespace-nowrap [&:has([role=checkbox])]:pr-0 [&>[role=checkbox]]:translate-y-[2px]",
  {
    variants: {
      variant: {
        default: "",
        mono: "font-mono",
        muted: "text-muted-foreground",
        monoMuted: "font-mono text-muted-foreground",
        /** Emphasised key column (name/pid). */
        monoStrong: "font-mono font-medium",
      },
      padding: {
        default: "p-2",
        /** Top-aligned cells in a multi-line row. */
        flush: "pt-0",
      },
    },
    defaultVariants: { variant: "default", padding: "default" },
  }
);

function TableCell({
  className,
  variant = "default",
  padding = "default",
  ...props
}: React.ComponentProps<"td"> & VariantProps<typeof tableCellVariants>) {
  return (
    <td
      data-slot="table-cell"
      className={cn(tableCellVariants({ variant, padding }), className)}
      {...props}
    />
  );
}

function TableCaption({
  className,
  ...props
}: React.ComponentProps<"caption">) {
  return (
    <caption
      data-slot="table-caption"
      className={cn("text-muted-foreground mt-4 text-sm", className)}
      {...props}
    />
  );
}

export {
  Table,
  TableHeader,
  TableBody,
  TableFooter,
  TableRow,
  TableHead,
  TableCell,
  TableCaption,
};
