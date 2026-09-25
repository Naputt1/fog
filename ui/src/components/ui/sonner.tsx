import type { CSSProperties } from "react";
import { Toaster as Sonner, type ToasterProps } from "sonner";

/**
 * App-wide toast host.
 *
 * The web UI ships a single dark theme (see `index.css`), so the Sonner theme
 * is fixed rather than read from a provider. Offsets keep toasts clear of the
 * fixed topbar and the mobile bottom navigation. Colors reuse the shadcn
 * popover/border tokens so toasts match cards and dialogs.
 */
function Toaster(props: ToasterProps) {
  return (
    <Sonner
      theme="dark"
      richColors
      closeButton
      position="bottom-right"
      offset={{ bottom: "calc(var(--bottom-nav-h) + 0.75rem)", right: "1rem" }}
      mobileOffset={{
        bottom: "calc(var(--bottom-nav-h) + 0.75rem)",
        left: "1rem",
        right: "1rem",
      }}
      className="toaster group"
      style={
        {
          "--normal-bg": "var(--popover)",
          "--normal-text": "var(--popover-foreground)",
          "--normal-border": "var(--border)",
        } as CSSProperties
      }
      {...props}
    />
  );
}

export { Toaster };
