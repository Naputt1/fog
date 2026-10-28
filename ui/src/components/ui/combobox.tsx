import { Combobox } from "@base-ui/react";
import { Check, ChevronDown } from "lucide-react";
import type { ReactNode } from "react";

import { cn } from "@/lib/utils";

/**
 * One pickable entry. `value` is what the caller stores, `label` is what the
 * field shows and filters on, `detail` is a muted second line (paths, ports)
 * and `keywords` widens the search to text that is not displayed.
 */
type ComboboxOption<T> = {
  value: T;
  label: string;
  detail?: string;
  keywords?: string;
};

type ComboboxFieldProps<T> = {
  id: string;
  label: string;
  value: T | null;
  options: readonly ComboboxOption<T>[];
  onChange: (value: T | null) => void;
  placeholder?: string;
  /** Shown under the field, e.g. why nothing can be picked yet. */
  hint?: ReactNode;
  emptyLabel?: string;
  disabled?: boolean;
  autoFocus?: boolean;
};

/** Select-shaped, searchable field: click it and type to narrow the list. */
function ComboboxField<T>({
  id,
  label,
  value,
  options,
  onChange,
  placeholder,
  hint,
  emptyLabel = "No matches",
  disabled = false,
  autoFocus = false,
}: ComboboxFieldProps<T>) {
  const selected = options.find((option) => option.value === value) ?? null;

  return (
    <div className="min-w-0">
      <label
        htmlFor={id}
        className="text-muted-foreground text-2xs mb-1 block font-mono tracking-wide uppercase"
      >
        {label}
      </label>
      <Combobox.Root
        items={options}
        value={selected}
        onValueChange={(next) => onChange(next ? next.value : null)}
        isItemEqualToValue={(item, current) => item.label === current?.label}
        itemToStringValue={(option) => option.label}
        filter={(option, query) => {
          const needle = query.trim().toLowerCase();
          if (!needle) return true;
          return `${option.label} ${option.keywords ?? ""}`
            .toLowerCase()
            .includes(needle);
        }}
        autoHighlight
        disabled={disabled}
      >
        <div className="relative">
          <Combobox.Input
            id={id}
            placeholder={placeholder}
            autoFocus={autoFocus}
            disabled={disabled}
            data-slot="combobox-input"
            className={cn(
              "border-input placeholder:text-muted-foreground dark:bg-input/30 h-9 w-full min-w-0 rounded-md border bg-transparent py-1 pr-8 pl-3 text-sm shadow-xs transition-[color,box-shadow] outline-none",
              "focus-visible:border-ring focus-visible:ring-ring/50 focus-visible:ring-[3px]",
              "disabled:pointer-events-none disabled:cursor-not-allowed disabled:opacity-50"
            )}
          />
          <Combobox.Trigger
            aria-label={`Show ${label.toLowerCase()} options`}
            disabled={disabled}
            data-slot="combobox-trigger"
            className="text-muted-foreground hover:text-foreground absolute top-1/2 right-2 -translate-y-1/2 rounded p-0.5 disabled:pointer-events-none disabled:opacity-50"
          >
            <ChevronDown className="size-4" aria-hidden />
          </Combobox.Trigger>
        </div>
        <Combobox.Portal>
          <Combobox.Positioner
            sideOffset={4}
            className="isolate z-[70] outline-none"
          >
            <Combobox.Popup
              data-slot="combobox-content"
              className="bg-popover text-popover-foreground border-border w-(--anchor-width) max-w-(--available-width) overflow-hidden rounded-md border shadow-lg"
            >
              {/* The empty element always renders its root; only its children
                  appear when nothing matches, so the padding lives inside. */}
              <Combobox.Empty className="text-muted-foreground text-sm">
                <div className="px-3 py-2">{emptyLabel}</div>
              </Combobox.Empty>
              <Combobox.List className="max-h-[min(16rem,var(--available-height))] scroll-py-1 overflow-y-auto overscroll-contain p-1 outline-0 data-empty:p-0">
                {(option: ComboboxOption<T>) => (
                  <Combobox.Item
                    key={option.label}
                    value={option}
                    data-slot="combobox-item"
                    className="data-highlighted:bg-accent data-highlighted:text-accent-foreground relative flex cursor-default items-center gap-2 rounded-sm py-1.5 pr-8 pl-2 text-sm outline-hidden select-none"
                  >
                    <span className="min-w-0 flex-1">
                      <span className="block truncate">{option.label}</span>
                      {option.detail ? (
                        <span className="text-muted-foreground text-3xs block truncate font-mono">
                          {option.detail}
                        </span>
                      ) : null}
                    </span>
                    <Combobox.ItemIndicator className="absolute right-2 flex size-4 items-center justify-center">
                      <Check className="size-4" aria-hidden />
                    </Combobox.ItemIndicator>
                  </Combobox.Item>
                )}
              </Combobox.List>
            </Combobox.Popup>
          </Combobox.Positioner>
        </Combobox.Portal>
      </Combobox.Root>
      {hint ? (
        <p className="text-muted-foreground text-2xs mt-1 font-mono">{hint}</p>
      ) : null}
    </div>
  );
}

export { ComboboxField, type ComboboxOption };
