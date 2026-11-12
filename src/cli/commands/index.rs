//! `fog index` and its subcommands: the standalone index server.
//!
//! These are dispatched before clap, because `fog index serve` would otherwise
//! be misparsed as the `[PID]` positional (which expects a number).

use crate::cli::presentation::error_label;
use crate::cli::{fail, usage_error};
use std::io;

/// Loads config for `fog index restart` — best-effort, no exit on failure.
fn load_runtime_config_for_index() -> crate::config::Config {
    crate::index::load_runtime_config()
}

/// Parses `fog index serve` flags: `--foreground` runs in the foreground
/// (blocking); otherwise the server detaches into the background by default.
/// `--port N` / `--port=N` / `-p N` overrides the configured port.
fn parse_index_serve_args(args: &[String]) -> (bool, Option<u16>) {
    let mut foreground = false;
    let mut port: Option<u16> = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--foreground" {
            foreground = true;
        } else if a == "--port" || a == "-p" {
            i += 1;
            match args.get(i).and_then(|s| s.parse::<u16>().ok()) {
                Some(p) => port = Some(p),
                None => usage_error(
                    &format!("{a} requires a port number"),
                    "e.g. `fog index serve --port 18080`",
                ),
            }
        } else if let Some(v) = a.strip_prefix("--port=") {
            match v.parse::<u16>() {
                Ok(p) => port = Some(p),
                Err(_) => usage_error(
                    &format!("invalid --port value '{v}'"),
                    "pass a port between 1 and 65535",
                ),
            }
        } else if a == "-h" || a == "--help" {
            println!("Usage: fog index serve [--foreground] [--port N]");
            println!();
            println!("Run the index server. Detaches into the background by default;");
            println!("pass --foreground to block in the terminal.");
            std::process::exit(0);
        } else {
            usage_error(
                &format!("unknown flag '{a}' for `fog index serve`"),
                "see `fog index serve --help`",
            );
        }
        i += 1;
    }
    // The detached child (FOG_INDEX_CHILD=1) must block; it never re-detaches.
    if std::env::var_os("FOG_INDEX_CHILD").is_some() {
        foreground = true;
    }
    (foreground, port)
}

/// Entry point for `fog index serve`: detaches into a background service by
/// default (logs to `$TMPDIR/fog-index-<port>.logs/daemon.log`), or blocks
/// with `--foreground`.
pub(crate) fn cmd_index_serve(args: &[String]) -> io::Result<()> {
    let (foreground, explicit_port) = parse_index_serve_args(args);
    let port = crate::index::resolve_serve_port(explicit_port);
    let network = crate::index::resolve_serve_network();
    if foreground {
        crate::index::serve_foreground(port, network)
    } else {
        crate::index::serve_detached(port, &network)
    }
}

/// Usage shown by `fog index` with no (or an unknown) subcommand.
/// Usage shown by `fog index` with no (or an unknown) subcommand.
const INDEX_USAGE: &str = "\
Usage: fog index <command>

Commands:
  serve    Run the index server (detached by default)
  kill     Stop the index server
  restart  Restart the index server

Options for `serve`:
  --foreground  Run in the foreground instead of detaching
  --port <N>    Listen on this port instead of the configured one
";

/// Dispatches `fog index ...` before clap parses the `[PID]` positional.
pub fn dispatch(argv: &[String]) -> io::Result<()> {
    match argv.get(1).map(String::as_str) {
        Some("serve") => cmd_index_serve(&argv[2..]),
        Some("kill") => {
            // `fog index kill` terminates the index server unconditionally.
            let killed = crate::index::kill_server(None);
            if killed {
                println!("index server stopped");
            } else {
                eprintln!("index server not running");
            }
            Ok(())
        }
        Some("restart") => {
            // `fog index restart` kills then re-ensures the server. This is
            // a manual override that ignores `index.enabled:false` so the
            // user can force-start the index even for projects that opt out
            // of auto-serving it on `fog <script>`.
            let cfg = load_runtime_config_for_index();
            crate::index::kill_for_config(&cfg);
            std::thread::sleep(std::time::Duration::from_millis(300));
            let port = cfg.index_port();
            let network = cfg.index_network();
            let verbose = argv.iter().any(|a| a == "-v" || a == "--verbose");
            crate::log::emit(&crate::index::ensure_with_port(port, &network), verbose);
            if crate::index::is_server_started(port) {
                println!("index server restarted on :{port}");
                println!(
                    "  logs: {}",
                    crate::index::index_log_dir(port)
                        .join("daemon.log")
                        .display()
                );
            } else {
                fail(&format!("index server did not start on :{port}"));
            }
            Ok(())
        }
        Some("-h") | Some("--help") | Some("help") => {
            print!("{INDEX_USAGE}");
            Ok(())
        }
        Some(other) => {
            eprintln!("{} unknown index command '{other}'", error_label());
            eprint!("{INDEX_USAGE}");
            std::process::exit(2);
        }
        None => {
            eprintln!("{} missing index command", error_label());
            eprint!("{INDEX_USAGE}");
            std::process::exit(2);
        }
    }
}
