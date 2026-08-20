---
title: Installation & Usage
---

# Installation & Usage

## Installation

### From source

```bash
git clone https://github.com/Naputt1/fog.git
cd fog
cargo build --release
```

The binary will be at `target/release/fog`.

### From crates.io

```bash
cargo install fog-tui  # installs binary `fog`
```

### With Cargo (directly from GitHub)

```bash
cargo install --git https://github.com/Naputt1/fog.git
```

Pin a specific version with `--tag`:

```bash
cargo install --git https://github.com/Naputt1/fog.git --tag v0.1.0
```

The `fog` binary is placed in `~/.cargo/bin/`.

If `ui/dist` is absent on a git install, `build.rs` fetches the prebuilt SPA from the GitHub Release. For offline builds use `FOG_SKIP_SPA_DOWNLOAD=1`.

## Quick start

Create a `fog.json` with at least one script:

```json
{
  "scripts": {
    "dev": {
      "service": [
        {
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
fog logs [pid]            # Print captured output of a detached instance
```

| Option | Description |
|--------|-------------|
| `-c`, `--config <PATH>` | Path to config file, or a directory containing `fog.json` (default: `fog.json`) |
| `--branch <BRANCH>` | Run in the git worktree for this branch (see [Agentic guide](/agentic)) |
| `-d`, `--detach` | Run the script in the background without the TUI; returns once the instance is serving |
| `--save-logs` | Save service output to `temp/<name>.txt` on exit |
| `--completions <SHELL>` | Print bash/zsh/fish completions |

### Managing instances

Every running `fog` instance listens on a Unix socket at `$TMPDIR/fog-<pid>.sock`. While a TUI session is running, you can inspect or stop it from another terminal:

```bash
fog ls        # show running instances, their scripts, proxy, and per-service status
fog kill      # shut down the only running instance
fog kill 1234 # shut down the instance with PID 1234
```

`fog ls` prints one line per instance:

```
pid   script  branch  proxy   services
1234  dev     main    :3000   api:healthy db:healthy
```

The `branch` column shows the git branch (or `.` when not in a worktree).

### Detached runs

`fog <script> -d` runs a script in the background without the TUI — useful for CI pipelines and AI agents that cannot drive an interactive terminal. Services keep their PTYs, health checks, dependency ordering, and reverse proxy, so management works exactly as with the TUI:

```bash
fog dev -d            # start in the background; prints the PID and returns
fog ls 1234           # check the instance's service status
fog logs 1234         # print the captured output of each service
fog kill 1234         # gracefully shut it down
```

Each detached instance tees every service's raw PTY output into `$TMPDIR/fog-<pid>.logs/<name>.log` (the daemon's own diagnostics go to `daemon.log`), and `fog logs <pid>` prints them with ANSI escape sequences stripped. The log files persist after the instance exits. This is separate from `--save-logs`, which on any exit (TUI or detached) writes `temp/<name>.txt` in the project directory.

### Agentic concurrent recipe — human + agent on the same branch

With the default `concurrent: true` you and an agent can run the same `dev` on the same branch at once without either killing the other. Mark the DB as `share: true` with a `health_check` so it is borrowed instead of duplicated. `ports: { "<name>": 0 }` allocates a random free port per instance; reference it as `${ports.<name>}` in `cmd`, `env`, `health_check.target` or `proxy.upstream` (see [Configuration](/configuration)):

```json
{
  "ports": { "api": 0 },
  "scripts": { "dev": { "service": [
    { "name": "db", "path": ".", "cmd": "docker compose up -d", "share": true, "health_check": { "kind": "tcp", "target": "localhost:5432" } },
    { "name": "api", "path": "backend", "cmd": "cargo run -- --port ${ports.api}", "health_check": { "kind": "tcp", "target": "localhost:${ports.api}" } }
  ]}}
}
```

```bash
# Terminal A — you (TUI)
fog dev
fog ls
# 1234  dev  main  :3000  db:healthy api:healthy

# Terminal B — agent (headless), same branch
fog dev -d --branch main
# → daemon started pid=5678
fog ls
# 1234  dev  main  :3000  db:healthy api:healthy
# 5678  dev  main  :3001  db:reused  api:healthy
# ↑ same branch, two proxies, DB borrowed (♻ reusing already-running), per-instance ports via ${ports.api}

fog logs 5678         # tail agent output
fog kill 5678         # stop just the agent; DB stays while your TUI lives
```

* `share` vs `reuse`: concurrent (`true`, default) honors `share`; single-instance (`concurrent: false`) honors `reuse` with live PTY handoff `♻ adopted from instance <pid>` on `s` switch or reclaim. The other flag is ignored.
* Switch branches in the TUI with `s` (fuzzy `f`, `d` to terminate branch, `↑/↓` + `Enter`). See the [Agentic guide](/agentic).
* Phone: open `http://<tailnet IP>` (e.g. `http://100.86.26.45`) on the same tailnet — directory + `http://<tailnet IP>:<port>` links. See [Agentic guide — Phone on the tailnet](/agentic#phone-on-the-tailnet). Wildcard `*.acme` DNS is laptop-only (`127.0.0.1`).

## Configuration

See the [Configuration reference](/configuration) for the full config schema,
including proxy routes, health checks, themes, and TLS.
