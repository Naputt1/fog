import { useEffect, useRef, useState } from "react";
import { ArrowUp, ChevronRight, Folder, X } from "lucide-react";

import { useDirectoryListing } from "@/lib/hooks";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import { cn } from "@/lib/utils";

/**
 * Whether the typed text means a path (`code/red-fox`, `~/code`) rather than a
 * filter for the folder names listed below it.
 */
function isPathLike(value: string): boolean {
  return value.includes("/") || value.startsWith("~");
}

/**
 * Folders matching the query: a path segment completes on the names that start
 * with it, a typed folder name also matches names that merely contain it, the
 * prefix hits first.
 */
function matchEntries<T extends { name: string }>(
  entries: T[],
  query: string,
  mode: "prefix" | "contains"
): T[] {
  const needle = query.trim().toLowerCase();
  if (!needle) return entries;
  const starts: T[] = [];
  const rest: T[] = [];
  for (const entry of entries) {
    const name = entry.name.toLowerCase();
    if (name.startsWith(needle)) starts.push(entry);
    else if (mode === "contains" && name.includes(needle)) rest.push(entry);
  }
  return [...starts, ...rest];
}

/** Row label with the part that matches the filter picked out. */
function Highlighted({ name, query }: { name: string; query: string }) {
  if (!query) return name;
  const at = name.toLowerCase().indexOf(query.toLowerCase());
  if (at < 0) return name;
  return (
    <>
      {name.slice(0, at)}
      <span className="text-primary">{name.slice(at, at + query.length)}</span>
      {name.slice(at + query.length)}
    </>
  );
}

/** One browsable directory row (also used for the `..` parent entry). */
function DirectoryRow({
  name,
  git,
  fog,
  icon,
  query = "",
  onClick,
}: {
  name: string;
  git?: boolean;
  fog?: boolean;
  icon: "folder" | "up";
  /** Filter text to highlight inside the name. */
  query?: string;
  onClick: () => void;
}) {
  const Icon = icon === "up" ? ArrowUp : Folder;
  return (
    <button
      type="button"
      onClick={onClick}
      className="hover:bg-accent focus-visible:bg-accent flex w-full items-center gap-2 px-3 py-2 text-left text-sm outline-none"
    >
      <Icon className="text-muted-foreground size-4 shrink-0" aria-hidden />
      <span className="min-w-0 flex-1 truncate font-mono">
        <Highlighted name={name} query={query} />
      </span>
      {git ? (
        <span className="text-muted-foreground border-border text-3xs rounded border px-1 font-mono uppercase">
          git
        </span>
      ) : null}
      {fog ? (
        <span className="text-primary border-primary/40 text-3xs rounded border px-1 font-mono uppercase">
          fog.json
        </span>
      ) : null}
      {icon === "folder" ? (
        <ChevronRight
          className="text-muted-foreground size-3.5 shrink-0"
          aria-hidden
        />
      ) : null}
    </button>
  );
}

/**
 * Folder picker inlined into the launch form (so it works inside the dialog,
 * the bottom sheet and the `/status` card without stacking modals).
 *
 * Only a directory that holds a `fog.json` can be picked, because that is what
 * fog launches; every row shows which subfolders are git repos and which are
 * launchable.
 *
 * The field does shell-style completion: a folder *name* (`red`) filters the
 * open folder, while a *path* (`~/code/r`, `/Users/me/c`) lists the folders in
 * its parent that match the last segment. Picking a folder leaves its path in
 * the field with a trailing `/` so the next name completes inside it.
 */
