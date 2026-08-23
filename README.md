# fog

[![CI](https://github.com/Naputt1/fog/actions/workflows/ci.yml/badge.svg)](https://github.com/Naputt1/fog/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/fog-tui.svg)](https://crates.io/crates/fog-tui)
[![License](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

**Terminal service orchestrator and reverse proxy dashboard.**

fog runs named scripts from `fog.json`. Each script starts local services in PTYs with full ANSI color and scrollback, plus an optional reverse proxy. All in one `ratatui` terminal UI — and a responsive web UI you can open from your phone on the tailnet.

## Features

- **Branches side-by-side.** Run `fog dev` on `main` and `feature-x` at once; `s` to switch in the TUI. Same branch can run twice — you and an agent share the DB without killing each other.
- **Phone overview.** Check status and live logs at `http://<tailnet IP>` from your phone — no DNS setup.
- **One command per service.** Each service in its own PTY with color and scrollback.
- **Built-in proxy.** Reverse proxy with request log and WebSocket support.
- **Simple config.** One `fog.json` with named scripts (`fog dev`).

See [agentic guide](https://naputt1.github.io/fog/agentic) for worktree switching and human+agent sharing, and [configuration](https://naputt1.github.io/fog/configuration) for full details.

## Installation

```bash
# from source
git clone https://github.com/Naputt1/fog.git && cd fog
cargo build --release  # -> target/release/fog

# from crates.io
cargo install fog-tui  # installs binary `fog`

# from git
cargo install --git https://github.com/Naputt1/fog.git
cargo install --git https://github.com/Naputt1/fog.git --tag v0.1.1
```

If `ui/dist` is absent on a git install, `build.rs` fetches the prebuilt SPA from the GitHub Release. For offline builds use `FOG_SKIP_SPA_DOWNLOAD=1`. Use `FOG_REQUIRE_SPA=1` to fail the build instead of embedding the fallback page.

## Quick start

Create `fog.json`:

```json
{
  "scripts": {
    "dev": {
      "service": [
        { "name": "web", "path": "/path/to/project", "cmd": "npm run dev" }
      ]
    }
  }
}
```

```bash
fog dev
```

For wildcard hostnames like `main.acme` and Traefik routing on `:80`, see [DNS and routing setup](https://naputt1.github.io/fog/configuration#dnsmasq) and the [configuration reference](https://naputt1.github.io/fog/configuration).

Web UI and API run on `127.0.0.1:18080` by default when enabled. See [configuration](https://naputt1.github.io/fog/configuration#index) for the index server, SPA build, and API.

## Usage

```bash
fog <script> [OPTIONS]    # run a script (e.g. fog dev)
fog ls [pid]              # list running instances
fog kill [pid]            # gracefully shut down
fog logs [pid]            # print captured output of a detached instance
```

| Option | Description |
|--------|-------------|
| `-c`, `--config <PATH>` | Path to config file or directory containing `fog.json` (default `fog.json`) |
| `--branch <BRANCH>` | Run in the git worktree for this branch |
| `-d`, `--detach` | Run in background without TUI, captures logs to `$TMPDIR/fog-<pid>.logs/` |
| `--save-logs` | Save service output to `temp/<name>.txt` on exit |
| `--completions <SHELL>` | Print bash/zsh/fish completions |

Each instance exposes a Unix socket at `$TMPDIR/fog-<pid>.sock`. `fog ls` and `fog kill` discover it there. Pass a PID when multiple instances run.

Docs: [https://naputt1.github.io/fog/](https://naputt1.github.io/fog/) for configuration, proxy, themes, keybindings, architecture and troubleshooting.

## Keybindings

| Key | Action |
|-----|--------|
| `q` / `Ctrl+q` | Quit |
| `j` / `k` / `Ctrl+n` / `Ctrl+p` / arrows | Next / previous tab |
| `i` | Enter terminal input |
| `Esc` | Exit input |
| `R` | Restart current service or proxy |
| `t` / `Ctrl+t` | Open shell tab |
| `d` | Close shell tab |
| `s` | Worktree switch |
| `↑`/`↓`, `PageUp`/`PageDown`, `g`/`G` | Scroll |
| `/` | Filter proxy logs |
| `?` | Toggle help |

Full reference in [keybindings](https://naputt1.github.io/fog/keybindings).

## License

MIT
