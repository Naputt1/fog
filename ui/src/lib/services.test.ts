import { describe, expect, it } from "vitest";

import type { InstanceStatus, Service } from "@/lib/api";
import {
  branchStats,
  buildInstanceViews,
  findBranch,
  findProject,
  findWorktree,
  groupByBranch,
  groupServices,
  instanceStats,
  projectServices,
  projectStats,
  endpointViews,
  worktreeFromBranch,
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
  svc({
    project: "fog",
    worktree: "",
    service: "web",
    ports: ["0.0.0.0:8080->80/tcp"],
  }),
  svc({
    project: "fog",
    worktree: "feat-mobile-web",
    service: "api",
    status: "exited",
  }),
  svc({
    project: "gems",
    worktree: "main",
    service: "db",
    ports: ["0.0.0.0:5432->5432/tcp"],
  }),
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

  it("hoists a project icon from any of its services", () => {
    const groups = groupServices([
      svc({ project: "fog", worktree: "", service: "web" }),
      svc({
        project: "fog",
        worktree: "feat-mobile-web",
        service: "api",
        icon: "https://example.com/fog.png",
      }),
      svc({ project: "gems", worktree: "main", service: "db" }),
    ]);
    expect(findProject(groups, "fog")?.icon).toBe(
      "https://example.com/fog.png"
    );
    expect(findProject(groups, "gems")?.icon).toBeNull();
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

describe("worktreeFromBranch", () => {
  it("slugifies a branch the way the server does", () => {
    expect(worktreeFromBranch("feat/mobile-web")).toBe("feat-mobile-web");
    expect(worktreeFromBranch("main")).toBe("main");
    // sanitize_hostname splits on dots before taking the first label.
    expect(worktreeFromBranch("feat/x.y")).toBe("feat-x");
  });

  it("maps a missing branch to the default token", () => {
    expect(worktreeFromBranch(null)).toBe("default");
    expect(worktreeFromBranch("")).toBe("default");
    expect(worktreeFromBranch(undefined)).toBe("default");
  });
});

const INSTANCES: InstanceStatus[] = [
  {
    pid: 200,
    script: "agent",
    project: "fog",
    branch: "feat/mobile-web",
    services: [{ name: "api", running: true, health: "healthy" }],
  },
  {
    pid: 100,
    script: "dev",
    project: "fog",
    branch: "feat/mobile-web",
    services: [
      { name: "api", running: true, health: "healthy" },
      { name: "worker", running: false, health: null },
    ],
  },
];

const DIRECTORY: Service[] = [
  svc({
    project: "fog",
    worktree: "feat-mobile-web",
    service: "api",
    container: "fog-api-1",
    ports: ["0.0.0.0:8080->80/tcp"],
  }),
];

describe("buildInstanceViews", () => {
  it("normalizes branch, sorts by script/pid and enriches services", () => {
    const views = buildInstanceViews(INSTANCES, DIRECTORY);
    expect(views.map((v) => [v.script, v.pid])).toEqual([
      ["agent", 200],
      ["dev", 100],
    ]);
    expect(views[0].worktree).toBe("feat-mobile-web");
    expect(views[0].project).toBe("fog");

    const dev = views[1];
    expect(dev.services.map((s) => s.name)).toEqual(["api", "worker"]);
    // api is running so it is enriched with the directory entry…
    const api = dev.services.find((s) => s.name === "api")!;
    expect(api.service?.container).toBe("fog-api-1");
    expect(api.service?.ports).toEqual(["0.0.0.0:8080->80/tcp"]);
    // …while the stopped worker has no directory entry.
    const worker = dev.services.find((s) => s.name === "worker")!;
    expect(worker.running).toBe(false);
    expect(worker.service).toBeNull();
  });
});

describe("groupByBranch / findBranch", () => {
  const buckets = groupByBranch(buildInstanceViews(INSTANCES, DIRECTORY));

  it("groups instances of a branch together", () => {
    expect(buckets).toHaveLength(1);
    expect(buckets[0].project).toBe("fog");
    expect(buckets[0].worktree).toBe("feat-mobile-web");
    expect(buckets[0].instances).toHaveLength(2);
  });

  it("looks a branch up case-insensitively", () => {
    expect(findBranch(buckets, "FOG", "feat-mobile-web")).not.toBeNull();
    expect(findBranch(buckets, "fog", "nope")).toBeNull();
  });
});

describe("branchStats / instanceStats", () => {
  const buckets = groupByBranch(buildInstanceViews(INSTANCES, DIRECTORY));

  it("dedupes shared services and unions ports", () => {
    // api is reported by both instances but counted once; worker is stopped.
    expect(branchStats(buckets[0])).toEqual({
      total: 2,
      running: 1,
      ports: ["0.0.0.0:8080->80/tcp"],
    });
  });

  it("reports one instance's own services", () => {
    expect(instanceStats(buckets[0].instances[1])).toEqual({
      total: 2,
      running: 1,
      ports: ["0.0.0.0:8080->80/tcp"],
    });
  });
});

describe("endpointViews", () => {
  it("merges IPC health with directory URLs by name", () => {
    const view = {
      name: "infra",
      running: true,
      health: "healthy" as string | null,
      endpoints: [
        { name: "web", health: "healthy" },
        { name: "api", health: "unhealthy" },
      ],
      service: svc({
        service: "infra",
        endpoints: [
          {
            name: "web",
            url: "https://web.main.acme/",
            port: "8080",
            health: "healthy",
          },
          { name: "api", url: "", port: "9000", health: "unhealthy" },
        ],
      }),
    };
    const merged = endpointViews(view);
    expect(merged.map((s) => s.name)).toEqual(["web", "api"]);
    expect(merged[0]).toMatchObject({
      name: "web",
      health: "healthy",
      url: "https://web.main.acme/",
      port: "8080",
    });
    expect(merged[1]).toMatchObject({
      name: "api",
      health: "unhealthy",
      port: "9000",
    });
  });

  it("is empty for a service with no declared endpoints", () => {
    const view = {
      name: "api",
      running: true,
      health: "healthy" as string | null,
      endpoints: [],
      service: svc({ service: "api" }),
    };
    expect(endpointViews(view)).toEqual([]);
  });
});
