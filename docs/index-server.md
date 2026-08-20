---
title: Index Server
---

# Index Server

Standalone service-directory **index server** (web UI + JSON API). By default every `fog <script>` serves the index unless opted out. This lives in the **fog config** (top-level `fog.json` alongside `theme`/`sidebar`, default `enabled:true`) — not per-script. It can be set **per-project** (`./fog.json`) or **globally** (`~/.config/fog/fog.json`, same schema); either can opt-out (both must be `true` to serve). The server is host-global (like `router`/`dnsmasq`) and is torn down automatically when the last fog instance exits — see `fog index kill/restart`.

## Top-level `index` field

```json
{
  "index": {
    "enabled": true,
    "port": 18080
  }
}
```

| Field | Required | Type | Default | Description |
|-------|----------|------|---------|-------------|
| `enabled` | No | `boolean` | `true` | Whether starting this project serves the index. Set `false` to opt this project out (useful for CI or projects that never need the web UI). |
| `port` | No | `integer` | `18080` | Port the index listens on. Overrides `router.index_port` when set. |

When `enabled:false`, `fog <script>` for this project will not start the index (and will not print the `+ service index server` line). The index can still be started manually via `fog index restart` or by starting another project that has `enabled:true` (the default). Other lifecycle still applies: `fog kill`/`fog restart` per-instance, `POST /api/server/kill` + `/restart`, and auto-teardown when the last instance exits.

The index port can also be set via `router.index_port` (default `18080`). If `index.port` is set it takes precedence.

## Service-directory index (unmatched hosts)

A request whose host matches **no** app router (e.g. opening the raw tailnet IP
`http://100.86.26.45` or the Traefik dashboard host directly) is served a
generated `index.html` instead of a 404. The page lists every running service
and the port it is reachable on, with click-to-copy links. Traefik routes
unmatched hosts to it via a low-priority catch-all router
(`Host(\`*\`)`, `priority = 1`), so specific app routers always win.

- fog runs a standalone index server (`fog index serve`) on loopback
  `index_port` (default `18080`), detached so it survives individual fog
  instances exiting.
- The file is regenerated when instances start and stop (bounded startup
  refreshes + a teardown refresh); refresh the browser to pick up changes.

> **Phone on tailnet:** open `http://<tailnet IP>` (e.g. `http://100.86.26.45`)
> on a phone on the same tailnet — no wildcard DNS needed on the phone. The
> directory is the catch-all, and service links are `http://<tailnet IP>:<port>`
> (raw exposed port), so tapping opens the service directly. `*.acme`
> hostnames resolve only on the laptop; the phone must use the IP:port links. See [Agentic guide — Phone on the tailnet](/agentic#phone-on-the-tailnet).

## Web UI & JSON API

The index server also serves the **fog web UI** — a React SPA (see `ui/`) that
renders the service directory, live logs, scripts, health and status. It exposes
a small JSON API consumed by the SPA:

- `GET /api/services` — live service directory
- `GET /api/status` — running fog instances
- `GET /api/scripts` — configured scripts
- `GET /api/config` — loaded configuration
- `GET /api/health` — per-service health
- `GET /api/launch/targets` — launchable projects/worktrees/scripts (see below)
- `POST /api/launch` — start a new detached fog instance (see below)
- `POST /api/instances/{pid}/services/{name}/action` — start/stop/restart a service of a running instance (see below)
- `/logs/stream` — SSE stream of a service's logs

Build the SPA with `cd ui && pnpm install && pnpm build`; `build.rs` embeds
`ui/dist/` into the binary at compile time. Without a build the server falls
back to the generated directory page above.

### Service controls

The `/status` page shows **Start**, **Stop** and **Restart** buttons on each
service row — the services of a running fog instance, as listed by
`GET /api/status` → `instances[].services[]`. The buttons call
`POST /api/instances/{pid}/services/{name}/action` with a JSON body:

```json
{ "action": "start" | "stop" | "restart" }
```

The index server forwards the request to the running fog instance over its IPC
Unix socket (localhost only) and responds `{"ok": true}` on success, or
`{"ok": false, "reason": "..."}` on failure; a pid that is not a running fog
instance yields `404`.

> **Localhost only** — the index server listens on `127.0.0.1` only, so the
> write path is available to local processes only. Any local process could
> issue these actions, so do not expose the index port (`18080`) externally.

### Start instances

The `/status` page includes a **Start instance** card with two modes:

1. **Known project** — pick a project, then a branch/worktree, then one of
   the scripts that worktree's `fog.json` defines, and press **Start**.
2. **New project** — enter an absolute config directory path and a script
   name (optionally a branch), and press **Start**.

Either mode spawns a new **detached** fog instance. Launching on a *different*
branch uses the existing [concurrent/share/reuse machinery](/configuration#concurrent-mode--sharing-services) —
different branches always run concurrently, so the freshly launched branch
instance runs side by side with any already-running ones.

The UI is backed by two endpoints:

- `GET /api/launch/targets` — returns
  `{"projects":[{"path","name","worktrees":[{"path","branch","scripts":[...]}]}]}`,
  enumerating launchable projects grouped by git repository: all worktrees/
  branches of the same repo (discovered from running fog instances' config dirs
  plus docker compose `working_dir` labels) appear under a single project named
  after the repo, each with its git worktrees and the scripts each worktree's
  `fog.json` defines.
- `POST /api/launch` — body
  `{"config_dir":"/abs/path","script":"dev","branch":"feature-x"|null}`; spawns
  a detached fog daemon and waits until its IPC socket serves status, replying
  `{"ok":true,"pid":1234}` on success or `400`/`500 {"error":"..."}` on failure.

Like the service-action endpoint, this is a **localhost-only write path** — the
index server binds to `127.0.0.1` only, so any local process could start fog
instances. Do not expose the index port (`18080`) externally.
