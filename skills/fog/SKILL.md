---
name: fog
description: Run and inspect a dev environment with fog, detached from the TUI. Use when a repo has a fog.json or the user asks to start, stop, or read the logs of a dev environment, or to run services on a branch or worktree. Covers `fog <script> -d`, `fog ls`, `fog logs`, `fog kill`, and coexisting with a human or another agent on the same branch. Not for authoring fog.json from scratch — see the schema.
---

# Using fog

fog runs a script's services — each in its own PTY — with health checks, `depends_on`
ordering, and an optional reverse proxy. An instance is identified by
`(project, script, branch)`, and scripts are **concurrent** by default: a human in the TUI
and an agent running detached share one environment without killing each other.

Your job as an agent is to drive fog **detached** and observe it through its commands.

## Agent rules

- **Run detached.** `fog <script> -d` starts in the background and returns; bare
  `fog <script>` opens the interactive TUI and blocks forever.
- **Kill only pids you started** (or ones the user names). A human or another agent may
  share the `(project, script, branch)`; killing it tears down their services too.
- **Pass explicit pids.** A bare `fog kill` / `fog logs` resolves against the local
  `fog.json` and can misfire when several instances run.
- **Probe before borrowing.** A `share: true` service needs a `health_check`; without one
  fog starts a duplicate — a second database. A shared resource also needs a fixed port.

## Process

### 1. Orient

Find `fog.json` in the current directory (or pass `-c <path>`, or a directory containing
it). Run `fog` with no script to list the scripts it defines.

Done when you can name the script you will run and the config path.

### 2. Start detached

```bash
fog dev -d                      # background daemon; prints "daemon started pid=<pid>"
fog dev -d --branch feature-x   # run the worktree checked out on that branch
fog dev -d --no-share           # ignore share/reuse, start everything fresh
```

Done when `fog ls` shows your instance on the expected branch.

### 3. Inspect

```bash
fog ls                     # every running instance (ignores a pid argument)
fog logs <pid>             # list that instance's services + status (daemon and proxy included)
fog logs <pid> -s api      # print one service's captured output, ANSI stripped
fog logs <pid> -s proxy    # the reverse-proxy request log
```

`fog ls` prints one row per instance — `pid script project branch proxy services` — with
each service's state (`healthy`, `reused`, `running`, `stopped`, `unhealthy`).

Done when you have read the state — and the log, if it matters — of every service you care
about.

### 4. Stop

```bash
fog kill <pid>             # graceful: each service's shutdown_cmd runs
fog kill <pid> --force     # escalate to SIGTERM then SIGKILL for a wedged instance
fog restart <pid>          # restart a running instance
fog kill --all             # every local instance (or every instance outside a fog.json dir)
```

The index server (the optional web UI/API on `127.0.0.1:18080`) is host-global; control it
with `fog index serve|kill|restart`. Its write endpoints are localhost-only — never expose
port 18080.

Done when `fog ls` no longer lists the instance you stopped.

## Coexisting with a human or another agent

Every script is concurrent by default: re-running `fog <script>` on the same branch starts
alongside the existing instance instead of killing it, and different branches always run
side-by-side.

- **Per-instance services** start fresh in each instance. Give them random ports so they
  never collide — `"ports": { "api": 0 }` — and reference `${ports.api}`.
- **Shared services** (`"share": true`) are **borrowed**: when a sibling already has one
  healthy, fog reuses it (`♻ reusing already-running`) instead of starting a second. Fog
  **reclaims** it only when the last same-branch instance exits. `reuse: true` is the
  single-instance (`"concurrent": false`) equivalent, handed over live on a re-run.
- **Branch isolation** comes from `--branch` and the injected `FOG_BRANCH` (slug) and
  `FOG_BRANCH_RAW` variables, so per-branch compose projects and hostnames never collide.

## Reference

- [references/config-touchpoints.md](references/config-touchpoints.md) — the config fields
  that change how you run fog, plus the pitfalls.
- JSON Schema: https://raw.githubusercontent.com/Naputt1/fog/main/fog.schema.json
- Docs: [configuration](https://naputt1.github.io/fog/configuration) ·
  [agentic](https://naputt1.github.io/fog/agentic) ·
  [index server](https://naputt1.github.io/fog/index-server) ·
  [troubleshooting](https://naputt1.github.io/fog/troubleshooting)
