use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::proxy::{ProxyInstance, RouteEntry};
use crate::theme::Theme;

/// How long the watcher waits for a burst of save events to settle before
/// forwarding a single "reload" signal. Editor saves emit several events;
/// without this every one would trigger a proxy restart (each blocking).
const DEBOUNCE_WINDOW: Duration = Duration::from_millis(250);

/// Spawns a background thread that watches a config file for changes and
/// returns a receiver that signals when the file has changed.
///
/// The thread exits when the returned receiver is dropped *or* when `stop` is
/// set, so repeated switches (which re-spawn a watcher) do not leak threads.
pub fn spawn_config_watcher(config_path: PathBuf, stop: Arc<AtomicBool>) -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        use notify::{EventKind, RecursiveMode, Watcher};

        // Watch the config's parent directory (or the directory itself when
        // `--config` points at one) rather than the file inode, so editors
        // that write via temp-file + rename (which replace the inode) keep
        // triggering reloads.
        let (watch_dir, target_name): (PathBuf, Option<String>) = if config_path.is_dir() {
            (config_path.clone(), Some("fog.json".to_string()))
        } else {
            (
                config_path.parent().unwrap_or(Path::new(".")).to_path_buf(),
                config_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned()),
            )
        };

        let (notify_tx, notify_rx) = std::sync::mpsc::channel();
        if let Ok(mut watcher) =
            notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    let relevant = match &target_name {
                        Some(name) => {
                            // Watching the parent dir: only react to the config
                            // file itself (survives temp-file+rename saves).
                            matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_))
                                && event.paths.iter().any(|p| {
                                    p.file_name().map(|n| n == name.as_str()).unwrap_or(false)
                                })
                        }
                        None => {
                            // `--config` points at a directory: any change is
                            // a potential fog.json rewrite.
                            matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_))
                        }
                    };
                    if relevant {
                        let _ = notify_tx.send(());
                    }
                }
            })
        {
            let _ = watcher.watch(&watch_dir, RecursiveMode::NonRecursive);
            loop {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                match notify_rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(()) => {
                        // Debounce: wait for the save burst to settle, then
                        // forward a single reload signal.
                        let settle = Instant::now() + DEBOUNCE_WINDOW;
                        while Instant::now() < settle
                            && notify_rx.recv_timeout(Duration::from_millis(50)).is_ok()
                        {
                        }
                        if tx.send(()).is_err() {
                            break;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        }
    });

    rx
}

/// Reloads configuration from a file and applies changes to the running app state.
pub fn reload_config(
    config_path: &PathBuf,
    script_name: &str,
    proxy: &mut Option<ProxyInstance>,
    theme: &mut Theme,
) {
    let contents = match std::fs::read_to_string(config_path) {
        Ok(c) => c,
        Err(_) => return,
    };
    let config: Config = match serde_json::from_str(&contents) {
        Ok(c) => c,
        Err(_) => return,
    };

    if let Some(tc) = &config.theme {
        *theme = Theme::from_config(Some(tc));
    }

    if let Some(pc) = config
        .scripts
        .get(script_name)
        .and_then(|s| s.proxy.as_ref())
        && let Some(p) = proxy
    {
        let new_routes: Vec<RouteEntry> = pc
            .routes
            .iter()
            .map(|r| RouteEntry {
                path: r.path.clone(),
                host: r.host.clone(),
                upstream: r.upstream.clone(),
                ws: r.ws.unwrap_or(false),
            })
            .collect();
        let new_host = pc.host.clone().unwrap_or_else(|| "0.0.0.0".to_string());
        if pc.port != p.port || new_host != p.host || new_routes != p.routes {
            p.port = pc.port;
            p.host = new_host;
            p.routes = new_routes;
            p.restart();
        }
    }
}
