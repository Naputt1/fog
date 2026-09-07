import { useEffect, useRef, useState } from "react";
import {
  Check,
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  ChevronUp,
  Copy,
  Delete,
  IndentIncrease,
  SquareTerminal,
} from "lucide-react";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

interface Modifiers {
  ctrl: boolean;
  alt: boolean;
}

interface TerminalKeypadProps {
  /** PTY terminal (can send keystrokes) vs read-only docker logs. */
  input: boolean;
  /** Whether an active terminal exists to drive. */
  enabled: boolean;
  onDispatch: (
    init: KeyboardEventInit & { key: string },
    keyCode: number
  ) => void;
  onRaw: (data: string) => void;
  onScroll: (amount: number) => void;
  /** Copy the terminal's scrollback + visible buffer. May return a promise. */
  onCopy: () => void | Promise<void>;
  className?: string;
}

const LETTERS = "ABCDEFGHIJKLMNOPQRSTUVWXYZ".split("");
const DIGITS = "0123456789".split("");

/** DOM `keyCode` values xterm's keymap switches on (synthetic events are 0). */
const KEY_CODES: Record<string, number> = {
  Escape: 27,
  Backspace: 8,
  Tab: 9,
  ArrowUp: 38,
  ArrowDown: 40,
  ArrowLeft: 37,
  ArrowRight: 39,
};

const MODIFIER_LABELS: { key: keyof Modifiers; label: string }[] = [
  { key: "ctrl", label: "Ctrl" },
  { key: "alt", label: "Alt" },
];

/**
 * On-screen keypad for the mobile terminal: sends keys a phone keyboard
 * cannot produce (Tab, Esc, Ctrl-combos…) by routing through xterm's own
 * keymap via `onDispatch`. Modifiers latch like sticky keys — tap Ctrl, then
 * tap a letter, and Ctrl+letter is delivered (auto-releases after one key).
 */
