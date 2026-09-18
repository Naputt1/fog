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
| `ports: { name: 0 }` | Allocate a random free port per instance. Reference it as `${ports.name}` in `cmd`, `env`, `health_check.target`, `proxy.upstream`, or an endpoint `port`. |
| `shutdown_cmd` | Runs on teardown — including when the last instance releases a shared resource. |
| `endpoint` | A multi-port service (e.g. a compose stack) declares one entry per exposed endpoint; each gets its own health and optional generated route. |
| `concurrent` | Script-level. `true` (default) runs alongside; `false` reclaims the previous same-branch instance. |

Templates: `${ports.*}`; `${branch}` / `${FOG_BRANCH}` (DNS-safe slug, `feat/book` →
`feat-book`); `${branch_raw}` / `${FOG_BRANCH_RAW}` (raw name — display only, never a
`Host()` or DNS name). `FOG_BRANCH`, `FOG_BRANCH_RAW`, and `FOG_BRANCH_SLUG` are injected
into every service process.

## Logs and remote view

- Detached runs tee each service's raw PTY output to `$TMPDIR/fog-<pid>.logs/<name>.log`
  (and `daemon.log`). The files persist after the instance exits; `fog logs <pid> -s <name>`
  reads them.
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
- **Only theme and proxy settings hot-reload.** Service add/remove/edit needs
  `fog restart <pid>` (or `fog kill` plus `fog dev -d`).
- **The proxy is HTTP/1.1 only** — no HTTP/2.
- **`fog logs` has no `--tail` flag.** Print the captured log and pipe through your own
  tooling.
- **`fog ls` ignores a pid argument** and lists every instance.
- **`fog kill` with no pid is ambiguous** when multiple local instances exist; pass the pid.
