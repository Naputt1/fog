---
title: Agentic Worktrees
---

# Agentic Worktrees: Human + Agent on the Same Repo

Fog supports **worktree-aware orchestration that stays concurrent**: you and an agent can run the same `fog <script>` on the same branch without killing each other, share a DB, see each other's status, and check everything from your phone.

## Two modes

Every `fog.json` script has a `concurrent` flag (default `true`).

| Mode | Flag | What `fog dev` does when one already runs on `(project, script, branch)` |
|------|------|-----------------------------------------------------------------------------|
| **Concurrent** (default) | `"concurrent": true` (or omitted) | Starts **alongside** — no kill. `share: true` services are borrowed when healthy instead of duplicated. |
| **Single-instance** | `"concurrent": false` | Reclaims the old instance on the same branch, handing over `reuse: true` services, then starts. |

Identity is `project = git rev-parse --git-common-dir` (shared by all worktrees) + `script` + `branch`. Different branches **always** run concurrently (`feature-x` and `main` never reclaim each other).

### `share` vs `reuse` — pick one per mode

| Mode | Honored flag | Health probe | Borrowed? | Torn down when |
|------|--------------|--------------|-----------|----------------|
| `concurrent: true` | `share` | `tcp`/`http`/`docker` must pass | `♻ reusing already-running ...` tab | Last instance serving that `(project,script)` exits |
| `concurrent: false` | `reuse` | same | `♻ adopted from instance <pid>` streaming PTY | Handover to successor; otherwise last instance |

The other flag is ignored (`concurrent:true + reuse:true` just starts). Both warn if `health_check` missing and start normally. See [Configuration](/configuration) for full semantics.

## Human + agent recipe (same branch)

```json
{
  "ports": { "api": 0, "web": 0 },
  "scripts": {
    "dev": {
      "service": [
        {
          "name": "db",
          "path": ".",
          "cmd": "docker compose up -d",
          "shutdown_cmd": "docker compose down",
          "share": true,
          "health_check": { "kind": "docker", "target": "postgres" }
        },
        {
          "name": "api",
          "path": "backend",
          "cmd": "cargo run -- --port ${ports.api}",
          "env": { "DATABASE_URL": "postgres://localhost:5432/dev_${branch}" },
          "depends_on": ["db"],
          "health_check": { "kind": "tcp", "target": "localhost:${ports.api}" }
        },
        {
          "name": "web",
          "path": "frontend",
          "cmd": "npm run dev -- --port ${ports.web}",
          "depends_on": ["api"]
        }
      ],
      "proxy": {
        "port": 3000,
        "routes": [
          { "path": "/api", "upstream": "http://localhost:${ports.api}" },
          { "path": "/", "upstream": "http://localhost:${ports.web}", "ws": true }
        ]
      }
    }
  }
}
```

**Terminal A (you, TUI):**

```bash
fog dev
# TUI shows db:healthy (shared) + api:healthy on random port + web
fog ls
# pid   script  branch  proxy  services
# 1234  dev     main    :3000   db:healthy api:healthy web:running
```

**Terminal B (agent, headless):**

```bash
fog dev -d --branch main
# prints: daemon started pid=5678 ... fog ls 5678
fog ls
# 1234  dev  main  :3000   db:healthy api:healthy web:running
# 5678  dev  main  :3001   db:reused  api:healthy web:running
# ↑ same branch, two proxies, DB not duplicated
fog logs 5678          # captured agent output, ANSI stripped
fog logs 5678 --tail 100  # or SSE via web UI /logs/stream?pid=5678
```

`db` with `share:true` was borrowed — first instance owns it, second shows re-used tab. `api`/`web` used `ports:{api:0}` random per-instance + templated `upstream`, so no collision. Last `fog kill` tears DB down; any sibling keeps it alive.

**Per-branch isolation:** `FOG_BRANCH` is injected into every service; compose files use `docker compose -p redfox-${FOG_BRANCH:-main}` so `main` and `feature-x` branches get distinct project names/ports. Templates `${branch}` / `${FOG_BRANCH}` work in `cmd`, `env`, `health_check.target`, and `proxy.upstream/host`.

## Worktree switch in the TUI — `s`

Press `s` in normal mode:

* Popup lists all `git worktree list` worktrees. Current worktree is amber `*`, running branches are blue `*`, selected row `>` highlight.
* `f` enters fuzzy filter (case-insensitive subsequence match on branch or path) — typing filters, `Backspace` works, `Esc` leaves search (stays open). `f` outside search toggles search; `d` outside search terminates the selected branch's live instances (shows `terminated N instances`).
* `↑`/`↓` cycle, `Enter` switches in-place: fog validates the target `fog.json` first (no teardown on error), then preserves borrowed (`share`/`reuse`) terminals, frees old ports, allocates `ports`, and rebuilds tabs/proxy. Config watcher is reset.
* Detached worktrees (no branch) show path as label and cannot be terminated by branch; `${branch}` templates error there.
* If port allocation fails mid-switch the app stays empty until restart — switch only after ports are free.

Different branches via `fog dev --branch feature-x` or `s` switch run side-by-side; same-branch re-run in `concurrent:false` reclaims with streaming handoff `♻ adopted from instance <pid>`.

## Phone on the tailnet

For the full index server and launch API, see [Index Server](/index-server).

1. On laptop, `tailscale ip -4` → e.g. `100.86.26.45`. Ensure a `fog dev` is running — the index server at `127.0.0.1:18080` is exposed on `:80` via Traefik's catch-all.
2. On phone (same tailnet, no DNS config), open `http://<tailnet IP>`. The directory lists every running service with `http://<tailnet IP>:<port>` links (raw exposed port), grouped by project/worktree — tap to open or copy. `*.acme` wildcard hostnames resolve only on the laptop; phone must use the IP:port links.
3. Live logs: `Logs` → pick service → streams with ANSI colors, follow/copy/clear.

Launching via phone (`Status → Start instance`) works but the layout is tight — prefer desktop for launch; phone is best for view/logs/copy.

## Troubleshooting for agentic use

* `fog ls` shows `branch` column — two rows `dev main` means concurrent correctly active.
* `fog kill <pid>` targets one instance; plain `fog kill` when only one exists is safe. `s → d` terminates by branch (never self).
* `share`/`reuse` without `health_check` warns `⚠ no health_check` and starts anyway — could duplicate DB; add `tcp` or `docker` probe.
* Random `ports:0` + `share:true` footgun: shared resource gets divergent per-instance ports — use fixed port for shared DB or avoid templating it.
* `R` on a `♻ reusing` tab kills the borrowed process and starts fresh in your worktree (take over).
* Grace: borrowed `reused` stays `pending` → `healthy` seeded, but if still unhealthy after ~10s (`DEFAULT_REUSE_GRACE`) fog auto-starts it once.
