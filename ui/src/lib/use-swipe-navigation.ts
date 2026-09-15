import { useEffect, useRef } from "react";

/** Elements a swipe must never hijack (text entry, terminals, opted-out). */
const DEFAULT_IGNORE =
  ".xterm, input, textarea, select, [contenteditable], [data-no-swipe]";

/** Minimum horizontal travel (px) for a swipe to count without velocity. */
const DISTANCE_THRESHOLD = 60;
/** Minimum horizontal velocity (px/ms) for a fast flick to count. */
const VELOCITY_THRESHOLD = 0.5;
/** Movement below this stays "undecided" so a tap is never read as a swipe. */
const AXIS_SLOP = 12;

interface Point {
  x: number;
  y: number;
  t: number;
}

function hasHorizontalScrollAncestor(
  start: Element,
  boundary: Element
): boolean {
  let el: Element | null = start;
  while (el && el !== boundary) {
    if (el.scrollWidth > el.clientWidth + 1) {
      const overflowX = getComputedStyle(el).overflowX;
      if (overflowX === "auto" || overflowX === "scroll") return true;
    }
    el = el.parentElement;
  }
  return false;
}

/**
 * Attach horizontal swipe-to-navigate to a surface.
 *
 * Returns a ref to spread onto the swipe target. A gesture only counts when it
 * is clearly horizontal (never fights vertical scrolling), starts outside the
 * ignore list and any horizontally-scrollable child, and clears either a
 * distance or velocity threshold. Gestures starting in an opt-out scope
 * (`ignore`) are left to that scope's own hook.
 */
export function useSwipeNavigation({
  onPrev,
  onNext,
  ignoreScope = "",
  enabled = true,
}: {
  onPrev: () => void;
  onNext: () => void;
  /** Extra selector whose subtree this hook should not handle. */
  ignoreScope?: string;
  enabled?: boolean;
}): React.RefObject<HTMLDivElement | null> {
  const ref = useRef<HTMLDivElement>(null);
  const onPrevRef = useRef(onPrev);
  const onNextRef = useRef(onNext);

  useEffect(() => {
    onPrevRef.current = onPrev;
    onNextRef.current = onNext;
  });

  useEffect(() => {
    const surface = ref.current;
    if (!surface || !enabled) return;

    const ignoreSelector = ignoreScope
      ? `${DEFAULT_IGNORE}, ${ignoreScope}`
      : DEFAULT_IGNORE;
    let start: Point | null = null;
    let axis: "undecided" | "horizontal" | "vertical" = "undecided";

    const reset = () => {
      start = null;
      axis = "undecided";
    };

    const onTouchStart = (e: TouchEvent) => {
      if (e.touches.length !== 1) return;
      const target = e.target as Element | null;
      if (target?.closest?.(ignoreSelector)) {
        reset();
        return;
      }
      if (target && hasHorizontalScrollAncestor(target, surface)) {
        reset();
        return;
      }
      const touch = e.touches[0];
      start = { x: touch.clientX, y: touch.clientY, t: e.timeStamp };
      axis = "undecided";
    };

    const onTouchMove = (e: TouchEvent) => {
      if (!start || e.touches.length !== 1) return;
      const touch = e.touches[0];
      const dx = touch.clientX - start.x;
      const dy = touch.clientY - start.y;
      if (axis === "undecided") {
        if (Math.abs(dx) < AXIS_SLOP && Math.abs(dy) < AXIS_SLOP) return;
        axis = Math.abs(dx) > Math.abs(dy) ? "horizontal" : "vertical";
      }
    };

    const onTouchEnd = (e: TouchEvent) => {
      if (!start) return;
      const wasHorizontal = axis === "horizontal";
      const dx = e.changedTouches[0].clientX - start.x;
      const dt = Math.max(1, e.timeStamp - start.t);
      reset();
      if (!wasHorizontal) return;
      const velocity = Math.abs(dx) / dt;
      if (Math.abs(dx) < DISTANCE_THRESHOLD && velocity < VELOCITY_THRESHOLD) {
        return;
      }
      if (dx > 0) onPrevRef.current();
      else onNextRef.current();
    };

    surface.addEventListener("touchstart", onTouchStart, { passive: true });
    surface.addEventListener("touchmove", onTouchMove, { passive: true });
    surface.addEventListener("touchend", onTouchEnd, { passive: true });
    surface.addEventListener("touchcancel", reset, { passive: true });
    return () => {
      surface.removeEventListener("touchstart", onTouchStart);
      surface.removeEventListener("touchmove", onTouchMove);
      surface.removeEventListener("touchend", onTouchEnd);
      surface.removeEventListener("touchcancel", reset);
    };
  }, [enabled, ignoreScope]);

  return ref;
}
