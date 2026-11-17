import { useState } from "react";
import { Network } from "lucide-react";

import type { InstancePort } from "@/lib/api";
import type { InstanceView } from "@/lib/services";
import { cn } from "@/lib/utils";
import { useMediaQuery } from "@/lib/use-media-query";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Sheet, SheetContent } from "@/components/ui/sheet";

/** Split `name@repo:branch:script` into the local name and its source. */
function splitPortName(name: string): { local: string; source?: string } {
  const at = name.indexOf("@");
  if (at === -1) return { local: name };
  return { local: name.slice(0, at), source: name.slice(at + 1) };
}

function PortName({
  port,
  wrap = false,
}: {
  port: InstancePort;
  wrap?: boolean;
}) {
  const { local, source } = splitPortName(port.name);
  return (
    <span
      className={cn("min-w-0 font-mono", wrap ? "break-all" : "truncate")}
      title={port.name}
    >
      <span className="text-foreground">{local}</span>
      {source ? <span className="text-muted-foreground">@{source}</span> : null}
      <span className="text-muted-foreground">:{port.port}</span>
    </span>
  );
}

function PortBadges({ port }: { port: InstancePort }) {
  return (
    <>
      {port.external ? (
        <Badge variant="outline" tone="unknown" size="mono">
          external
        </Badge>
      ) : null}
      {port.shared ? (
        <Badge variant="secondary" size="mono">
          shared
        </Badge>
      ) : null}
    </>
  );
}

/**
 * Declared ports of one instance: symbolic name, assigned port, and a badge
 * when the port is a shared resource or a cross-instance reference. The tooltip
 * lists the services that use it.
 */
export function PortList({ ports }: { ports: readonly InstancePort[] }) {
  if (ports.length === 0) return null;
  return (
    <div className="flex flex-wrap items-center gap-1.5 font-mono text-xs">
      <span className="text-muted-foreground text-2xs mr-0.5 tracking-wider uppercase">
        ports
      </span>
      {ports.map((port) => (
        <span
          key={port.name}
          title={
            port.services.length
              ? `used by ${port.services.join(", ")}`
              : undefined
          }
          className="border-border/70 bg-card inline-flex max-w-full items-center gap-1.5 rounded-md border px-2 py-0.5"
        >
          <PortName port={port} />
          <PortBadges port={port} />
        </span>
      ))}
    </div>
  );
}

/** Expanded list for the dialog/sheet, where provenance can wrap. */
function PortDetails({ ports }: { ports: readonly InstancePort[] }) {
  return (
    <ul className="divide-border divide-y">
      {ports.map((port) => (
        <li
          key={port.name}
          className="flex flex-col gap-1 py-2.5 first:pt-0 last:pb-0"
        >
          <div className="flex min-w-0 flex-wrap items-center gap-2">
            <PortName port={port} wrap />
            <span className="ml-auto flex shrink-0 items-center gap-1">
              <PortBadges port={port} />
            </span>
          </div>
          <span className="text-muted-foreground text-2xs font-mono">
            {port.services.length
              ? `used by ${port.services.join(", ")}`
              : "not reported as used by a service"}
          </span>
        </li>
      ))}
    </ul>
  );
}

/**
 * Compact `ports N` trigger for instance rows. Opens the same details in a
 * centered dialog on desktop and a bottom sheet on mobile. Renders nothing when
 * the instance reports no ports (older fog or a script without a ports map).
 */
export function InstancePortsButton({
  inst,
  className,
}: {
  inst: InstanceView;
  className?: string;
}) {
  const [open, setOpen] = useState(false);
  const isDesktop = useMediaQuery("(min-width: 1024px)");
  const ports = inst.ports;
  if (ports.length === 0) return null;

  const subtitle = `${inst.script} · ${inst.project}${
    inst.branch ? ` @ ${inst.branch}` : ""
  } · pid ${inst.pid}`;

  const panel = (
    <>
      <DialogHeader>
        <div className="flex min-w-0 flex-col pr-10">
          <DialogTitle>Ports</DialogTitle>
          <span className="text-muted-foreground text-2xs truncate font-mono">
            {subtitle}
          </span>
        </div>
      </DialogHeader>
      <div className="p-4">
        <PortDetails ports={ports} />
      </div>
    </>
  );

  return (
    <>
      <Button
        size="xs"
        variant="outline"
        font="mono"
        className={className}
        aria-haspopup="dialog"
        aria-label={`Ports for ${inst.script} pid ${inst.pid}`}
        onClick={() => setOpen(true)}
      >
        <Network aria-hidden />
        ports {ports.length}
      </Button>
      {isDesktop ? (
        <Dialog open={open} onOpenChange={setOpen}>
          <DialogContent
            spacing="none"
            className="max-h-[80vh] max-w-md overflow-y-auto"
          >
            {panel}
          </DialogContent>
        </Dialog>
      ) : (
        <Sheet open={open} onOpenChange={setOpen}>
          <SheetContent
            side="bottom"
            shape="roundedTop"
            spacing="none"
            className="max-h-[80dvh] overflow-y-auto"
          >
            <div
              aria-hidden
              className="bg-muted-foreground/30 mx-auto mt-2 h-1 w-10 shrink-0 rounded-full"
            />
            {panel}
          </SheetContent>
        </Sheet>
      )}
    </>
  );
}
