---
title: Index Server
---

# Index Server

Standalone service-directory **index server** (web UI + JSON API). By default every `fog <script>` serves the index unless opted out. This lives in the **fog config** (top-level `fog.json` alongside `theme`/`sidebar`, default `enabled:true`) — not per-script. It can be set **per-project** (`./fog.json`) or **globally** (`~/.config/fog/fog.json`, same schema); either can opt-out (both must be `true` to serve). The server is host-global (like `router`/`dnsmasq`) and is torn down automatically when the last fog instance exits — see `fog index kill/restart`.

> **No Docker required.** The index server runs without Docker: it binds
> `127.0.0.1:<port>` and serves the web UI plus every IPC-backed endpoint
> (`/api/status`, `/api/health`, `/api/config`, `/api/scripts`, native services
> in `/api/services`, fog-instance logs over SSE, and the launch/service
> actions). Docker-backed content is simply absent when Docker is unavailable:
> compose service discovery in `/api/services`, container log streaming,
> compose-derived launch targets, and the Traefik catch-all route to the raw
> index port. Docker is still required for the Traefik router itself.

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
- `GET /api/projects/{name}/icon` — a project's configured icon (see below)
- `POST /api/launch` — start a new detached fog instance (see below)
- `POST /api/instances/{pid}/services/{name}/action` — start/stop/restart a service of a running instance (see below)
- `/logs/stream` — SSE stream of a service's logs

Each service may also carry an optional `endpoints` array on
`GET /api/services` — the declared [`endpoint`](/configuration#endpoints)
entries of that service, each with its own `name`, `url`, `port` and
`health`. It is omitted when the service exposes a single implicit endpoint.
Matching `/api/status` services report the same names with their live health
under `services[].endpoints[]`. The UI nests endpoints under their parent
service row on the project/branch and status pages.

Each service also carries an optional `icon` field on `GET /api/services`, taken
from the owning project's `fog.json` (`project.icon` — see
[Project metadata](/configuration#project-metadata)). For a URL/data URI it is
the value verbatim; for a filesystem path it is `/api/projects/{name}/icon`,
which the index server resolves against that project's config dir and serves
with the matching image content-type (URL/data icons `302`-redirect). The SPA
uses it as the project card image on the project list, falling back to the
default glyph on any error. Only files named in `project.icon` are read; the
request path selects a project, never a file.

Build the SPA with `cd ui && bun install && bun run build`; `build.rs` embeds
`ui/dist/` into the binary at compile time. Without a build the server falls
back to the generated directory page above.

### Service controls

Start, **Stop** and **Restart** buttons appear on each service row of:

- the `/status` page — the services of a running fog instance, as listed by
  `GET /api/status` → `instances[].services[]`;
- the project → branch pages (`/projects/{project}/{branch}`) — the branch's
  services across its instances, plus the per-service logs/terminal drawer.

The branch views are driven by `GET /api/status` (so **stopped** services are
still listed and can be restarted) and enriched with `GET /api/services` for the
URL/ports/container of running services. When several scripts (or several
instances of the same script) serve one branch, the branch list splits them per
instance, and each instance has a **Kill** button. The branch script page
carries a **Kill** in its header for the instance it shows — one per instance
when several are listed.

The buttons call
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

The `/status` page includes a **Start instance** card:

1. Pick a project, then a branch/worktree, then one of the scripts that
   worktree's `fog.json` defines, and press **Start**. Every picker is
   searchable: click it and type to narrow the list, which matches a project or
   worktree by name *and* path.
2. A folder fog has never seen is one **Browse** away: the button beside the
   project picker walks the filesystem (`GET /api/fs/list` marks the folders that
   are git repos and the ones holding a `fog.json`), and picking one adds it to
   the project list and selects it, ready for a worktree and script. Folders
   already in the list show an `added` chip and cannot be picked again.

**Browse** only lets you pick a folder that holds a `fog.json` — a worktree of a
known repo included, which adds that worktree as its own entry. Its field
completes as you type: a folder *name* (`red`) filters the open folder, while a
*path* (`~/code/r`, `/Users/me/c`) lists the folders in its parent that match the
last segment, ones starting with it first. Picking a folder keeps its path in the
field with a trailing `/`, ready for the next name. Every config dir a `fog` run
is started from — and every browsed folder — is remembered in
`$XDG_STATE_HOME/fog/projects.json` (`GET`/`POST`/`DELETE /api/launch/projects`),
so it stays in the project picker after its instance stops, even when nothing is
running to discover it from. A wrong entry is dropped with
`DELETE /api/launch/projects?path=/abs/dir`.

The same form opens from the services index (`/`) header **Start instance**
button — a centered dialog on desktop, a bottom sheet on mobile — and closes
once the instance has started.

A branch script page (`/projects/{project}/{branch}/{script}`) has the same
button in its header, and in its "no running services" card. There the form is
scoped to that page's project: the project picker and **Browse** are gone, its
worktree matching the page's branch and the page's script are preselected, so
starting a second instance of the script showing is Start and nothing else.
The project page and a branch's script list (`/projects/{project}`,
`/projects/{project}/{branch}`) carry the same header button, scoped to that
project and branch, so only the rest of the form is left to fill.

A launch always spawns a new **detached** fog instance. Launching on a
*different* branch uses the existing [concurrent/share/reuse machinery](/configuration#concurrent-mode--sharing-services) —
different branches always run concurrently, so the freshly launched branch
instance runs side by side with any already-running ones.

The UI is backed by these endpoints (`GET /api/launch/inspect?path=/abs/dir`
still reports one dir's scripts, branch and worktrees for scripts and tools):

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
- `GET /api/fs/list?dir=/abs/path` — one folder's subdirectories for the browse
  picker: `{"path","parent","fog","entries":[{"name","path","git","fog"}]}`;
  `dir` may start with `~` and defaults to the home directory.
- `GET|POST|DELETE /api/launch/projects` — the remembered config dirs. `POST`
  takes `{"path":"/abs/path"}` (400 without a `fog.json`), `DELETE` takes
  `?path=`, and the registry file lives under the fog state dir.

Like the service-action endpoint, this is a **localhost-only write path** — the
index server binds to `127.0.0.1` only, so any local process could start fog
instances. Do not expose the index port (`18080`) externally.
