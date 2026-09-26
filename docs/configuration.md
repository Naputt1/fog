---
title: Configuration
---

# Configuration Reference

fog is configured via a JSON file (default: `fog.json`). Use the `-c` / `--config` flag to specify a different path.

See [`fog.schema.json`](https://github.com/Naputt1/fog/blob/main/fog.schema.json) for the formal JSON Schema.

## Top-level structure

```json
{
  "$schema": "https://raw.githubusercontent.com/Naputt1/fog/main/fog.schema.json",
  "scripts": {
    "dev": {
      "service": [...],
      "proxy": {...}
    }
  },
  "max_scrollback": 2000,
  "sidebar": {...},
  "theme": {...}
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `scripts` | `object` | — | Named profiles; each defines its own services and proxy |
| `ports` | `object` | — | Allocatable ports for templating (`${ports.<name>}`); `0` = random free port. Override per run with `--port <name>=<port>` |
| `native_routes` | `array` | — | Explicit Traefik file-provider routes for native services |
| `max_scrollback` | `integer` | `2000` | Maximum scrollback lines per terminal (min: 100) |
| `sidebar` | `object` | `null` | Sidebar width constraints |
| `theme` | `object` | `null` | Color theme overrides (see [Themes](/themes)) |
| `project` | `object` | `null` | UI metadata for this project (e.g. `icon`) shown in the index web UI |
| `index` | `object` | `{ enabled: true }` | Standalone index server (service directory + web UI); set `enabled:false` to opt this project out. See [Index Server](/index-server) |
| `router` | `object` | `null` | Central Traefik router (host-global). See [Router & DNS](/router) |
| `dnsmasq` | `object` | `null` | Wildcard DNS setup. See [Router & DNS](/router) |

Run-time port overrides: `fog <script> --port <name>=<port>` (repeatable) replaces an allocated port for one run without editing `fog.json`; `--port <name>=0` re-randomizes it. A name not declared in `ports` is added for that run. Overrides apply before templating, so every `${ports.<name>}` consumer — service `cmd`/`env`/health checks, endpoint routes and native routes — resolves to the overridden value.

## Scripts

A script bundles a set of services and an optional proxy under a name. Run it with `fog <name>` (e.g. `fog dev`). Each script is fully self-contained — services shared between scripts (like a database) must be defined in each script that needs them.

```json
{
  "scripts": {
    "infra": {
      "service": [
        { "name": "db", "path": ".", "cmd": "docker compose up -d" }
      ]
    },
    "dev": {
      "service": [
        { "name": "db", "path": ".", "cmd": "docker compose up -d" },
        { "name": "api", "path": "backend", "cmd": "cargo run" }
      ],
      "proxy": {
        "port": 3000,
        "routes": [{ "path": "/api", "upstream": "http://localhost:8080" }]
      }
    }
  }
}
```

### Script fields

| Field | Required | Type | Default | Description |
|-------|----------|------|---------|-------------|
| `service` | No | `array` | — | List of service entries to manage |
| `proxy` | No | `object` | — | Reverse proxy configuration (see below) |
| `terminal` | No | `object` | — | Terminal WebSocket gateway hardening (auth, per-IP/size limits; see [Terminal protocol](/terminal-protocol)) |
| `concurrent` | No | `boolean` | `true` | Allow multiple concurrent instances of this script in the same project+branch (see [Concurrent mode](#concurrent-mode--sharing-services)) |

`fog` requires a script name; running `fog` with no arguments lists the available scripts.

## Service entries

```json
{
  "scripts": {
    "dev": {
      "service": [
        {
          "name": "backend",
          "path": "/path/to/project",
          "cmd": "npm run dev",
          "health_check": {
            "kind": "tcp",
            "target": "localhost:3000",
            "interval_ms": 5000,
            "timeout_ms": 2000
          }
        }
      ]
    }
  }
}
```

### Fields

| Field | Required | Type | Description |
|-------|----------|------|-------------|
| `name` | No | `string` | Display name for the tab. Defaults to the directory name. |
| `path` | **Yes** | `string` | Working directory for the command (relative to the config file). |
| `cmd` | **Yes** | `string` | Shell command to execute (e.g. `cargo run`, `npm run dev`, `air`). |
| `health_check` | No | `object` | Service-level health check. Shorthand/aggregate — prefer per-[endpoint](#endpoints) checks. |
| `endpoint` | No | `array` | Endpoints this service exposes (see [Endpoints](#endpoints)). Omit for a single implicit endpoint. |
| `shutdown_cmd` | No | `string` | Shell command to run on shutdown (e.g. `docker compose down`). |
| `reuse` | No | `boolean` | Reuse this service across worktree switches (only honored in single-instance mode, see below). |
| `share` | No | `boolean` | Share this service between multiple concurrent instances (only honored in concurrent mode, see below). |

The command is executed inside a shell (`$SHELL` or `bash`) using `cd <path> && <cmd>`.

## Endpoints

A `service` entry is the process fog runs; an **endpoint** is one thing that process actually exposes. Most services expose a single endpoint, so `endpoint` is optional and omitting it preserves the existing behavior (the service itself is the endpoint). A stack that exposes several — e.g. `docker compose up` bringing up `web`, `api` and `db` — declares one entry per exposed endpoint.

```json
{
  "name": "infra",
  "path": "./infra",
  "cmd": "docker compose up -d",
  "shutdown_cmd": "docker compose down",
  "endpoint": [
    { "name": "web", "host": "web.${branch}.acme", "port": "${ports.web}",
      "health_check": { "kind": "tcp", "target": "localhost:${ports.web}" } },
    { "name": "api", "host": "api.${branch}.acme", "port": "${ports.api}",
      "path_prefix": "/v1",
      "health_check": { "kind": "tcp", "target": "localhost:${ports.api}" } },
    { "name": "db", "host": "${branch}.db.acme", "port": "${ports.db}" }
  ]
}
```

| Field | Required | Type | Description |
|-------|----------|------|-------------|
| `name` | **Yes** | `string` | Display name for the endpoint (e.g. the compose service name). Unique within the owning service. |
| `host` | No | `string` | Host rule to route to this endpoint (e.g. `"web.${branch}.acme"`). When omitted, no route is generated (display + health only). |
| `port` | No | `string` | Host-published port template (`"${ports.web}"`) or literal port. Required for a generated route. |
| `path_prefix` | No | `string` | Optional PathPrefix to combine with `host`. |
| `health_check` | No | `object` \| `array` | Health check for this endpoint (same shape as the service-level one). This is the preferred place for health checks. |

Behavior:

- **Health is per endpoint.** Each endpoint with a `health_check` is probed on its own adaptive cadence and reported separately. The service's own health (the tab indicator, `depends_on` gating, `share`/`reuse` probing) is the **AND of all endpoints' checks plus any service-level `health_check`** — so a compose stack shows per-endpoint health while still gating its dependents on the whole stack. Because of this, a service-level `health_check` is only needed as a shorthand for a single-endpoint service or for a check that is not tied to an endpoint.
- **Routing.** Entries with both `host` and a resolvable `port` get a generated Traefik route (`Host(<host>)` → `http://host.docker.internal:<port>`) written as a file-provider config, exactly like [`native_routes`](/router#native-routes). Routes are named `ep-<branch>-<service>-<name>-<host>` and removed on shutdown. Because the target is a host-published port, `port` must be a port the process (or a compose service) publishes to the host.
- **Display.** The index web UI nests each endpoint under its parent service with its own health badge and link. If a docker-discovered container's name matches a declared endpoint name, it is nested under the parent instead of listed separately, and inherits that container's routed URL/published ports.
- **No per-endpoint controls.** Endpoints are display/routing/health only: `start`/`stop`/`restart` still act on the parent service.

Endpoint `host`/`port`/`path_prefix`/`health_check.target` support the same `${ports.*}`, `${branch}` and `${branch_raw}` templates as the rest of the config.

## Worktree-aware runs & service reuse

> `reuse` below applies only when the script sets `"concurrent": false` (single-instance mode). Scripts default to `concurrent: true` — see [Concurrent mode & sharing services](#concurrent-mode--sharing-services).

fog identifies the git repository a script runs in via `git rev-parse --git-common-dir`, which is shared by every worktree of the same repo. The instance identity is `(project, script, branch)`: two worktrees on **different branches** run concurrently, while starting the **same script on the same branch** while another instance is already running (e.g. a human plus an agent) makes fog:

1. Acquire a per-(project, script, branch) owner lock (`flock`, in the temp directory) so concurrent startups are decided deterministically.
2. Ask the old instance (on the same branch) to shut down — killing its non-reused services and tearing down its proxy — and **wait until it has fully exited** (socket gone) before starting its own services, so there is no port conflict during the switch.
3. Start its own services.

Instances on a **different branch** are never reclaimed, so `fog dev --branch feature-x` and `fog dev --branch main` can run side by side.

If another `fog <script>` is **mid-start** for the same project+branch when you launch one (two worktrees starting at the same moment, or a human racing an agent), fog waits up to 30s for it to finish. If that instance started after you did, fog backs off with an error message rather than fighting over ports — use `fog kill <pid>` to replace it.

Services flagged with `"reuse": true` are treated specially to save time when switching worktrees:
- **Handover**: the old instance's `shutdown_cmd` is skipped for that service (e.g. no `docker compose down`), and if a live process is running its PTY output is piped into the new instance's tab.
- **Probe-first**: at startup fog probes the resource once via `health_check`. If it is already reachable, the service's `cmd` is **not** run and the tab shows a `♻ reusing already-running ...` notice instead. If it is **not** reachable, fog runs the `cmd` immediately — no misleading "reusing" tab, no delay.
- **Mid-session fallback**: if a borrowed service later becomes unreachable (e.g. the handed-over process died), fog starts the `cmd` itself after a short grace period (~10s).
- **Take over**: pressing `R` on a reused tab kills the borrowed process and starts the `cmd` fresh in this worktree.
- **Persistence**: reused resources survive only as long as a live successor takes them over (handover in a reclaim/worktree switch). A shared resource is torn down only when the **last** instance serving that (project, script, branch) exits. When the very last instance on the branch exits — via `q`, Ctrl+C, or `fog kill <pid>` — with no successor, fog kills the borrowed process (if any) and runs its `shutdown_cmd`. A sibling on a **different branch** never keeps it alive: shared resources are branch-scoped (e.g. a compose project named `red-fox-infra-${FOG_BRANCH}` gives every branch its own containers).

```json
{
  "name": "db",
  "path": ".",
  "cmd": "docker compose up -d",
  "shutdown_cmd": "docker compose down",
  "reuse": true,
  "health_check": [
    { "kind": "tcp", "target": "localhost:5432" }
  ]
}
```

> `reuse` works best **with** a `health_check` — fog needs it to verify the resource is already up. If `reuse` is set without one, fog warns and starts the `cmd` normally.

### Concurrent mode & sharing services

By default a script is **concurrent** (`"concurrent": true`): running `fog <script>` again in the same project+branch **starts alongside** the existing instances instead of killing them. `fog ls` lists every instance (pass a PID to `fog kill`/`fog logs` to target a specific one). This is handy when a human and an agent — or two agents — want to work against the same environment at once.

Per-instance services (everything not flagged `share`) are started fresh in each instance and torn down when that instance exits, so instances never collide on ports unless the config says otherwise.

Services flagged `"share": true` are **shared between all concurrent instances** of the project+script:

- **Probe-first**: at startup fog probes the resource once via `health_check`. If it is already reachable (a sibling instance started it), the service's `cmd` is **not** re-run and the tab shows a `♻ reusing already-running ...` notice instead — so a database or compose stack is not duplicated.
- **Start when down**: if it is not reachable, fog runs the `cmd` (the first instance to start becomes the owner). No misleading "reusing" tab, no delay.
- **Mid-session fallback**: if a borrowed shared service later becomes unreachable, fog starts the `cmd` itself after a short grace period (~10s).
- **Last-instance teardown**: a shared resource is torn down (`shutdown_cmd`, e.g. `docker compose down`) only when the **last** instance serving that (project, script, **branch**) exits. A same-branch sibling instance keeps it alive; an instance on another branch does not, since each branch owns its own (branch-suffixed) resources.
- **Port adoption**: when this instance borrows a shared resource a sibling already started, it adopts that sibling's live ports for the port names the shared service references (`cmd`, `shutdown_cmd`, `env`, health checks and endpoint ports), so dependent services resolve `${ports.*}` to the running container instead of a freshly allocated port. Adoption is **independent of the transient health probe**: the ports of a shared resource belong to whichever sibling started it, so a racing instance cannot end up pointing its dependents at its own port while the real container listens elsewhere. A shared service templated with `${ports.*}` is only borrowed when its ports were adopted this way; if no sibling owns it, fog starts it (on its own ports) instead of borrowing a resource it cannot address.

```json
{
  "name": "db",
  "path": ".",
  "cmd": "docker compose up -d",
  "shutdown_cmd": "docker compose down",
  "share": true,
  "health_check": [
    { "kind": "tcp", "target": "localhost:5432" }
  ]
}
```

> Like `reuse`, `share` works best **with** a `health_check` — fog needs it to verify the resource is already up. If `share` is set without one, fog warns and starts the `cmd` normally (concurrent instances could then race to start the same resource).

> `reuse` and `share` are mutually exclusive across modes: `reuse` is only honored when `"concurrent": false` (single-instance), and `share` is only honored when `"concurrent": true`. Set `"concurrent": false` to restore the old behavior where a re-run kills the previous instance first (handing over `reuse` services).

### `FOG_BRANCH` environment variable

When fog runs a script in a git worktree (via `--branch` or an in-place worktree switch), it injects `FOG_BRANCH` (DNS-safe **slug**, `/` → `-`, lowercased, `feat/book` → `feat-book`) and `FOG_BRANCH_RAW` (original `feat/book`) into every service process, plus `FOG_BRANCH_SLUG` as an alias for the slug. This lets a compose file derive per-branch names — compose project, container hostnames, and host ports — so multiple branches of the same repo never collide. Templates `${branch}` / `${FOG_BRANCH}` resolve to the slug; `${branch_raw}` / `${FOG_BRANCH_RAW}` resolve to the raw name:

```json
{
  "name": "api",
  "path": ".",
  "cmd": "docker compose -p redfox-${FOG_BRANCH:-main} up -d"
}
```

A branch like `feat/book` sanitizes to `feat-book` (`feat/book.acme` would be an invalid DNS hostname). Use `${branch_raw}` only for display/logging — never for `Host()` or DNS. If the slug would be empty or exceed 63 chars (DNS label limit) fog errors.

When the script is not in a git worktree (or the worktree is detached), `FOG_BRANCH` is unset; use `:-` fallbacks (e.g. `${FOG_BRANCH:-main}`) in compose files to keep them usable outside fog.

### Health check

Once configured, a background thread periodically checks the service health and updates the sidebar indicator.

| Field | Required | Type | Default | Description |
|-------|----------|------|---------|-------------|
| `kind` | **Yes** | `string` | — | `"tcp"`, `"http"`, or `"docker"` |
| `target` | **Yes** | `string` | — | Address to check (e.g. `"localhost:8080"`), or the compose service name for `"docker"` |
| `compose_file` | No | `string` | `"docker-compose.yml"` | Compose file used by the `"docker"` kind, relative to the service `path` |
| `interval_ms` | No | `integer` | `5000` | Steady-state check interval in milliseconds, used once the service is healthy (min: 100) |
| `timeout_ms` | No | `integer` | `2000` | Connection/subprocess timeout in milliseconds (min: 100) |
| `start_interval_ms` | No | `integer` | `500` | Fast check interval in milliseconds until the first successful check, then `interval_ms` takes over (min: 100) |
| `start_period_ms` | No | `integer` | `0` | Grace window after the service starts during which failed checks report `starting` instead of `unhealthy` |
| `retries` | No | `integer` | `3` | Consecutive failed checks before the service is reported `unhealthy` (min: 1) |

A service with no configured health check is considered ready as soon as its process is running; a service **with** a health check gates its dependents (`depends_on`) until the check passes.

Health checking is **adaptive**: fog probes once immediately, then polls every `start_interval_ms` while the service is starting, and switches to the slower `interval_ms` once it is healthy. This mirrors Docker Compose's `start_interval`/`start_period` and keeps `depends_on` startup fast without hammering a healthy service. Failed checks are damped by `retries`, and failures inside `start_period_ms` show as `starting` rather than `unhealthy`.

Both `"tcp"` and `"http"` health checks currently probe via a TCP connection to the target address (the `target` may be prefixed with `tcp://`, `http://`, or `https://` — the prefix is stripped before connecting). `http` is reserved for a future HTTP-status check; today it behaves like `tcp`.

The `"docker"` kind checks the actual container from the service's compose file instead of a port: it runs `docker compose -f <compose_file> ps --format json <service>` and passes when the service is **running** and, when the compose file defines a healthcheck for it, reports **healthy**. This is more robust than a TCP probe (e.g. it fails when the container is up on a recycled port but actually unhealthy), and pairs well with `"reuse": true` services.

```json
{
  "name": "infra",
  "path": "./infra",
  "cmd": "docker compose up -d",
  "shutdown_cmd": "docker compose down",
  "reuse": true,
  "health_check": {
    "kind": "docker",
    "target": "postgres",
    "compose_file": "docker-compose.yml"
  }
}
```

## Proxy configuration

```json
{
  "scripts": {
    "dev": {
      "service": [],
      "proxy": {
        "port": 3000,
        "host": "0.0.0.0",
        "routes": [
          {
            "path": "/api",
            "host": "api.local",
            "upstream": "http://localhost:8080",
            "ws": false
          }
        ],
        "tls_cert": "/path/to/cert.pem",
        "tls_key": "/path/to/key.pem",
        "max_log_entries": 1000
      }
    }
  }
}
```

| Field | Required | Type | Default | Description |
|-------|----------|------|---------|-------------|
| `port` | **Yes** | `integer` | — | Port to listen on (1–65535) |
| `host` | No | `string` | `"0.0.0.0"` | Host address to bind to |
| `routes` | **Yes** | `array` | — | List of route definitions |
| `tls_cert` | No | `string` | `null` | Path to PEM-encoded TLS certificate |
| `tls_key` | No | `string` | `null` | Path to PEM-encoded PKCS8 private key |
| `max_log_entries` | No | `integer` | `1000` | Maximum request log entries to retain |

### Route fields

| Field | Required | Type | Default | Description |
|-------|----------|------|---------|-------------|
| `path` | **Yes** | `string` | — | Incoming path prefix or wildcard pattern |
| `host` | No | `string` | `null` | Host header pattern for virtual hosting |
| `upstream` | **Yes** | `string` | — | Upstream URL to forward requests to |
| `ws` | No | `boolean` | `false` | Enable WebSocket proxying for this route |

For details on route matching, see the [Proxy docs](/proxy).

## Sidebar

```json
{
  "sidebar": {
    "min_width": 12,
    "max_width": 30
  }
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `min_width` | `integer` | `12` | Minimum sidebar width in columns (8–50) |
| `max_width` | `integer` | `30` | Maximum sidebar width in columns (8–50) |

The sidebar width is computed dynamically: `max(name_length + 5, min_width)` clamped to `max_width`.

## Project metadata

Optional UI metadata for this project, shown in the web UI.

```json
{
  "project": {
    "icon": "frontend/public/logo.svg"
  }
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `project.icon` | `string` | `null` | Project icon shown on the index project list page. Accepts an `http(s)://` URL, a `data:image/…` URI, or a filesystem path. |

`project.icon` accepts three forms:

- **URL** (`http://` / `https://`) — used verbatim.
- **Data URI** (`data:image/…`) — used verbatim.
- **Filesystem path** — relative paths resolve against this `fog.json`'s directory; absolute paths and `~/` are allowed. Supported extensions: `svg`, `png`, `jpg`, `jpeg`, `gif`, `webp`, `avif`, `ico` (max 2 MiB). The [index server](/index-server) serves the file at `/api/projects/{name}/icon`.

Anything unset, unreadable, or of an unsupported type falls back to the default glyph. The icon is read from each project's own `fog.json`, so the index server can show a distinct icon per project in the grouped project list.

> **Note:** The index server serves only files named in `project.icon` — the request path selects a project, never a file. Because the index is reachable beyond loopback (the Traefik catch-all on the tailnet), avoid pointing `project.icon` at sensitive files.

## Host-global services

The standalone [Index Server](/index-server) (web UI + API, `index` field) and the central [Router & DNS](/router) (`router` + `dnsmasq`) are **host-global** — started once and reused across projects and branches. They outlive any single `fog <script>` instance.

- **Index server** — directory of running services at `http://<tailnet IP>` + React SPA on `127.0.0.1:18080`. Opt out per-project with `index: { enabled: false }`. Details: [Index Server](/index-server).
- **Router & DNS** — wildcard `*.acme` → `127.0.0.1` via `dnsmasq` and Traefik on `:80`/`:443` with auto-discovery. TLS via `mkcert`. Details: [Router & DNS](/router).

Both are idempotent and tear down automatically (index) or manually (`docker rm -f fog-router-traefik`).

## Theme

```json
{
  "theme": {
    "proxy": "cyan",
    "terminal": "green",
    "stopped": "red",
    "highlight": "magenta",
    "status_200": "green",
    "status_300": "yellow",
    "status_400": "red",
    "status_500": "red",
    "scrollbar": "cyan"
  }
}
```

See the [Themes docs](/themes) for available colors and customization examples.

## Complete example

```json
{
  "$schema": "https://raw.githubusercontent.com/Naputt1/fog/main/fog.schema.json",
  "scripts": {
    "dev": {
      "service": [
        {
          "name": "api",
          "path": "backend",
          "cmd": "cargo run",
          "health_check": {
            "kind": "tcp",
            "target": "localhost:8080",
            "interval_ms": 3000
          }
        },
        {
          "path": "frontend",
          "cmd": "npm run dev"
        }
      ],
      "proxy": {
        "port": 3000,
        "routes": [
          {
            "path": "/api",
            "upstream": "http://localhost:8080"
          },
          {
            "path": "/",
            "upstream": "http://localhost:5173",
            "ws": true
          }
        ]
      }
    }
  },
  "max_scrollback": 5000,
  "sidebar": {
    "min_width": 14,
    "max_width": 28
  },
  "theme": {
    "proxy": "#00bcd4",
    "terminal": "#4caf50"
  }
}
```

## Hot-reloading

fog watches the config file for changes at runtime. When a change is detected, it:

1. Reloads the color theme immediately
2. If proxy settings changed (port, host, routes), restarts the proxy

Service entries are **not** hot-reloaded — add/remove services requires a restart of fog.
