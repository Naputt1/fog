import { useState } from "react";
import { Link, useLocation } from "@tanstack/react-router";
import {
  Boxes,
  TerminalSquare,
  HeartPulse,
  Activity,
  PanelLeft,
  Cog,
  type LucideIcon,
} from "lucide-react";

import { cn } from "@/lib/utils";
import { Button } from "@/components/ui/button";
import { Sheet, SheetContent, SheetTrigger } from "@/components/ui/sheet";
import { ScrollArea } from "@/components/ui/scroll-area";
import { BrandCloud } from "@/components/brand-cloud";

interface NavItem {
  to: string;
  label: string;
  icon: LucideIcon;
  /** Path segment used to match active state ("" for the index route). */
  match: string;
}

const NAV_ITEMS: NavItem[] = [
  { to: "/", label: "Services", icon: Boxes, match: "" },
  { to: "/logs", label: "Logs", icon: TerminalSquare, match: "/logs" },
  { to: "/health", label: "Health", icon: HeartPulse, match: "/health" },
  { to: "/status", label: "Status", icon: Activity, match: "/status" },
];

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

function SidebarNav({ onNavigate }: { onNavigate?: () => void }) {
  const location = useLocation();

  return (
    <nav className="flex flex-col gap-1 px-2">
      {NAV_ITEMS.map((item) => {
        const active =
          item.match === ""
            ? location.pathname === "/"
            : location.pathname.startsWith(item.match);
        const Icon = item.icon;
        return (
          <Link
            key={item.to}
            to={item.to}
            onClick={onNavigate}
            className={cn(
              "text-muted-foreground hover:bg-accent hover:text-accent-foreground flex items-center gap-2.5 rounded-md px-2.5 py-2 text-sm font-medium transition-colors",
              active &&
                "bg-accent text-accent-foreground shadow-[inset_0_0_0_1px_var(--color-primary)/25]"
            )}
            activeOptions={{ exact: item.match === "" }}
          >
            <Icon className="size-4 shrink-0" />
            {item.label}
          </Link>
        );
      })}
    </nav>
  );
}

function SidebarContent({ onNavigate }: { onNavigate?: () => void }) {
  return (
    <div className="flex h-full flex-col gap-4">
      <div className="border-border flex h-14 items-center border-b px-4">
        <Brand />
      </div>
      <SidebarNav onNavigate={onNavigate} />
      <div className="border-border mt-auto border-t p-4">
        <div className="text-muted-foreground flex items-center gap-2 font-mono text-xs">
          <Cog className="size-3.5" />
          127.0.0.1:18080
        </div>
      </div>
    </div>
  );
}

export function AppShell({ children }: { children: React.ReactNode }) {
  const [mobileOpen, setMobileOpen] = useState(false);
  const location = useLocation();
  const current =
    NAV_ITEMS.find((i) =>
      i.match === ""
        ? location.pathname === "/"
        : location.pathname.startsWith(i.match)
    ) ?? NAV_ITEMS[0];

  return (
    <Sheet open={mobileOpen} onOpenChange={setMobileOpen}>
      <div className="bg-background text-foreground flex min-h-dvh w-full overflow-x-hidden">
        {/* Desktop sidebar */}
        <aside className="border-border bg-card hidden w-60 shrink-0 border-r md:block">
          <SidebarContent />
        </aside>

        <div className="flex min-w-0 flex-1 flex-col">
          {/* Topbar */}
          <header className="border-border bg-card/60 flex h-14 shrink-0 items-center gap-3 border-b px-4 backdrop-blur">
            <SheetTrigger asChild className="md:hidden">
              <Button variant="ghost" size="icon" aria-label="Open navigation">
                <PanelLeft className="size-5" />
              </Button>
            </SheetTrigger>

            <div className="flex min-w-0 items-center gap-2 font-mono">
              <span className="text-primary">~</span>
              <span className="text-foreground truncate text-sm">
                /{current.label.toLowerCase()}
              </span>
            </div>

            <div className="ml-auto flex items-center gap-3">
              <div className="border-border bg-background flex items-center gap-1.5 rounded-full border px-2.5 py-1">
                <span className="bg-primary size-1.5 rounded-full" />
                <span className="text-muted-foreground font-mono text-[11px]">
                  connected
                </span>
              </div>
            </div>
          </header>

          <ScrollArea className="flex-1">
            <main className="mx-auto w-full max-w-6xl min-w-0 p-4 md:p-6">
              {children}
            </main>
          </ScrollArea>
        </div>
      </div>

      {/* Mobile sidebar (fixed overlay, side="left") */}
      <SheetContent side="left" className="w-60 p-0">
        <SidebarContent onNavigate={() => setMobileOpen(false)} />
      </SheetContent>
    </Sheet>
  );
}
