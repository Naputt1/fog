---
title: fog
---

# fog

**Terminal-based service orchestrator & reverse-proxy dashboard.**

fog lets you define a set of local services, launch them simultaneously inside
pseudo-terminals, view their real-time colored output with scrollback, and
optionally expose a reverse proxy that routes traffic to them — all in a single
terminal interface.

<div style="margin: 2rem 0; text-align: center">
  <a href="/fog/getting-started" style="display: inline-block; padding: 0.6rem 1.8rem; background: var(--vp-c-brand-1); color: #fff; border-radius: 8px; text-decoration: none; font-weight: 600;">Get Started</a>
  <a href="https://github.com/Naputt1/fog" style="display: inline-block; padding: 0.6rem 1.8rem; margin-left: 0.8rem; border: 1px solid var(--vp-c-brand-1); color: var(--vp-c-brand-1); border-radius: 8px; text-decoration: none; font-weight: 600;">GitHub</a>
</div>

## Features

- **Multi-service orchestration** — Spawn each service in its own PTY, with full VT100/ANSI color rendering and scrollback (up to 2000 lines, configurable).
- **Built-in reverse proxy** — HTTP/1.1 and WebSocket proxy with a live request log showing method, path, status code, latency, and upstream target.
- **Live sidebar** — Vertical sidebar with status indicators (● running, ● healthy, ○ stopped, ● unhealthy). Click to switch tabs.
- **Mouse interaction** — Click tabs, scroll with the wheel, drag-select text (copied to system clipboard via OSC 52).
- **Keyboard navigation** — Vim-style `j`/`k` tab switching, terminal input mode (`i`), restart services (`R`), open shell tabs (`t`).
- **Configuration hot-reload** — Edit your config at runtime to update themes and proxy settings without restarting.
- **TLS support** — Terminate TLS connections directly in the proxy using PEM certificates.
- **Health checks** — Periodic TCP health checks per service with sidebar status indicators.
