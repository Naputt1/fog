import type { Service } from "@/lib/api";

export interface WorktreeBucket {
  /** Git worktree name; "" means services started in the default checkout. */
  worktree: string;
  services: Service[];
}

export interface ProjectBucket {
  project: string;
  worktrees: WorktreeBucket[];
  total: number;
}

/** Label used when a service has no git worktree (started from the main checkout). */
export const DEFAULT_WORKTREE = "default";

/**
 * Group services by project (git-derived from the repo) and within each
 * project by worktree. The default checkout ("") sorts first, remaining
 * worktrees alphabetically, and services by name within a worktree.
 */
export function groupServices(services: Service[]): ProjectBucket[] {
  const byProject = new Map<string, Map<string, Service[]>>();
  for (const svc of services) {
    let byWorktree = byProject.get(svc.project);
    if (!byWorktree) {
      byWorktree = new Map();
      byProject.set(svc.project, byWorktree);
    }
    const bucket = byWorktree.get(svc.worktree);
    if (bucket) bucket.push(svc);
    else byWorktree.set(svc.worktree, [svc]);
  }

  const projects: ProjectBucket[] = [];
  for (const [project, byWorktree] of byProject) {
    const worktrees: WorktreeBucket[] = [];
    let total = 0;
    for (const [worktree, list] of byWorktree) {
      list.sort((a, b) => a.service.localeCompare(b.service));
      worktrees.push({ worktree, services: list });
      total += list.length;
    }
    worktrees.sort((a, b) => {
      if (a.worktree === "") return -1;
      if (b.worktree === "") return 1;
      return a.worktree.localeCompare(b.worktree);
    });
    projects.push({ project, worktrees, total });
  }
  projects.sort((a, b) => a.project.localeCompare(b.project));
  return projects;
}

/**
 * URL token for a worktree bucket. An empty worktree (default checkout) maps
 * to {@link DEFAULT_WORKTREE} so the path segment is never empty.
 */
export function worktreeParam(worktree: string): string {
  return worktree || DEFAULT_WORKTREE;
}

/** Inverse of {@link worktreeParam}: the "default" token maps back to "". */
export function worktreeFromParam(param: string): string {
  return param === DEFAULT_WORKTREE ? "" : param;
}

/** Case-insensitive lookup of a project bucket by name. */
export function findProject(
  groups: ProjectBucket[],
  project: string
): ProjectBucket | null {
  const needle = project.toLowerCase();
  return groups.find((g) => g.project.toLowerCase() === needle) ?? null;
}

/** Lookup a worktree bucket from its URL token within a project. */
export function findWorktree(
  project: ProjectBucket,
  branchParam: string
): WorktreeBucket | null {
  const wanted = worktreeFromParam(branchParam);
  return (
    project.worktrees.find((w) => w.worktree === wanted) ?? null
  );
}

export interface BucketStats {
  /** Number of services in the bucket. */
  total: number;
  /** Services whose docker status is "running". */
  running: number;
  /** Unique host ports published by the bucket's services. */
  ports: string[];
}

/** Rollup counts + ports for a worktree bucket. */
export function worktreeStats(wt: WorktreeBucket): BucketStats {
  return rollup(wt.services);
}

/** Rollup counts + ports for a project bucket. */
export function projectStats(project: ProjectBucket): BucketStats {
  return rollup(project.worktrees.flatMap((w) => w.services));
}

/** Flatten every service in a project, across its worktrees. */
export function projectServices(project: ProjectBucket): Service[] {
  return project.worktrees.flatMap((w) => w.services);
}

function rollup(services: Service[]): BucketStats {
  const ports = new Set<string>();
  let running = 0;
  for (const svc of services) {
    if (svc.status === "running") running += 1;
    for (const port of svc.ports) ports.add(port);
  }
  return { total: services.length, running, ports: [...ports] };
}
