import * as React from "react";
import { cva, type VariantProps } from "class-variance-authority";
import { XIcon } from "lucide-react";
import { Dialog as DialogPrimitive } from "@base-ui/react/dialog";

import { cn } from "@/lib/utils";

type SheetSide = "top" | "right" | "bottom" | "left";

/**
 * Side-specific geometry + the off-screen transform Base UI animates from
 * (`data-starting-style`) and back to (`data-ending-style`).
 */
const SIDE_CLASSES: Record<SheetSide, string> = {
  right:
    "inset-y-0 right-0 h-full w-3/4 border-l sm:max-w-sm data-[starting-style]:translate-x-full data-[ending-style]:translate-x-full",
  left: "inset-y-0 left-0 h-full w-3/4 border-r sm:max-w-sm data-[starting-style]:-translate-x-full data-[ending-style]:-translate-x-full",
  top: "inset-x-0 top-0 h-auto border-b data-[starting-style]:-translate-y-full data-[ending-style]:-translate-y-full",
  bottom:
    "inset-x-0 bottom-0 h-auto border-t data-[starting-style]:translate-y-full data-[ending-style]:translate-y-full",
};

function Sheet({ ...props }: DialogPrimitive.Root.Props) {
  return <DialogPrimitive.Root data-slot="sheet" {...props} />;
}

function SheetPortal({ ...props }: DialogPrimitive.Portal.Props) {
  return <DialogPrimitive.Portal data-slot="sheet-portal" {...props} />;
}

function SheetOverlay({ className, ...props }: DialogPrimitive.Backdrop.Props) {
  return (
    <DialogPrimitive.Backdrop
      data-slot="sheet-overlay"
      className={cn(
        "fixed inset-0 z-50 bg-black/50 transition-opacity duration-200 data-[ending-style]:opacity-0 data-[starting-style]:opacity-0",
        className
      )}
      {...props}
    />
  );
}

function SheetContent({
  className,
  children,
  side = "right",
  showCloseButton = true,
  spacing = "default",
  shape = "default",
  ...props
}: DialogPrimitive.Popup.Props & {
  side?: SheetSide;
  showCloseButton?: boolean;
  /** `none` removes the gap between children (full-bleed layouts). */
  spacing?: "default" | "none";
  shape?: "default" | "roundedTop";
}) {
  return (
    <SheetPortal>
      <SheetOverlay />
      <DialogPrimitive.Popup
        data-slot="sheet-content"
        className={cn(
          "bg-background fixed z-50 flex flex-col gap-4 shadow-lg transition-transform duration-300 ease-in-out",
          spacing === "none" && "gap-0",
          shape === "roundedTop" && "rounded-t-2xl",
          SIDE_CLASSES[side],
          className
        )}
        {...props}
      >
        {children}
        {showCloseButton && (
          <DialogPrimitive.Close className="ring-offset-background focus-visible:ring-ring absolute top-4 right-4 rounded-xs opacity-70 transition-opacity hover:opacity-100 focus-visible:ring-2 focus-visible:ring-offset-2 focus-visible:outline-hidden disabled:pointer-events-none">
            <XIcon className="size-4" />
            <span className="sr-only">Close</span>
          </DialogPrimitive.Close>
        )}
      </DialogPrimitive.Popup>
    </SheetPortal>
  );
}

const sheetHeaderVariants = cva("flex flex-col", {
  variants: {
    variant: {
      default: "gap-1.5 p-4",
      /** Boarding header: bordered, padded bar. */
      bar: "gap-1.5 border-b border-border px-4 py-3",
      /** `bar` plus top safe-area padding (edge-anchored sheets). */
      barSafe: "gap-1.5 border-b border-border pt-safe px-4 py-3",
    },
    spacing: {
      default: "",
      roomy: "gap-2",
    },
  },
  defaultVariants: { variant: "default", spacing: "default" },
});

function SheetHeader({
  className,
  variant = "default",
  spacing = "default",
  ...props
}: React.ComponentProps<"div"> & VariantProps<typeof sheetHeaderVariants>) {
  return (
    <div
      data-slot="sheet-header"
      className={cn(sheetHeaderVariants({ variant, spacing }), className)}
      {...props}
    />
  );
}

const sheetTitleVariants = cva("text-foreground font-semibold", {
  variants: {
    variant: {
      default: "",
      /** Mono title, truncated (record names). */
      strip: "font-mono text-sm truncate",
      /** Small uppercase mono label (section headings). */
      label: "font-mono text-xs tracking-wider uppercase",
    },
  },
  defaultVariants: { variant: "default" },
});

function SheetTitle({
  className,
  variant = "default",
  ...props
}: DialogPrimitive.Title.Props & VariantProps<typeof sheetTitleVariants>) {
  return (
    <DialogPrimitive.Title
      data-slot="sheet-title"
      className={cn(sheetTitleVariants({ variant }), className)}
      {...props}
    />
  );
}

export { Sheet, SheetContent, SheetHeader, SheetTitle };
