---
title: Router & DNS
---

# Router & DNS

fog can set up two **host-global** services that outlive any single `fog <script>` instance: wildcard DNS (`dnsmasq`) and a central reverse proxy (`router` via Traefik). Both are idempotent and shared across projects and branches — no per-app speculative instances that would collide on `:80` or `:53`.

Configure them at the top level of `fog.json` (like `theme`/`sidebar`), not per-script. See [`fog.schema.json`](https://github.com/Naputt1/fog/blob/main/fog.schema.json) for the full schema.

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
2. On **macOS** (Homebrew): writes the domain mapping into `$prefix/etc/dnsmasq.d/fog-<domain>.conf`, pins the listener to `address:port`, ensures `conf-dir` is enabled in `dnsmasq.conf`, creates `/etc/resolver/<domain>` (via `sudo`), then **starts** dnsmasq as a **root LaunchDaemon** via `sudo brew services start dnsmasq`. Any stale user-level LaunchAgent is booted out first.
3. On **Linux**: writes `/etc/dnsmasq.d/fog-<domain>.conf` and starts dnsmasq via `sudo systemctl start dnsmasq`.
4. On other platforms it warns that the setup is unsupported.

The setup is **idempotent**: existing files are left untouched and dnsmasq is
only restarted when something changed — and if the daemon is already running it
is left alone. If dnsmasq is **not** running, fog starts it automatically and
verifies it actually came up on `address:port`. Detached (`-d`) runs use `sudo -n` so a password prompt cannot
hang them; if a privileged step is needed but cannot run headless, fog prints a
warning telling you to run `fog <script>` interactively once. Any failure is a
warning, never a hard error.

> **Why root?** macOS 26+ restricts binding to privileged ports (<1024) to
> root, and macOS ignores the `port` directive in `/etc/resolver/<domain>` files. dnsmasq therefore must listen on `:53` as a **root LaunchDaemon**. fog only ever binds it to `127.0.0.1` (`bind-interfaces`), so the daemon is not exposed to the LAN.

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
| `index_port` | No | `integer` | `18080` | Port of the standalone service-directory [Index Server](/index-server) |
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
their labels, so branches appearing and disappearing are routed automatically. The router is **never** torn down when a project or branch exits
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

### Native routes

For non-container services you can declare explicit file-provider routes:

```json
{
  "native_routes": [
    { "host": "${branch}.acme", "service": "api", "port": "${ports.api}" }
  ]
}
```

Each entry maps a `Host` rule to a service's allocated port via `host.docker.internal`. See [Configuration](/configuration) for the full `native_routes` schema.
