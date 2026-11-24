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
    // Validate `--port` before anything stops. Overrides win over the ports a
    // restart keeps, and `=0` re-randomizes one on purpose.
    let port_overrides = crate::ports::parse_port_overrides(&cli.port).unwrap_or_else(|e| {
        die(
            &e.to_string(),
            Some("pass --port NAME=PORT; nothing was stopped"),
            2,
        )
    });

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
        kept_ports: Vec<(String, u16)>,
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
        // Keep the ports the target was assigned for entries declared as `0`
        // (random): re-randomizing them on restart would move every URL, route
        // and dependent reference pointing at them. Fixed entries are not
        // pinned because they come from the config loaded above, so editing one
        // still takes effect on restart. `--port` overrides win over both.
        let kept_ports = restart_kept_ports(&status.ports, config.ports.as_ref(), &port_overrides);
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
            kept_ports,
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
            kept_ports,
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
        if !kept_ports.is_empty() {
            let listing: Vec<String> = kept_ports
                .iter()
                .map(|(name, port)| format!("{name} -> {port}"))
                .collect();
            println!("  keeping ports: {}", listing.join(", "));
        }
        let new_pid = match spawn_instance_detached(
            &config_path,
            &script,
            None,
            &without,
            &forced_share,
            no_share,
            &kept_ports,
            &cli.port,
        ) {
            Ok(pid) => pid,
            Err(e) => {
                die(
                    &format!("cannot restart '{script}' (pid {target_pid}): {e}"),
                    Some(
                        "kept ports must stay free until the new instance binds them; \
                         free the port, or pass --port <name>=<port> (or =0) to change it",
                    ),
                    1,
                );
            }
        };
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

/// The ports a restart keeps: every port the target was assigned for a `ports`
/// entry declared as `0` (random), minus names the command's `--port` overrides
/// (which win, including `=0` to re-randomize). A fixed entry is left out so the
/// reloaded config decides it, and a cross-instance key is left out so it
/// re-resolves against the target's current port.
fn restart_kept_ports(
    status_ports: &ipc::PortMap,
    specs: Option<&std::collections::HashMap<String, u16>>,
    overrides: &std::collections::HashMap<String, u16>,
) -> Vec<(String, u16)> {
    let Some(specs) = specs else {
        return Vec::new();
    };
    let mut kept: Vec<(String, u16)> = status_ports
        .iter()
        .filter(|(name, _)| specs.get(name.as_str()) == Some(&0))
        .filter(|(name, _)| !overrides.contains_key(name.as_str()))
        .map(|(name, port)| (name.clone(), *port))
        .collect();
    kept.sort();
    kept
}

/// Spawns a `fog <script>` detached instance for restart, mirroring
/// `daemonize` / `index::spawn_detached` but for generic scripts. `without`,
/// `forced_share` and `kept_ports` replay the killed instance's per-run choices
/// so the relaunch keeps the same skips, shares and auto-assigned ports.
#[allow(clippy::too_many_arguments)]
fn spawn_instance_detached(
    config_path: &Path,
    script: &str,
    branch: Option<&str>,
    without: &[String],
    forced_share: &[String],
    no_share: bool,
    kept_ports: &[(String, u16)],
    port_overrides: &[String],
) -> io::Result<u32> {
    let extra_args =
        restart_extra_args(without, forced_share, no_share, kept_ports, port_overrides);
    crate::ipc::spawn_detached(
        config_path,
        script,
        branch,
        &extra_args,
        &[],
        DAEMON_READY_TIMEOUT,
    )
}

/// Per-run flags for the replacement instance. Kept ports come first, then the
/// user's `--port` arguments verbatim (the run validates and applies both, with
/// the user's overrides last).
fn restart_extra_args(
    without: &[String],
    forced_share: &[String],
    no_share: bool,
    kept_ports: &[(String, u16)],
    port_overrides: &[String],
) -> Vec<String> {
    let mut extra_args = Vec::with_capacity(
        (without.len() + forced_share.len() + kept_ports.len() + port_overrides.len()) * 2 + 1,
    );
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
    for (name, port) in kept_ports {
        extra_args.push("--port".to_string());
        extra_args.push(format!("{name}={port}"));
    }
    for port in port_overrides {
        extra_args.push("--port".to_string());
        extra_args.push(port.clone());
    }
    extra_args
}

#[cfg(test)]
mod tests {
    use super::{restart_extra_args, restart_kept_ports};
    use std::collections::HashMap;

    #[test]
    fn test_restart_extra_args_replay_skip_and_share() {
        assert_eq!(
            restart_extra_args(&["db".to_string()], &["redis".to_string()], false, &[], &[]),
            vec!["--without", "db", "--share", "redis"]
        );
    }

    #[test]
    fn test_restart_extra_args_replay_no_share_without_forced_names() {
        assert_eq!(
            restart_extra_args(&["db".to_string()], &["redis".to_string()], true, &[], &[]),
            vec!["--without", "db", "--no-share"]
        );
    }

    #[test]
    fn test_restart_extra_args_replay_kept_and_override_ports() {
        assert_eq!(
            restart_extra_args(
                &[],
                &[],
                false,
                &[("api".to_string(), 51234), ("web".to_string(), 51800)],
                &["worker=6000".to_string()],
            ),
            vec![
                "--port",
                "api=51234",
                "--port",
                "web=51800",
                "--port",
                "worker=6000",
            ]
        );
    }

    #[test]
    fn test_restart_kept_ports_only_pins_random_local_names() {
        let status_ports = [
            ("api".to_string(), 51234u16),
            ("web".to_string(), 51800),
            ("db".to_string(), 5432),
            ("api@repo:main".to_string(), 4000),
        ]
        .into_iter()
        .collect();
        let specs = [
            ("api".to_string(), 0u16),
            ("web".to_string(), 0),
            ("db".to_string(), 5432),
        ]
        .into_iter()
        .collect::<HashMap<_, _>>();

        // Random names are kept in sorted order; a fixed spec and a
        // cross-instance key are not.
        assert_eq!(
            restart_kept_ports(&status_ports, Some(&specs), &HashMap::new()),
            vec![("api".to_string(), 51234), ("web".to_string(), 51800)]
        );
        // An explicit `--port` wins over the kept value, including `=0` to
        // re-randomize.
        let overrides = [("api".to_string(), 0u16)].into_iter().collect();
        assert_eq!(
            restart_kept_ports(&status_ports, Some(&specs), &overrides),
            vec![("web".to_string(), 51800)]
        );
        // No top-level `ports` means nothing to keep.
        assert!(restart_kept_ports(&status_ports, None, &HashMap::new()).is_empty());
    }
}
