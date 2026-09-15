import { Link, useLocation } from "@tanstack/react-router";

import { NAV_ITEMS } from "@/lib/nav";
import { cn } from "@/lib/utils";

/**
 * Thumb-reachable primary navigation for phones. A regular flex child at the
 * bottom of the shell (not `position: fixed`, which misbehaves under the app's
 * `overflow: clip`), so it reserves its own space and clears the home
 * indicator via the safe-area inset. The active destination is marked with
 * `aria-current="page"`, which also drives its styling.
 */
export function BottomNav() {
  const { pathname } = useLocation();

  return (
    <nav
      aria-label="Primary"
      className="border-border bg-card/95 pb-safe shrink-0 border-t backdrop-blur md:hidden"
    >
      <ul className="mx-auto grid max-w-lg grid-cols-4">
        {NAV_ITEMS.map((item) => {
          const active =
            item.match === ""
              ? pathname === "/"
              : pathname.startsWith(item.match);
          const Icon = item.icon;
          return (
            <li key={item.to}>
              <Link
                to={item.to}
                activeOptions={{ exact: item.match === "" }}
                aria-current={active ? "page" : undefined}
                className={cn(
                  "text-muted-foreground focus-visible:ring-ring/60 flex min-h-14 flex-col items-center justify-center gap-1 px-1 text-[11px] font-medium transition-colors outline-none focus-visible:ring-2 focus-visible:ring-inset",
                  active && "text-primary"
                )}
              >
                <Icon className="size-5 shrink-0" aria-hidden="true" />
                {item.label}
              </Link>
            </li>
          );
        })}
      </ul>
    </nav>
  );
}
