import { useSyncExternalStore } from "react";

/**
 * Subscribe to a CSS media query, returning whether it currently matches.
 *
 * Lets a component render a different surface per viewport (e.g. a centered
 * dialog on desktop vs. a bottom sheet on mobile) instead of mounting both and
 * hiding one with CSS — only one dialog is ever live, so there is no duplicate
 * scroll lock or focus trap. Browser-only (no SSR): the server snapshot is
 * `false`.
 */
export function useMediaQuery(query: string): boolean {
  const subscribe = (onChange: () => void) => {
    const mql = window.matchMedia(query);
    mql.addEventListener("change", onChange);
    return () => mql.removeEventListener("change", onChange);
  };

  return useSyncExternalStore(
    subscribe,
    () => window.matchMedia(query).matches,
    () => false
  );
}
