# fog

[![CI](https://github.com/Naputt1/fog/actions/workflows/ci.yml/badge.svg)](https://github.com/Naputt1/fog/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/badge/crates.io-0.1.0-orange)](https://crates.io/crates/fog)
[![License](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

**Terminal-based service orchestrator & reverse-proxy dashboard.**

fog lets you define named *scripts* — each a set of local services and an optional reverse proxy — launch them simultaneously inside pseudo-terminals, view their real-time colored output with scrollback, and expose a proxy that routes traffic to them, all in a single [ratatui](https://github.com/ratatui-org/ratatui) terminal interface.

<!-- Add a screenshot here: docs/screenshot.png (~1200×800) -->

## Features

- **Script-based profiles** — Define named profiles (e.g. `dev`, `infra`) that each select which services and proxy to run. `fog dev` launches only what that profile needs.
- **Multi-service orchestration** — Spawn each service in its own PTY, with full VT100/ANSI color rendering and scrollback (up to 2000 lines, configurable).
- **Built-in reverse proxy** — HTTP/1.1 and WebSocket proxy with a live request log showing method, path, status code, latency, and upstream target.
- **Live sidebar** — Vertical sidebar with status indicators (● running, ● healthy, ○ stopped, ● unhealthy). Click to switch tabs.
- **Mouse interaction** — Click tabs, scroll with the wheel, drag-select text (copied to system clipboard via OSC 52).
- **Keyboard navigation** — Vim-style `j`/`k` tab switching, terminal input mode (`i`), restart services (`R`), open shell tabs (`t`).
- **Configuration hot-reload** — Edit `fog.json` at runtime to update themes and proxy settings without restarting.
- **Instance management** — `fog ls` lists running instances and their service status; `fog kill` gracefully shuts one down.
- **Detached runs** — `fog <script> -d` starts a script in the background without the TUI (ideal for CI and AI agents), tees each service's output to `$TMPDIR/fog-<pid>.logs/`, and returns immediately with the PID; `fog logs <pid>` prints the captured output.
- **Worktree-aware runs** — Starting a script that is already running in another worktree of the same git repo on the same branch shuts the old instance down first (and only once it has fully exited, so ports don't collide). Services flagged `reuse: true` (e.g. a shared `docker compose` database) are handed over instead of torn down, so switching worktrees doesn't restart your infra. A per-(project, script, branch) owner lock makes concurrent starts deterministic, and **different branches run concurrently** — `fog dev --branch feature-x` and `fog dev --branch main` can run side by side. Start directly on a branch with `fog <script> --branch <name>`, or switch worktrees from inside the TUI with `s`.
- **TLS support** — Terminate TLS connections directly in the proxy using PEM certificates.
- **Health checks** — Periodic health checks per service with sidebar status indicators. `tcp`/`http` probe an address; `docker` verifies the actual container from the service's compose file is running (and, when a compose healthcheck is defined, `healthy`).
- **Per-branch env** — Services started in a git worktree get `FOG_BRANCH`, so compose files can derive per-branch project names, hostnames, and ports.
- **Automatic wildcard DNS** — Configure `"dnsmasq": { "domains": ["acme"] }` and fog sets up (and **starts**) `*.acme → 127.0.0.1` on startup, so per-branch dev hostnames like `main.acme` resolve with no manual DNS setup. On macOS it installs a **root LaunchDaemon** (`sudo brew services start dnsmasq`, bound to `:53`) and creates `/etc/resolver/<domain>`; on Linux it uses `systemctl`. See [DNS & routing setup](#dns--routing-setup).
- **Central router (Traefik)** — Optionally run a single host-global Traefik per machine that auto-discovers label-opted-in app containers and routes per-branch hostnames on `:80` (dashboard on `:8080`). See [DNS & routing setup](#dns--routing-setup).

## Installation

### From source

```bash
git clone https://github.com/Naputt1/fog.git
cd fog
cargo build --release
```

The binary will be at `target/release/fog`.

### With Cargo (directly from GitHub)

```bash
cargo install --git https://github.com/Naputt1/fog.git
```

Pin a specific version with `--tag`:

```bash
cargo install --git https://github.com/Naputt1/fog.git --tag v0.1.0
```

The `fog` binary is placed in `~/.cargo/bin/`.

## Quick start

Create a `fog.json` with at least one script:

```json
{
  "scripts": {
    "dev": {
      "service": [
        {
          "name": "web",
          "path": "/path/to/project",
          "cmd": "npm run dev"
        }
      ]
    }
  }
}
```

Then run:

```bash
fog dev
```

## DNS & routing setup

Optional, but recommended if you want per-branch hostnames like
`http://main.acme` (no ports) to resolve locally. Two moving parts:

- **dnsmasq** turns `*.acme` into `127.0.0.1` (DNS resolution).
- **Traefik** (central router) inspects the `Host` header and forwards to the
  right container (HTTP routing). It requires dnsmasq — one maps names to an
  IP, the other maps a hostname to a route.

### 1. Install dnsmasq (macOS)

```bash
brew install dnsmasq        # skip this if you don't need wildcard hostnames
```

### 2. Configure fog

Add to your `fog.json`:

```json
{
  "dnsmasq": {
    "domains": ["acme"],
    "address": "127.0.0.1",
    "port": 53
  }
}
```

`port` defaults to 53. On macOS the daemon runs as a root LaunchDaemon so it can
bind the privileged port, and `/etc/resolver/<domain>` only needs a plain
`nameserver` line (macOS renders but ignores a `port` directive in resolver files).

### 3. Run `fog dev` interactively once

The **first** run approves a couple of `sudo` prompts; fog then:
- writes `*.acme → 127.0.0.1` into dnsmasq's config and pins it to `127.0.0.1:53`
- creates `/etc/resolver/acme` (via sudo)
- installs and starts the **root LaunchDaemon** via `sudo brew services start dnsmasq`

Detached `-d` runs use `sudo -n` and won't prompt — always do the first setup run
interactively.

### 4. Verify

```bash
python3 -c "import socket; print(socket.gethostbyname('main.acme'))"
# → 127.0.0.1
```

### Central router (Traefik) — optional

To route the resolved hostnames to containers on `:80`, add a `router` section:

```json
{
  "router": {
    "hostname": "router.acme",
    "dashboard_port": 8080,
    "shared_network": "fog-router"
  }
}
```

On startup fog creates one host-global Traefik container (`fog-router-traefik`)
and the shared `fog-router` Docker network, publishing `:80` (web) and
`dashboard_port:8080` (dashboard). App services opt in by attaching to that
network and declaring Traefik container labels (e.g. a frontend's
`traefik.http.routers.<name>.rule=Host(\`<branch>.acme\`)`). The router
auto-discovers them and routes each branch, and is **never** torn down when a
project or branch exits. Open the dashboard at
`http://router.acme:8080` (or `127.0.0.1:8080`).

Full field reference and both sections are documented in
[Configuration](https://github.com/Naputt1/fog/blob/main/docs/configuration.md).

## Usage

```bash
fog <script> [OPTIONS]    # Run a script in the TUI (e.g. `fog dev`)
fog ls [pid]              # List running instances and service status
fog kill [pid]            # Gracefully shut down a running instance
fog logs [pid]            # Print captured output of a detached instance
```

| Option | Description |
|--------|-------------|
| `-c`, `--config <PATH>` | Path to config file, or a directory containing `fog.json` (default: `fog.json`) |
| `--branch <BRANCH>` | Run the script in the git worktree checked out on this branch |
| `-d`, `--detach` | Run the script in the background without the TUI, capturing service output to `$TMPDIR/fog-<pid>.logs/`; returns once the instance is serving |
| `--save-logs` | Save service output to `temp/<name>.txt` on exit |
| `--completions <SHELL>` | Print a bash/zsh/fish completion script (`--branch` completes to all worktrees) |

Each running `fog` instance exposes a Unix socket in `$TMPDIR/fog-<pid>.sock`. `fog ls` discovers these sockets and queries their live service status; `fog kill` asks an instance to shut down gracefully. When multiple instances are running, pass a PID to target one (`fog ls 1234`, `fog kill 1234`).

See the [full documentation](https://naputt1.github.io/fog/) for configuration reference, theming, architecture details, and more.

## Keybindings

| Key | Context | Action |
|-----|---------|--------|
| `q` / `Ctrl+q` | Any | Quit fog |
| `j` / `→` / `Ctrl+n` | Normal | Next tab |
| `k` / `←` / `Ctrl+p` | Normal | Previous tab |
| `i` | Normal (non-proxy) | Enter terminal input mode |
| `Esc` | Terminal input | Exit to normal mode |
| `R` | Normal | Restart current service or proxy |
| `t` / `Ctrl+t` | Normal | Open a new shell tab |
| `d` | Normal | Close current shell tab |
| `s` | Normal | Open worktree switch popup |
| `↑` / `↓` | Normal | Scroll output |
| `PageUp` / `PageDown` | Normal | Scroll by page |
| `g` / `Home` | Normal | Scroll to top |
| `G` / `End` | Normal | Scroll to bottom |
| `/` | Proxy tab | Filter proxy logs |
| `?` | Any | Toggle help overlay |

Full reference in [Keybindings](./docs/keybindings.md).

## License

MIT
