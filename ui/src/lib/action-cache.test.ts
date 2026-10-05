import { describe, expect, it } from "vitest";

import type { StatusSnapshot } from "@/lib/api";
import {
  applyServiceAction,
  isInstanceKilling,
  KILL_GRACE_MS,
  optimisticServicePatch,
  restoreService,
  type KillIntentState,
} from "./action-cache";

function snapshot(): StatusSnapshot {
  return {
    instances: [
      {
        pid: 1,
        script: "dev",
        services: [
          { name: "web", running: true, health: "healthy" },
          { name: "db", running: false, health: null },
        ],
      },
      {
        pid: 2,
        script: "dev",
        services: [{ name: "web", running: false, health: null }],
      },
    ],
  };
}

describe("optimisticServicePatch", () => {
  it("marks start/restart running and starting", () => {
    expect(optimisticServicePatch("start")).toEqual({
      running: true,
      health: "starting",
    });
    expect(optimisticServicePatch("restart")).toEqual({
      running: true,
      health: "starting",
    });
  });

  it("marks stop not running with no health", () => {
    expect(optimisticServicePatch("stop")).toEqual({
      running: false,
      health: null,
    });
  });
});

describe("applyServiceAction", () => {
  it("patches only the target service of the target instance", () => {
    const before = snapshot();
    const after = applyServiceAction(before, 1, "web", "stop");

    expect(after?.instances[0].services[0]).toEqual({
      name: "web",
      running: false,
      health: null,
    });
    // Sibling service untouched.
    expect(after?.instances[0].services[1]).toEqual({
      name: "db",
      running: false,
      health: null,
    });
    // Same-named service on another pid untouched.
    expect(after?.instances[1].services[0]).toEqual({
      name: "web",
      running: false,
      health: null,
    });
    // Input is not mutated.
    expect(before.instances[0].services[0].running).toBe(true);
  });

  it("starts a stopped service optimistically", () => {
    const after = applyServiceAction(snapshot(), 1, "db", "start");
    expect(after?.instances[0].services[1]).toEqual({
      name: "db",
      running: true,
      health: "starting",
    });
  });

  it("returns the same snapshot when nothing matches", () => {
    const before = snapshot();
    expect(applyServiceAction(before, 999, "web", "start")).toBe(before);
    expect(applyServiceAction(before, 1, "missing", "start")).toBe(before);
  });

  it("passes through undefined", () => {
    expect(applyServiceAction(undefined, 1, "web", "start")).toBeUndefined();
  });
});

describe("restoreService", () => {
  it("restores only the touched service's running/health", () => {
    const before = snapshot();
    const after = applyServiceAction(before, 1, "web", "stop");
    const restored = restoreService(after, 1, "web", {
      running: true,
      health: "healthy",
    });

    expect(restored?.instances[0].services[0]).toEqual({
      name: "web",
      running: true,
      health: "healthy",
    });
    // Other services are unaffected by the restore.
    expect(restored?.instances[0].services[1]).toEqual(
      before.instances[0].services[1]
    );
    expect(restored?.instances[1]).toEqual(before.instances[1]);
  });

  it("keeps a concurrent patch on another service when one action fails", () => {
    // A + B applied optimistically, then A is rolled back.
    const withBoth = applyServiceAction(
      applyServiceAction(snapshot(), 1, "web", "stop"),
      1,
      "db",
      "start"
    );
    const afterRollback = restoreService(withBoth, 1, "web", {
      running: true,
      health: "healthy",
    });

    // A (web) is back to its pre-action state.
    expect(afterRollback?.instances[0].services[0]).toEqual({
      name: "web",
      running: true,
      health: "healthy",
    });
    // B (db) keeps its successful optimistic patch.
    expect(afterRollback?.instances[0].services[1]).toEqual({
      name: "db",
      running: true,
      health: "starting",
    });
  });

  it("returns the same snapshot when nothing matches", () => {
    const before = snapshot();
    expect(
      restoreService(before, 999, "web", { running: true, health: "healthy" })
    ).toBe(before);
  });

  it("passes through undefined", () => {
    expect(
      restoreService(undefined, 1, "web", { running: true, health: "healthy" })
    ).toBeUndefined();
  });
});

describe("isInstanceKilling", () => {
  const now = 1_000_000;
  const intent = (over: Partial<KillIntentState>): KillIntentState => ({
    pid: 1,
    script: "dev",
    status: "pending",
    submittedAt: now,
    ...over,
  });

  it("is true for a pending or successful kill within the grace window", () => {
    expect(
      isInstanceKilling([intent({ status: "pending" })], 1, "dev", now)
    ).toBe(true);
    expect(
      isInstanceKilling([intent({ status: "success" })], 1, "dev", now)
    ).toBe(true);
  });

  it("is false for a failed kill", () => {
    expect(
      isInstanceKilling([intent({ status: "error" })], 1, "dev", now)
    ).toBe(false);
  });

  it("is false once the grace window elapses", () => {
    expect(
      isInstanceKilling(
        [intent({ submittedAt: now - KILL_GRACE_MS })],
        1,
        "dev",
        now
      )
    ).toBe(false);
  });

  it("is false for a different pid", () => {
    expect(isInstanceKilling([intent({})], 2, "dev", now)).toBe(false);
  });

  it("requires the script to match when known", () => {
    expect(
      isInstanceKilling([intent({ script: "dev" })], 1, "other", now)
    ).toBe(false);
    expect(
      isInstanceKilling([intent({ script: undefined })], 1, "dev", now)
    ).toBe(true);
  });
});
