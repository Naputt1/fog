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
- **Worktree-aware runs** — Starting a script that is already running in another worktree of the same git repo shuts the old instance down first (and only once it has fully exited, so ports don't collide). Services flagged `reuse: true` (e.g. a shared `docker compose` database) are handed over instead of torn down, so switching worktrees doesn't restart your infra. A per-project owner lock makes concurrent starts deterministic.
- **TLS support** — Terminate TLS connections directly in the proxy using PEM certificates.
- **Health checks** — Periodic TCP health checks per service with sidebar status indicators.

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

## Usage

```bash
fog <script> [OPTIONS]    # Run a script in the TUI (e.g. `fog dev`)
fog ls [pid]              # List running instances and service status
fog kill [pid]            # Gracefully shut down a running instance
```

| Option | Description |
|--------|-------------|
| `-c`, `--config <PATH>` | Path to config file (default: `fog.json`) |
| `--save-logs` | Save service output to `temp/<name>.txt` on exit |

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
| `↑` / `↓` | Normal | Scroll output |
| `PageUp` / `PageDown` | Normal | Scroll by page |
| `g` / `Home` | Normal | Scroll to top |
| `G` / `End` | Normal | Scroll to bottom |
| `/` | Proxy tab | Filter proxy logs |
| `?` | Any | Toggle help overlay |

Full reference in [Keybindings](./docs/keybindings.md).

## License

MIT
