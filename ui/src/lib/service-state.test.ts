import { describe, expect, it } from "vitest";

import {
  SERVICE_STATES,
  SERVICE_STATE_STYLE,
  isBadgeState,
  isServiceState,
  parseServiceState,
  serviceStateKey,
  serviceStateStyle,
} from "@/lib/service-state";

describe("service state vocabulary", () => {
  it("matches the Rust wire strings", () => {
    // Kept as a literal: this is the SPA half of the wire contract.
    expect(SERVICE_STATES).toEqual([
      "pending",
      "starting",
      "running",
      "healthy",
      "unhealthy",
      "stopped",
      "failed",
      "unknown",
    ]);
  });

  it("gives every state a tone and a dot", () => {
    for (const state of SERVICE_STATES) {
      expect(SERVICE_STATE_STYLE[state].tone).toBeTruthy();
      expect(SERVICE_STATE_STYLE[state].dot).toMatch(/^bg-/);
    }
    expect(isBadgeState("killing")).toBe(true);
    expect(isBadgeState("crashed")).toBe(false);
  });

  it("parses known states and rejects unknown ones", () => {
    expect(parseServiceState("healthy")).toBe("healthy");
    expect(parseServiceState("HEALTHY")).toBe("healthy");
    expect(parseServiceState("crashed")).toBeNull();
    expect(isServiceState("running")).toBe(true);
    expect(isServiceState("running-ish")).toBe(false);
  });

  it("keeps unknown strings for display", () => {
    expect(serviceStateKey("HEALTHY")).toBe("healthy");
    expect(serviceStateKey("crashed")).toBe("crashed");
  });

  it("falls back to the neutral style", () => {
    expect(serviceStateStyle("crashed")).toEqual(SERVICE_STATE_STYLE.unknown);
    expect(serviceStateStyle("killing")).toEqual(SERVICE_STATE_STYLE.killing);
  });
});
