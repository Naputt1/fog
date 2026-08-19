import { useId } from "react";

const CLOUD_PATH =
  "M8 10 H40 C40 18 38 24 34 26 C30 28 26 36 24 36 C22 36 18 28 14 26 C10 24 8 18 8 10 Z";

/**
 * The fog logo mark: an upside-down cloud (fog is a cloud sitting on the
 * ground) with gradient shading. "full" renders the detailed multi-color
 * mark; "mono" renders a single-color silhouette that inherits `currentColor`
 * for small inline uses.
 */
export function BrandCloud({
  variant = "full",
  className,
}: {
  variant?: "full" | "mono";
  className?: string;
}) {
  const uid = useId().replace(/[^a-zA-Z0-9]/g, "");
  const gradId = `fog-grad-${uid}`;

  if (variant === "mono") {
    return (
      <svg
        viewBox="0 0 48 46"
        fill="none"
        className={className}
        aria-hidden="true"
      >
        <path fill="currentColor" d={CLOUD_PATH} />
      </svg>
    );
  }

  return (
    <svg
      viewBox="0 0 48 46"
      fill="none"
      className={className}
      aria-hidden="true"
    >
      <defs>
        <linearGradient
          id={gradId}
          x1="24"
          y1="10"
          x2="24"
          y2="40"
          gradientUnits="userSpaceOnUse"
        >
          <stop stopColor="#c4b5fd" />
          <stop offset="0.45" stopColor="#a78bfa" />
          <stop offset="1" stopColor="#7c3aed" />
        </linearGradient>
      </defs>
      <path fill={`url(#${gradId})`} d={CLOUD_PATH} />
    </svg>
  );
}
