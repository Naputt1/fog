import { useCallback, useEffect, useRef, useState } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebLinksAddon } from "@xterm/addon-web-links";
import "@xterm/xterm/css/xterm.css";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

const WS_PATH = "/ws/terminal";

/** Max auto-reconnect attempts (0 = never auto-reconnect). */
const MAX_RECONNECT_ATTEMPTS = 5;
/** Initial reconnect delay in ms; doubles per attempt up to 8s. */
const BASE_RECONNECT_MS = 400;
const MAX_RECONNECT_MS = 8_000;
/** Debounce window for FitAddon on window/container resize. */
const FIT_DEBOUNCE_MS = 100;

/** Close codes treated as permanent errors: do not auto-reconnect. */
const PERMANENT_CLOSE_CODES = new Set([1008, 1009, 1011]);

/**
 * Dark theme tuned to the fog dashboard palette (GitHub-dark-ish).
 */
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

type ConnState = "connecting" | "connected" | "disconnected";

export function TerminalView({ className, service }: { className?: string; service?: string }) {
  const containerRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitAddonRef = useRef<FitAddon | null>(null);
  const wsRef = useRef<WebSocket | null>(null);
  const resizeObserverRef = useRef<ResizeObserver | null>(null);
  const reconnectAttemptRef = useRef(0);
  const reconnectTimerRef = useRef<number | null>(null);
  const fitTimerRef = useRef<number | null>(null);

  const [connState, setConnState] = useState<ConnState>("connecting");
  // Bumped to force a fresh terminal + socket mount (auto or manual reconnect).
  const [attempt, setAttempt] = useState(0);

  const clearTimers = useCallback(() => {
    if (reconnectTimerRef.current !== null) {
      window.clearTimeout(reconnectTimerRef.current);
      reconnectTimerRef.current = null;
    }
    if (fitTimerRef.current !== null) {
      window.clearTimeout(fitTimerRef.current);
      fitTimerRef.current = null;
    }
  }, []);

  // Debounced fit so rapid window/container resizes do not spam the layout
  // engine and the PTY resize path.
  const fit = useCallback(() => {
    if (fitTimerRef.current !== null) {
      window.clearTimeout(fitTimerRef.current);
    }
    fitTimerRef.current = window.setTimeout(() => {
      fitTimerRef.current = null;
      try {
        fitAddonRef.current?.fit();
      } catch {
        /* container not measurable yet; fall back to PTY default size */
      }
    }, FIT_DEBOUNCE_MS);
  }, []);

  // Single owner of teardown: closes the socket, disposes the terminal,
  // drops listeners/timers, and clears the DOM node so a fresh instance can
  // mount. Does NOT reset `connState` — the parent reconnect loop drives state.
  const teardown = useCallback(() => {
    window.removeEventListener("resize", fit);
    wsRef.current?.close();
    wsRef.current = null;
    resizeObserverRef.current?.disconnect();
    resizeObserverRef.current = null;
    if (termRef.current) {
      termRef.current.dispose();
      termRef.current = null;
    }
    fitAddonRef.current = null;
    clearTimers();
    if (containerRef.current) {
      containerRef.current.innerHTML = "";
    }
  }, [fit, clearTimers]);

  // Mount a fresh terminal + socket for each attempt; schedule auto-retry on
  // non-permanent close. Returns the unmount cleanup for this attempt.
  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;

    const term = new Terminal({
      cursorBlink: true,
      scrollback: 5_000,
      fontFamily:
        '"JetBrains Mono", ui-monospace, SFMono-Regular, Menlo, monospace',
      fontSize: 13,
      lineHeight: 1.25,
      theme: TERMINAL_THEME,
    });
    const fitAddon = new FitAddon();
    term.loadAddon(fitAddon);
    term.loadAddon(new WebLinksAddon());
    term.open(el);
    term.focus();

    termRef.current = term;
    fitAddonRef.current = fitAddon;

    let unmounted = false;
    const svcQuery = service ? `?service=${encodeURIComponent(service)}` : "";
    const ws = new WebSocket(
      `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}${WS_PATH}${svcQuery}`
    );
    ws.binaryType = "arraybuffer";
    wsRef.current = ws;

    ws.onopen = () => {
      console.info(`[terminal] ws open: ${location.host}${WS_PATH}${svcQuery}${service ? ` (service=${service})` : ""}`);
      setConnState("connected");
      // Reconnect budget resets on successful open
      reconnectAttemptRef.current = 0;
      fit();
      const { cols, rows } = term;
      ws.send(JSON.stringify({ type: "resize", cols, rows }));
    };

    ws.onmessage = (ev) => {
      if (ev.data instanceof ArrayBuffer) {
        term.write(new Uint8Array(ev.data));
      } else if (typeof ev.data === "string") {
        term.write(ev.data);
      }
    };

    ws.onclose = (ev) => {
      console.warn(
        `[terminal] ws closed code=${ev.code} reason="${ev.reason}" wasClean=${ev.wasClean}`
      );
      if (unmounted) return;
      const permanent = PERMANENT_CLOSE_CODES.has(ev.code);
      setConnState("disconnected");

      if (permanent) {
        term.write(
          "\r\n\x1b[31m[connection closed permanently (code " +
            ev.code +
            ")]\x1b[0m\r\n"
        );
        return;
      }

      term.write("\r\n\x1b[31m[connection closed — retrying]\x1b[0m\r\n");

      // Exponential backoff: 400ms → 800ms → 1.6s → 3.2s → 6.4s → capped 8s.
      if (reconnectAttemptRef.current >= MAX_RECONNECT_ATTEMPTS) {
        return; // exhausted budget; manual reconnect clears it
      }
      const attempt = reconnectAttemptRef.current + 1;
      const delay = Math.min(
        BASE_RECONNECT_MS * 2 ** (attempt - 1),
        MAX_RECONNECT_MS
      );
      reconnectAttemptRef.current = attempt;
      setConnState("connecting");
      reconnectTimerRef.current = window.setTimeout(() => {
        reconnectTimerRef.current = null;
        setAttempt((a) => a + 1);
      }, delay);
    };

    ws.onerror = (ev) => {
      console.error("[terminal] ws error", ev);
      setConnState("disconnected");
    };

    term.onData((data) => {
      if (ws.readyState === WebSocket.OPEN) {
        ws.send(new TextEncoder().encode(data));
      }
    });

    term.onResize(({ cols, rows }) => {
      if (ws.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify({ type: "resize", cols, rows }));
      }
    });

    window.addEventListener("resize", fit);
    const resizeObserver = new ResizeObserver(fit);
    resizeObserver.observe(el);
    resizeObserverRef.current = resizeObserver;

    return () => {
      unmounted = true;
      teardown();
    };
  }, [attempt, fit, teardown, service]);

  // Manual reconnect: reset the backoff budget and force a fresh mount.
  const handleReconnect = useCallback(() => {
    clearTimers();
    reconnectAttemptRef.current = 0;
    setConnState("connecting");
    setAttempt((a) => a + 1);
  }, [clearTimers]);

  return (
    <div className={cn("overflow-hidden rounded-lg border", className)}>
      <div className="border-border bg-card/60 flex h-10 shrink-0 items-center gap-2 border-b px-3">
        <span className="flex items-center gap-2 font-mono text-xs">
          <span
            className={cn(
              "size-2 rounded-full",
              connState === "connected" && "bg-emerald-500",
              connState === "connecting" && "bg-amber-400 animate-pulse",
              connState === "disconnected" && "bg-destructive"
            )}
          />
          <span className="text-muted-foreground">
            {connState === "connected" && "connected"}
            {connState === "connecting" && "connecting…"}
            {connState === "disconnected" && "disconnected"}
          </span>
        </span>
        <div className="ml-auto">
          <Button
            variant="ghost"
            size="sm"
            onClick={handleReconnect}
            disabled={connState === "connecting"}
          >
            Reconnect
          </Button>
        </div>
      </div>
      <div ref={containerRef} className="h-[70vh] w-full bg-[#0d1117] p-2" />
    </div>
  );
}