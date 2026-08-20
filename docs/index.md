---
layout: home
title: fog
titleTemplate: Terminal service orchestrator & reverse-proxy dashboard
hero:
  name: fog
  text: Service orchestrator & reverse proxy
  tagline: Named scripts in fog.json — each service in its own PTY with color and scrollback, plus an optional reverse proxy. Switch branches with s, share a DB with an agent, check logs from your phone.
  actions:
    - theme: brand
      text: Get Started
      link: /getting-started
    - theme: alt
      text: View on GitHub
      link: https://github.com/Naputt1/fog
features:
  - title: Branches side-by-side
    details: Run fog dev on main and feature-x at once; s to switch in the TUI. Same branch can run twice — you and an agent share the DB without killing each other.
  - title: Phone overview
    details: Check status and live logs at http://<tailnet IP> from your phone — no DNS setup. Served by the host-global index server.
  - title: One command per service
    details: Each service in its own PTY with full ANSI color and scrollback. health_check, depends_on, and restart with R.
  - title: Built-in proxy
    details: Reverse proxy with request log and WebSocket support. Host-global Traefik router with wildcard *.acme DNS when you need it.
  - title: Simple config
    details: One fog.json with named scripts (fog dev). Ports templating, native_routes, worktree-aware sharing.
  - title: Agentic-ready
    details: Concurrent by default — human and agent run the same script on the same branch. See Agentic Worktrees.
---
