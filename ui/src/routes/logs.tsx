import {
  Fragment,
  memo,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
} from "react";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import {
  ArrowDownToLine,
  Check,
  ChevronsUpDown,
  Copy,
  Eraser,
} from "lucide-react";

import { subscribeLogs, type Service } from "@/lib/api";
import { useServices } from "@/lib/hooks";
import { PageHeader, ErrorState } from "@/components/page-state";
import { Button } from "@/components/ui/button";
import { ScrollArea } from "@/components/ui/scroll-area";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuLabel,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { cn } from "@/lib/utils";

export const Route = createFileRoute("/logs")({
  validateSearch: (search: Record<string, unknown>) => ({
    service: typeof search.service === "string" ? search.service : undefined,
  }),
  component: LogsPage,
});

/* ------------------------------------------------------------------ */
/* ANSI / SGR rendering                                                */
/*                                                                     */
/* Lightweight port of the old server-rendered page's ANSI renderer:   */
/* SGR codes (colors, bold, dim, italic, underline, inverse) map to    */
/* inline CSS; everything else (cursor movement, OSC, ...) is dropped. */
/* xterm-256 colors are resolved to hex. React escapes all text, so    */
/* there is no HTML-injection surface.                                 */
/* ------------------------------------------------------------------ */

const MAX_LOG_LINES = 2_000;

const FG_COLORS: Record<number, string> = {
  30: "#c9d1d9",
  31: "#ff7b72",
  32: "#3fb950",
  33: "#d29922",
  34: "#58a6ff",
  35: "#bc8cff",
  36: "#39c5cf",
  37: "#c9d1d9",
  90: "#f0f6fc",
  91: "#ffa198",
  92: "#56d364",
  93: "#e3b341",
  94: "#79c0ff",
  95: "#d2a8ff",
  96: "#56d4dd",
  97: "#f0f6fc",
};

const BG_COLORS: Record<number, string> = {
  40: "#161b22",
  41: "#ff7b72",
  42: "#3fb950",
  43: "#d29922",
  44: "#58a6ff",
  45: "#bc8cff",
  46: "#39c5cf",
  47: "#c9d1d9",
  100: "#161b22",
  101: "#ff7b72",
  102: "#3fb950",
  103: "#d29922",
  104: "#58a6ff",
  105: "#bc8cff",
  106: "#39c5cf",
  107: "#c9d1d9",
};

/** xterm-256 palette: 16 basic, 6x6x6 color cube, 24-step gray ramp. */
function xtermColor(n: number): string {
  const basic = [
    "#000000",
    "#cc0000",
    "#4e9a06",
    "#c4a000",
    "#3465a4",
    "#75507b",
    "#06989a",
    "#d3d7cf",
    "#555753",
    "#ef2929",
    "#8ae234",
    "#fce94f",
    "#729fcf",
    "#ad7fa8",
    "#34e2e2",
    "#eeeeec",
  ];
  if (n < 16) return basic[n] ?? "#eeeeec";
  if (n < 232) {
    const ramp = [0, 95, 135, 175, 215, 255];
    const nn = n - 16;
    const r = ramp[Math.floor(nn / 36)] ?? 0;
    const g = ramp[Math.floor((nn % 36) / 6)] ?? 0;
    const b = ramp[nn % 6] ?? 0;
    return `#${hex(r)}${hex(g)}${hex(b)}`;
  }
  const v = 8 + (n - 232) * 10;
  return `#${hex(v)}${hex(v)}${hex(v)}`;
}

function hex(n: number): string {
  return n.toString(16).padStart(2, "0");
}

interface AnsiStyle {
  bold: boolean;
  dim: boolean;
  italic: boolean;
  underline: boolean;
  inverse: boolean;
  fg: string;
  bg: string;
}

const EMPTY_STYLE: AnsiStyle = {
  bold: false,
  dim: false,
  italic: false,
  underline: false,
  inverse: false,
  fg: "",
  bg: "",
};

