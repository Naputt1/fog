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
  ├── Parses CLI args (clap): `fog <script>` | `fog ls` | `fog kill [pid]`
  ├── Loads config, looks up the named script's services & proxy
  ├── Spawns IPC server (Unix socket) sharing IpcState with the App
  ├── Spawns Terminal for each service entry
  ├── Creates ProxyInstance (if the script configures one)
  ├── Spawns config watcher
  └── Runs App::run() in ratatui terminal
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

1. **Startup**: Parse config, spawn terminals, start proxy, enter TUI. Before spawning, fog detects the git project (`git rev-parse --git-common-dir`) and reclaims any running instance of the same script in the same project: it sends a `kill` request carrying the `reuse` service names over IPC. The old instance hands over live reused services (PTY master fd via `SCM_RIGHTS` + scrollback) and then exits.
2. **Running**: Event loop polls input at 50ms intervals, draws UI, handles events.
3. **Shutdown**: On `q` / `Ctrl+C` / SIGINT:
   - `exit` flag is set
   - Each `Terminal` drops → kills child process group (SIGTERM, wait 500ms, SIGKILL), kills descendants; reuse-flagged services skip their `shutdown_cmd` so shared resources survive
   - `ProxyInstance` drops → sets shutdown flag, joins proxy thread
   - Terminal leaves raw mode, restores alternate screen, disables mouse capture
   - If `--save-logs` was passed, writes output files to `temp/<name>.txt`

## Key design decisions

- **PTY-based spawning** (via `portable-pty`): Services run in pseudo-terminals rather than as simple subprocesses, enabling full VT100/ANSI color rendering, interactive shell support, and proper terminal semantics
- **Synchronous TUI + async proxy**: The main thread runs a synchronous event loop with `crossterm::event::poll`; the proxy runs on a background thread with its own Tokio runtime — avoids blocking the UI
- **Line caching**: Each `Terminal` caches rendered styled lines per (offset, count, generation) tuple. The generation counter increments on every new data write, so the cache is invalidated exactly when content changes. This avoids re-parsing on every frame for unchanged content.
- **No `unwrap()` in production code**: Production paths use `expect()` with context or `?`. Tests may use `unwrap()`.
- **Configuration hot-reload**: Theme and proxy settings can be updated without restarting fog. Service definitions require a restart.
- **OSC 52 clipboard**: Text selection copies to clipboard via the OSC 52 escape sequence, supported by iTerm2, kitty, tmux, and other modern terminals — no external clipboard tooling needed.
