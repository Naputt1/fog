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
| `max_scrollback` | `integer` | `2000` | Maximum scrollback lines per terminal (min: 100) |
| `sidebar` | `object` | `null` | Sidebar width constraints |
| `theme` | `object` | `null` | Color theme overrides |
| `index` | `object` | `{ enabled: true }` | Standalone index server (service directory + web UI); set `enabled:false` to opt this project out of serving the index |
| `router` | `object` | `null` | Central Traefik router (host-global) |
| `dnsmasq` | `object` | `null` | Wildcard DNS setup |

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
| `health_check` | No | `object` | Health check configuration (see below). |
| `shutdown_cmd` | No | `string` | Shell command to run on shutdown (e.g. `docker compose down`). |
| `reuse` | No | `boolean` | Reuse this service across worktree switches (only honored in single-instance mode, see below). |
| `share` | No | `boolean` | Share this service between multiple concurrent instances (only honored in concurrent mode, see below). |

The command is executed inside a shell (`$SHELL` or `bash`) using `cd <path> && <cmd>`.

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
- **Persistence**: reused resources survive only as long as a live successor takes them over (handover in a reclaim/worktree switch). With concurrent branches, a shared resource is torn down only when the **last** instance serving that (project, script) exits — a sibling branch's `fog dev` keeps it alive. When the very last instance exits — via `q`, Ctrl+C, or `fog kill <pid>` — with no successor, fog kills the borrowed process (if any) and runs its `shutdown_cmd`.

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
- **Last-instance teardown**: a shared resource is torn down (`shutdown_cmd`, e.g. `docker compose down`) only when the **last** instance serving that (project, script) exits — a sibling instance or a different branch's `fog dev` keeps it alive.

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

When fog runs a script in a git worktree (via `--branch` or an in-place worktree switch), it injects a `FOG_BRANCH` environment variable naming the checked-out branch into every service process. This lets a compose file derive per-branch names — compose project, container hostnames, and host ports — so multiple branches of the same repo never collide:

```json
{
  "name": "api",
  "path": ".",
  "cmd": "docker compose -p redfox-${FOG_BRANCH:-main} up -d"
}
```

When the script is not in a git worktree (or the worktree is detached), `FOG_BRANCH` is unset; use `:-` fallbacks (e.g. `${FOG_BRANCH:-main}`) in compose files to keep them usable outside fog.

### Health check

Once configured, a background thread periodically checks the service health and updates the sidebar indicator.

| Field | Required | Type | Default | Description |
|-------|----------|------|---------|-------------|
| `kind` | **Yes** | `string` | — | `"tcp"`, `"http"`, or `"docker"` |
| `target` | **Yes** | `string` | — | Address to check (e.g. `"localhost:8080"`), or the compose service name for `"docker"` |
| `compose_file` | No | `string` | `"docker-compose.yml"` | Compose file used by the `"docker"` kind, relative to the service `path` |
| `interval_ms` | No | `integer` | `5000` | Check interval in milliseconds (min: 100) |
| `timeout_ms` | No | `integer` | `2000` | Connection/subprocess timeout in milliseconds (min: 100) |

Both `"tcp"` and `"http"` health checks work the same way: they attempt a TCP connection to the target address. The `target` field can be prefixed with `tcp://`, `http://`, or `https://` — the prefix is stripped before connecting.

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

## index

Standalone service-directory index server (web UI + JSON API). By default every `fog <script>` serves the index unless opted out. This lives in the **fog config** (top-level `fog.json` alongside `theme`/`sidebar`, default `enabled:true`) — not per-script. It can be set **per-project** (`./fog.json`) or **globally** (`~/.config/fog/fog.json`, same schema); either can opt-out (both must be `true` to serve). The server is host-global (like `router`/`dnsmasq`) and is torn down automatically when the last fog instance exits — see `fog index kill/restart`.

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

## dnsmasq

Optional wildcard-DNS setup applied automatically on startup. Each domain is
mapped so any `*.<domain>` hostname resolves to `address` — handy for per-branch
dev URLs like `main.acme` or `feature-x.acme`.

