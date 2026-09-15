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
