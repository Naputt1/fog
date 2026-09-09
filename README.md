# fog

[![CI](https://github.com/Naputt1/fog/actions/workflows/ci.yml/badge.svg)](https://github.com/Naputt1/fog/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/fog-tui.svg)](https://crates.io/crates/fog-tui)
[![License](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

**The dev-environment orchestrator for humans and coding agents.**

You and your agent run the same `fog dev` on the same branch — concurrently, without killing each other. Shared DBs get borrowed instead of duplicated, your agent runs headless while you watch the TUI, and you check both from your phone on the tailnet. Each service runs in its own PTY with full ANSI color and scrollback, behind an optional built-in reverse proxy — all in one `ratatui` terminal UI.

```bash
# Terminal A — you, the TUI
fog dev

# Terminal B — your agent, headless, same branch
fog dev -d
# → shares the DB, streams logs you can watch live
```

![fog demo](assets/demo.gif)

## Why fog

AI agents changed how we develop, but dev tooling still assumes one human per environment. fog is worktree-aware and **concurrent by default**: run `main` and `feature-x` side-by-side, or the *same* branch twice — human in the TUI, agent headless — and fog shares healthy services (the DB) while isolating the rest with per-instance ports and `${branch}` templating.

## Features

- **Humans + agents on one environment.** `fog dev` and `fog dev -d` on the same branch coexist: shared DBs are borrowed (`share: true`), ports are randomized per instance, logs stream to the web UI. See the [agentic guide](https://naputt1.github.io/fog/agentic).
- **Branches side-by-side.** Run `fog dev` on `main` and `feature-x` at once; `s` switches worktrees in-place in the TUI. Same branch can run twice — you and an agent share the DB without killing each other.
- **Phone overview.** Check status and live logs at `http://<tailnet IP>` from your phone — no DNS setup.
- **One command per service.** Each service in its own PTY with color and scrollback. `health_check`, `depends_on`, restart with `R`.
- **Built-in proxy.** Reverse proxy with request log, filter, and WebSocket support.
- **Simple config.** One `fog.json` with named scripts (`fog dev`). Ports templating, native_routes, worktree-aware sharing.

Full docs: [configuration](https://naputt1.github.io/fog/configuration), [agentic guide](https://naputt1.github.io/fog/agentic), [index server](https://naputt1.github.io/fog/index-server).

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

## Web terminal

The web UI includes a live terminal at `/ws/terminal` that bridges your browser
to a PTY shell (via the built-in reverse proxy). Open the service's terminal
tab in the UI for a full ANSI-color shell.

The gateway is served before route matching and can be hardened per script with
a `terminal` config block:

```json
{
  "scripts": {
    "dev": {
      "service": [ { "name": "web", "path": "/path", "cmd": "npm run dev" } ],
      "proxy": { "port": 8080, "routes": [] },
      "terminal": {
        "auth_token": "s3cret",
        "max_sessions_per_ip": 8,
        "max_message_bytes": 65536,
        "idle_timeout_secs": 900
      }
    }
  }
}
```

When `auth_token` is set, the client must connect with
`/ws/terminal?auth_token=<token>`; missing or wrong tokens are rejected with
`401`. Sessions are capped per client IP (`429`), oversized frames close with
code `1009`, and PTY output is buffered in a bounded 64-frame drop-oldest
queue. See the [terminal protocol](https://naputt1.github.io/fog/terminal-protocol)
for full details.

## Usage

```bash
fog <script> [OPTIONS]    # run a script (e.g. fog dev)
fog ls [pid]              # list running instances
fog kill [pid]            # gracefully shut down
fog logs [pid]                  # list services and their status
fog logs [pid] -s <name>        # print captured output of one service
```

| Option | Description |
|--------|-------------|
| `-c`, `--config <PATH>` | Path to config file or directory containing `fog.json` (default `fog.json`) |
| `--branch <BRANCH>` | Run in the git worktree for this branch |
| `-d`, `--detach` | Run in background without TUI, captures logs to `$TMPDIR/fog-<pid>.logs/` |
| `-s`, `--service <NAME>` | With `fog logs`: show one service instead of listing (`daemon` and `proxy` included) |
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
