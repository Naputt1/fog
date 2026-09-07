import {
  useCallback,
  useEffect,
  useImperativeHandle,
  useRef,
  useState,
  type Ref,
} from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { ChevronDown, ChevronUp } from "lucide-react";
import "@xterm/xterm/css/xterm.css";

import { subscribeLogs } from "@/lib/api";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import type { TerminalHandle } from "@/components/terminal/terminal-handle";

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
  ref,
}: {
  className?: string;
  container: string | null;
  pid: number | null;
  service: string | null;
  ref?: Ref<TerminalHandle>;
}) {
  const elRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const [connected, setConnected] = useState(false);
  const [fitted, setFitted] = useState(false);
  // Touch-scroll bookkeeping.
  const touchLastYRef = useRef<number | null>(null);
  const touchAccumRef = useRef(0);
  const touchActiveRef = useRef(false);
  const touchOnScrollbarRef = useRef(false);

  const fit = useCallback(() => {
    try {
      fitRef.current?.fit();
    } catch {
      /* not measurable yet */
    }
  }, []);

  // Read-only view: key dispatch is a no-op; only scroll/focus/copy apply.
  useImperativeHandle(
    ref,
    () => ({
      dispatchKey: () => {},
      sendRaw: () => {},
      scroll: (amount) => termRef.current?.scrollLines(amount),
      scrollToTop: () => termRef.current?.scrollToTop(),
      scrollToBottom: () => termRef.current?.scrollToBottom(),
      copyText: () => {
        const term = termRef.current;
        if (!term) return "";
        const buf = term.buffer.active;
        const lines: string[] = [];
        for (let i = 0; i < buf.length; i++) {
          const line = buf.getLine(i);
          if (!line) continue;
          lines.push(line.translateToString(true));
        }
        return lines.join("\n");
      },
      focus: () => termRef.current?.focus(),
    }),
    []
  );

  const handleScrollTop = useCallback(() => termRef.current?.scrollToTop(), []);
  const handleScrollBottom = useCallback(
    () => termRef.current?.scrollToBottom(),
    []
  );

  // Finger-swipe scrolls the log buffer; see TerminalView for the rationale.
  // Stands aside when the touch targets xterm's own scrollbar so a thumb drag
  // scrolls properly instead of getting stuck on a few lines per gesture.
  const handleTouchStart = useCallback((e: TouchEvent) => {
    const term = termRef.current;
    if (!term || e.touches.length !== 1) return;
    touchActiveRef.current = true;
    touchLastYRef.current = e.touches[0].clientY;
    touchAccumRef.current = 0;
    const target = e.target as Element | null;
    touchOnScrollbarRef.current = !!target?.closest?.(
      ".xterm-scrollable-element .scrollbar"
    );
  }, []);

  const handleTouchMove = useCallback((e: TouchEvent) => {
    const term = termRef.current;
    const el = elRef.current;
    if (
      !touchActiveRef.current ||
      touchOnScrollbarRef.current ||
      e.touches.length !== 1
    )
      return;
    if (!term || !el) return;
    const last = touchLastYRef.current;
    if (last == null) {
      touchLastYRef.current = e.touches[0].clientY;
      return;
    }
    const deltaY = e.touches[0].clientY - last;
    touchLastYRef.current = e.touches[0].clientY;
    e.preventDefault();
    const rowHeight = Math.max(1, el.clientHeight / Math.max(1, term.rows));
    touchAccumRef.current += deltaY / rowHeight;
    const lines = Math.round(touchAccumRef.current);
    if (lines !== 0) {
      touchAccumRef.current -= lines;
      term.scrollLines(lines);
    }
  }, []);

  const handleTouchEnd = useCallback(() => {
    touchActiveRef.current = false;
    touchLastYRef.current = null;
    touchAccumRef.current = 0;
    touchOnScrollbarRef.current = false;
  }, []);

  useEffect(() => {
    const el = elRef.current;
    if (!el) return;
    const term = new Terminal({
      cursorBlink: false,
      disableStdin: true,
      scrollback: 5000,
      fontFamily:
        '"JetBrains Mono", ui-monospace, SFMono-Regular, Menlo, monospace',
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
    el.addEventListener("touchstart", handleTouchStart, { passive: true });
    el.addEventListener("touchmove", handleTouchMove, { passive: false });
    el.addEventListener("touchend", handleTouchEnd);
    window.addEventListener("resize", fit);
    return () => {
      window.removeEventListener("resize", fit);
      ro.disconnect();
      el.removeEventListener("touchstart", handleTouchStart);
      el.removeEventListener("touchmove", handleTouchMove);
      el.removeEventListener("touchend", handleTouchEnd);
      term.dispose();
      termRef.current = null;
      fitRef.current = null;
    };
  }, [fit, handleTouchStart, handleTouchMove, handleTouchEnd]);

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
    const logService =
      pid != null ? (service ?? container ?? "") : (container ?? service ?? "");
    if (!logService) return;

    term.clear();
    term.writeln(
      `\x1b[90m[log] connecting ${logService}${pid != null ? ` (pid ${pid})` : ""} …\x1b[0m`
    );
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
    <div
      className={cn(
        "flex flex-col overflow-hidden rounded-lg border",
        className
      )}
    >
      <div className="border-border bg-card/60 flex h-10 shrink-0 items-center gap-2 border-b px-3">
        <span className="flex items-center gap-2 font-mono text-xs">
          <span
            className={cn(
              "size-2 rounded-full",
              connected ? "bg-emerald-500" : "animate-pulse bg-amber-400"
            )}
          />
          <span className="text-muted-foreground">
            {connected ? "streaming" : "connecting…"}
          </span>
        </span>
        <div className="ml-auto flex items-center gap-1">
          <Button
            variant="ghost"
            size="icon-sm"
            className="lg:hidden"
            onClick={handleScrollTop}
            aria-label="Scroll to top"
          >
            <ChevronUp className="size-4" />
          </Button>
          <Button
            variant="ghost"
            size="icon-sm"
            className="lg:hidden"
            onClick={handleScrollBottom}
            aria-label="Scroll to bottom"
          >
            <ChevronDown className="size-4" />
          </Button>
          <Button variant="ghost" size="sm" onClick={handleClear}>
            Clear
          </Button>
        </div>
      </div>
      <div className="flex min-h-0 flex-1 bg-[#0d1117] p-2">
        <div
          ref={elRef}
          className={cn("h-full min-h-[280px] w-full", !fitted && "opacity-0")}
        />
      </div>
    </div>
  );
}
