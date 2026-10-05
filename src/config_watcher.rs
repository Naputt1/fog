use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::proxy::{ProxyInstance, RouteEntry};
use crate::theme::Theme;

/// Builds a [`ProxyInstance`] from a script's proxy config, mirroring what the
/// startup path in `runtime::build_with_opts` does: template routes are resolved
/// against the live port map and branch, then the proxy is constructed (but not
/// started — the caller starts it).
///
/// Reload cannot re-resolve `${ports.*}` templates reliably (the port map is a
/// startup snapshot), so a config introducing templates is rejected rather than
/// silently forwarding to an unresolved `${...}` upstream.
fn proxy_from_config(
    pc: &crate::config::ProxyConfig,
    terminal_cfg: Option<crate::config::TerminalConfig>,
    ports: &crate::ports::PortMap,
    branch: Option<&str>,
) -> Result<ProxyInstance, String> {
    let mut routes = Vec::with_capacity(pc.routes.len());
    for r in &pc.routes {
        let mut upstream = r.upstream.clone();
        if crate::ports::has_template(&upstream) {
            upstream = crate::ports::resolve_template(&upstream, ports, branch)
                .map_err(|e| format!("proxy upstream template error: {e}"))?;
        }
        let mut host = r.host.clone();
        if let Some(h) = &host
            && crate::ports::has_template(h)
        {
            host = Some(
                crate::ports::resolve_template(h, ports, branch)
                    .map_err(|e| format!("proxy host template error: {e}"))?,
            );
        }
        routes.push(RouteEntry {
            path: r.path.clone(),
            host,
            upstream,
            ws: r.ws.unwrap_or(false),
        });
    }
    Ok(ProxyInstance::new(
        pc.port,
        pc.host.clone(),
        routes,
        pc.max_log_entries.unwrap_or(1000),
        pc.tls_cert.clone(),
        pc.tls_key.clone(),
    )
    .with_terminal_config(terminal_cfg.unwrap_or_default()))
}

/// How long the watcher waits for a burst of save events to settle before
/// forwarding a single "reload" signal. Editor saves emit several events;
/// without this every one would trigger a proxy restart (each blocking).
const DEBOUNCE_WINDOW: Duration = Duration::from_millis(250);

