import { Suspense, lazy, useRef, useState } from "react";

import { Button } from "@/components/ui/button";
import { LoadingState } from "@/components/page-state";
// xterm (~300K+) is the heaviest dependency of the dashboard. Both views are
// split into on-demand chunks so route bundles stay lean; the chunk loads when
// a terminal is actually rendered, behind a skeleton. `ref` passes straight
// through the lazy wrapper (React 19 ref-as-prop).
const TerminalView = lazy(() =>
  import("@/components/terminal/TerminalView").then((m) => ({
    default: m.TerminalView,
  }))
);
const LogView = lazy(() =>
  import("@/components/terminal/LogView").then((m) => ({ default: m.LogView }))
);
import { TerminalKeypad } from "@/components/terminal/TerminalKeypad";
import type { TerminalHandle } from "@/components/terminal/terminal-handle";
import { cn } from "@/lib/utils";

/** Which surface a {@link ServiceTerminal} is showing. */
export type TerminalMode = "logs" | "terminal";

/** The service a {@link ServiceTerminal} is attached to. */
export interface TerminalTarget {
  /** Docker container name, used to address `docker logs`. */
  container: string;
  /** Compose/fog service name, used for native PTY addressing. */
  service: string;
  /** Fog PID for native services; null for docker containers (no PTY). */
  pid: number | null;
}

/**
 * Logs (SSE) / Terminal (PTY) pane with its mode toggle, live checkbox and the
 * mobile keypad. Shared by the `/logs` page and the per-service bottom drawer,
 * which only differ in how they select the target and persist the mode.
 */
export function ServiceTerminal({
  active,
  mode,
  onModeChange,
  showModeToggle = true,
  className,
}: {
  active: TerminalTarget | null;
  mode: TerminalMode;
  onModeChange: (mode: TerminalMode) => void;
  showModeToggle?: boolean;
  className?: string;
}) {
  const termApiRef = useRef<TerminalHandle | null>(null);
  const [live, setLive] = useState(true);

  // Logs (SSE) is the default: fast on slow networks, read-only. PTY is
  // opt-in via the toggle (and unavailable for docker containers, which have
  // no fog PTY — they always use `docker logs`).
  const isDocker = active?.pid == null;
  const showTerminal = mode === "terminal" && !isDocker;

  const handleCopy = () => {
    const text = termApiRef.current?.copyText() ?? "";
    if (!text) return;
    if (navigator.clipboard?.writeText) {
      return navigator.clipboard.writeText(text);
    }
    // Fallback for contexts without the async clipboard API.
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.style.position = "fixed";
    ta.style.opacity = "0";
    document.body.appendChild(ta);
    ta.select();
    try {
      document.execCommand("copy");
    } finally {
      document.body.removeChild(ta);
    }
  };

  return (
    <div
      className={cn(
        "flex min-h-0 min-w-0 flex-1 flex-col gap-3",
        className
      )}
    >
      {showModeToggle ? (
        <div className="flex shrink-0 flex-wrap items-center gap-2">
          <div className="inline-flex rounded-md border p-1">
            <Button
              variant={showTerminal ? "ghost" : "default"}
              size="sm"
              className="h-7 font-mono text-xs"
              onClick={() => onModeChange("logs")}
            >
              Logs (SSE)
            </Button>
            <Button
              variant={showTerminal ? "default" : "ghost"}
              size="sm"
              className={cn("h-7 font-mono text-xs", isDocker && "opacity-50")}
              onClick={() => onModeChange("terminal")}
              disabled={isDocker}
              title={
                isDocker
                  ? "PTY not available for docker containers"
                  : "Interactive PTY shell (bidirectional) via WebSocket"
              }
            >
              Terminal (PTY)
            </Button>
          </div>
          <span className="text-muted-foreground font-mono text-xs">
            {showTerminal
              ? `interactive shell${active ? ` — ${active.service} workdir` : " — ephemeral"}`
              : "read-only stream from docker/fog"}
          </span>
        </div>
      ) : null}
      {isDocker && showModeToggle ? (
        <div className="text-muted-foreground flex shrink-0 items-center gap-2 font-mono text-xs">
          <span className="rounded-full bg-amber-500/20 px-2 py-0.5 text-amber-600">
            docker logs — read-only
          </span>
          <span>PTY not available for container — streaming `docker logs`</span>
        </div>
      ) : (
        showTerminal && (
          <label className="flex shrink-0 items-center gap-2 text-sm">
            <input
              type="checkbox"
              checked={live}
              onChange={(e) => setLive(e.target.checked)}
              disabled={!active}
              className="rounded"
            />
            <span className={active ? "" : "text-muted-foreground"}>
              Live — same PTY as TUI (mirror service, bidirectional). Unchecked
              = fresh shell in service workdir.
            </span>
          </label>
        )
      )}
      {showTerminal ? (
        <Suspense fallback={<LoadingState label="Loading terminal…" />}>
          <TerminalView
            key={`${active?.service ?? "__shell__"}:${live ? "live" : "cwd"}`}
            ref={termApiRef}
            service={active?.service}
            live={live && !!active}
            className="flex min-h-[320px] flex-1 lg:min-h-0"
          />
        </Suspense>
      ) : (
        <Suspense fallback={<LoadingState label="Loading logs…" />}>
          <LogView
            key={`${active?.container ?? "__none__"}`}
            ref={termApiRef}
            container={active?.container ?? null}
            pid={active?.pid ?? null}
            service={active?.service ?? null}
            className="flex min-h-[320px] flex-1 lg:min-h-0"
          />
        </Suspense>
      )}
      <TerminalKeypad
        input={showTerminal}
        enabled={!!active}
        onDispatch={(init, keyCode) =>
          termApiRef.current?.dispatchKey(init, keyCode)
        }
        onRaw={(data) => termApiRef.current?.sendRaw(data)}
        onScroll={(amount) => termApiRef.current?.scroll(amount)}
        onCopy={handleCopy}
        className="shrink-0 lg:hidden"
      />
    </div>
  );
}
