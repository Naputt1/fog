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

## Configuration

See the [Configuration reference](/configuration) for the full config schema,
including proxy routes, health checks, themes, and TLS.