```json
{
  "dnsmasq": {
    "domains": ["acme"],
    "address": "127.0.0.1",
    "port": 53
  }
}
```

| Field | Required | Type | Default | Description |
|-------|----------|------|---------|-------------|
| `domains` | **Yes** | `array` | — | Domains to wildcard-map (e.g. `["acme"]` → `*.acme`) |
| `address` | No | `string` | `"127.0.0.1"` | Address that `*.<domain>` resolves to |
| `port` | No | `integer` | `53` | Port dnsmasq listens on. On macOS the daemon runs as a root LaunchDaemon so it can bind this (privileged) port |

When `fog <script>` starts and a `dnsmasq` section is configured, fog:

1. Verifies `dnsmasq` is installed; if not, it **warns and continues** (install it with `brew install dnsmasq`).
2. On **macOS** (Homebrew): writes `address=/.<domain>/<address>` into
   `$prefix/etc/dnsmasq.d/fog-<domain>.conf`, pins the listener to
   `address:port` via `fog-port.conf` (`port`, `listen-address`,
   `bind-interfaces`), ensures `conf-dir` is enabled in `dnsmasq.conf`, creates
   `/etc/resolver/<domain>` (via `sudo`) with a plain `nameserver <address>`
   line, then **starts** dnsmasq as a **root LaunchDaemon** via
   `sudo brew services start dnsmasq` (which registers
   `/Library/LaunchDaemons/homebrew.mxcl.dnsmasq.plist` and survives reboots).
   Any stale user-level `homebrew.mxcl.dnsmasq` LaunchAgent is booted out first.
3. On **Linux**: writes `/etc/dnsmasq.d/fog-<domain>.conf` and `fog-port.conf`
   (via `sudo`) and starts dnsmasq via `sudo systemctl start dnsmasq`.
4. On other platforms it warns that the setup is unsupported.

The setup is **idempotent**: existing files are left untouched and dnsmasq is
only restarted when something changed — and if the daemon is already running it
is left alone. If dnsmasq is **not** running, fog starts it automatically (the
"fog starts the DNS too" behavior), and verifies it actually came up on
`address:port`. Detached (`-d`) runs use `sudo -n` so a password prompt cannot
hang them; if a privileged step is needed but cannot run headless, fog prints a
warning telling you to run `fog <script>` interactively once. Any failure is a
warning, never a hard error.

> **Why root?** macOS `26`+ restricts binding to privileged ports (<1024) to
> root, and macOS renders (*but ignores*) the `port` directive in
> `/etc/resolver/<domain>` files. dnsmasq therefore must listen on the standard
> :53 as a **root LaunchDaemon** — running `brew services` under `sudo`
> installs exactly that. fog only ever binds it to `127.0.0.1`
> (`bind-interfaces`), so the daemon is not exposed to the LAN.

## router

Optional **central reverse proxy** (Traefik) setup applied automatically on
startup, sharing dnsmasq's philosophy: the router is a host-global resource that
fog starts **once** and every project/branch reuses, so no app runs its own
speculative instance (which would collide on the published `:80` port).

Apps opt into routing by attaching a service to the shared network and
declaring standard Traefik container labels:

```json
{
  "router": {
    "image": "traefik:v3",
    "hostname": "router.acme",
    "dashboard_port": 8080,
    "shared_network": "fog-router"
  }
}
```

| Field | Required | Type | Default | Description |
|-------|----------|------|---------|-------------|
| `image` | No | `string` | `"traefik:v3"` | Traefik image to run |
| `hostname` | No | `string` | — | Traefik dashboard hostname (e.g. `router.acme`); must be covered by `dnsmasq.domains` |
| `index_port` | No | `integer` | `18080` | Port of the standalone service-directory index server (see below) |
| `dashboard_port` | No | `integer` | `8080` | Host port for the Traefik dashboard |
| `shared_network` | No | `string` | `"fog-router"` | External Docker network shared with app services |
| `tls` | No | `object` | `{ enabled: false }` | Optional HTTPS termination (see below) |

