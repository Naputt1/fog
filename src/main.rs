#![deny(unsafe_op_in_unsafe_fn)]

use std::io::stdout;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::{fs, io};
use clap::Parser;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use fog::app::App;
use fog::config::Config;
use fog::proxy::{ProxyInstance, RouteEntry};
use fog::terminal::Terminal;
use fog::theme::Theme;
use std::sync::mpsc;

const DEFAULT_SCROLLBACK: usize = 2000;

/// Command-line interface arguments parsed via clap.
#[derive(Parser)]
#[command(name = "fog", version = env!("CARGO_PKG_VERSION"))]
struct Cli {
    /// Path to the configuration file. Defaults to `fog.json`.
    #[arg(short, long, default_value = "fog.json")]
    config: std::path::PathBuf,

    /// Save service output to `temp/<name>.txt` on exit.
    #[arg(long, help = "Save service output to temp/<name>.txt on exit")]
    save_logs: bool,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();

    let contents = match fs::read_to_string(&cli.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: could not read config '{}': {}", cli.config.display(), e);
            std::process::exit(1);
        }
    };

    let config: Config = match serde_json::from_str(&contents) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: invalid config '{}': {}", cli.config.display(), e);
            std::process::exit(1);
        }
    };

    let config_path = cli
        .config
        .canonicalize()
        .unwrap_or_else(|_| cli.config.clone());
    let config_dir = config_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();

    let sigint = Arc::new(AtomicBool::new(false));
    let sig = sigint.clone();
    if ctrlc::set_handler(move || {
        sig.store(true, Ordering::SeqCst);
    }).is_err() {
        eprintln!("warning: could not set Ctrl+C handler");
    }

    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)?;

    let scrollback = config.max_scrollback.unwrap_or(DEFAULT_SCROLLBACK);
    let sidebar_min = config.sidebar.as_ref().and_then(|s| s.min_width).unwrap_or(12);
    let sidebar_max = config.sidebar.as_ref().and_then(|s| s.max_width).unwrap_or(30);
    let theme = Theme::from_config(config.theme.as_ref());

    let items: Vec<Terminal> = config
        .service
        .unwrap_or_default()
        .into_iter()
        .map(|entry| {
            let service_path = config_dir.join(&entry.path);
            let name = entry.name.clone().unwrap_or_else(|| {
                service_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
            let service_path = service_path.to_string_lossy().into_owned();
            match Terminal::spawn_command(&service_path, &entry.cmd, name.clone(), scrollback) {
                Ok(mut t) => {
                    t.save_logs = cli.save_logs;
                    t.health_check = entry.health_check.clone();
                    t.start_health_checks();
                    t
                }
                Err(e) => Terminal::spawn_error(name, format!("Failed to spawn: {e}"), scrollback),
            }
        })
        .collect();

    let proxy = config.proxy.map(|pc| {
        let routes: Vec<RouteEntry> = pc
            .routes
            .into_iter()
            .map(|r| RouteEntry {
                path: r.path,
                upstream: r.upstream,
                ws: r.ws.unwrap_or(false),
            })
            .collect();
        let max_log_entries = pc.max_log_entries.unwrap_or(1000);
        let mut p = ProxyInstance::new(pc.port, routes, max_log_entries, pc.tls_cert, pc.tls_key);
        p.start();
        p
    });

    let (config_tx, config_rx) = mpsc::channel();

    let watch_path = config_path.clone();
    std::thread::spawn(move || {
        use notify::{EventKind, RecursiveMode, Watcher};
        let (tx, rx) = std::sync::mpsc::channel();
        if let Ok(mut watcher) = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            if let Ok(event) = res {
                match event.kind {
                    EventKind::Modify(_) | EventKind::Create(_) => {
                        let _ = tx.send(());
                    }
                    _ => {}
                }
            }
        }) {
            let _ = watcher.watch(&watch_path, RecursiveMode::NonRecursive);
            loop {
                if rx.recv().is_ok() {
                    let _ = config_tx.send(());
                }
            }
        }
    });

    ratatui::run(|terminal| App::new(items, proxy, sigint, scrollback, sidebar_min, sidebar_max, theme, config_path, config_rx).run(terminal))?;

    disable_raw_mode()?;
    execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture)?;

    Ok(())
}
