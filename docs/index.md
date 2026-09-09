---
layout: home
title: fog
titleTemplate: Dev-environment orchestrator for humans and coding agents
hero:
  name: fog
  text: The dev-environment orchestrator for humans and coding agents
  tagline: You and your agent run the same fog dev on the same branch — concurrently, without killing each other. Shared DBs get borrowed, your agent runs headless, and you check everything from your phone. Named scripts in fog.json, each service in its own PTY, plus a built-in reverse proxy.
  actions:
    - theme: brand
      text: Get Started
      link: /getting-started
    - theme: alt
      text: View on GitHub
      link: https://github.com/Naputt1/fog
features:
  - title: Humans + agents on one environment
    details: fog dev and fog dev -d on the same branch coexist — shared DBs are borrowed (share: true), ports are randomized per instance, logs stream to the web UI. See the Agentic Worktrees guide.
  - title: Branches side-by-side
    details: Run fog dev on main and feature-x at once; s switches worktrees in-place in the TUI. Same branch can run twice — you and an agent share the DB without killing each other.
  - title: Phone overview
    details: Check status and live logs at http://<tailnet IP> from your phone — no DNS setup. Served by the host-global index server.
  - title: One command per service
    details: Each service in its own PTY with full ANSI color and scrollback. health_check, depends_on, and restart with R.
  - title: Built-in proxy
    details: Reverse proxy with request log and WebSocket support. Host-global Traefik router with wildcard *.acme DNS when you need it.
  - title: Simple config
    details: One fog.json with named scripts (fog dev). Ports templating, native_routes, worktree-aware sharing.
---

![fog demo](https://raw.githubusercontent.com/Naputt1/fog/main/assets/demo.gif)
