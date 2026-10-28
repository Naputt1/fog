import { createFileRoute } from "@tanstack/react-router";
import { Minus, Plus, RotateCcw } from "lucide-react";
import { toast } from "sonner";

import { PageHeader } from "@/components/page-state";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import {
  FONT_SIZE_MAX,
  FONT_SIZE_MIN,
  LOG_LINES_OPTIONS,
  POLL_MS_OPTIONS,
  isDefaultPreferences,
  resetPreferences,
  setPreference,
  usePreferences,
} from "@/lib/preferences";
import { cn } from "@/lib/utils";

export const Route = createFileRoute("/settings")({
  component: SettingsPage,
});

const POLL_CHOICES = POLL_MS_OPTIONS.map((value) => ({
  value,
  label: value === 0 ? "Off" : `${value / 1_000}s`,
}));

const LOG_LINE_CHOICES = LOG_LINES_OPTIONS.map((value) => ({
  value,
  label: value >= 1_000 ? `${value / 1_000}k` : String(value),
}));

const VIEW_CHOICES = [
  { value: "logs", label: "Logs (SSE)" },
  { value: "terminal", label: "Terminal (PTY)" },
] as const;

function SettingGroup({
  title,
  children,
}: {
  title: string;
  children: React.ReactNode;
}) {
  return (
    <Card padding="none" spacing="none">
      <div className="border-border border-b px-4 py-3 sm:px-6">
        <div className="font-mono text-sm leading-none font-semibold">
          {title}
        </div>
      </div>
      <div className="divide-border divide-y">{children}</div>
    </Card>
  );
}

function SettingRow({
  label,
  htmlFor,
  children,
}: {
  label: string;
  htmlFor?: string;
  children: React.ReactNode;
}) {
  return (
    <div className="flex flex-col gap-3 px-4 py-4 sm:flex-row sm:items-center sm:justify-between sm:gap-6 sm:px-6">
      <div className="min-w-0">
        {htmlFor ? (
          <label htmlFor={htmlFor} className="text-sm font-medium">
            {label}
          </label>
        ) : (
          <div className="text-sm font-medium">{label}</div>
        )}
      </div>
      <div className="flex shrink-0 flex-wrap items-center gap-2">
        {children}
      </div>
    </div>
  );
}

function Segmented<T extends string | number>({
  label,
  value,
  options,
  onChange,
}: {
  label: string;
  value: T;
  options: readonly { value: T; label: string }[];
  onChange: (value: T) => void;
}) {
  return (
    <div
      role="radiogroup"
      aria-label={label}
      className="bg-muted/60 inline-flex flex-wrap gap-0.5 rounded-md p-0.5"
    >
      {options.map((option) => {
        const selected = option.value === value;
        return (
          <button
            key={option.value}
            type="button"
            role="radio"
            aria-checked={selected}
            onClick={() => onChange(option.value)}
            className={cn(
              "rounded px-2.5 py-1 font-mono text-xs transition-colors",
              selected
                ? "bg-primary text-primary-foreground"
                : "text-muted-foreground hover:text-foreground"
            )}
          >
            {option.label}
          </button>
        );
      })}
    </div>
  );
}

function FontSizeControl({ value }: { value: number }) {
  return (
    <div
      role="group"
      aria-label="Terminal font size"
      className="border-border bg-background inline-flex items-center rounded-md border"
    >
      <Button
        variant="ghost"
        size="icon-sm"
        aria-label="Decrease font size"
        disabled={value <= FONT_SIZE_MIN}
        onClick={() => setPreference("terminalFontSize", value - 1)}
      >
        <Minus className="size-3.5" aria-hidden />
      </Button>
      <span
        aria-live="polite"
        className="text-foreground w-14 text-center font-mono text-xs tabular-nums"
      >
        {value} px
      </span>
      <Button
        variant="ghost"
        size="icon-sm"
        aria-label="Increase font size"
        disabled={value >= FONT_SIZE_MAX}
        onClick={() => setPreference("terminalFontSize", value + 1)}
      >
        <Plus className="size-3.5" aria-hidden />
      </Button>
    </div>
  );
}

function SettingsPage() {
  const preferences = usePreferences();
  const isDefault = isDefaultPreferences(preferences);

  const reset = () => {
    resetPreferences();
    toast.success("Settings reset to defaults");
  };

  return (
    <div className="min-w-0 space-y-4">
      <PageHeader
        title="Settings"
        actions={
          <Button
            variant="outline"
            size="sm"
            font="mono"
            disabled={isDefault}
            onClick={reset}
          >
            <RotateCcw aria-hidden />
            Reset
          </Button>
        }
      />

      <SettingGroup title="Refresh">
        <SettingRow label="Polling interval">
          <Segmented
            label="Polling interval"
            value={preferences.pollMs}
            options={POLL_CHOICES}
            onChange={(value) => setPreference("pollMs", value)}
          />
        </SettingRow>
      </SettingGroup>

      <SettingGroup title="Terminal & logs">
        <SettingRow label="Default view">
          <Segmented
            label="Default view"
            value={preferences.defaultLogView}
            options={VIEW_CHOICES}
            onChange={(value) => setPreference("defaultLogView", value)}
          />
        </SettingRow>
        <SettingRow label="Initial log lines">
          <Segmented
            label="Initial log lines"
            value={preferences.initialLogLines}
            options={LOG_LINE_CHOICES}
            onChange={(value) => setPreference("initialLogLines", value)}
          />
        </SettingRow>
        <SettingRow label="Font size">
          <FontSizeControl value={preferences.terminalFontSize} />
        </SettingRow>
      </SettingGroup>

      <SettingGroup title="Navigation">
        <SettingRow
          label="Swipe between pages"
          htmlFor="preference-swipe-navigation"
        >
          <Switch
            id="preference-swipe-navigation"
            aria-label="Swipe between pages"
            checked={preferences.swipeNavigation}
            onCheckedChange={(checked) =>
              setPreference("swipeNavigation", checked)
            }
          />
        </SettingRow>
      </SettingGroup>
    </div>
  );
}