/** Apply one SGR parameter sequence (the part between ESC [ and m). */
function applySGR(seq: string, cur: AnsiStyle): AnsiStyle {
  let params = seq.split(";");
  if (params.length === 1 && params[0] === "") params = ["0"];
  const next: AnsiStyle = { ...cur };
  let i = 0;
  while (i < params.length) {
    const p = params[i] ?? "0";
    if (p === "0") return { ...EMPTY_STYLE };
    if (p === "1") next.bold = true;
    else if (p === "2") next.dim = true;
    else if (p === "3") next.italic = true;
    else if (p === "4") next.underline = true;
    else if (p === "7") next.inverse = true;
    else if (p === "22") {
      next.bold = false;
      next.dim = false;
    } else if (p === "23") next.italic = false;
    else if (p === "24") next.underline = false;
    else if (p === "27") next.inverse = false;
    else if (p === "39") next.fg = "";
    else if (p === "49") next.bg = "";
    else if (p >= "30" && p <= "37") next.fg = FG_COLORS[Number(p)] ?? "";
    else if (p >= "90" && p <= "97") next.fg = FG_COLORS[Number(p)] ?? "";
    else if (p >= "40" && p <= "47") next.bg = BG_COLORS[Number(p)] ?? "";
    else if (p >= "100" && p <= "107") next.bg = BG_COLORS[Number(p)] ?? "";
    else if (p === "38" || p === "48") {
      const mode = params[i + 1];
      if (mode === "5") {
        const c = xtermColor(parseInt(params[i + 2] ?? "0", 10));
        if (p === "38") next.fg = c;
        else next.bg = c;
        i += 2;
      } else if (mode === "2") {
        const r = params[i + 2] ?? "0";
        const g = params[i + 3] ?? "0";
        const b = params[i + 4] ?? "0";
        const rgb = `rgb(${r},${g},${b})`;
        if (p === "38") next.fg = rgb;
        else next.bg = rgb;
        i += 4;
      }
    }
    i += 1;
  }
  return next;
}

function styleToCss(s: AnsiStyle): CSSProperties {
  const css: CSSProperties = {};
  if (s.fg) css.color = s.fg;
  if (s.bg) css.backgroundColor = s.bg;
  if (s.bold) css.fontWeight = 700;
  if (s.dim) css.opacity = 0.7;
  if (s.italic) css.fontStyle = "italic";
  if (s.underline) css.textDecoration = "underline";
  if (s.inverse) css.filter = "invert(1)";
  return css;
}

interface ParsedSegment {
  text: string;
  style: CSSProperties | null;
}

interface ParsedLine {
  segments: ParsedSegment[];
  plain: string;
}

/**
 * Parse one raw log line into styled segments. Carriage returns overwrite
 * the line (progress bars, spinners) — only the final `\r`-separated segment
 * is kept, mirroring what a terminal shows. Returns the plain (ANSI-free)
 * text alongside for clipboard copying.
 */
function parseAnsiLine(line: string): ParsedLine {
  const parts = line.split("\r");
  const keep = parts[parts.length - 1] ?? "";
  const segments: ParsedSegment[] = [];
  let plain = "";
  let cur: AnsiStyle = { ...EMPTY_STYLE };
  let buf = "";

  const flush = () => {
    if (buf.length === 0) return;
    const styled =
      cur.bold ||
      cur.dim ||
      cur.italic ||
      cur.underline ||
      cur.inverse ||
      cur.fg !== "" ||
      cur.bg !== "";
    segments.push({
      text: buf,
      style: styled ? styleToCss(cur) : null,
    });
    plain += buf;
    buf = "";
  };

  let i = 0;
  const n = keep.length;
  while (i < n) {
    const c = keep[i];
    if (c === "\x1b") {
      if (keep[i + 1] === "[") {
        // CSI: consume until the final byte (0x40–0x7e).
        let j = i + 2;
        while (
          j < n &&
          !(keep.charCodeAt(j) >= 0x40 && keep.charCodeAt(j) <= 0x7e)
        ) {
          j += 1;
        }
        const finalByte = j < n ? keep[j] : "";
        const seq = keep.slice(i + 2, j);
        i = j < n ? j + 1 : n;
        if (finalByte === "m") {
          flush();
          cur = applySGR(seq, cur);
        }
        continue;
      }
      if (keep[i + 1] === "]") {
        // OSC: skip until BEL (0x07) or ST (ESC \).
        let j = i + 2;
        while (j < n) {
          if (keep[j] === "\u0007") {
            j += 1;
            break;
          }
          if (keep[j] === "\x1b" && keep[j + 1] === "\\") {
            j += 2;
            break;
          }
          j += 1;
        }
        i = Math.min(j, n);
        continue;
      }
      // Two-character escape (e.g. ESC M): skip the second char.
      i = Math.min(i + 2, n);
      continue;
    }
    buf += c;
    i += 1;
  }
  flush();
  return { segments, plain };
}

