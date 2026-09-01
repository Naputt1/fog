import { useEffect, useRef, useState, useCallback } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";

import { subscribeLogs } from "@/lib/api";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

const TERMINAL_THEME = {
  background: "#0d1117",
  foreground: "#e6edf3",
  cursor: "#e6edf3",
  cursorAccent: "#0d1117",
  selectionBackground: "#264f78",
  black: "#484f58",
  red: "#ff7b72",
  green: "#3fb950",
  yellow: "#d29922",
  blue: "#58a6ff",
  magenta: "#bc8cff",
  cyan: "#39c5cf",
  white: "#c9d1d9",
  brightBlack: "#6e7681",
  brightRed: "#ffa198",
  brightGreen: "#56d364",
  brightYellow: "#e3b341",
  brightBlue: "#79c0ff",
  brightMagenta: "#d2a8ff",
  brightCyan: "#56d4dd",
  brightWhite: "#f0f6fc",
} as const;

export function LogView({
  className,
  container,
  pid,
  service,
}: {
  className?: string;
  container: string | null;
  pid: number | null;
  service: string | null;
}) {
  const elRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const [connected, setConnected] = useState(false);
  const [fitted, setFitted] = useState(false);

  const fit = useCallback(() => {
    try {
      fitRef.current?.fit();
    } catch {
      /* not measurable yet */
    }
  }, []);

  useEffect(() => {
    const el = elRef.current;
    if (!el) return;
    const term = new Terminal({
      cursorBlink: false,
      disableStdin: true,
      scrollback: 5000,
      fontFamily: '"JetBrains Mono", ui-monospace, SFMono-Regular, Menlo, monospace',
      fontSize: 13,
      lineHeight: 1.25,
      theme: TERMINAL_THEME,
    });
    const fitAddon = new FitAddon();
    term.loadAddon(fitAddon);
    el.style.visibility = "hidden";
    term.open(el);
    try {
      fitAddon.fit();
      el.style.visibility = "";
      setFitted(true);
    } catch {
      el.style.visibility = "";
    }
    termRef.current = term;
    fitRef.current = fitAddon;
    const ro = new ResizeObserver(fit);
    ro.observe(el);
    window.addEventListener("resize", fit);
    return () => {
      window.removeEventListener("resize", fit);
      ro.disconnect();
      term.dispose();
      termRef.current = null;
      fitRef.current = null;
    };
  }, [fit]);

  useEffect(() => {
    const term = termRef.current;
    if (!term) return;
    if (!container && !service) {
      term.clear();
      term.writeln("\x1b[33mNo service selected\x1b[0m");
      return;
    }
    // For docker logs, the backend expects `?service=<container>` (pid null).
    // For native fog services, it expects `?pid=<pid>&service=<name>`.
    const logService = pid != null ? (service ?? container ?? "") : (container ?? service ?? "");
    if (!logService) return;

    term.clear();
    term.writeln(`\x1b[90m[log] connecting ${logService}${pid != null ? ` (pid ${pid})` : ""} …\x1b[0m`);
    setConnected(false);

    const unsub = subscribeLogs(logService, {
      pid,
      onOpen: () => {
        setConnected(true);
        term.writeln("\x1b[32m[log] connected\x1b[0m");
      },
      onLine: (line) => {
        // strip trailing carriage returns, write line with newline
        term.writeln(line.text ?? "");
      },
      onError: () => setConnected(false),
    });
    return () => {
      unsub();
      setConnected(false);
    };
  }, [container, pid, service]);

  const handleClear = useCallback(() => {
    termRef.current?.clear();
  }, []);

  return (
    <div className={cn("flex flex-col overflow-hidden rounded-lg border", className)}>
      <div className="border-border bg-card/60 flex h-10 shrink-0 items-center gap-2 border-b px-3">
        <span className="flex items-center gap-2 font-mono text-xs">
          <span className={cn("size-2 rounded-full", connected ? "bg-emerald-500" : "bg-amber-400 animate-pulse")} />
          <span className="text-muted-foreground">{connected ? "streaming" : "connecting…"}</span>
        </span>
        <div className="ml-auto flex items-center gap-2">
          <Button variant="ghost" size="sm" onClick={handleClear}>
            Clear
          </Button>
        </div>
      </div>
      <div className="bg-[#0d1117] flex min-h-0 flex-1 p-2">
        <div ref={elRef} className={cn("h-full min-h-[280px] w-full", !fitted && "opacity-0")} />
      </div>
    </div>
  );
}
