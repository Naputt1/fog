import { describe, expect, it } from "vitest";

import type { Service } from "@/lib/api";
import {
  findProject,
  findWorktree,
  groupServices,
  projectServices,
  projectStats,
  worktreeFromParam,
  worktreeParam,
  worktreeStats,
} from "./services";

function svc(overrides: Partial<Service>): Service {
  return {
    project: "fog",
    worktree: "",
    service: "api",
    container: "fog-api-1",
    status: "running",
    url: null,
    ports: [],
    health: "unknown",
    ...overrides,
  };
}

const SERVICES: Service[] = [
  svc({ project: "fog", worktree: "", service: "web", ports: ["0.0.0.0:8080->80/tcp"] }),
  svc({ project: "fog", worktree: "feat-mobile-web", service: "api", status: "exited" }),
  svc({ project: "gems", worktree: "main", service: "db", ports: ["0.0.0.0:5432->5432/tcp"] }),
];

describe("groupServices", () => {
  it("groups by project then worktree, default checkout first", () => {
    const groups = groupServices(SERVICES);
    expect(groups.map((g) => g.project)).toEqual(["fog", "gems"]);
    expect(groups[0].worktrees.map((w) => w.worktree)).toEqual([
      "",
      "feat-mobile-web",
    ]);
    expect(groups[0].total).toBe(2);
  });
});

describe("worktreeParam", () => {
  it("maps the empty worktree to the default token", () => {
    expect(worktreeParam("")).toBe("default");
    expect(worktreeParam("feat-mobile-web")).toBe("feat-mobile-web");
  });

  it("round-trips through worktreeFromParam", () => {
    expect(worktreeFromParam("default")).toBe("");
    expect(worktreeFromParam("main")).toBe("main");
  });
});

describe("findProject / findWorktree", () => {
  const groups = groupServices(SERVICES);

  it("finds a project case-insensitively and misses unknown ones", () => {
    expect(findProject(groups, "FOG")?.project).toBe("fog");
    expect(findProject(groups, "nope")).toBeNull();
  });

  it("resolves a worktree from its URL token", () => {
    const fog = findProject(groups, "fog")!;
    expect(findWorktree(fog, "default")?.worktree).toBe("");
    expect(findWorktree(fog, "feat-mobile-web")?.worktree).toBe(
      "feat-mobile-web"
    );
    expect(findWorktree(fog, "missing")).toBeNull();
  });
});

describe("stats", () => {
  const groups = groupServices(SERVICES);
  const fog = findProject(groups, "fog")!;

  it("rolls up counts and ports for a worktree", () => {
    const wt = findWorktree(fog, "default")!;
    expect(worktreeStats(wt)).toEqual({
      total: 1,
      running: 1,
      ports: ["0.0.0.0:8080->80/tcp"],
    });
  });

  it("rolls up counts across a project's worktrees", () => {
    expect(projectStats(fog)).toEqual({
      total: 2,
      running: 1,
      ports: ["0.0.0.0:8080->80/tcp"],
    });
    expect(projectServices(fog)).toHaveLength(2);
  });
});
