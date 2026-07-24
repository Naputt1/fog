# fog

[![CI](https://github.com/Naputt1/fog/actions/workflows/ci.yml/badge.svg)](https://github.com/Naputt1/fog/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/badge/crates.io-0.1.0-orange)](https://crates.io/crates/fog)
[![License](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

**Terminal-based service orchestrator & reverse-proxy dashboard.**

fog lets you define a set of local services, launch them simultaneously inside pseudo-terminals, view their real-time colored output with scrollback, and optionally expose a reverse proxy that routes traffic to them — all in a single [ratatui](https://github.com/ratatui-org/ratatui) terminal interface.

<!-- Add a screenshot here: docs/screenshot.png (~1200×800) -->

## Features

- **Multi-service orchestration** — Spawn each service in its own PTY, with full VT100/ANSI color rendering and scrollback (up to 2000 lines, configurable).
- **Built-in reverse proxy** — HTTP/1.1 and WebSocket proxy with a live request log showing method, path, status code, latency, and upstream target.
- **Live sidebar** — Vertical sidebar with status indicators (● running, ● healthy, ○ stopped, ● unhealthy). Click to switch tabs.
- **Mouse interaction** — Click tabs, scroll with the wheel, drag-select text (copied to system clipboard via OSC 52).
- **Keyboard navigation** — Vim-style `j`/`k` tab switching, terminal input mode (`i`), restart services (`R`), open shell tabs (`t`).
- **Configuration hot-reload** — Edit `fog.json` at runtime to update themes and proxy settings without restarting.
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

Create a `fog.json`:

```json
{
  "service": [
    {
      "path": "/path/to/project",
      "cmd": "npm run dev"
    }
  ]
}
```

Then run:

```bash
fog
```

## Usage

```bash
fog [OPTIONS]
```

| Option | Description |
|--------|-------------|
| `-c`, `--config <PATH>` | Path to config file (default: `fog.json`) |
| `--save-logs` | Save service output to `temp/<name>.txt` on exit |

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