When `fog <script>` starts and a `router` section is configured, fog:

1. Creates the shared Docker network (`shared_network`) if it does not exist.
2. Starts a single `fog-router-<image>` Traefik container on it, publishing `:80`
   (web) and `dashboard_port:8080` (dashboard), with the Docker provider enabled
   (`exposedByDefault=false`) so only label-opted-in services are routed.
3. Assumes the network is already attached by app services — an app that does
   not declare the network is simply not routed.

The setup is **idempotent**: an existing/healthy router is left running and the
network is created only once. Traefik auto-discovers per-branch services from
their labels, so branches appearing and disappearing are routed and untouted
automatically. The router is **never** torn down when a project or branch exits
(it is a host-global resource, like dnsmasq); stopping it is a manual
`docker rm -f fog-router-traefik`. Any failure is a warning, never a hard error.

### router.tls — HTTPS termination

To serve `https://<branch>.<domain>` (no browser warnings), enable TLS:

```json
{
  "router": {
    "hostname": "router.acme",
    "shared_network": "fog-router",
    "tls": { "enabled": true }
  }
}
```

| Field | Required | Type | Default | Description |
|-------|----------|------|---------|-------------|
| `enabled` | No | `boolean` | `false` | Enable HTTPS on the central router |
| `cert_dir` | No | `string` | `~/.config/fog/certs` | Where wildcard certificates are stored |

When TLS is enabled, fog generates a **local-CA wildcard certificate** (via
[mkcert](https://github.com/FiloSottile/mkcert)) for each `dnsmasq` domain plus
the router hostname and `localhost`, stores it under `cert_dir`, and writes a
Traefik file-provider config that Traefik hot-reloads. Traefik then terminates
HTTPS on a `:443` `websecure` entrypoint while HTTP on `:80` keeps working.

Prerequisites (one-time):

```bash
brew install mkcert
mkcert -install          # installs the local CA into the OS trust store (sudo)
```

TLS is **sticky host-wide**: because the router is shared by every project, a
project whose `router` config does not enable `tls` will never tear down an
already-running HTTPS router (it would break other projects' HTTPS). Disabling
TLS requires removing the router manually (`docker rm -f fog-router-traefik`)
and re-running `fog <script>`.

Apps must opt the router into TLS per route by adding the label
`traefik.http.routers.<name>.tls=true` to their service — otherwise Traefik
serves plain HTTP on `:80` but not HTTPS on `:443`.

### Service-directory index (unmatched hosts)

A request whose host matches **no** app router (e.g. opening the raw tailnet IP
`http://100.86.26.45` or the Traefik dashboard host directly) is served a
generated `index.html` instead of a 404. The page lists every running service,
its hostname, and the internal port DNS forwards to it, with click-to-copy
links — handy for opening dev apps from a phone on the tailnet.

- fog runs a standalone index server (`fog index serve`) on loopback
  `index_port` (default `18080`), detached so it survives individual fog
  instances exiting.
- The file is regenerated when instances start and stop (bounded startup
  refreshes + a teardown refresh); refresh the browser to pick up changes.
- Traefik routes unmatched hosts to it via a low-priority catch-all router
  (`Host(\`*\`)`, `priority = 1`), so specific app routers always win.

#### Web UI & JSON API

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

##### Service controls

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

##### Start instances

The `/status` page includes a **Start instance** card with two modes:

1. **Known project** — pick a project, then a branch/worktree, then one of
   the scripts that worktree's `fog.json` defines, and press **Start**.
2. **New project** — enter an absolute config directory path and a script
   name (optionally a branch), and press **Start**.

Either mode spawns a new **detached** fog instance. Launching on a *different*
branch uses the existing [concurrent/share/reuse machinery](#concurrent-mode--sharing-services) —
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

Build the SPA with `cd ui && pnpm install && pnpm build`; `build.rs` embeds
`ui/dist/` into the binary at compile time. Without a build the server falls
back to the generated directory page above.

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
    "status_500": "red"
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