export function DirectoryBrowser({
  startDir,
  onPick,
  onCancel,
}: {
  /** Directory to open; an empty string opens the home directory. */
  startDir?: string;
  /** Called with the canonical path of the chosen config dir. */
  onPick: (path: string) => void;
  onCancel: () => void;
}) {
  const [dir, setDir] = useState(startDir?.trim() ?? "");
  const [typed, setTyped] = useState(startDir?.trim() ?? "");
  const inputRef = useRef<HTMLInputElement>(null);
  // Bumped whenever a folder is opened, so the caret lands at the end of the
  // completed path once React has rendered it.
  const [focusTick, setFocusTick] = useState(0);

  const text = typed.trim();
  const slash = text.lastIndexOf("/");
  // A path that is not the open folder completes its last segment against the
  // parent: `~/code/r` lists the `r…` folders of `~/code`.
  const completing = isPathLike(text) && text !== dir;
  const partial = completing && !text.endsWith("/") && slash >= 0;
  const fragment = partial ? text.slice(slash + 1) : "";
  const queryDir = completing
    ? partial
      ? slash === 0
        ? "/"
        : text.slice(0, slash)
      : text
    : dir;

  const listing = useDirectoryListing(queryDir);
  const current = listing.data;

  // Commit the canonical path (`~` expanded) while the field is not being used
  // to type a filter or a path.
  const idle = !completing && text === dir;
  useEffect(() => {
    if (!idle) return;
    const canonical = current?.path;
    if (canonical && canonical !== dir) {
      setDir(canonical);
      setTyped(canonical);
    }
  }, [idle, current?.path, dir]);

  const entries = current?.entries ?? [];
  const filter = partial ? fragment : isPathLike(text) ? "" : text;
  const matches = matchEntries(
    entries,
    filter,
    partial ? "prefix" : "contains"
  );

  // A completed segment (`~/code/red-fox`) is itself pickable, so Use works
  // without one more click.
  const exact = partial
    ? entries.find(
        (entry) => entry.name.toLowerCase() === fragment.toLowerCase()
      )
    : undefined;
  const pickTarget = partial ? exact : current;

  // Opening a folder completes its path with a trailing `/` and leaves the
  // caret at the end, so the next folder name typed keeps completing inside it.
  const openDir = (path: string) => {
    setDir(path);
    setTyped(path.endsWith("/") ? path : `${path}/`);
    setFocusTick((tick) => tick + 1);
  };

  useEffect(() => {
    const input = inputRef.current;
    if (!focusTick || !input) return;
    input.focus();
    input.setSelectionRange(input.value.length, input.value.length);
  }, [focusTick]);

  const hint = listing.isPending
    ? "Reading folder…"
    : listing.isError
      ? (listing.error?.message ?? "Could not read this folder.")
      : filter && matches.length > 0
        ? `${matches.length} of ${entries.length} folders match “${filter}”.`
        : partial && !exact
          ? "Pick a folder below to open it."
          : pickTarget?.fog
            ? "This folder can be launched."
            : "No fog.json here — keep looking.";

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-3">
      <p className="text-muted-foreground text-xs">
        Pick the config dir — the folder holding a <code>fog.json</code>. Type a
        folder name to filter, or a path to complete it.
      </p>

      <form
        className="flex items-center gap-2"
        onSubmit={(event) => {
          event.preventDefault();
          if (filter) {
            // Enter opens the only hit; several hits stay for picking.
            if (matches.length === 1) openDir(matches[0].path);
            return;
          }
          openDir(text || dir);
        }}
      >
        <Input
          ref={inputRef}
          value={typed}
          placeholder="~/code/project"
          aria-label="Folder to browse"
          font="mono"
          autoFocus
          // Selecting the path on focus makes typing mean "search" rather than
          // "append to the current path" — but not right after a folder was
          // opened, where the caret belongs at the end of the completed path.
          onFocus={(event) => {
            if (!event.target.value.endsWith("/")) event.target.select();
          }}
          onChange={(event) => setTyped(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Escape" && filter) {
              event.preventDefault();
              openDir(current?.path ?? dir);
            }
          }}
        />
        {filter ? (
          <Button
            type="button"
            size="sm"
            variant="outline"
            aria-label="Clear filter"
            onClick={() => openDir(current?.path ?? dir)}
          >
            <X aria-hidden />
          </Button>
        ) : (
          <Button type="submit" size="sm" variant="outline">
            Go
          </Button>
        )}
      </form>

      <div className="border-border h-64 overflow-y-auto rounded-md border sm:h-72">
        {listing.isPending ? (
          <div className="space-y-2 p-3">
            {[0, 1, 2, 3].map((i) => (
              <Skeleton key={i} className="h-6 w-full" />
            ))}
          </div>
        ) : listing.isError ? (
          <p className="text-destructive p-3 font-mono text-xs">
            {listing.error?.message ?? "Could not read this folder."}
          </p>
        ) : (
          <>
            {current?.parent ? (
              <DirectoryRow
                name=".."
                icon="up"
                onClick={() => openDir(current.parent ?? "")}
              />
            ) : null}
            {entries.length === 0 ? (
              <p className="text-muted-foreground p-3 font-mono text-xs">
                No subfolders here.
              </p>
            ) : matches.length === 0 ? (
              <p className="text-muted-foreground p-3 font-mono text-xs">
                No folders match “{filter}”.
              </p>
            ) : (
              matches.map((entry) => (
                <DirectoryRow
                  key={entry.path}
                  name={entry.name}
                  git={entry.git}
                  fog={entry.fog}
                  icon="folder"
                  query={filter}
                  onClick={() => openDir(entry.path)}
                />
              ))
            )}
          </>
        )}
      </div>

      <div className="border-border flex flex-wrap items-center justify-between gap-2 border-t pt-3">
        <p
          className={cn(
            "text-2xs min-w-0 flex-1 truncate font-mono",
            listing.isError ? "text-destructive" : "text-muted-foreground"
          )}
          title={pickTarget?.path ?? current?.path}
        >
          {hint}
        </p>
        <div className="flex shrink-0 items-center gap-2">
          <Button variant="ghost" size="sm" onClick={onCancel}>
            Cancel
          </Button>
          <Button
            size="sm"
            disabled={!pickTarget?.fog}
            onClick={() => {
              if (pickTarget) onPick(pickTarget.path);
            }}
          >
            Use this folder
          </Button>
        </div>
      </div>
    </div>
  );
}
