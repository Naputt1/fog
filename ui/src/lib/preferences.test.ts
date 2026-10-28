import { describe, expect, it } from "vitest";

import {
  DEFAULT_PREFERENCES,
  isDefaultPreferences,
  normalizePreferences,
} from "./preferences";

describe("normalizePreferences", () => {
  it("returns the defaults for missing or non-object input", () => {
    expect(normalizePreferences(undefined)).toEqual(DEFAULT_PREFERENCES);
    expect(normalizePreferences(null)).toEqual(DEFAULT_PREFERENCES);
    expect(normalizePreferences("nope")).toEqual(DEFAULT_PREFERENCES);
  });

  it("keeps valid values", () => {
    expect(
      normalizePreferences({
        pollMs: 10_000,
        defaultLogView: "terminal",
        initialLogLines: 2_000,
        terminalFontSize: 16,
        swipeNavigation: false,
      })
    ).toEqual({
      pollMs: 10_000,
      defaultLogView: "terminal",
      initialLogLines: 2_000,
      terminalFontSize: 16,
      swipeNavigation: false,
    });
  });

  it("rejects polling and backfill values outside the offered options", () => {
    const normalized = normalizePreferences({
      pollMs: 12_345,
      initialLogLines: 50_000,
    });
    expect(normalized.pollMs).toBe(DEFAULT_PREFERENCES.pollMs);
    expect(normalized.initialLogLines).toBe(
      DEFAULT_PREFERENCES.initialLogLines
    );
  });

  it("clamps the terminal font size into its supported range", () => {
    expect(normalizePreferences({ terminalFontSize: 2 }).terminalFontSize).toBe(
      11
    );
    expect(
      normalizePreferences({ terminalFontSize: 99 }).terminalFontSize
    ).toBe(18);
    expect(
      normalizePreferences({ terminalFontSize: 14.6 }).terminalFontSize
    ).toBe(15);
    expect(
      normalizePreferences({ terminalFontSize: "14" }).terminalFontSize
    ).toBe(DEFAULT_PREFERENCES.terminalFontSize);
  });

  it("falls back on an unknown log view", () => {
    expect(
      normalizePreferences({ defaultLogView: "both" }).defaultLogView
    ).toBe("logs");
  });
});

describe("isDefaultPreferences", () => {
  it("detects a fully default object", () => {
    expect(isDefaultPreferences(DEFAULT_PREFERENCES)).toBe(true);
    expect(
      isDefaultPreferences({ ...DEFAULT_PREFERENCES, swipeNavigation: false })
    ).toBe(false);
  });
});
