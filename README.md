# fog

**Terminal-based service orchestrator & reverse-proxy dashboard.**

fog lets you define a set of local services, launch them simultaneously inside pseudo-terminals, view their real-time colored output with scrollback, and optionally expose a reverse proxy that routes traffic to them — all in a single [ratatui](https://github.com/ratatui-org/ratatui) terminal interface.

## Features

- **Multi-service orchestration** — Spawn each service in its own PTY, with full VT100/ANSI color rendering and scrollback (up to 2000 lines).
- **Built-in reverse proxy** — HTTP/1.1 and WebSocket proxy with a live request log showing method, path, status code, latency, and upstream target.
- **Live sidebar** — Vertical sidebar with status indicators (green = running, red = stopped).
- **Mouse interaction** — Click to switch tabs, scroll with the wheel, drag-select text (copied to system clipboard via OSC 52).
- **Keyboard navigation** — Vim-style `j`/`k` tab switching, terminal input mode (`i`), restart services (`R`), open shell tabs (`t`).

## Installation

### From source

```bash
git clone https://github.com/Naputt1/fog.git
cd fog
cargo build --release
```

The binary will be at `target/release/fog`.

### With Cargo (directly from GitHub)

Similar to `go install <repo>@latest`:

```bash
cargo install --git https://github.com/Naputt1/fog.git
```

Pin a specific version with `--tag`:

```bash
cargo install --git https://github.com/Naputt1/fog.git --tag v0.1.0
```

The `fog` binary is placed in `~/.cargo/bin/`.

## Usage

```bash
fog [OPTIONS]
```

| Option | Description |
|--------|-------------|
| `-c`, `--config <PATH>` | Path to config file (default: `fog.json`) |

### Configuration

Create a `fog.json` file in the current directory (or specify one with `--config`):

```json
{
  "$schema": "https://raw.githubusercontent.com/Naputt1/fog/main/fog.schema.json",
  "service": [
    {
      "path": "/path/to/backend",
      "cmd": "air"
    },
    {
      "path": "/path/to/frontend",
      "cmd": "npm run dev"
    }
  ],
  "proxy": {
    "port": 3000,
    "routes": [
      {
        "path": "/api",
        "upstream": "http://localhost:8080/api"
      },
      {
        "path": "/",
        "upstream": "http://localhost:5173",
        "ws": true
      }
    ]
  }
}
```

See [`fog.schema.json`](./fog.schema.json) for the full schema.

## Keybindings

| Key | Context | Action |
|-----|---------|--------|
| `q` / `Ctrl+q` | Any | Quit fog |
| `j` / `Right` / `Ctrl+n` | Normal | Next tab |
| `k` / `Left` / `Ctrl+p` | Normal | Previous tab |
| `i` | Normal (non-proxy) | Enter terminal input mode |
| `Esc` | Terminal input | Exit to normal mode |
| `R` | Normal | Restart current service or proxy |
| `t` / `Ctrl+t` | Normal | Open a new shell tab |
| `d` | Normal | Close current shell tab |
| `Up` / `Down` | Normal | Scroll output |
| `PageUp` / `PageDown` | Normal | Scroll by page |
| `g` / `Home` | Normal | Scroll to top |
| `G` / `End` | Normal | Scroll to bottom |

## License

MIT
