use std::path::PathBuf;
use std::sync::mpsc;

use crate::config::Config;
use crate::proxy::{ProxyInstance, RouteEntry};
use crate::theme::Theme;

/// Spawns a background thread that watches a config file for changes.
/// Returns a receiver that signals when the file has changed.
pub fn spawn_config_watcher(config_path: PathBuf) -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        use notify::{EventKind, RecursiveMode, Watcher};

        let (notify_tx, notify_rx) = std::sync::mpsc::channel();
        if let Ok(mut watcher) =
            notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    match event.kind {
                        EventKind::Modify(_) | EventKind::Create(_) => {
                            let _ = notify_tx.send(());
                        }
                        _ => {}
                    }
                }
            })
        {
            let _ = watcher.watch(&config_path, RecursiveMode::NonRecursive);
            loop {
                if notify_rx.recv().is_ok() {
                    let _ = tx.send(());
                }
            }
        }
    });

    rx
}

/// Reloads configuration from a file and applies changes to the running app state.
pub fn reload_config(config_path: &PathBuf, proxy: &mut Option<ProxyInstance>, theme: &mut Theme) {
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

    if let Some(ref pc) = config.proxy
        && let Some(p) = proxy
    {
        let new_routes: Vec<RouteEntry> = pc
            .routes
            .iter()
            .map(|r| RouteEntry {
                path: r.path.clone(),
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
