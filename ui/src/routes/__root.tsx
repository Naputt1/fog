import { createRootRoute, Outlet } from "@tanstack/react-router";
import { AppShell } from "@/components/app-shell";
import { Toaster } from "@/components/ui/sonner";

export const Route = createRootRoute({
  component: RootComponent,
});

function RootComponent() {
  return (
    <AppShell>
      <Outlet />
      <Toaster />
    </AppShell>
  );
}
