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
| `$schema` | `string` | — | Editor autocomplete pointer to [`fog.schema.json`](https://github.com/Naputt1/fog/blob/main/fog.schema.json). Ignored by fog |
| `ports` | `object` | — | Allocatable ports for templating (`${ports.<name>}`); `0` = random free port. Override per run with `--port <name>=<port>`. Use `${ports.<name>@<repo>:<branch>[:<script>]}` to read a port from another running instance (see [Cross-instance ports](#cross-instance-ports)) |
| `native_routes` | `array` | — | Explicit Traefik file-provider routes for native services |
| `max_scrollback` | `integer` | `2000` | Maximum scrollback lines per terminal (min: 100) |
| `sidebar` | `object` | `null` | Sidebar width constraints |
| `theme` | `object` | `null` | Color theme overrides (see [Themes](/themes)) |
| `project` | `object` | `null` | UI metadata for this project (e.g. `icon`) shown in the index web UI |
| `index` | `object` | `{ enabled: true }` | Standalone index server (service directory + web UI); set `enabled:false` to opt this project out. See [Index Server](/index-server) |
| `router` | `object` | `null` | Central Traefik router (host-global). See [Router & DNS](/router) |
| `dnsmasq` | `object` | `null` | Wildcard DNS setup. See [Router & DNS](/router) |

Run-time port overrides: `fog <script> --port <name>=<port>` (repeatable) replaces an allocated port for one run without editing `fog.json`; `--port <name>=0` re-randomizes it. A name not declared in `ports` is added for that run. Overrides apply before templating, so every `${ports.<name>}` consumer — service `cmd`/`env`/health checks, endpoint routes and native routes — resolves to the overridden value. A cross-instance key (`--port api@my-repo:main=4000`, or `--port api@my-repo:main:dev=4000` with a script segment) pins that reference for one run and skips lookup and auto-start.

## Validation

fog validates the config as it loads it and exits with every problem it finds before starting anything. Unknown keys are errors, not warnings: a misspelled `healt_check` or `depend_on` would otherwise silently drop a health check or a dependency, which is the hardest kind of mistake to notice.

```text
error: invalid config 'fog.json': unknown field `healt_check`, expected one of `name`, `path`, `cmd`, ... at line 6 column 11
```

Cross references that the parser cannot check are reported with a dotted path, one line per problem:

| Check | Example |
|-------|---------|
| Empty `cmd` | `scripts.dev.service[0].cmd: cmd must not be empty` |
| Duplicate service names in one script | `scripts.dev.service[1]: duplicate service name 'db' (also service[0])` |
| `depends_on` names a missing service | `scripts.dev.service[0].depends_on: service 'api' depends on unknown service 'db2'` |
| `depends_on` forms a cycle | `scripts.dev.service: circular dependency detected between services` |
| `share: true` / `reuse: true` without a health check | `scripts.dev.service[0].share: share: true needs a health_check; without one fog cannot tell whether the service is already up, so it starts the command again` |
| A `ports` name contains `@` or `:` | `ports.api@main: port name 'api@main' must not contain '@' or ':' (reserved for cross-instance references ...)` |
| A `native_routes` port is not a port | `native_routes[0]: native_routes for service 'api' has invalid port 'http'` |
| Endpoint problems (empty/duplicate name, unusable port) | `scripts.dev: service 'api' has duplicate endpoint name 'web'` |
| Unknown or malformed `${...}` template | `scripts.dev.service[0].cmd: unknown port 'db' in '${ports.db}' (available: api)` |

Only fog's own atoms are templates: `${ports.<name>}`, `${branch}`, `${FOG_BRANCH}`, `${branch_raw}` and `${FOG_BRANCH_RAW}`. Shell expansions such as `${HOME}` are passed through to the shell unchanged.

The same structs parse the global config at `~/.config/fog/fog.json`. It has no entry point of its own, so if it is invalid fog warns once (`warning: invalid config '...'; using the built-in defaults`) and continues rather than failing the run — check that warning line for the key it rejected.

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

fog identifies the git repository a script runs in via `git rev-parse --git-common-dir`, which is shared by every worktree of the same repo. A config kept outside its repository (for example `fog dev -c /tmp/override.json` pointing at a repo worktree) takes the repository of its first service `path`, so the instance is still named after the repo it drives; only a config with no repository in its config dir or services falls back to the config directory name. The instance identity is `(project, script, branch)`: two worktrees on **different branches** run concurrently, while starting the **same script on the same branch** while another instance is already running (e.g. a human plus an agent) makes fog:

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

> `reuse` requires a `health_check` — fog needs it to verify the resource is already up. A `reuse` service without one fails config validation (see [Validation](#validation)).

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

> Like `reuse`, `share` requires a `health_check` — fog needs it to verify the resource is already up. A `share` service without one fails config validation (see [Validation](#validation)), since concurrent instances could otherwise race to start the same resource.

> `reuse` and `share` are mutually exclusive across modes: `reuse` is only honored when `"concurrent": false` (single-instance), and `share` is only honored when `"concurrent": true`. Set `"concurrent": false` to restore the old behavior where a re-run kills the previous instance first (handing over `reuse` services).

### Overriding sharing for one run

`--share <SERVICE>` (repeatable) turns sharing on for the named service, whatever its config says: `share` behavior in a concurrent script, `reuse` behavior in a single-instance one. It exists for a resource that is safe to share (a database, a compose stack) when the config left the flag off. `--no-share` is the opposite: it starts every service fresh for the run. The two flags conflict.

A forced service is still held to the same rules: it needs a `health_check` to be verified and borrowed (without one fog warns and starts it), and it is only borrowed when a same-branch sibling already owns it. The name must be one the script defines — an explicit `name`, else the `path` basename — otherwise fog exits `2` with the list of valid names. The override survives detach and worktree switches, and `fog attach` replays it.

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

### Cross-instance ports

`${ports.<name>}` reads this instance's own allocation. A service can also read a port owned by a **different fog instance** — another repo, or another branch of this one — with:

```
${ports.<name>@<repo>:<branch>[:<script>]}
```

- `<repo>` is the project name shown by the index and its URLs (the lowercased repository directory name; matching is case-insensitive), the repository directory name itself, or the exact git-common-dir identity.
- `<branch>` accepts the raw branch, its DNS-safe slug, or the short worktree segment the index uses (`feat/login`, `feat-login` and `feat-login` all match; `release/1.2` also matches `release-1`, the segment shown in the UI URL). `${branch}` / `${FOG_BRANCH}` / `${FOG_BRANCH_SLUG}` inside the atom expand to the **referencing** instance's branch slug (and `${branch_raw}` / `${FOG_BRANCH_RAW}` to the raw name) and must match exactly. Two tokens are special: `?` uses the target worktree on the referencing branch when the repo has one, otherwise the target's main worktree (`*` always means the main worktree). That keeps a per-branch stack working when only one of the two repos has the branch; the fallback is logged as `fell back to '<branch>'`.
- `<name>` is a port declared in that instance's `ports` map. Port names may not contain `@` or `:`; both mark cross-instance references.
- `<script>` is optional. It narrows the reference to one script of that project+branch (useful when several scripts run there) and authorizes fog to **start** that script when no matching instance is running.

For example, a frontend on branch `feat/login` reads the backend repo's API port for the same branch — and the backend stays on `main` rather than tracking the frontend branch — with:

```json
{
  "name": "web",
  "path": ".",
  "cmd": "npm run dev",
  "env": {
    "SAME_BRANCH_API": "http://localhost:${ports.api@cinema_ticket:${branch}:dev}",
    "BACKEND_API": "http://localhost:${ports.api@cinema_ticket:?:dev}",
    "MAIN_API": "http://localhost:${ports.api@cinema_ticket:*:dev}"
  }
}
```

`${branch}` is exact: if the backend has no `feat/login` worktree, startup fails. `?` is the pragmatic choice for a per-branch stack — it follows the frontend branch when the backend has one and otherwise stays on the backend's main worktree. `*` pins the backend to its main worktree even when a same-named branch exists.

References resolve once, at startup, by querying running instances over fog's local IPC sockets. A reference never needs a top-level `ports` map of its own.

Without `:script`, the target must already be running. With it, a stopped target is started for you when all of these hold:

- the project is registered — either run it once with fog or add it in the index UI's launch picker;
- the branch has a git worktree (`git worktree list`) in that repository;
- that worktree's `fog.json` exists at the same relative config location and declares both the script and the referenced port.

fog then launches `fog --config <worktree>/fog.json <script>` detached, waits until the new instance reports that every service has started, and reads the value from the new instance. Services that declare a health check count as started only after their startup grace, so a slow compile finishes before your services begin; a service without a health check counts as started as soon as it spawns, so add a health check (or a service that binds before it exits) when the dependent needs a socket that is already listening. (The wait is per instance, not per service: fog does not gate your `depends_on` on one specific service of another repo.) The started instance keeps running after the calling instance exits; the startup log records `started <repo>:<branch>:<script> (pid …)` and every resolved value as `resolved 'ports.<name>@…' = <port> (pid …)`, visible in `daemon.log` and `fog logs <pid>`. If the started instance fails, the caller's error includes the last `error:` line from that instance's `daemon.log`, so a cycle or a failed build is readable without opening the log. Several references to the same target start it once, and a per-target lock file in the temp directory keeps two parents from racing — the OS releases the lock if a parent dies mid-start, so a crash cannot wedge the project.

Instances started this way record their parent. `fog ls` shows it in the `started-by` column (`<repo>:<branch>:<script> (pid)`), and `fog kill <parent>` prints a line for each auto-started instance it leaves running:

```text
b:main:dev (pid 50699) was started by this instance and is still running; fog kill 50699
```

Add `--with-dependents` to stop them along with the parent, or kill them individually.

Startup fails with an actionable message when:

- no instance of that repo+branch is running and the reference has no `:script` (start it first, add `:script`, or pin the value);
- the project is not registered, the branch has no worktree, the branch token is ambiguous, `?` finds a detached main worktree, the script does not exist, or it does not declare that port (available names are listed);
- the started instance exits or does not become ready in time (its log directory is shown);
- the reference would re-start an instance already in the current start chain (a cycle such as `a:main:dev` → `b:main:web` → `a:main:dev`);
- the instance runs but does not declare that port (its declared ports are listed);
- two matching instances (different scripts) disagree on the value (both are listed).

To break a dependency, choose a script, or pin a value for one run, override the reference key exactly like a normal port:

```sh
fog dev --port api@cinema_ticket:feat-login=4000
fog dev --port api@cinema_ticket:feat-login:worker=4000
fog dev --port 'api@cinema_ticket:${branch}:worker=4000'   # template key verbatim
```

An override wins over both live lookup and auto-start, so it also works while the target is stopped. A key written with the expanded branch (as above) matches a config that uses `${branch}`. A cross-instance override must be a fixed port: `=0` is rejected, because the parent cannot re-randomize a port it does not own — a live target keeps its value and a cold-started target allocates its own. `fog.json` on the target still says `ports: { api: 0 }`.

`@` is reserved in port names. Values are captured at startup: if the owning instance later re-allocates, restart the referencing script. Cross-instance references are not used for [shared-port adoption](#concurrent-mode--sharing-services), which stays scoped to the same project+script+branch.

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
| `host` | No | `string` | `"127.0.0.1"` | Host address to bind to. Defaults to loopback. Binding a non-loopback address (`"0.0.0.0"`) exposes the built-in `/ws/terminal` gateway on that interface and therefore requires `terminal.auth_token` to be set. |
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

Both are idempotent. The index server stays up after the last instance exits and is stopped explicitly with `fog index kill`; the router is removed manually (`docker rm -f fog-router-traefik`).

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
