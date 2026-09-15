import { useState } from "react";
import { Link, useLocation, useNavigate } from "@tanstack/react-router";
import { PanelRight, Cog } from "lucide-react";

import { RightSidebarContext } from "@/lib/right-sidebar-context";
import { NAV_ITEMS, isNavActive, navIndexForPath } from "@/lib/nav";
import { useSwipeNavigation } from "@/lib/use-swipe-navigation";

import { cn, getHostLabel } from "@/lib/utils";
import { Button } from "@/components/ui/button";
import { BottomNav } from "@/components/bottom-nav";
import { BrandCloud } from "@/components/brand-cloud";

/** Swipe surface that owns its own gesture; the shell must not also react. */
const SERVICE_SWIPE_SCOPE = '[data-swipe-scope="services"]';

function Brand() {
  return (
    <div className="flex items-center gap-2 px-2">
      <div className="border-primary/40 bg-primary/10 text-primary flex size-7 items-center justify-center rounded border">
        <BrandCloud className="size-5" />
      </div>
      <div className="leading-tight">
        <div className="font-mono text-sm font-semibold tracking-tight">
          fog
        </div>
        <div className="text-muted-foreground font-mono text-[10px] tracking-wider uppercase">
          dashboard
        </div>
      </div>
    </div>
  );
}

function SidebarNav() {
  const location = useLocation();

  return (
    <nav aria-label="Primary" className="flex flex-col gap-1 px-2">
      {NAV_ITEMS.map((item) => {
        const active = isNavActive(item, location.pathname);
        const Icon = item.icon;
        return (
          <Link
            key={item.to}
            to={item.to}
            aria-current={active ? "page" : undefined}
            className={cn(
              "text-muted-foreground hover:bg-accent hover:text-accent-foreground flex items-center gap-2.5 rounded-md px-2.5 py-2 text-sm font-medium transition-colors",
              active &&
                "bg-accent text-accent-foreground shadow-[inset_0_0_0_1px_var(--color-primary)/25]"
            )}
            activeOptions={{ exact: item.match === "" }}
          >
            <Icon className="size-4 shrink-0" aria-hidden="true" />
            {item.label}
          </Link>
        );
      })}
    </nav>
  );
}

function SidebarContent() {
  const hostLabel = getHostLabel();
  return (
    <div className="flex h-full flex-col gap-4">
      <div className="border-border flex h-14 items-center border-b px-4">
        <Brand />
      </div>
      <SidebarNav />
      <div className="border-border mt-auto border-t p-4">
        <div className="text-muted-foreground flex items-center gap-2 font-mono text-xs">
          <Cog className="size-3.5" />
          {hostLabel}
        </div>
      </div>
    </div>
  );
}

export function AppShell({ children }: { children: React.ReactNode }) {
  const [rightOpen, setRightOpen] = useState(false);
  const [rightEnabled, setRightEnabled] = useState(false);
  const location = useLocation();
  const navigate = useNavigate();
  const index = navIndexForPath(location.pathname);
  const current = NAV_ITEMS[index] ?? NAV_ITEMS[0];

  const goTo = (nextIndex: number) => {
    const target = NAV_ITEMS[nextIndex];
    if (!target) return;
    void navigate({ to: target.to });
  };

  const swipeRef = useSwipeNavigation({
    onPrev: () => goTo(index <= 0 ? NAV_ITEMS.length - 1 : index - 1),
    onNext: () => goTo((index + 1) % NAV_ITEMS.length),
    ignoreScope: SERVICE_SWIPE_SCOPE,
  });

  return (
    <RightSidebarContext.Provider
      value={{
        open: rightOpen,
        setOpen: setRightOpen,
        enabled: rightEnabled,
        setEnabled: setRightEnabled,
      }}
    >
      <div className="bg-background text-foreground flex h-dvh w-full flex-col overflow-hidden md:flex-row">
        <a
          href="#main"
          className="bg-card focus-visible:ring-ring sr-only rounded-md px-3 py-2 text-sm focus-visible:not-sr-only focus-visible:absolute focus-visible:top-3 focus-visible:left-3 focus-visible:z-50 focus-visible:ring-2"
        >
          Skip to content
        </a>

        {/* Desktop sidebar */}
        <aside className="border-border bg-card hidden w-60 shrink-0 border-r md:block">
          <SidebarContent />
        </aside>

        <div className="flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden">
          {/* Topbar */}
          <header className="border-border bg-card/60 pt-safe flex min-h-14 shrink-0 items-center gap-3 border-b px-4 backdrop-blur">
            <div className="flex min-w-0 items-center gap-2 font-mono">
              <span className="text-primary">~</span>
              <span className="text-foreground truncate text-sm">
                /{current.label.toLowerCase()}
              </span>
            </div>

            <div className="ml-auto flex items-center gap-3">
              {rightEnabled && (
                <Button
                  variant="ghost"
                  size="icon"
                  aria-label="Open services"
                  className="lg:hidden"
                  onClick={() => setRightOpen(true)}
                >
                  <PanelRight className="size-5" />
                </Button>
              )}
            </div>
          </header>

          <div
            ref={swipeRef}
            className="min-h-0 flex-1 overflow-y-auto overscroll-contain"
          >
            <main
              id="main"
              tabIndex={-1}
              className="mx-auto w-full max-w-6xl min-w-0 p-4 outline-none md:p-6"
            >
              {children}
            </main>
          </div>

          <BottomNav />
        </div>
      </div>
    </RightSidebarContext.Provider>
  );
}
