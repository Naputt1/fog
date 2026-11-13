//! `fog restart`: stop a target instance and relaunch it detached with the same
//! script and config.
//!
//! The old instance must be fully gone (socket removed, ports released) before
//! the replacement starts, or the relaunch races the owner lock it just freed.

use crate::cli::commands::daemon::DAEMON_READY_TIMEOUT;
use crate::cli::{Cli, die, resolve_config_path, resolve_run_config};
use crate::ipc;
use std::io::{self, stdout};
use std::path::{Path, PathBuf};

use super::kill::stop_instance;
use super::resolve_targets;
use super::wait_for_socket_gone;

pub(crate) fn cmd_restart(pid: Option<u32>, all: bool, cli: &Cli) -> io::Result<()> {
    let instances = ipc::find_instances()?;

    if instances.is_empty() {
        die(
            "no running fog instances",
            Some("start one with `fog <script>`"),
            1,
        );
    }

    let targets = resolve_targets(&instances, pid, all, cli, "restart");
    for (target_pid, path) in &targets {
        let target_pid = *target_pid;
        // Capture the target's status before killing so we can relaunch it.
        let status = ipc::query_status(path).unwrap_or_else(|e| {
            die(
                &format!("could not query instance {target_pid}: {e}"),
                None,
                1,
            )
        });
        let script = status.script.clone();
        let config_dir = status.config_dir.clone();
        let branch = status.branch.clone();

        // Wait for the old instance to fully exit before relaunching to avoid
        // port conflicts and owner-lock races. `--force` escalates to SIGKILL
        // so a wedged instance does not block the restart.
        if stop_instance(target_pid, path, cli.force, false, &mut stdout()) {
            println!("stopped fog instance {target_pid} (script '{script}')");
        } else {
            eprintln!(
                "warning: instance {target_pid} did not stop; restarting anyway may conflict"
            );
        }
        // Extra grace for socket file removal.
        wait_for_socket_gone(path);

        // Resolve config path for the relaunch. Prefer the killed instance's
        // config_dir (worktree-accurate), fall back to cli --config.
        let config_path = if let Some(dir) = config_dir {
            let p = PathBuf::from(&dir).join("fog.json");
            if p.exists() {
                p
            } else {
                resolve_config_path(&resolve_run_config(cli))
            }
        } else {
            resolve_config_path(&resolve_run_config(cli))
        };

        // Spawn a detached instance with the same script. The config_path already
        // points at the correct worktree's fog.json, so no --branch is needed
        // (branch is inferred from the worktree containing config_dir).
        let new_pid = spawn_instance_detached(&config_path, &script, None, &status.forced_share)?;
        println!("restarted fog '{script}' (old pid {target_pid} → new pid {new_pid})");
        if let Some(b) = branch {
            println!("  branch: {b}");
        }
        println!("  status: fog ls {new_pid}");
        println!("  logs:   fog logs {new_pid}");
    }
    Ok(())
}

/// Spawns a `fog <script>` detached instance for restart, mirroring
/// `daemonize` / `index::spawn_detached` but for generic scripts. `forced_share`
/// replays the killed instance's `--share` names so the relaunch keeps sharing
/// the same services instead of starting duplicates.
fn spawn_instance_detached(
    config_path: &Path,
    script: &str,
    branch: Option<&str>,
    forced_share: &[String],
) -> io::Result<u32> {
    let mut extra_args = Vec::with_capacity(forced_share.len() * 2);
    for service in forced_share {
        extra_args.push("--share".to_string());
        extra_args.push(service.clone());
    }
    crate::ipc::spawn_detached(
        config_path,
        script,
        branch,
        &extra_args,
        &[],
        DAEMON_READY_TIMEOUT,
    )
}