export function TerminalKeypad({
  input,
  enabled,
  onDispatch,
  onRaw,
  onScroll,
  onCopy,
  className,
}: TerminalKeypadProps) {
  const [mods, setMods] = useState<Modifiers>({ ctrl: false, alt: false });
  const [copied, setCopied] = useState(false);
  const copyTimerRef = useRef<number | null>(null);
  const anyLatched = mods.ctrl || mods.alt;

  useEffect(() => {
    return () => {
      if (copyTimerRef.current !== null)
        window.clearTimeout(copyTimerRef.current);
    };
  }, []);

  const handleCopy = () => {
    const result = onCopy();
    if (result && typeof (result as Promise<void>).then === "function") {
      void (result as Promise<void>).then(() => flashCopied()).catch(() => {});
    } else {
      flashCopied();
    }
  };

  const flashCopied = () => {
    setCopied(true);
    if (copyTimerRef.current !== null)
      window.clearTimeout(copyTimerRef.current);
    copyTimerRef.current = window.setTimeout(() => {
      copyTimerRef.current = null;
      setCopied(false);
    }, 1200);
  };

  const handleRaw = (data: string) => {
    setMods({ ctrl: false, alt: false });
    onRaw(data);
  };

  const handleDispatch = (
    init: KeyboardEventInit & { key: string },
    keyCode: number
  ) => {
    setMods({ ctrl: false, alt: false });
    onDispatch(init, keyCode);
  };

  const handleSpecial = (
    init: KeyboardEventInit & { key: string },
    keyCode: number,
    latchedKey?: keyof Modifiers
  ) => {
    if (latchedKey) {
      // Toggle a modifier latch instead of sending.
      setMods((prev) => ({ ...prev, [latchedKey]: !prev[latchedKey] }));
      return;
    }
    handleDispatch({ ...init, ctrlKey: mods.ctrl, altKey: mods.alt }, keyCode);
  };

  const handleLetter = (ch: string) => {
    const keyCode = ch.toUpperCase().charCodeAt(0);
    if (mods.ctrl) {
      handleDispatch({ key: ch.toLowerCase(), ctrlKey: true }, keyCode);
    } else if (mods.alt) {
      // Alt+letter → ESC + letter (word-jump style sequences in shells).
      handleRaw(`\x1b${ch.toLowerCase()}`);
    } else {
      handleRaw(ch.toLowerCase());
    }
  };

  const handleArrow = (dir: "up" | "down" | "left" | "right") => {
    const keyMap = {
      up: ["ArrowUp", KEY_CODES.ArrowUp] as const,
      down: ["ArrowDown", KEY_CODES.ArrowDown] as const,
      left: ["ArrowLeft", KEY_CODES.ArrowLeft] as const,
      right: ["ArrowRight", KEY_CODES.ArrowRight] as const,
    } as const;
    const [key, keyCode] = keyMap[dir];
    if (anyLatched) {
      handleDispatch({ key, ctrlKey: mods.ctrl, altKey: mods.alt }, keyCode);
    } else if (input) {
      handleDispatch({ key }, keyCode);
    } else {
      // Read-only: arrows scroll the log buffer.
      if (dir === "up") onScroll(-3);
      else if (dir === "down") onScroll(3);
      else if (dir === "left") onScroll(-15);
      else onScroll(15);
    }
  };

  return (
    <div
      className={cn(
        "border-border bg-card/60 flex flex-wrap items-stretch gap-1.5 rounded-lg border p-2",
        className
      )}
    >
      {anyLatched && (
        <div className="text-primary flex w-full items-center gap-1.5 pb-0.5 font-mono text-[11px]">
          <SquareTerminal className="size-3" />
          {[mods.ctrl && "Ctrl", mods.alt && "Alt"]
            .filter(Boolean)
            .join("+")}{" "}
          held — tap a key
        </div>
      )}

      {anyLatched && input && (
        <div className="flex w-full flex-wrap gap-1">
          {LETTERS.map((ch) => (
            <Button
              key={ch}
              type="button"
              variant="ghost"
              size="icon-xs"
              disabled={!enabled}
              onPointerDown={(e) => {
                e.preventDefault();
                handleLetter(ch);
              }}
              className="min-w-7 px-1 font-mono"
            >
              {ch}
            </Button>
          ))}
          {DIGITS.map((ch) => (
            <Button
              key={ch}
              type="button"
              variant="ghost"
              size="icon-xs"
              disabled={!enabled}
              onPointerDown={(e) => {
                e.preventDefault();
                handleLetter(ch);
              }}
              className="min-w-7 px-1 font-mono"
            >
              {ch}
            </Button>
          ))}
        </div>
      )}

      {input && (
        <div className="flex flex-wrap gap-1.5">
          <KeypadButton
            enabled={enabled}
            onActivate={() =>
              handleSpecial({ key: "Escape" }, KEY_CODES.Escape)
            }
          >
            Esc
          </KeypadButton>
          <KeypadButton
            enabled={enabled}
            onActivate={() => handleSpecial({ key: "Tab" }, KEY_CODES.Tab)}
            title="Tab"
          >
            <IndentIncrease className="size-4" />
          </KeypadButton>
          <KeypadButton
            enabled={enabled}
            onActivate={() =>
              handleSpecial({ key: "Backspace" }, KEY_CODES.Backspace)
            }
            title="Backspace"
          >
            <Delete className="size-4" />
          </KeypadButton>

          {MODIFIER_LABELS.map(({ key, label }) => (
            <KeypadButton
              key={key}
              enabled={enabled}
              active={mods[key]}
              onActivate={() => handleSpecial({ key: "_" }, 0, key)}
            >
              {label}
            </KeypadButton>
          ))}
        </div>
      )}

      <div className="flex flex-wrap gap-1.5">
        <KeypadButton
          enabled={enabled}
          onActivate={handleCopy}
          title="Copy terminal output"
          className={copied ? "bg-primary/20 text-primary" : undefined}
        >
          {copied ? <Check className="size-4" /> : <Copy className="size-4" />}
        </KeypadButton>
        <KeypadButton
          enabled={enabled}
          onActivate={() => handleArrow("up")}
          title="Up / scroll up"
        >
          <ChevronUp className="size-4" />
        </KeypadButton>
        <KeypadButton
          enabled={enabled}
          onActivate={() => handleArrow("down")}
          title="Down / scroll down"
        >
          <ChevronDown className="size-4" />
        </KeypadButton>
        <KeypadButton
          enabled={enabled}
          onActivate={() => handleArrow("left")}
          title="Left / page up"
        >
          <ChevronLeft className="size-4" />
        </KeypadButton>
        <KeypadButton
          enabled={enabled}
          onActivate={() => handleArrow("right")}
          title="Right / page down"
        >
          <ChevronRight className="size-4" />
        </KeypadButton>
      </div>
    </div>
  );
}

function KeypadButton({
  enabled,
  active = false,
  onActivate,
  className,
  children,
  title,
}: {
  enabled: boolean;
  active?: boolean;
  onActivate: () => void;
  className?: string;
  children: React.ReactNode;
  title?: string;
}) {
  return (
    <Button
      type="button"
      variant={active ? "default" : "ghost"}
      size="icon-sm"
      disabled={!enabled}
      title={title}
      onPointerDown={(e) => {
        e.preventDefault();
        onActivate();
      }}
      className={cn("min-w-11 flex-1 gap-1 font-mono", className)}
    >
      {children}
    </Button>
  );
}
