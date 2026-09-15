import type { InstanceStatus, Service } from "@/lib/api";

export interface WorktreeBucket {
  /** Git worktree name; "" means services started in the default checkout. */
  worktree: string;
  services: Service[];
}

export interface ProjectBucket {
  project: string;
  /** Configured project icon (image URL/data URI), or null when none. */
  icon: string | null;
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
    let icon: string | null = null;
    for (const [worktree, list] of byWorktree) {
      list.sort((a, b) => a.service.localeCompare(b.service));
      worktrees.push({ worktree, services: list });
      total += list.length;
      icon ??= list.find((s) => s.icon)?.icon ?? null;
    }
    worktrees.sort((a, b) => {
      if (a.worktree === "") return -1;
      if (b.worktree === "") return 1;
      return a.worktree.localeCompare(b.worktree);
    });
    projects.push({ project, icon, worktrees, total });
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
  return project.worktrees.find((w) => w.worktree === wanted) ?? null;
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

/* ------------------------------------------------------------------ */
/* Instance-centric model                                              */
/*                                                                     */
/* `groupServices` groups the docker directory (`/api/services`),      */
/* which lists only *running* containers and cannot identify the fog   */
/* instance (pid/script) that owns a service. Service controls         */
/* (start/stop/restart) need that pid, and stopped services are only   */
/* reported by `/api/status` → `instances[].services[]`. The helpers   */
/* below join both endpoints into a control-aware view used by the     */
/* project/branch pages.                                               */
/* ------------------------------------------------------------------ */

/** One service as reported by a fog instance, enriched from the directory. */
export interface InstanceServiceView {
  /** Service name (also the IPC control target). */
  name: string;
  /** Whether the instance reports the process as running. */
  running: boolean;
  /** Health detail from the instance, null when unset. */
  health: string | null;
  /**
   * Matching `/api/services` directory entry (URL/ports/container), or null
   * for a stopped/native-less service that docker discovery does not list.
   */
  service: Service | null;
}

/** A running fog instance and the services it manages. */
export interface InstanceView {
  /** Fog process pid — the target of IPC service actions and instance kill. */
  pid: number;
  /** Script the instance was started with. */
  script: string;
  /** Lowercased project name, matching {@link ProjectBucket.project}. */
  project: string;
  /** Normalized worktree token, matching {@link WorktreeBucket.worktree}. */
  worktree: string;
  /** Raw git branch, null when the instance reports none. */
  branch: string | null;
  /** Services in stable name order (running and stopped). */
  services: InstanceServiceView[];
}

/** A branch (worktree) of a project and every instance serving it. */
export interface BranchBucket {
  project: string;
  worktree: string;
  /** Instances serving this branch, sorted by script then pid. */
  instances: InstanceView[];
}

/**
 * Replicates the Rust `ports::sanitize_hostname` transform for one branch
 * label: lowercase valid DNS labels, otherwise slugify non-`[a-z0-9-]` runs
 * to single dashes. Kept client-side so instance branches (`feat/x`) can be
 * matched to the `/api/services` worktree token (`feat-x`).
 */
function sanitizeHostname(host: string): string {
  return host
    .split(".")
    .map((label) => {
      if (!label) return label;
      const valid =
        label.length <= 63 &&
        /^[a-z0-9-]+$/.test(label) &&
        !label.startsWith("-") &&
        !label.endsWith("-");
      if (valid) return label.toLowerCase();
      return (
        label
          .toLowerCase()
          .replace(/[^a-z0-9-]+/g, "-")
          .replace(/-+/g, "-")
          .replace(/^-+|-+$/g, "") || DEFAULT_WORKTREE
      );
    })
    .join(".");
}

/**
 * Normalizes an instance's raw git branch to the worktree token used by
 * `/api/services` (and by this module's bucket keys). Empty/None maps to
 * {@link DEFAULT_WORKTREE}, matching the native-service synthesis on the server.
 */
export function worktreeFromBranch(branch?: string | null): string {
  if (!branch) return DEFAULT_WORKTREE;
  const token = sanitizeHostname(branch).split(".")[0];
  return token || DEFAULT_WORKTREE;
}

function serviceKey(project: string, worktree: string, name: string): string {
  return `${project.toLowerCase()}\u0000${worktree}\u0000${name}`;
}

/**
 * Joins `GET /api/status` instances with the `GET /api/services` directory.
 *
 * Each instance's services are enriched with the matching directory entry so
 * the UI can show URL/ports/container and still control stopped services that
 * docker discovery omits. Instance branch (`feat/x`) is normalized to the
 * directory's worktree token (`feat-x`) before matching.
 */
export function buildInstanceViews(
  instances: InstanceStatus[],
  services: Service[]
): InstanceView[] {
  const exact = new Map<string, Service>();
  const byProjectService = new Map<string, Service[]>();
  for (const svc of services) {
    exact.set(serviceKey(svc.project, svc.worktree, svc.service), svc);
    const listKey = `${svc.project.toLowerCase()}\u0000${svc.service}`;
    const list = byProjectService.get(listKey);
    if (list) list.push(svc);
    else byProjectService.set(listKey, [svc]);
  }

  const lookup = (
    project: string,
    worktree: string,
    name: string
  ): Service | null => {
    const hit = exact.get(serviceKey(project, worktree, name));
    if (hit) return hit;
    // Fallback: a unique directory entry for this project+service (covers
    // docker/native worktree-token drift for the default checkout).
    const candidates = byProjectService.get(
      `${project.toLowerCase()}\u0000${name}`
    );
    return candidates && candidates.length === 1 ? candidates[0] : null;
  };

  const views = instances.map((inst) => {
    const project = (inst.project ?? inst.script).toLowerCase();
    const worktree = worktreeFromBranch(inst.branch);
    const svcs: InstanceServiceView[] = inst.services
      .map((s) => ({
        name: s.name,
        running: s.running,
        health: s.health,
        service: lookup(project, worktree, s.name),
      }))
      .sort((a, b) => a.name.localeCompare(b.name));
    return {
      pid: inst.pid,
      script: inst.script,
      project,
      worktree,
      branch: inst.branch ?? null,
      services: svcs,
    };
  });

  views.sort((a, b) => {
    if (a.script !== b.script) return a.script.localeCompare(b.script);
    return a.pid - b.pid;
  });
  return views;
}

/** Groups instance views into per-project, per-branch buckets. */
export function groupByBranch(views: InstanceView[]): BranchBucket[] {
  const byProject = new Map<string, Map<string, InstanceView[]>>();
  for (const view of views) {
    let byWorktree = byProject.get(view.project);
    if (!byWorktree) {
      byWorktree = new Map();
      byProject.set(view.project, byWorktree);
    }
    const list = byWorktree.get(view.worktree);
    if (list) list.push(view);
    else byWorktree.set(view.worktree, [view]);
  }

  const buckets: BranchBucket[] = [];
  for (const [project, byWorktree] of byProject) {
    for (const [worktree, instances] of byWorktree) {
      buckets.push({ project, worktree, instances });
    }
  }
  buckets.sort((a, b) => {
    if (a.project !== b.project) return a.project.localeCompare(b.project);
    if (a.worktree === "") return -1;
    if (b.worktree === "") return 1;
    return a.worktree.localeCompare(b.worktree);
  });
  return buckets;
}

/** Case-insensitive lookup of a branch bucket from its URL token. */
export function findBranch(
  buckets: BranchBucket[],
  project: string,
  branchParam: string
): BranchBucket | null {
  const needle = project.toLowerCase();
  return (
    buckets.find(
      (b) =>
        b.project.toLowerCase() === needle &&
        (b.worktree === branchParam ||
          (branchParam === DEFAULT_WORKTREE && b.worktree === ""))
    ) ?? null
  );
}

/** Rollup counts + ports for one instance's services. */
export function instanceStats(view: InstanceView): BucketStats {
  let running = 0;
  const ports = new Set<string>();
  for (const svc of view.services) {
    if (svc.running) running += 1;
    for (const port of svc.service?.ports ?? []) ports.add(port);
  }
  return { total: view.services.length, running, ports: [...ports] };
}

/**
 * Rollup counts + ports for a branch, deduplicating services that several
 * instances share (concurrent mode): a name is "running" when any instance
 * reports it running, so the branch header never double-counts shared services.
 */
export function branchStats(bucket: BranchBucket): BucketStats {
  const byName = new Map<string, { running: boolean; ports: Set<string> }>();
  for (const inst of bucket.instances) {
    for (const svc of inst.services) {
      let entry = byName.get(svc.name);
      if (!entry) {
        entry = { running: false, ports: new Set() };
        byName.set(svc.name, entry);
      }
      entry.running = entry.running || svc.running;
      for (const port of svc.service?.ports ?? []) entry.ports.add(port);
    }
  }
  let running = 0;
  const ports = new Set<string>();
  for (const entry of byName.values()) {
    if (entry.running) running += 1;
    for (const port of entry.ports) ports.add(port);
  }
  return { total: byName.size, running, ports: [...ports] };
}
