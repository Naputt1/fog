//! `fog attach`: adopt a running instance's script without starting a new one,
//! by pointing the normal run path at the existing instance directory.
//!
//! Attaching must not allocate ports or reclaim services: the live instance
//! already owns them, and the attach only needs its PTYs.

use crate::cli::presentation::error_label;
use crate::cli::{Cli, die};
use crate::ipc;
use clap::Parser;
use std::ffi::OsString;
use std::io;
use std::path::Path;

use super::run::run_script_inner;

/// `fog attach [PID]` reattaches an interactive TUI to a running instance,
/// adopting its live services and replaying their screen history. With no PID
/// it targets the only running instance.
pub(crate) fn cmd_attach(cli: &Cli) -> io::Result<()> {
    let (pid, status) = match cli.pid {
        Some(pid) => {
            let path = ipc::socket_path(pid);
            match ipc::query_status(&path) {
                Ok(status) => (pid, status),
                Err(e) => {
                    die(
                        &format!("no running fog instance with pid {pid}: {e}"),
                        Some("run `fog ls` to list running instances"),
                        1,
                    );
                }
            }
        }
        None => {
            let mut live: Vec<(u32, ipc::StatusResponse)> = Vec::new();
            for (pid, path) in ipc::find_instances().unwrap_or_default() {
                if let Ok(status) = ipc::query_status(&path) {
                    live.push((pid, status));
                }
            }
            match live.len() {
                0 => {
                    die(
                        "no running fog instances",
                        Some("start one with `fog <script>`"),
                        1,
                    );
                }
                1 => live.pop().expect("checked len"),
                _ => {
                    eprintln!(
                        "{} multiple fog instances are running; pass a pid:",
                        error_label()
                    );
                    for (pid, status) in &live {
                        eprintln!(
                            "  fog attach {pid}    {} ({})",
                            status.script,
                            status.branch.as_deref().unwrap_or("-")
                        );
                    }
                    std::process::exit(1);
                }
            }
        }
    };

    if status.no_share {
        die(
            &format!("instance {pid} runs with --no-share; its services cannot be handed over"),
            None,
            1,
        );
    }

    // Reconstruct the target's invocation: its script plus its exact config
    // file, so the reattached TUI loads the same config.
    let config_path = status.config_path.clone().or_else(|| {
        status
            .config_dir
            .as_ref()
            .map(|d| Path::new(d).join("fog.json").to_string_lossy().into_owned())
    });
    let Some(config_path) = config_path else {
        die(
            &format!("instance {pid} did not report a config path"),
            Some("restart it with a current fog build"),
            1,
        );
    };

    let mut argv: Vec<OsString> = vec![
        OsString::from("fog"),
        OsString::from(&status.script),
        OsString::from("--config"),
        OsString::from(&config_path),
    ];
    // Replay `--share` so the attached session keeps the same sharing behavior
    // (borrowed services stay shared for teardown, and a detach successor
    // rebuilds it too).
    for service in &status.forced_share {
        argv.push(OsString::from("--share"));
        argv.push(OsString::from(service));
    }
    let mut attach_cli = Cli::parse_from(argv);
    attach_cli.verbose = cli.verbose;
    attach_cli.save_logs = cli.save_logs;

    run_script_inner(&status.script, &attach_cli, Some(pid))
}