/* ------------------------------------------------------------------ */
/* Service grouping (project → worktree → services)                    */
/* ------------------------------------------------------------------ */

interface WorktreeBucket {
  worktree: string;
  services: Service[];
}

interface ProjectBucket {
  project: string;
  worktrees: WorktreeBucket[];
}

/** Group services by project, then worktree ("" = default checkout first). */
function groupServices(services: Service[]): ProjectBucket[] {
  const byProject = new Map<string, Map<string, Service[]>>();
  for (const svc of services) {
    let byWorktree = byProject.get(svc.project);
    if (!byWorktree) {
      byWorktree = new Map();
      byProject.set(svc.project, byWorktree);
    }
    const bucket = byWorktree.get(svc.worktree);
    if (bucket) bucket.push(svc);
    else byWorktree.set(svc.worktree, [svc]);
  }

  const projects: ProjectBucket[] = [];
  for (const [project, byWorktree] of byProject) {
    const worktrees: WorktreeBucket[] = [];
    for (const [worktree, list] of byWorktree) {
      list.sort((a, b) => a.service.localeCompare(b.service));
      worktrees.push({ worktree, services: list });
    }
    worktrees.sort((a, b) => {
      if (a.worktree === "") return -1;
      if (b.worktree === "") return 1;
      return a.worktree.localeCompare(b.worktree);
    });
    projects.push({ project, worktrees });
  }
  projects.sort((a, b) => a.project.localeCompare(b.project));
  return projects;
}

/* ------------------------------------------------------------------ */
/* Clipboard helper (mirrors the services page's copy behavior)        */
/* ------------------------------------------------------------------ */

async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    try {
      const ta = document.createElement("textarea");
      ta.value = text;
      ta.style.position = "fixed";
      ta.style.opacity = "0";
      document.body.appendChild(ta);
      ta.select();
      const ok = document.execCommand("copy");
      document.body.removeChild(ta);
      return ok;
    } catch {
      return false;
    }
  }
}

/* ------------------------------------------------------------------ */
/* Page                                                                */
/* ------------------------------------------------------------------ */

interface LogEntry {
  id: number;
  plain: string;
  segments: ParsedSegment[];
  meta: boolean;
}

type ConnState = "idle" | "connecting" | "streaming" | "reconnecting" | "ended";

const CONN_META: Record<ConnState, { dot: string; label: string }> = {
  idle: { dot: "bg-muted-foreground", label: "idle" },
  connecting: { dot: "bg-warning animate-pulse", label: "connecting…" },
  streaming: { dot: "bg-primary", label: "streaming" },
  reconnecting: { dot: "bg-warning animate-pulse", label: "reconnecting…" },
  ended: { dot: "bg-muted-foreground", label: "stream ended" },
};

const LogLineView = memo(function LogLineView({ entry }: { entry: LogEntry }) {
  return (
    <div className={cn(entry.meta && "text-muted-foreground italic")}>
      {entry.segments.length === 0 ? (
        <span>&nbsp;</span>
      ) : (
        entry.segments.map((seg, i) => (
          <Fragment key={i}>
            {seg.style ? <span style={seg.style}>{seg.text}</span> : seg.text}
          </Fragment>
        ))
      )}
    </div>
  );
});

