import { useId } from "react";

/**
 * The fog logo mark: a stack of isometric strata settling out of the fog. The
 * base slab is solid, and each one above it is smaller and fainter until the
 * stack dissolves into haze.
 *
 * "full" renders the gradient mark. "mono" renders a single-colour silhouette
 * in `currentColor` for small inline uses.
 */

const RATIO = 0.5714; // isometric rhombus half-height per half-width
const CX = 32;

/** Top slab — faintest and smallest — first. */
const SLABS = [
  { cy: 12.7, w: 11.4, depth: 3.4, opacity: 0.2 },
  { cy: 22.7, w: 15.2, depth: 4.2, opacity: 0.46 },
  { cy: 31.5, w: 19, depth: 5, opacity: 0.72 },
  { cy: 39.5, w: 21.5, depth: 5.8, opacity: 1 },
] as const;

const round = (value: number) => Math.round(value * 100) / 100;

function faces({ cy, w, depth }: { cy: number; w: number; depth: number }) {
  const h = round(w * RATIO);
  const left = round(CX - w);
  const right = round(CX + w);
  const bottom = round(cy + h);

  return {
    top: `M${CX} ${round(cy - h)} L${right} ${cy} L${CX} ${bottom} L${left} ${cy} Z`,
    right: `M${CX} ${bottom} L${right} ${cy} L${right} ${round(cy + depth)} L${CX} ${round(bottom + depth)} Z`,
    left: `M${left} ${cy} L${CX} ${bottom} L${CX} ${round(bottom + depth)} L${left} ${round(cy + depth)} Z`,
  };
}

export function BrandMark({
  variant = "full",
  className,
}: {
  variant?: "full" | "mono";
  className?: string;
}) {
  const uid = useId().replace(/[^a-zA-Z0-9]/g, "");
  const id = (name: string) => `fog-${name}-${uid}`;

  if (variant === "mono") {
    return (
      <svg viewBox="0 0 64 64" fill="none" className={className} aria-hidden>
        {SLABS.map((slab) => {
          const path = faces(slab);
          return (
            <g key={slab.cy}>
              <path fill="currentColor" opacity={0.78} d={path.left} />
              <path fill="currentColor" opacity={0.62} d={path.right} />
              <path fill="currentColor" d={path.top} />
            </g>
          );
        })}
      </svg>
    );
  }

  return (
    <svg viewBox="0 0 64 64" fill="none" className={className} aria-hidden>
      <defs>
        <linearGradient
          id={id("face")}
          x1="32"
          y1="6"
          x2="32"
          y2="40"
          gradientUnits="userSpaceOnUse"
        >
          <stop stopColor="var(--color-brand-1)" />
          <stop offset="0.48" stopColor="var(--color-brand-2)" />
          <stop offset="1" stopColor="var(--color-brand-3)" />
        </linearGradient>
        <linearGradient
          id={id("side-left")}
          x1="11"
          y1="30"
          x2="32"
          y2="57"
          gradientUnits="userSpaceOnUse"
        >
          <stop stopColor="var(--color-brand-side-1)" />
          <stop offset="1" stopColor="var(--color-brand-side-2)" />
        </linearGradient>
        <linearGradient
          id={id("side-right")}
          x1="32"
          y1="33"
          x2="53"
          y2="57"
          gradientUnits="userSpaceOnUse"
        >
          <stop stopColor="var(--color-brand-side-3)" />
          <stop offset="1" stopColor="var(--color-brand-side-4)" />
        </linearGradient>
      </defs>
      {SLABS.map((slab) => {
        const path = faces(slab);
        return (
          <g key={slab.cy} opacity={slab.opacity}>
            <path fill={`url(#${id("side-left")})`} d={path.left} />
            <path fill={`url(#${id("side-right")})`} d={path.right} />
            <path fill={`url(#${id("face")})`} d={path.top} />
          </g>
        );
      })}
    </svg>
  );
}
