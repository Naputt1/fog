import { createFileRoute } from "@tanstack/react-router";
import { useEffect, useState } from "react";

import { TerminalView } from "@/components/terminal/TerminalView";
import { PageHeader } from "@/components/page-state";

export const Route = createFileRoute("/terminal")({
  component: TerminalPage,
  validateSearch: (search: Record<string, unknown>) => ({
    service: (search.service as string) || undefined,
  }),
});

function TerminalPage() {
  const { service: initialService } = Route.useSearch();
  const [service, setService] = useState<string | undefined>(initialService);
  const [services, setServices] = useState<string[]>([]);
  const navigate = Route.useNavigate();

  useEffect(() => {
    // Discover running fog services for the "attach to service" picker.
    // Uses the index server's fog IPC discovery (same as /api/services but for services).
    // Fallback to empty list if not available.
    fetch("/api/status")
      .then((r) => (r.ok ? r.json() : null))
      .then((data) => {
        if (data && Array.isArray(data.services)) {
          setServices(data.services.map((s: { name: string }) => s.name));
        } else if (data && Array.isArray(data.instances)) {
          const names = new Set<string>();
          for (const inst of data.instances as Array<{ services?: Array<{ name: string }> }>) {
            for (const s of inst.services || []) names.add(s.name);
          }
          setServices(Array.from(names));
        }
      })
      .catch(() => {});
    // Also try /api/services (docker) as additional source
    fetch("/api/services")
      .then((r) => (r.ok ? r.json() : null))
      .then((data) => {
        if (data && Array.isArray(data.services)) {
          setServices((prev) => {
            const merged = new Set(prev);
            for (const s of data.services as Array<{ service: string }>) merged.add(s.service);
            return Array.from(merged);
          });
        }
      })
      .catch(() => {});
  }, []);

  const onServiceChange = (value: string) => {
    const next = value || undefined;
    setService(next);
    navigate({ search: (prev) => ({ ...prev, service: next }) } as never);
  };

  return (
    <div className="space-y-4">
      <PageHeader
        title="Terminal"
        description={
          service
            ? `Shell in service "${service}" workdir (cwd + env). Service keeps running after disconnect.`
            : "Interactive shell attached to the local fog host via WebSocket. Attach to a service to get its workdir."
        }
      />
      <div className="flex items-center gap-2">
        <label className="text-sm text-muted-foreground">Attach to service:</label>
        <select
          value={service || ""}
          onChange={(e) => onServiceChange(e.target.value)}
          className="h-8 rounded-md border bg-background px-2 text-sm"
        >
          <option value="">— ephemeral shell —</option>
          {services.map((name) => (
            <option key={name} value={name}>
              {name}
            </option>
          ))}
        </select>
        <span className="text-xs text-muted-foreground">
          {service ? `?service=${service}` : "no service (generic shell)"}
        </span>
      </div>
      <TerminalView key={service || "__shell__"} service={service} />
      <p className="text-xs text-muted-foreground">
        Backend: <code>GET /ws/terminal{service ? `?service=${service}` : ""}</code> spawns
        xterm-256color shell {service ? `in service workdir` : ""}. True live PTY attach (share
        service MasterPty) is not yet implemented — this is a cwd+env attach. Service PTY lives
        in fog daemon process and is not shareable via index server without IPC FD passing.
      </p>
    </div>
  );
}