#!/usr/bin/env bun
// Rasterize the SVG app icons into the PNG sizes the web manifest and iOS
// need. Requires `rsvg-convert` (brew install librsvg).
import { execFileSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const uiDir = join(dirname(fileURLToPath(import.meta.url)), "..");

// iOS applies its own squircle mask, so its source is the full-bleed square
// rather than the rounded tile used elsewhere.
const targets = [
  { svg: "public/icon.svg", png: "public/icon-512.png", size: 512 },
  { svg: "public/icon.svg", png: "public/icon-192.png", size: 192 },
  {
    svg: "scripts/apple-touch-icon.svg",
    png: "public/apple-touch-icon.png",
    size: 180,
  },
  {
    svg: "public/maskable-icon.svg",
    png: "public/maskable-512.png",
    size: 512,
  },
];

for (const { svg, png, size } of targets) {
  execFileSync("rsvg-convert", [
    "-w",
    String(size),
    "-h",
    String(size),
    join(uiDir, svg),
    "-o",
    join(uiDir, png),
  ]);
  console.log(`${svg} -> ${png} (${size}px)`);
}
