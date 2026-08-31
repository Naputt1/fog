import { createFileRoute } from "@tanstack/react-router";
import { useEffect, useState } from "react";

import { TerminalView } from "@/components/terminal/TerminalView";
import { PageHeader } from "@/components/page-state";

export const Route = createFileRoute("/terminal")({
  validateSearch: (search: Record<string, unknown>) => ({
    service: (search.service as string) || undefined,
  }),
  component: TerminalRedirect,
});

function TerminalRedirect() {
  const search = Route.useSearch();
  const navigate = Route.useNavigate();
  useEffect(() => {
    void navigate({ to: "/logs", search: search as never, replace: true });
  }, [navigate, search]);
  return null;
}