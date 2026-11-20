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
    // A shell user may pad a name; clap does not trim values.
    let fresh_share: Vec<String> = cli
        .share
        .iter()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect();

    // Plan every restart before stopping anything: a typo in `--without`, a
    // missing script, or an unreadable config must not take down a target
    // that cannot be replaced. This also makes `--all` all-or-nothing.
    struct RestartPlan {
        pid: u32,
        path: PathBuf,
        script: String,
        branch: Option<String>,
        config_path: PathBuf,
        without: Vec<String>,
        forced_share: Vec<String>,
        no_share: bool,
    }
    let mut plans = Vec::with_capacity(targets.len());
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
        let config_path = restart_config_path(
            status.config_path.as_deref(),
            status.config_dir.as_deref(),
            cli,
        );
        let config = crate::config::load(&config_path).unwrap_or_else(|e| {
            die(
                &format!("cannot restart instance {target_pid}: {e}"),
                Some("fix the target's config; nothing was stopped"),
                1,
            )
        });
        let issues = config.validate();
        if !issues.is_empty() {
            die(
                &format!(
                    "cannot restart instance {target_pid}: '{}' has {} problem{}: {}",
                    config_path.display(),
                    issues.len(),
                    if issues.len() == 1 { "" } else { "s" },
                    issues[0]
                ),
                Some("fix the config; nothing was stopped"),
                1,
            );
        }
        let Some(target_script) = config.scripts.get(&script) else {
            die(
                &format!(
                    "cannot restart instance {target_pid}: script '{script}' is no longer in '{}'",
                    config_path.display()
                ),
                Some("restore the script; nothing was stopped"),
                1,
            );
        };
        // Replayed opt-outs are filtered because the config may have changed;
        // names passed to this command are strict so a typo is caught.
        let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
        let replayed: Vec<String> = status
            .services
            .iter()
            .filter(|service| service.skipped)
            .map(|service| service.name.clone())
            .collect();
        let mut without = crate::runtime::filter_without(target_script, config_dir, &replayed);
        match crate::runtime::resolve_without(target_script, config_dir, &cli.without) {
            Ok(names) => without.extend(names),
            Err(e) => {
                die(
                    e,
                    Some("pass an effective service name; nothing was stopped"),
                    2,
                );
            }
        }
        let mut without: Vec<String> = without.into_iter().collect();
        without.sort();
        if let Some(name) = fresh_share
            .iter()
            .find(|name| without.iter().any(|skipped| skipped == *name))
        {
            die(
                crate::error::FogError::new(format!(
                    "service '{name}' cannot be both skipped and force-shared"
                )),
                Some("use either `--without` or `--share` for it; nothing was stopped"),
                2,
            );
        }
        // Fresh `--share` names are strict like `--without`: catch a typo
        // before stopping anything. Replayed names are filtered because the
        // config may have changed since the target started.
        let known: Vec<String> = crate::runtime::script_service_names(target_script, config_dir);
        if let Some(name) = fresh_share
            .iter()
            .find(|name| !known.iter().any(|known| known == *name))
        {
            die(
                crate::error::FogError::new(format!("unknown service in --share: {name}")),
                Some("pass an effective service name; nothing was stopped"),
                2,
            );
        }
        // A target started with --no-share keeps it; an explicit --share asks
        // for the opposite, so it wins over the replayed flag.
        let no_share = cli.no_share || (status.no_share && fresh_share.is_empty());
        let forced_share: Vec<String> = if no_share {
            Vec::new()
        } else {
            let mut names: Vec<String> = crate::runtime::filter_forced_share(
                target_script,
                config_dir,
                &status.forced_share,
            )
            .into_iter()
            .collect();
            names.extend(fresh_share.iter().cloned());
            names.sort();
            names.dedup();
            // A skipped service cannot also run as a forced shared owner.
            names.retain(|name| !without.contains(name));
            names
        };
        plans.push(RestartPlan {
            pid: target_pid,
            path: path.clone(),
            script,
            branch: status.branch,
            config_path,
            without,
            forced_share,
            no_share,
        });
    }

    for plan in plans {
        let RestartPlan {
            pid: target_pid,
            path,
            script,
            branch,
            config_path,
            without,
            forced_share,
            no_share,
        } = plan;
        // Wait for the old instance to fully exit before relaunching to avoid
        // port conflicts and owner-lock races. `--force` escalates to SIGKILL
        // so a wedged instance does not block the restart.
        if stop_instance(target_pid, &path, cli.force, false, &mut stdout()) {
            println!("stopped fog instance {target_pid} (script '{script}')");
        } else {
            eprintln!(
                "warning: instance {target_pid} did not stop; restarting anyway may conflict"
            );
        }
        // Extra grace for socket file removal.
        wait_for_socket_gone(&path);

        // Spawn a detached instance with the same script. The config_path already
        // points at the correct worktree's fog.json, so no --branch is needed
        // (branch is inferred from the worktree containing config_dir).
        let new_pid = spawn_instance_detached(
            &config_path,
            &script,
            None,
            &without,
            &forced_share,
            no_share,
        )?;
        println!("restarted fog '{script}' (old pid {target_pid} → new pid {new_pid})");
        if let Some(b) = branch {
            println!("  branch: {b}");
        }
        println!("  status: fog ls {new_pid}");
        println!("  logs:   fog logs {new_pid}");
    }
    Ok(())
}

