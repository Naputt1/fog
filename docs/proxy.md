---
title: Proxy
---

# Reverse Proxy

fog includes a built-in HTTP/1.1 and WebSocket reverse proxy that logs all requests in real-time to the proxy tab.

## How it works

The proxy runs on a dedicated background thread with its own single-threaded Tokio runtime. It uses:

- **hyper** for HTTP/1.1 service and client
- **rustls** for TLS termination
- **tokio-rustls** for async TLS accept

## Route matching

Routes are matched in order of definition. The first matching route handles the request.

### Prefix matching

By default, routes use prefix matching. An incoming path matches if it starts with the route path:

| Route path | Incoming path | Matches | Upstream suffix |
|------------|---------------|---------|-----------------|
| `/api` | `/api` | Yes | `/` |
| `/api` | `/api/users` | Yes | `/users` |
| `/api` | `/api/` | Yes | `/` |
| `/api` | `/other` | No | — |

Prefix matching requires the match boundary to be a `/` — `/api/users` matches `/api`, but `/apixyz` does not.

### Wildcard matching

Routes containing `*` in path segments use wildcard pattern matching. Each `*` matches any characters within a single path segment:

| Route path | Incoming path | Matches |
|------------|---------------|---------|
| `/api/*` | `/api/foo` | Yes |
| `/api/*` | `/api/foo/bar` | No (different segment count) |
| `/api/v*/users` | `/api/v2/users` | Yes |
| `/*/js/main.js` | `/static/js/main.js` | Yes |

Wildcard routes require an equal number of path segments between the pattern and the incoming path.

### Host matching

Each route can optionally specify a `host` pattern. If set, the route only matches requests whose `Host` header matches the pattern (before the port, if any). Host patterns support `*` wildcards:

| Host pattern | Request Host | Matches |
|--------------|--------------|---------|
| `custom.*` | `custom.com` | Yes |
| `*.example.com` | `api.example.com` | Yes |
| `*.example.com` | `other.com` | No |
| `custom.*` | `custom.com:8080` | Yes (port stripped) |

If omitted, the route matches any host.

### Unmatched routes

Requests that don't match any route receive a `404` response with body `"no matching route"`. The request is still logged in the proxy tab.

## WebSocket proxying

WebSocket connections are proxied by:

1. Detecting the WebSocket upgrade (`Connection: Upgrade` + `Upgrade: websocket`)
2. Performing the HTTP upgrade with the client
3. Opening a TCP connection to the upstream
4. Sending the upgrade request (with filtered hop-by-hop headers)
5. Bidirectionally piping data between client and upstream

A route must be configured with `"ws": true` or the request must be a WebSocket upgrade request for WS proxying to activate. If a regular HTTP request hits a WS-configured route, it is proxied as normal HTTP.

On successful WebSocket connections, a log entry with status `101` is recorded. If the WebSocket upgrade or connection fails, a `502` is logged.

## TLS

If `tls_cert` and `tls_key` paths are provided in the config, the proxy accepts TLS connections. Both must be PEM-encoded — the certificate chain and a PKCS8 private key.

```
{
  "proxy": {
    "port": 443,
    "routes": [...],
    "tls_cert": "/etc/ssl/certs/fog.pem",
    "tls_key": "/etc/ssl/private/fog.pem"
  }
}
```

TLS errors (e.g. bad certificate) are logged in the proxy request log with status `0`.

## Request log

The proxy maintains a circular buffer of request log entries, sized by `max_log_entries` (default: 1000). Each entry records:

- **Method** — HTTP method (or `"WS"` for WebSocket, `"ERR"` for errors)
- **Path** — The incoming request path
- **Status** — HTTP status code (or `0` for errors)
- **Latency** — Request duration in milliseconds
- **Upstream** — The upstream target the request was forwarded to

### Filtering

In the proxy tab, press `/` to enter filter mode. Type to filter logs by method, path, status code, or upstream. Press `Enter` or `Esc` to exit filter mode.

## Logging behavior

| Scenario | Status | Logged? |
|----------|--------|---------|
| Successful proxy | Upstream status | Yes |
| No matching route | `404` | Yes |
| Upstream unreachable | `502` | Yes |
| WebSocket connected | `101` | Yes |
| WebSocket error | `502` | Yes |
| TLS accept failed | `0` (ERR) | Yes |
| Bind failure | `0` (ERR) | Yes |

## Hot-reloading

Proxy configuration (port, host, routes) is hot-reloaded when the config file changes. The proxy is automatically restarted with the new settings.
