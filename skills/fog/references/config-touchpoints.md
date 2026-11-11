# Config touch-points

Reference for the `fog.json` fields that change *how you run* fog, not how you author a
config. For the full schema, see
[`fog.schema.json`](https://raw.githubusercontent.com/Naputt1/fog/main/fog.schema.json)
and the [configuration docs](https://naputt1.github.io/fog/configuration).

## Fields that affect an agent run

| Field | Why it matters |
|-------|----------------|
| `share: true` | Borrow a healthy service across concurrent instances. Requires a `health_check`. Honored only when `concurrent: true` (default). |
| `reuse: true` | Hand over a live service across single-instance re-runs. Requires a `health_check`. Honored only when `concurrent: false`. |
| `health_check` | `kind` (`tcp` / `http` / `docker`) + `target`. Gates `depends_on` and the `share` / `reuse` probe. Adaptive: fast `start_interval_ms` until healthy, then `interval_ms`. |
| `depends_on` | Hold this service until the named ones are healthy. |
| `ports: { name: 0 }` | Allocate a random free port per instance. Reference it as `${ports.name}` in `cmd`, `env`, `health_check.target`, `proxy.upstream`, or an endpoint `port`. Use `${ports.name@repo:branch[:script]}` to read a port from another fog instance (see [configuration](https://naputt1.github.io/fog/configuration#cross-instance-ports)). |
| `shutdown_cmd` | Runs on teardown — including when the last instance releases a shared resource. |
| `endpoint` | A multi-port service (e.g. a compose stack) declares one entry per exposed endpoint; each gets its own health and optional generated route. |
| `concurrent` | Script-level. `true` (default) runs alongside; `false` reclaims the previous same-branch instance. |

Templates: `${ports.*}`; `${branch}` / `${FOG_BRANCH}` (DNS-safe slug, `feat/book` →
`feat-book`); `${branch_raw}` / `${FOG_BRANCH_RAW}` (raw name — display only, never a
`Host()` or DNS name). Cross-instance ports use
`${ports.<name>@<repo>:<branch>[:<script>]}`: repo is the index/UI project name (matched
case-insensitively), branch accepts the raw name, its slug, the index worktree segment
(`release/1.2` → `release-1`), or `${branch}` for the referencing branch (exact). Two
tokens are special: `?` follows the referencing branch when the target repo has a worktree
for it and otherwise falls back to its main worktree (logged as `fell back to '<branch>'`);
`*` always means the main worktree. Port names may not contain `@` or `:`. Values resolve once at
startup against live instances and are logged (`resolved … = PORT (pid N)`); add `:script`
to let fog start that project+branch+script when it is stopped (it must be in the project
registry, have a worktree on the branch, and declare the port), waiting until the target
reports every service started.
`--port <name>@<repo>:<branch>[:<script>]=PORT` pins one (fixed port only — `=0` is
rejected) and skips auto-start.
`FOG_BRANCH`, `FOG_BRANCH_RAW`, and `FOG_BRANCH_SLUG` are injected into every service
process.

## Logs and remote view

- Detached runs tee each service's raw PTY output to `$TMPDIR/fog-<pid>.logs/<name>.log`
  (and `daemon.log`), capped at 8 MiB per service. The files persist after the instance
  exits; `fog logs <pid> -s <name>` reads them, and `fog prune` removes the logs of every
  finished instance (`--dry-run` reports the size first). The proxy log is memory-only and
  is gone once the instance exits.
- `--save-logs` additionally writes `temp/<name>.txt` on exit.
- Web UI / API on `127.0.0.1:18080`: `GET /api/services`, `/api/status`, `/api/health`;
  live logs via SSE at `/logs/stream?pid=<pid>`. The directory is also reachable from a
  phone over a tailnet at `http://<tailnet IP>`; service links use raw
  `http://<tailnet IP>:<port>`.

## Pitfalls

- **`share: true` without a `health_check`** starts a duplicate DB — fog warns and starts
  anyway. Add a `tcp` or `docker` probe.
- **`share: true` with `ports: {x: 0}`** gives each instance a different port for the same
  shared resource. Give anything shared a fixed port.
- **Cross-instance ports fail fast.** If `${ports.api@other-repo:main}` has no live
  `other-repo` instance on that branch, startup aborts. Start the owner first, add a
  `:script` segment so fog can start it, use `?`/`${branch}` to pick the branch,
  pin it with `--port`, or drop the reference. A failed cold start reports the child's
  last `error:` line from `daemon.log` inline. Cold-started instances record their
  parent: `fog ls` shows a `started-by` column, `fog kill <parent>` warns about
  survivors, and `--with-dependents` stops them too.
- **Only theme and proxy settings hot-reload.** Service add/remove/edit needs
  `fog restart <pid>` (or `fog kill` plus `fog dev -d`).
- **The proxy is HTTP/1.1 only** — no HTTP/2.
- **`fog logs` slices with `--head`/`--tail`.** `fog logs <pid> -s <name> --tail 100`
  prints the last 100 lines; `--head N` keeps the first `N`; signed forms follow
  `head -n`/`tail -n` (`--head -N` drops the last `N`, `--tail +N` starts at line `N`).
  `--head N --tail M` prints both ends with a `... K lines omitted ...` marker. Both
  flags require `-s <name>` and reduce how much output you pull into context.
- **`fog ls` ignores a pid argument** and lists every instance.
- **`fog kill` with no pid is ambiguous** when multiple local instances exist; pass the pid.
