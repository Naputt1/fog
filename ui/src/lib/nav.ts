import {
  Activity,
  Boxes,
  HeartPulse,
  SquareTerminal,
  type LucideIcon,
} from "lucide-react";

export interface NavItem {
  to: string;
  label: string;
  icon: LucideIcon;
  /** Path segment used to match active state ("" for the index route). */
  match: string;
}

export const NAV_ITEMS: NavItem[] = [
  { to: "/", label: "Services", icon: Boxes, match: "" },
  { to: "/logs", label: "Terminal", icon: SquareTerminal, match: "/logs" },
  { to: "/health", label: "Health", icon: HeartPulse, match: "/health" },
  { to: "/status", label: "Status", icon: Activity, match: "/status" },
];

/**
 * Whether a nav destination is active for a pathname. The Services item
 * (`match: ""`) owns both the index and every drill-down route under
 * `/projects`, so the tab stays lit while browsing a project/branch.
 */
export function isNavActive(item: NavItem, pathname: string): boolean {
  if (item.match === "") {
    return pathname === "/" || pathname.startsWith("/projects");
  }
  return pathname.startsWith(item.match);
}

/** Index of the nav item matching a pathname, or -1. */
export function navIndexForPath(pathname: string): number {
  return NAV_ITEMS.findIndex((item) => isNavActive(item, pathname));
}