/// Config path for a restart: prefer the target instance's config file (or its
/// config directory for older status payloads), fall back to the command's
/// `--config`.
fn restart_config_path(config_path: Option<&str>, config_dir: Option<&str>, cli: &Cli) -> PathBuf {
    if let Some(path) = config_path {
        let p = PathBuf::from(path);
        if p.exists() {
            return p;
        }
    }
    if let Some(dir) = config_dir {
        let p = PathBuf::from(dir).join("fog.json");
        if p.exists() {
            return p;
        }
    }
    resolve_config_path(&resolve_run_config(cli))
}

/// Spawns a `fog <script>` detached instance for restart, mirroring
/// `daemonize` / `index::spawn_detached` but for generic scripts. `without` and
/// `forced_share` replay the killed instance's per-run service overrides so the
/// relaunch keeps the same skip/share behavior.
fn spawn_instance_detached(
    config_path: &Path,
    script: &str,
    branch: Option<&str>,
    without: &[String],
    forced_share: &[String],
    no_share: bool,
) -> io::Result<u32> {
    let extra_args = restart_extra_args(without, forced_share, no_share);
    crate::ipc::spawn_detached(
        config_path,
        script,
        branch,
        &extra_args,
        &[],
        DAEMON_READY_TIMEOUT,
    )
}

/// Per-run flags for the replacement instance.
fn restart_extra_args(without: &[String], forced_share: &[String], no_share: bool) -> Vec<String> {
    let mut extra_args = Vec::with_capacity((without.len() + forced_share.len()) * 2 + 1);
    for service in without {
        extra_args.push("--without".to_string());
        extra_args.push(service.clone());
    }
    if no_share {
        extra_args.push("--no-share".to_string());
    } else {
        for service in forced_share {
            extra_args.push("--share".to_string());
            extra_args.push(service.clone());
        }
    }
    extra_args
}

#[cfg(test)]
mod tests {
    use super::restart_extra_args;

    #[test]
    fn test_restart_extra_args_replay_skip_and_share() {
        assert_eq!(
            restart_extra_args(&["db".to_string()], &["redis".to_string()], false),
            vec!["--without", "db", "--share", "redis"]
        );
    }

    #[test]
    fn test_restart_extra_args_replay_no_share_without_forced_names() {
        assert_eq!(
            restart_extra_args(&["db".to_string()], &["redis".to_string()], true),
            vec!["--without", "db", "--no-share"]
        );
    }
}
