---
title: Architecture
---

# Architecture

fog is a terminal-based service orchestrator built in Rust using [ratatui](https://github.com/ratatui-org/ratatui) for the TUI and [hyper](https://github.com/hyperium/hyper) for the reverse proxy.

## Threading model

fog runs three concurrent threads:

```
┌────────────────────────────────────────┐
│         Main Thread (TUI)              │
│  - ratatui event loop                  │
│  - Keyboard/mouse handling             │
│  - Terminal PTY read/write             │
│  - UI rendering                        │
│                                        │
│  App::run()                             │
│    ├── draw() ← every 50ms             │
│    └── handle_events() ← on input       │
└────────────────────────────────────────┘

┌────────────────────────────────────────┐
│      Background: Proxy Thread          │
│  - Own Tokio current-thread runtime    │
│  - HTTP/1.1 + WebSocket proxying       │
│  - Request log buffer (Arc<Mutex>)     │
│  - TLS termination (rustls)            │
│                                        │
│  ProxyInstance::start()                 │
│    └── thread::spawn()                  │
│        └── tokio::runtime::block_on()   │
└────────────────────────────────────────┘

┌────────────────────────────────────────┐
│   Background: Config Watcher Thread    │
│  - notify file watcher                 │
│  - Signals main loop on config change  │
│                                        │
│  spawn_config_watcher()                 │
│    └── thread::spawn()                  │
│        └── notify::Watcher             │
└────────────────────────────────────────┘

    Per Terminal:
┌────────────────────────────────────────┐
│   Background: PTY Reader Thread        │
│  - Reads child process stdout          │
│  - Feeds vt100 parser                  │
│  - Increments screen generation        │
│                                        │
│  spawn_reader()                         │
│    └── thread::spawn()                  │
│        └── loop { reader.read() }      │
└────────────────────────────────────────┘

    Per Terminal (if health_check configured):
┌────────────────────────────────────────┐
│   Background: Health Check Thread      │
│  - Periodic TCP connect checks         │
│  - Updates HealthStatus (Arc<Mutex>)   │
│                                        │
│  Terminal::start_health_checks()        │
│    └── thread::spawn()                  │
│        └── loop { TcpStream::connect } │
└────────────────────────────────────────┘
```

### Inter-thread communication

- **Main ↔ Proxy**: `Arc<AtomicBool>` flags for running/shutdown control, `Arc<Mutex<VecDeque<LogEntry>>>` for request log sharing
- **Main ↔ Config watcher**: `std::sync::mpsc::Receiver<()>` — receives a signal when the config file changes
- **Main ↔ PTY reader**: `Arc<Mutex<vt100::Parser>>` and `Arc<AtomicUsize>` generation counter — the main thread reads styled output, the reader thread writes raw data
- **Main ↔ Health check**: `Arc<Mutex<HealthStatus>>` — health check thread writes status, main thread reads it during rendering

## Component architecture

```
main.rs
  │
  ├── Parses CLI args (clap): `fog <script>` | `fog ls` | `fog kill [pid]` | `fog logs [pid]`; `-d` runs headlessly as a daemon
  ├── Loads config, looks up the named script's services & proxy
  ├── Single-instance scripts only (`"concurrent": false`): acquires a
  │   per-(project, script, branch) owner lock and reclaims any existing
  │   instance of the same script in the same project+branch. Concurrent
  │   scripts (default) skip coordination and start alongside existing instances.
  ├── Spawns IPC server (Unix socket) sharing IpcState with the App
  ├── Spawns Terminal for each service entry
  ├── Releases the owner lock (services are up)
  ├── Creates ProxyInstance (if the script configures one)
  ├── Spawns config watcher (TUI runs only; detached daemons skip it)
  └── Runs App::run() in ratatui terminal, or App::run_headless() when detached
       │
       └── App
            ├── items: Vec<Terminal>      ← service terminals
            ├── proxy: Option<ProxyInstance>
            ├── tabs: ClickTab            ← sidebar widget
            ├── theme: Theme
            ├── ipc_state: Arc<IpcState>  ← published to the IPC socket
            └── mode: Mode                ← Normal / TerminalInput / ProxyFilter
                 │
                 ├── draw()
                 │    ├── ClickTab::draw()       ← sidebar
                 │    ├── draw_terminal_content() ← terminal output
                 │    └── draw_proxy_content()   ← proxy logs
                 │
                 └── handle_events()
                      ├── handle_key()           ← keyboard dispatch
                      └── Mouse events           ← tab switch, scroll, select
```

## VT100 parsing pipeline

```
Child process stdout
       │
       ▼
PTY reader thread
  reads raw bytes in 4KB chunks
       │
       ▼
vt100::Parser::process(bytes)
  parses ANSI escape sequences
  maintains scrollback buffer
       │
       ▼
Arc<Mutex<vt100::Parser>> shared state
       │
       ▼
Terminal::get_screen(visible_rows, offset)
  │
  ├── Check line_cache (offset, count, generation)
  │     └── Cache hit? Return cached styled lines
  │
  └── Cache miss?
        ├── Lock parser, set scrollback position
        ├── Iterate cells → build Vec<Line<Span>> with ANSI styles
        ├── Update cache
        └── Return styled lines
               │
               ▼
        selection::apply_sel()  ← highlight selection ranges
               │
               ▼
        ratatui Paragraph widget  ← render to screen
```

## Process lifecycle

1. **Startup**: Parse config, spawn terminals, start proxy, enter TUI. Before spawning, fog detects the git project (`git rev-parse --git-common-dir`). For **single-instance** scripts (`"concurrent": false`) it coordinates with any other instance running the same script in the same project+branch (see *Cross-instance coordination* below): if it should take over, it sends a `kill` request carrying the `reuse` service names over IPC; the old instance hands over live reused services (PTY master fd via `SCM_RIGHTS`) and then exits. **Concurrent** scripts (the default) skip this entirely and start alongside existing instances; services flagged `share: true` are borrowed (not re-started) when their `health_check` already passes.
2. **Running**: Event loop polls input at 50ms intervals, draws UI, handles events.
3. **Shutdown**: On `q` / `Ctrl+C` / SIGINT, or a replacement's `kill` request:
   - `exit` flag is set
   - On a reclaim (`kill` with `reuse` names), the App extracts the requested live services, then waits for the IPC thread to send them before dropping terminals
   - Each `Terminal` drops → kills child process group (SIGTERM, wait 500ms, SIGKILL), kills descendants; services that were handed off are released without being killed, and their `shutdown_cmd` is skipped so the live successor keeps the resource. Any other service — including a borrowed/assumed-up reuse/share service with no successor — runs its `shutdown_cmd` on drop, so the last instance tears the resource down. Shared (reuse/share) services skip their `shutdown_cmd` whenever **any** sibling instance still serves the same (project, script) — a concurrent instance on the same branch or another branch.
   - `ProxyInstance` drops → sets shutdown flag, joins proxy thread
   - Terminal leaves raw mode, restores alternate screen, disables mouse capture
   - If `--save-logs` was passed, writes output files to `temp/<name>.txt`

## Cross-instance coordination

Worktree-aware runs are made deterministic by a per-(project, script) **owner lock** (`src/lock.rs`) using `flock(2)` on a temp file (`fog-owner-<hash>.lock`). `flock` is released automatically when the holding process dies, so stale locks are impossible.

- The lock is held **only for the startup critical section** (scan → reclaim → spawn services). Once the instance is serving, the lock is dropped so a later worktree switch can replace it.
- On startup, if the lock is free, the instance reclaims any existing (old) instance under the lock.
- If the lock is held, another instance is mid-start; the new instance waits (up to 30s), re-acquires, and re-scans:
  - an instance that **started after this attempt began** is a concurrent starter that already won → fog backs off with an error (`fog kill <pid>` replaces it), and
  - an older instance is reclaimed normally.
- The reclaim protocol is **single-winner**: the first `kill`+`reuse` connection claims the handoff; a second concurrent reclaim (or a plain `kill`) is refused instead of racing for the same live processes. Handoffs are only sent after the App confirms it prepared them, and a process whose fd cannot be transferred is killed to avoid orphans.

`find_instances` ignores lock files: it only matches `fog-<pid>.sock` sockets.

## Key design decisions

- **PTY-based spawning** (via `portable-pty`): Services run in pseudo-terminals rather than as simple subprocesses, enabling full VT100/ANSI color rendering, interactive shell support, and proper terminal semantics
- **Synchronous TUI + async proxy**: The main thread runs a synchronous event loop with `crossterm::event::poll`; the proxy runs on a background thread with its own Tokio runtime — avoids blocking the UI
- **Line caching**: Each `Terminal` caches rendered styled lines per (offset, count, generation) tuple. The generation counter increments on every new data write, so the cache is invalidated exactly when content changes. This avoids re-parsing on every frame for unchanged content.
- **No `unwrap()` in production code**: Production paths use `expect()` with context or `?`. Tests may use `unwrap()`.
- **Configuration hot-reload**: Theme and proxy settings can be updated without restarting fog. Service definitions require a restart.
- **OSC 52 clipboard**: Text selection copies to clipboard via the OSC 52 escape sequence, supported by iTerm2, kitty, tmux, and other modern terminals — no external clipboard tooling needed.
