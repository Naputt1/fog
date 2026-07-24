---
title: Configuration
---

# Configuration Reference

fog is configured via a JSON file (default: `fog.json`). Use the `-c` / `--config` flag to specify a different path.

See [`fog.schema.json`](https://github.com/Naputt1/fog/blob/main/fog.schema.json) for the formal JSON Schema.

## Top-level structure

```json
{
  "$schema": "https://raw.githubusercontent.com/Naputt1/fog/main/fog.schema.json",
  "service": [...],
  "proxy": {...},
  "max_scrollback": 2000,
  "sidebar": {...},
  "theme": {...}
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `service` | `array` | `[]` | List of service entries to manage |
| `proxy` | `object` | `null` | Reverse proxy configuration |
| `max_scrollback` | `integer` | `2000` | Maximum scrollback lines per terminal (min: 100) |
| `sidebar` | `object` | `null` | Sidebar width constraints |
| `theme` | `object` | `null` | Color theme overrides |

## Service entries

```json
{
  "service": [
    {
      "name": "backend",
      "path": "/path/to/project",
      "cmd": "npm run dev",
      "health_check": {
        "kind": "tcp",
        "target": "localhost:3000",
        "interval_ms": 5000,
        "timeout_ms": 2000
      }
    }
  ]
}
```

### Fields

| Field | Required | Type | Description |
|-------|----------|------|-------------|
| `name` | No | `string` | Display name for the tab. Defaults to the directory name. |
| `path` | **Yes** | `string` | Working directory for the command (relative to the config file). |
| `cmd` | **Yes** | `string` | Shell command to execute (e.g. `cargo run`, `npm run dev`, `air`). |
| `health_check` | No | `object` | Health check configuration (see below). |

The command is executed inside a shell (`$SHELL` or `bash`) using `cd <path> && <cmd>`.

### Health check

Once configured, a background thread periodically checks the service health and updates the sidebar indicator.

| Field | Required | Type | Default | Description |
|-------|----------|------|---------|-------------|
| `kind` | **Yes** | `string` | — | `"tcp"` or `"http"` (both use TCP connect) |
| `target` | **Yes** | `string` | — | Address to check (e.g. `"localhost:8080"`) |
| `interval_ms` | No | `integer` | `5000` | Check interval in milliseconds (min: 100) |
| `timeout_ms` | No | `integer` | `2000` | Connection timeout in milliseconds (min: 100) |

Both `"tcp"` and `"http"` health checks work the same way: they attempt a TCP connection to the target address. The `target` field can be prefixed with `tcp://`, `http://`, or `https://` — the prefix is stripped before connecting.

## Proxy configuration

```json
{
  "proxy": {
    "port": 3000,
    "host": "0.0.0.0",
    "routes": [
      {
        "path": "/api",
        "host": "api.local",
        "upstream": "http://localhost:8080",
        "ws": false
      }
    ],
    "tls_cert": "/path/to/cert.pem",
    "tls_key": "/path/to/key.pem",
    "max_log_entries": 1000
  }
}
```

| Field | Required | Type | Default | Description |
|-------|----------|------|---------|-------------|
| `port` | **Yes** | `integer` | — | Port to listen on (1–65535) |
| `host` | No | `string` | `"0.0.0.0"` | Host address to bind to |
| `routes` | **Yes** | `array` | — | List of route definitions |
| `tls_cert` | No | `string` | `null` | Path to PEM-encoded TLS certificate |
| `tls_key` | No | `string` | `null` | Path to PEM-encoded PKCS8 private key |
| `max_log_entries` | No | `integer` | `1000` | Maximum request log entries to retain |

### Route fields

| Field | Required | Type | Default | Description |
|-------|----------|------|---------|-------------|
| `path` | **Yes** | `string` | — | Incoming path prefix or wildcard pattern |
| `host` | No | `string` | `null` | Host header pattern for virtual hosting |
| `upstream` | **Yes** | `string` | — | Upstream URL to forward requests to |
| `ws` | No | `boolean` | `false` | Enable WebSocket proxying for this route |

For details on route matching, see the [Proxy docs](/proxy).

## Sidebar

```json
{
  "sidebar": {
    "min_width": 12,
    "max_width": 30
  }
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `min_width` | `integer` | `12` | Minimum sidebar width in columns (8–50) |
| `max_width` | `integer` | `30` | Maximum sidebar width in columns (8–50) |

The sidebar width is computed dynamically: `max(name_length + 5, min_width)` clamped to `max_width`.

## Theme

```json
{
  "theme": {
    "proxy": "cyan",
    "terminal": "green",
    "stopped": "red",
    "highlight": "magenta",
    "status_200": "green",
    "status_300": "yellow",
    "status_400": "red",
    "status_500": "red"
  }
}
```

See the [Themes docs](/themes) for available colors and customization examples.

## Complete example

```json
{
  "$schema": "https://raw.githubusercontent.com/Naputt1/fog/main/fog.schema.json",
  "service": [
    {
      "name": "api",
      "path": "backend",
      "cmd": "cargo run",
      "health_check": {
        "kind": "tcp",
        "target": "localhost:8080",
        "interval_ms": 3000
      }
    },
    {
      "path": "frontend",
      "cmd": "npm run dev"
    }
  ],
  "proxy": {
    "port": 3000,
    "routes": [
      {
        "path": "/api",
        "upstream": "http://localhost:8080"
      },
      {
        "path": "/",
        "upstream": "http://localhost:5173",
        "ws": true
      }
    ]
  },
  "max_scrollback": 5000,
  "sidebar": {
    "min_width": 14,
    "max_width": 28
  },
  "theme": {
    "proxy": "#00bcd4",
    "terminal": "#4caf50"
  }
}
```

## Hot-reloading

fog watches the config file for changes at runtime. When a change is detected, it:

1. Reloads the color theme immediately
2. If proxy settings changed (port, host, routes), restarts the proxy

Service entries are **not** hot-reloaded — add/remove services requires a restart of fog.
