import {
  useCallback,
  useImperativeHandle,
  useLayoutEffect,
  useRef,
  useState,
  type Ref,
} from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { ChevronDown, ChevronUp } from "lucide-react";
import "@xterm/xterm/css/xterm.css";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import type { TerminalHandle } from "@/components/terminal/terminal-handle";

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

export function TerminalView({
  className,
  service,
  live = false,
  ref,
}: {
  className?: string;
  service?: string;
  live?: boolean;
  ref?: Ref<TerminalHandle>;
}) {
  const containerRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitAddonRef = useRef<FitAddon | null>(null);
  const wsRef = useRef<WebSocket | null>(null);
  const resizeObserverRef = useRef<ResizeObserver | null>(null);
  const reconnectAttemptRef = useRef(0);
  const reconnectTimerRef = useRef<number | null>(null);
  const fitTimerRef = useRef<number | null>(null);
  // Touch-scroll bookkeeping: last touch Y, accumulated fractional scroll,
  // and whether the gesture started on xterm's scrollbar (see handleTouchStart).
  const touchLastYRef = useRef<number | null>(null);
  const touchAccumRef = useRef(0);
  const touchActiveRef = useRef(false);
  const touchOnScrollbarRef = useRef(false);

  const [connState, setConnState] = useState<ConnState>("connecting");
  const [fitted, setFitted] = useState(false);
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

  const fitImmediate = useCallback(() => {
    try {
      fitAddonRef.current?.fit();
      setFitted(true);
    } catch {
      /* container not measurable yet; fall back to PTY default size */
    }
  }, []);

  // Debounced fit so rapid window/container resizes do not spam the layout
  // engine and the PTY resize path. First paint uses fitImmediate for instant layout.
  const fit = useCallback(() => {
    if (fitTimerRef.current !== null) {
      window.clearTimeout(fitTimerRef.current);
    }
    fitTimerRef.current = window.setTimeout(() => {
      fitTimerRef.current = null;
      fitImmediate();
    }, FIT_DEBOUNCE_MS);
  }, [fitImmediate]);

  // Finger-swiping on the terminal scrolls its internal scrollback. xterm has
  // no touch support, so we translate vertical drag distance into scroll lines
  // ourselves and grab the gesture with preventDefault so the page does not
  // scroll instead. Fractional pixel deltas accumulate across events so a
  // continuous drag scrolls as far as it should (no per-event rounding stall).
  //
  // When the touch starts on xterm's own scrollbar overlay (`.scrollbar`), we
  // stand aside and let xterm's mouse-emulation drag scroll the buffer — that
  // is the case where a thumb drag previously got stuck at a few lines per
  // grab because we were swallowing the gesture.
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
    if (
      !touchActiveRef.current ||
      touchOnScrollbarRef.current ||
      e.touches.length !== 1
    ) {
      return;
    }
    if (!term) return;
    const container = containerRef.current;
    if (!container) return;
    const last = touchLastYRef.current;
    if (last == null) {
      touchLastYRef.current = e.touches[0].clientY;
      return;
    }
    const deltaY = e.touches[0].clientY - last;
    touchLastYRef.current = e.touches[0].clientY;
    e.preventDefault();
    // Accumulate fractional rows so slow/small movement still scrolls.
    const rowHeight = Math.max(
      1,
      container.clientHeight / Math.max(1, term.rows)
    );
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

  // Single owner of teardown: closes the socket, disposes the terminal,
  // drops listeners/timers, and clears the DOM node so a fresh instance can
  // mount. Does NOT reset `connState` — the parent reconnect loop drives state.
  const teardown = useCallback(() => {
    window.removeEventListener("resize", fit);
    wsRef.current?.close();
    wsRef.current = null;
    resizeObserverRef.current?.disconnect();
    resizeObserverRef.current = null;
    if (containerRef.current) {
      containerRef.current.removeEventListener("touchstart", handleTouchStart, {
        passive: true,
      } as EventListenerOptions);
      containerRef.current.removeEventListener("touchmove", handleTouchMove, {
        passive: false,
      } as EventListenerOptions);
      containerRef.current.removeEventListener("touchend", handleTouchEnd);
    }
    if (termRef.current) {
      termRef.current.dispose();
      termRef.current = null;
    }
    fitAddonRef.current = null;
    setFitted(false);
    clearTimers();
    if (containerRef.current) {
      containerRef.current.innerHTML = "";
    }
  }, [fit, clearTimers, handleTouchStart, handleTouchMove, handleTouchEnd]);

  // Mount a fresh terminal + socket for each attempt; schedule auto-retry on
  // non-permanent close. Returns the unmount cleanup for this attempt.
  // useLayoutEffect ensures first paint already fits, avoiding slow width shrink on mobile.
  useLayoutEffect(() => {
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
    // Hide until first fit to avoid flashing default 80-col width then shrinking.
    el.style.visibility = "hidden";
    term.open(el);
    // Instantly fit to container size before browser paints default 80-col width.
    try {
      fitAddon.fit();
      el.style.visibility = "";
      setFitted(true);
    } catch {
      el.style.visibility = "";
    }
    term.focus();

    termRef.current = term;
    fitAddonRef.current = fitAddon;

    let unmounted = false;
    const params = new URLSearchParams();
    if (service) params.set("service", service);
    if (live && service) params.set("live", "1");
    const q = params.toString() ? `?${params.toString()}` : "";
    const ws = new WebSocket(
      `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}${WS_PATH}${q}`
    );
    ws.binaryType = "arraybuffer";
    wsRef.current = ws;

    ws.onopen = () => {
      console.info(
        `[terminal] ws open: ${location.host}${WS_PATH}${q}${service ? ` (service=${service}${live ? ", live" : ""})` : ""}`
      );
      setConnState("connected");
      // Reconnect budget resets on successful open
      reconnectAttemptRef.current = 0;
      fitImmediate();
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
    el.addEventListener("touchstart", handleTouchStart, { passive: true });
    el.addEventListener("touchmove", handleTouchMove, { passive: false });
    el.addEventListener("touchend", handleTouchEnd);

    return () => {
      unmounted = true;
      teardown();
    };
  }, [
    attempt,
    fit,
    teardown,
    service,
    handleTouchStart,
    handleTouchMove,
    handleTouchEnd,
  ]);

  // Manual reconnect: reset the backoff budget and force a fresh mount.
  const handleReconnect = useCallback(() => {
    clearTimers();
    reconnectAttemptRef.current = 0;
    setConnState("connecting");
    setAttempt((a) => a + 1);
  }, [clearTimers]);

  // Route synthetic key events through xterm's own keymap by dispatching real
  // KeyboardEvents on its hidden textarea. This is how the mobile keypad can
  // compose Ctrl+letter / Shift+Tab etc. exactly like a desktop keyboard.
  // xterm's keymap switches on `keyCode`, but synthetic events always report
  // keyCode 0 — so we shadow it onto the constructed event.
  const buildKeyEvent = useCallback(
    (
      type: "keydown" | "keyup",
      init: KeyboardEventInit & { key: string },
      keyCode: number
    ) => {
      const base: KeyboardEventInit = {
        bubbles: true,
        cancelable: true,
        ctrlKey: false,
        altKey: false,
        shiftKey: false,
        metaKey: false,
        repeat: false,
        ...init,
      };
      const ev = new KeyboardEvent(type, base);
      Object.defineProperty(ev, "keyCode", {
        value: keyCode,
        configurable: true,
      });
      return ev;
    },
    []
  );

  const sendKey = useCallback(
    (init: KeyboardEventInit & { key: string }, keyCode: number) => {
      const term = termRef.current;
      const el = term?.textarea;
      if (!el) return;
      el.dispatchEvent(buildKeyEvent("keydown", init, keyCode));
      el.dispatchEvent(buildKeyEvent("keyup", init, keyCode));
      term?.focus();
    },
    [buildKeyEvent]
  );

  const scrollByLines = useCallback((amount: number) => {
    termRef.current?.scrollLines(amount);
  }, []);

  const copyText = useCallback(() => {
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
  }, []);

  useImperativeHandle(
    ref,
    () => ({
      dispatchKey: sendKey,
      sendRaw: (data) => {
        const term = termRef.current;
        if (term) term.input(data, true);
        else if (wsRef.current?.readyState === WebSocket.OPEN) {
          wsRef.current.send(new TextEncoder().encode(data));
        }
      },
      scroll: scrollByLines,
      scrollToTop: () => termRef.current?.scrollToTop(),
      scrollToBottom: () => termRef.current?.scrollToBottom(),
      copyText,
      focus: () => termRef.current?.focus(),
    }),
    [ref, sendKey, scrollByLines, copyText]
  );

  const handleScrollTop = useCallback(() => termRef.current?.scrollToTop(), []);
  const handleScrollBottom = useCallback(
    () => termRef.current?.scrollToBottom(),
    []
  );

  return (
    <div
      className={cn(
        "flex flex-col overflow-hidden rounded-lg border transition-none",
        className
      )}
    >
      <div className="border-border bg-card/60 flex h-10 shrink-0 items-center gap-2 border-b px-3 transition-none">
        <span className="flex items-center gap-2 font-mono text-xs">
          <span
            className={cn(
              "size-2 rounded-full",
              connState === "connected" && "bg-emerald-500",
              connState === "connecting" && "animate-pulse bg-amber-400",
              connState === "disconnected" && "bg-destructive"
            )}
          />
          <span className="text-muted-foreground">
            {connState === "connected" && "connected"}
            {connState === "connecting" && "connecting…"}
            {connState === "disconnected" && "disconnected"}
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
      <div className="flex min-h-0 flex-1 bg-[#0d1117] p-2 transition-none">
        <div
          ref={containerRef}
          className={cn(
            "h-full min-h-[280px] w-full transition-none",
            !fitted && "opacity-0"
          )}
        />
      </div>
    </div>
  );
}
