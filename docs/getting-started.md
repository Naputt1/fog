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
| `-d`, `--detach` | Run the script in the background without the TUI; returns once the instance is serving |
| `--save-logs` | Save service output to `temp/<name>.txt` on exit |

### Managing instances

Every running `fog` instance listens on a Unix socket at `$TMPDIR/fog-<pid>.sock`. While a TUI session is running, you can inspect or stop it from another terminal:

```bash
fog ls        # show running instances, their scripts, proxy, and per-service status
fog kill      # shut down the only running instance
fog kill 1234 # shut down the instance with PID 1234
```

`fog ls` prints one line per instance:

```
pid   script  proxy   services
1234  dev     :3000   api:healthy db:healthy
```

### Detached runs

`fog <script> -d` runs a script in the background without the TUI — useful for CI pipelines and AI agents that cannot drive an interactive terminal. Services keep their PTYs, health checks, dependency ordering, and reverse proxy, so management works exactly as with the TUI:

```bash
fog dev -d            # start in the background; prints the PID and returns
fog ls 1234           # check the instance's service status
fog logs 1234         # print the captured output of each service
fog kill 1234         # gracefully shut it down
```

Each detached instance tees every service's raw PTY output into `$TMPDIR/fog-<pid>.logs/<name>.log` (the daemon's own diagnostics go to `daemon.log`), and `fog logs <pid>` prints them with ANSI escape sequences stripped. The log files persist after the instance exits.

## Configuration

See the [Configuration reference](/configuration) for the full config schema,
including proxy routes, health checks, themes, and TLS.