function LogsPage() {
  const navigate = useNavigate();
  const search = Route.useSearch();
  const { data: services, isLoading, isError } = useServices();
  const serviceNames = useMemo(
    () => (services ?? []).map((s) => s.service),
    [services]
  );

  // URL ?service= wins; otherwise fall back to the first running service.
  const active =
    search.service ?? (serviceNames.length > 0 ? serviceNames[0] : null);

  const [conn, setConn] = useState<ConnState>("idle");
  const [connNote, setConnNote] = useState<string | null>(null);
  const [entries, setEntries] = useState<LogEntry[]>([]);
  const [follow, setFollow] = useState(true);
  const [copied, setCopied] = useState(false);

  const idRef = useRef(0);
  const surfaceRef = useRef<HTMLDivElement | null>(null);
  const viewportRef = useRef<HTMLElement | null>(null);

  /* --- live stream: opens on service change, closes on switch/unmount --- */
  useEffect(() => {
    if (!active) {
      setEntries([]);
      setConn("idle");
      setConnNote(null);
      return;
    }

    setEntries([]);
    setConn("connecting");
    setConnNote(null);
    setCopied(false);

    let done = false;
    let cleanup: (() => void) | null = null;

    const append = (text: string, meta: boolean) => {
      setEntries((prev) => {
        const parsed = parseAnsiLine(text);
        const entry: LogEntry = {
          id: ++idRef.current,
          plain: parsed.plain,
          segments: parsed.segments,
          meta,
        };
        return prev.length >= MAX_LOG_LINES
          ? [...prev.slice(prev.length - MAX_LOG_LINES + 1), entry]
          : [...prev, entry];
      });
    };

    cleanup = subscribeLogs(active, {
      onOpen: () => {
        setConn("streaming");
        setConnNote(null);
      },
      // EventSource fires `error` on transient reconnects too; the browser
      // manages retries and a later `open` restores the streaming state.
      onError: () => {
        if (done) return;
        setConn((c) =>
          c === "streaming" || c === "connecting" ? "reconnecting" : c
        );
      },
      onLine: (line) => {
        const text = line.text;
        // Server control messages (errors, stream termination, info).
        if (text.startsWith("[fog] ")) {
          const msg = text.slice("[fog] ".length);
          done = true;
          setConn("ended");
          setConnNote(msg);
          append(text, true);
          // Close so EventSource stops reconnecting after the stream ends.
          cleanup?.();
          return;
        }
        append(text, false);
      },
    });

    return () => {
      done = true;
      cleanup?.();
    };
  }, [active]);

  /* --- auto-scroll: follow stays glued to the bottom until the user
         scrolls up (which pauses it); scrolling back down resumes. --- */
  useEffect(() => {
    viewportRef.current =
      surfaceRef.current?.querySelector<HTMLElement>(
        '[data-slot="scroll-area-viewport"]'
      ) ?? null;

    const vp = viewportRef.current;
    if (!vp) return;
    const onScroll = () => {
      const nearBottom = vp.scrollHeight - vp.scrollTop - vp.clientHeight < 8;
      setFollow(nearBottom);
    };
    vp.addEventListener("scroll", onScroll, { passive: true });
    return () => vp.removeEventListener("scroll", onScroll);
  }, []);

  useEffect(() => {
    if (follow) {
      const vp = viewportRef.current;
      if (vp) vp.scrollTop = vp.scrollHeight;
    }
  }, [entries.length, follow]);

  const onToggleFollow = useCallback(() => {
    setFollow((f) => !f);
  }, []);

  const onCopy = useCallback(async () => {
    const text = entries.map((e) => e.plain).join("\n");
    if (!text) return;
    if (await copyText(text)) {
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1_500);
    }
  }, [entries]);

  const onClear = useCallback(() => setEntries([]), []);

  const status = CONN_META[conn];
  const statusLabel = conn === "ended" && connNote ? connNote : status.label;

  const groups = useMemo(() => groupServices(services ?? []), [services]);

  return (
    <div className="space-y-6">
      <PageHeader
        title="Logs"
        description="Live streaming output from a running service via SSE (docker logs --follow)."
      />

      {/* Service picker */}
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-muted-foreground font-mono text-xs tracking-wider uppercase">
          Service
        </span>
        {isError ? (
          <ErrorState message="Could not load services to select from." />
        ) : (
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button
                variant="outline"
                size="sm"
                className="min-w-44 justify-between font-mono"
                disabled={isLoading && !active}
              >
                <span className="max-w-56 truncate">
                  {active ?? "select service…"}
                </span>
                <ChevronsUpDown className="size-3.5 opacity-60" />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="start" className="max-h-[40vh] w-72">
              {isLoading ? (
                <DropdownMenuLabel className="font-mono">
                  loading…
                </DropdownMenuLabel>
              ) : groups.length === 0 ? (
                <DropdownMenuLabel className="font-mono">
                  no services running
                </DropdownMenuLabel>
              ) : (
                <DropdownMenuRadioGroup
                  value={active ?? undefined}
                  onValueChange={(name) =>
                    void navigate({ to: "/logs", search: { service: name } })
                  }
                >
                  {groups.map((project) => (
                    <Fragment key={project.project}>
                      <DropdownMenuLabel className="text-primary/80 font-mono text-[11px] tracking-wider uppercase">
                        {project.project}
                      </DropdownMenuLabel>
                      {project.worktrees.map((wt) => (
                        <Fragment key={`${project.project}:${wt.worktree}`}>
                          <DropdownMenuLabel
                            inset
                            className="text-muted-foreground py-1 font-mono text-[10px] tracking-wider uppercase"
                          >
                            {wt.worktree || "default"} · {wt.services.length}
                          </DropdownMenuLabel>
                          {wt.services.map((svc) => (
                            <DropdownMenuRadioItem
                              key={svc.service}
                              value={svc.service}
                              className="font-mono text-xs"
                            >
                              {svc.service}
                            </DropdownMenuRadioItem>
                          ))}
                        </Fragment>
                      ))}
                    </Fragment>
                  ))}
                </DropdownMenuRadioGroup>
              )}
            </DropdownMenuContent>
          </DropdownMenu>
        )}
      </div>

      {/* Terminal surface */}
      <div
        ref={surfaceRef}
        className="terminal-surface overflow-hidden rounded-lg"
      >
        <div className="border-border/60 flex flex-wrap items-center gap-x-3 gap-y-2 border-b px-3 py-2">
          <span className="text-muted-foreground font-mono text-xs">
            {active ? `${active}.log` : "no service selected"}
          </span>
          <span
            className="text-muted-foreground flex items-center gap-1.5 font-mono text-[11px]"
            title={statusLabel}
          >
            <span className={cn("size-1.5 rounded-full", status.dot)} />
            <span className="max-w-64 truncate">{statusLabel}</span>
          </span>
          <span className="text-muted-foreground/70 font-mono text-[11px]">
            {entries.length} lines
          </span>

          <div className="ml-auto flex items-center gap-1.5">
            <Button
              variant={follow ? "default" : "outline"}
              size="sm"
              className="font-mono"
              onClick={onToggleFollow}
              title={
                follow
                  ? "Pause auto-scroll (or scroll up)"
                  : "Resume auto-scroll to latest output"
              }
            >
              <ArrowDownToLine />
              follow
            </Button>
            <Button
              variant="outline"
              size="sm"
              className="font-mono"
              onClick={onCopy}
              disabled={entries.length === 0}
              title="Copy plain log text to clipboard"
            >
              {copied ? <Check /> : <Copy />}
              {copied ? "copied" : "copy"}
            </Button>
            <Button
              variant="outline"
              size="sm"
              className="font-mono"
              onClick={onClear}
              disabled={entries.length === 0}
              title="Clear the log buffer (stream keeps running)"
            >
              <Eraser />
              clear
            </Button>
          </div>
        </div>

        <ScrollArea className="h-[62vh]">
          <pre className="text-foreground/90 p-4 font-mono text-[13px] leading-relaxed break-words whitespace-pre-wrap">
            {entries.length === 0 ? (
              <span className="text-muted-foreground">
                {active
                  ? "Waiting for output…"
                  : "Select a service to stream its logs."}
              </span>
            ) : (
              entries.map((entry) => (
                <LogLineView key={entry.id} entry={entry} />
              ))
            )}
          </pre>
        </ScrollArea>
      </div>
    </div>
  );
}