/// Spawns a background thread that watches a config file for changes.
///
/// Returns the receiver that signals on each change, and the stop flag that
/// owns the thread. The caller must keep the flag and set it when replacing or
/// dropping the watcher; the thread observes it within ~100ms and exits. The
/// receiver alone cannot stop it: `tx.send` only fails on the *next* change.
pub fn spawn_config_watcher(config_path: PathBuf) -> (mpsc::Receiver<()>, Arc<AtomicBool>) {
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();

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
            if watcher
                .watch(&watch_dir, RecursiveMode::NonRecursive)
                .is_err()
            {
                // Nothing to watch (missing parent, permissions): exit rather
                // than spin forever with a dead watcher.
                return;
            }
            loop {
                if thread_stop.load(Ordering::SeqCst) {
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

    (rx, stop)
}

/// Reloads configuration from a file and applies changes to the running app state.
///
/// On success returns the resolved [`Config`]. On failure returns an error that
/// names the offending path and the underlying reason; the previous config is
/// left in effect (nothing is mutated before parsing succeeds).
///
/// Proxy presence is reconciled: a new `proxy` block starts one, removing it
/// stops (and frees) the running one, and changing an existing one restarts it.
pub fn reload_config(
    config_path: &Path,
    script_name: &str,
    proxy: &mut Option<ProxyInstance>,
    theme: &mut Theme,
    ports: &crate::ports::PortMap,
    branch: Option<&str>,
) -> Result<Config, String> {
    let contents = std::fs::read_to_string(config_path).map_err(|e| {
        format!(
            "config reload: cannot read '{}': {e}",
            config_path.display()
        )
    })?;
    let config: Config = serde_json::from_str(&contents).map_err(|e| {
        format!(
            "config reload: invalid JSON in '{}': {e}",
            config_path.display()
        )
    })?;

    let pc = config
        .scripts
        .get(script_name)
        .and_then(|s| s.proxy.as_ref());
    let terminal_cfg = config
        .scripts
        .get(script_name)
        .and_then(|s| s.terminal.clone());

    // Resolve the target proxy *before* mutating any live state, so a template
    // error leaves the previous config (and theme) fully in effect.
    let target = match pc {
        Some(pc) => Some(proxy_from_config(pc, terminal_cfg, ports, branch)?),
        None => None,
    };

    if let Some(tc) = &config.theme {
        *theme = Theme::from_config(Some(tc));
    }

    match (target, proxy.as_mut()) {
        // Existing proxy: reconfigure in place, restarting only when a field
        // that affects the running listener changed.
        (Some(target), Some(p)) => {
            p.reconfigure(target);
        }
        // `None -> Some`: the config added a proxy; start it now.
        (Some(mut target), None) => {
            target.start();
            *proxy = Some(target);
        }
        // `Some -> None`: the config dropped the proxy; shut it down. Replacing
        // the `Option` drops the old instance, which joins its listener thread.
        (None, Some(_)) => {
            *proxy = None;
        }
        (None, None) => {}
    }

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(dir: &Path, json: &str) -> PathBuf {
        let path = dir.join("fog.json");
        std::fs::write(&path, json).unwrap();
        path
    }

    fn proxy_config(port: u16) -> String {
        format!(r#"{{"scripts":{{"dev":{{"proxy":{{"port":{port},"routes":[]}}}}}}}}"#)
    }

    #[test]
    fn test_reload_invalid_json_returns_error_and_preserves_state() {
        let dir = std::env::temp_dir().join(format!("fog-reload-invalid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = write_config(&dir, "this is not json");

        let mut proxy: Option<ProxyInstance> = None;
        let mut theme = Theme::from_config(None);
        let ports = crate::ports::PortMap::new();

        let err = reload_config(&path, "dev", &mut proxy, &mut theme, &ports, None)
            .expect_err("invalid JSON must surface an error");
        assert!(
            err.contains("invalid JSON") && err.contains("fog.json"),
            "error should name the cause and path: {err}"
        );
        // Nothing was mutated: no proxy was started from the bad config.
        assert!(proxy.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_reload_toggles_proxy_presence() {
        let dir = std::env::temp_dir().join(format!("fog-reload-toggle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ports = crate::ports::PortMap::new();
        let mut theme = Theme::from_config(None);
        let mut proxy: Option<ProxyInstance> = None;

        // None -> Some: config adds a proxy (port 0 = OS-assigned, no conflict
        // with any other test's listener).
        let path = write_config(&dir, &proxy_config(0));
        reload_config(&path, "dev", &mut proxy, &mut theme, &ports, None).unwrap();
        assert!(
            proxy.is_some(),
            "proxy must be started when added to config"
        );
        assert!(proxy.as_ref().unwrap().is_running());

        // Some -> None: config drops the proxy.
        let path = write_config(&dir, r#"{"scripts":{"dev":{}}}"#);
        reload_config(&path, "dev", &mut proxy, &mut theme, &ports, None).unwrap();
        assert!(proxy.is_none(), "proxy must be shut down when removed");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_reload_updates_existing_proxy_in_place() {
        let dir = std::env::temp_dir().join(format!("fog-reload-update-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ports = crate::ports::PortMap::new();
        let mut theme = Theme::from_config(None);
        let mut proxy: Option<ProxyInstance> = None;

        let path = write_config(&dir, &proxy_config(0));
        reload_config(&path, "dev", &mut proxy, &mut theme, &ports, None).unwrap();
        assert!(proxy.as_ref().unwrap().is_running());

        // A route change restarts the existing instance; presence is unchanged.
        let path = write_config(
            &dir,
            r#"{"scripts":{"dev":{"proxy":{"port":0,"routes":[{"path":"/","upstream":"http://127.0.0.1:9"}]}}}}"#,
        );
        reload_config(&path, "dev", &mut proxy, &mut theme, &ports, None).unwrap();
        assert!(proxy.is_some());
        let p = proxy.as_ref().unwrap();
        assert_eq!(p.routes.len(), 1);
        assert_eq!(p.routes[0].upstream, "http://127.0.0.1:9");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
