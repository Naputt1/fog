//! Command bodies for `fog`'s built-in subcommands, plus the target selection
//! that `kill` and `logs` share.
//!
//! Target selection is one implementation on purpose: `kill` and `logs` accept
//! the same PID/all/service flags, and letting them drift would let one command
//! address an instance the other cannot.

pub mod attach;
pub mod daemon;
pub mod index;
pub mod kill;
pub mod logs;
pub mod ls;
pub mod prune;
pub mod reclaim;
pub mod restart;
pub mod run;

use crate::cli::{Cli, fail, resolve_config_path};
use crate::error::{FogError, FogResult};
use crate::ipc;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Waits until the old instance's socket is gone or unreachable, guaranteeing
/// it fully cleaned up (and released its ports) before we spawn replacements.
fn wait_for_socket_gone(path: &std::path::Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if !path.exists() || ipc::query_status(path).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Normalizes a directory for comparison: canonicalized when possible,
/// otherwise an absolute path without symlink resolution.
fn normalize_dir(dir: &Path) -> String {
    if let Ok(c) = dir.canonicalize() {
        return c.to_string_lossy().into_owned();
    }
    if dir.is_absolute() {
        return dir.to_string_lossy().into_owned();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(dir).to_string_lossy().into_owned())
        .unwrap_or_else(|_| dir.to_string_lossy().into_owned())
}

/// Whether an instance's `config_dir` matches the local config directory.
/// Both sides are normalized so symlinked checkouts compare equal.
fn config_dir_matches(instance_dir: Option<&str>, local_dir: &Path) -> bool {
    match instance_dir {
        Some(d) => normalize_dir(Path::new(d)) == normalize_dir(local_dir),
        None => false,
    }
}

/// Resolves the local config directory for PID-less scoping, without the
/// side effects of `resolve_run_config` (no stderr noise, no exit).
///
/// Honors `--branch` (relative `--config` is resolved against that branch's
/// worktree) and `--config` (file or directory). Returns `None` when no
/// `fog.json` exists at the resolved location.
fn local_config_dir(cli: &Cli) -> Option<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let base = match &cli.branch {
        Some(branch) => {
            let worktrees = crate::worktree::list(&cwd)?;
            let wt = worktrees
                .iter()
                .find(|w| w.branch.as_deref() == Some(branch.as_str()))?;
            if cli.config.is_absolute() {
                cli.config.clone()
            } else {
                wt.path.join(&cli.config)
            }
        }
        None => cli.config.clone(),
    };
    let config_path = resolve_config_path(&base);
    if !config_path.is_file() {
        return None;
    }
    let absolute = if config_path.is_absolute() {
        config_path
    } else {
        cwd.join(&config_path)
    };
    absolute.parent().map(Path::to_path_buf)
}

/// Best-effort status snapshot per instance PID; unreachable instances are
/// skipped (stale sockets are left for `cmd_ls` to clean).
fn query_statuses(
    instances: &[(u32, PathBuf)],
) -> std::collections::HashMap<u32, ipc::StatusResponse> {
    let mut out = std::collections::HashMap::new();
    for (pid, path) in instances {
        if let Ok(status) = ipc::query_status(path) {
            out.insert(*pid, status);
        }
    }
    out
}

/// Pure target selection shared by kill/restart/logs.
///
/// - Explicit `pid` always wins (errors when unknown; `--all` + PID is an error).
/// - `--all` with a local config selects every local match (error when none);
///   without a local config it selects everything globally.
/// - No PID, no `--all`: with a local config, one local match is selected,
///   several produce a scoped list error, and zero fall back to the legacy
///   global rule (single instance selected, several listed). Without a local
///   config the legacy global rule applies directly.
///
/// Returns owned `(pid, socket)` pairs so multi-target commands can iterate.
/// Selects the instances `fog kill`/`restart`/`logs` should act on. The
/// returned message is plain text: the caller prints it as the command's error.
fn select_targets(
    instances: &[(u32, PathBuf)],
    statuses: &std::collections::HashMap<u32, ipc::StatusResponse>,
    pid: Option<u32>,
    all: bool,
    local_dir: Option<&Path>,
    cmd: &str,
) -> FogResult<Vec<(u32, PathBuf)>> {
    if let Some(pid) = pid {
        if all {
            return Err(FogError::new(format!(
                "--all cannot be used with a PID (got {pid})"
            )));
        }
        return instances
            .iter()
            .find(|(p, _)| *p == pid)
            .map(|(p, path)| vec![(*p, path.clone())])
            .ok_or_else(|| FogError::new(format!("no fog instance with pid {pid}")));
    }

    if all {
        match local_dir {
            Some(dir) => {
                let matched: Vec<(u32, PathBuf)> = instances
                    .iter()
                    .filter(|(p, _)| {
                        statuses
                            .get(p)
                            .and_then(|s| s.config_dir.as_deref())
                            .is_some_and(|d| config_dir_matches(Some(d), dir))
                    })
                    .map(|(p, path)| (*p, path.clone()))
                    .collect();
                if matched.is_empty() {
                    return Err(FogError::new(format!(
                        "no fog instances from this config ({}); nothing to apply --all to",
                        dir.display()
                    )));
                }
                return Ok(matched);
            }
            // Outside a directory with fog.json, --all is global.
            None => {
                return Ok(instances.to_vec());
            }
        }
    }

    // No PID, no --all: prefer the local config scope when resolvable.
    if let Some(dir) = local_dir {
        let matched: Vec<(u32, PathBuf)> = instances
            .iter()
            .filter(|(p, _)| {
                statuses
                    .get(p)
                    .and_then(|s| s.config_dir.as_deref())
                    .is_some_and(|d| config_dir_matches(Some(d), dir))
            })
            .map(|(p, path)| (*p, path.clone()))
            .collect();
        match matched.len() {
            1 => return Ok(matched),
            0 => { /* fall through to the legacy global rule */ }
            _ => {
                let mut msg = format!(
                    "multiple fog instances from this config ({}), specify a pid:",
                    dir.display()
                );
                for (p, _) in &matched {
                    msg.push_str(&format!("\n  fog {cmd} {p}"));
                }
                if matches!(cmd, "kill" | "restart") {
                    msg.push_str(&format!(
                        "\n  fog {cmd} --all   (apply to all {n})",
                        n = matched.len()
                    ));
                }
                return Err(FogError::new(msg));
            }
        }
    }

    if instances.len() == 1 {
        Ok(vec![instances[0].clone()])
    } else {
        let mut msg = "multiple fog instances running, specify a pid:".to_string();
        for (p, _) in instances {
            msg.push_str(&format!("\n  fog {cmd} {p}"));
        }
        Err(FogError::new(msg))
    }
}

/// Resolves targets and exits with the rendered error on failure.
fn resolve_targets(
    instances: &[(u32, PathBuf)],
    pid: Option<u32>,
    all: bool,
    cli: &Cli,
    cmd: &str,
) -> Vec<(u32, PathBuf)> {
    let local = local_config_dir(cli);
    let statuses = query_statuses(instances);
    match select_targets(instances, &statuses, pid, all, local.as_deref(), cmd) {
        Ok(targets) => targets,
        Err(message) => fail(message),
    }
}

/// Builds a status response for tests; shared by the command modules' tests.
#[cfg(test)]
pub(crate) fn test_status(
    services: Vec<ipc::ServiceStatus>,
    proxy: Option<ipc::ProxyStatus>,
) -> ipc::StatusResponse {
    ipc::StatusResponse {
        pid: 1234,
        script: "dev".to_string(),
        services,
        proxy,
        project: None,
        branch: None,
        config_dir: None,
        config_path: None,
        started_at: 0,
        ports: Default::default(),
        native_routes: Vec::new(),
        no_share: false,
        forced_share: Vec::new(),
        autostarted_by: None,
        autostarted_by_label: None,
        started: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::temp_dir;
    use std::fs;

    fn status_with_dir(config_dir: Option<&str>) -> ipc::StatusResponse {
        let mut s = test_status(vec![], None);
        s.config_dir = config_dir.map(str::to_string);
        s
    }

    fn target_fixtures() -> (
        Vec<(u32, PathBuf)>,
        std::collections::HashMap<u32, ipc::StatusResponse>,
    ) {
        // Fixed pseudo-dirs (need not exist): normalize_dir falls back to the
        // absolute string, so equal strings still match.
        let dir_a = format!("/tmp/fog-test-scope-a-{}", std::process::id());
        let dir_b = format!("/tmp/fog-test-scope-b-{}", std::process::id());
        let instances = vec![
            (101, PathBuf::from("/tmp/fog-101.sock")),
            (102, PathBuf::from("/tmp/fog-102.sock")),
            (103, PathBuf::from("/tmp/fog-103.sock")),
        ];
        let mut statuses = std::collections::HashMap::new();
        statuses.insert(101, status_with_dir(Some(&dir_a)));
        statuses.insert(102, status_with_dir(Some(&dir_a)));
        statuses.insert(103, status_with_dir(Some(&dir_b)));
        (instances, statuses)
    }

    #[test]
    fn test_select_targets_explicit_pid_wins_over_local() {
        let (instances, statuses) = target_fixtures();
        let local = PathBuf::from("/elsewhere");
        let got = select_targets(
            &instances,
            &statuses,
            Some(103),
            false,
            Some(&local),
            "kill",
        )
        .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 103);
    }

    #[test]
    fn test_select_targets_unknown_pid_errors() {
        let (instances, statuses) = target_fixtures();
        let err = select_targets(&instances, &statuses, Some(999), false, None, "kill")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no fog instance with pid 999"));
    }

    #[test]
    fn test_select_targets_pid_and_all_conflict() {
        let (instances, statuses) = target_fixtures();
        let err = select_targets(&instances, &statuses, Some(101), true, None, "kill")
            .unwrap_err()
            .to_string();
        assert!(err.contains("--all cannot be used with a PID"));
    }

    #[test]
    fn test_select_targets_all_scoped_to_local() {
        let dir_a = temp_dir();
        let dir_b = temp_dir();
        let instances = vec![
            (11, PathBuf::from("/tmp/fog-11.sock")),
            (12, PathBuf::from("/tmp/fog-12.sock")),
            (13, PathBuf::from("/tmp/fog-13.sock")),
        ];
        let mut statuses = std::collections::HashMap::new();
        statuses.insert(11, status_with_dir(Some(&dir_a.to_string_lossy())));
        statuses.insert(12, status_with_dir(Some(&dir_a.to_string_lossy())));
        statuses.insert(13, status_with_dir(Some(&dir_b.to_string_lossy())));
        let got = select_targets(&instances, &statuses, None, true, Some(&dir_a), "kill").unwrap();
        let mut pids: Vec<u32> = got.iter().map(|(p, _)| *p).collect();
        pids.sort();
        assert_eq!(pids, vec![11, 12]);
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);
    }

    #[test]
    fn test_select_targets_all_local_no_match_errors() {
        let (instances, statuses) = target_fixtures();
        let local = temp_dir();
        let err = select_targets(&instances, &statuses, None, true, Some(&local), "kill")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no fog instances from this config"));
        let _ = fs::remove_dir_all(&local);
    }

    #[test]
    fn test_select_targets_all_global_without_local() {
        let (instances, statuses) = target_fixtures();
        let got = select_targets(&instances, &statuses, None, true, None, "kill").unwrap();
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn test_select_targets_pidless_single_local_match() {
        let dir_a = temp_dir();
        let dir_b = temp_dir();
        let instances = vec![
            (21, PathBuf::from("/tmp/fog-21.sock")),
            (22, PathBuf::from("/tmp/fog-22.sock")),
        ];
        let mut statuses = std::collections::HashMap::new();
        statuses.insert(21, status_with_dir(Some(&dir_a.to_string_lossy())));
        statuses.insert(22, status_with_dir(Some(&dir_b.to_string_lossy())));
        let got = select_targets(&instances, &statuses, None, false, Some(&dir_b), "logs").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 22);
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);
    }

    #[test]
    fn test_select_targets_pidless_multi_local_lists_scoped() {
        let (instances, statuses) = target_fixtures();
        // dir of pid 101/102: recover from statuses.
        let local = PathBuf::from(statuses[&101].config_dir.as_deref().unwrap());
        let err = select_targets(&instances, &statuses, None, false, Some(&local), "kill")
            .unwrap_err()
            .to_string();
        assert!(err.contains("multiple fog instances from this config"));
        assert!(err.contains("fog kill 101"));
        assert!(err.contains("fog kill 102"));
        assert!(
            !err.contains("103"),
            "scoped error must not list other configs"
        );
        assert!(err.contains("--all"));
    }

    #[test]
    fn test_select_targets_pidless_multi_local_logs_has_no_all_hint() {
        let (instances, statuses) = target_fixtures();
        let local = PathBuf::from(statuses[&101].config_dir.as_deref().unwrap());
        let err = select_targets(&instances, &statuses, None, false, Some(&local), "logs")
            .unwrap_err()
            .to_string();
        assert!(err.contains("fog logs 101"));
        assert!(!err.contains("--all"));
    }

    #[test]
    fn test_select_targets_pidless_zero_local_falls_back_to_single() {
        let dir_a = temp_dir();
        let instances = vec![(31, PathBuf::from("/tmp/fog-31.sock"))];
        let mut statuses = std::collections::HashMap::new();
        statuses.insert(31, status_with_dir(Some(&dir_a.to_string_lossy())));
        let elsewhere = temp_dir();
        let got =
            select_targets(&instances, &statuses, None, false, Some(&elsewhere), "kill").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 31);
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&elsewhere);
    }

    #[test]
    fn test_select_targets_pidless_zero_local_multi_lists_global() {
        let dir_a = temp_dir();
        let dir_b = temp_dir();
        let instances = vec![
            (41, PathBuf::from("/tmp/fog-41.sock")),
            (42, PathBuf::from("/tmp/fog-42.sock")),
        ];
        let mut statuses = std::collections::HashMap::new();
        statuses.insert(41, status_with_dir(Some(&dir_a.to_string_lossy())));
        statuses.insert(42, status_with_dir(Some(&dir_b.to_string_lossy())));
        let elsewhere = temp_dir();
        let err = select_targets(&instances, &statuses, None, false, Some(&elsewhere), "kill")
            .unwrap_err()
            .to_string();
        assert!(err.contains("multiple fog instances running"));
        assert!(err.contains("fog kill 41"));
        assert!(err.contains("fog kill 42"));
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);
        let _ = fs::remove_dir_all(&elsewhere);
    }

    #[test]
    fn test_select_targets_ignores_instances_without_config_dir() {
        let dir_a = temp_dir();
        let instances = vec![
            (51, PathBuf::from("/tmp/fog-51.sock")),
            (52, PathBuf::from("/tmp/fog-52.sock")),
        ];
        let mut statuses = std::collections::HashMap::new();
        statuses.insert(51, status_with_dir(None));
        statuses.insert(52, status_with_dir(Some(&dir_a.to_string_lossy())));
        // Only pid 52 matches the local dir; pid 51 (unknown dir) is ignored
        // rather than making the result ambiguous.
        let got = select_targets(&instances, &statuses, None, false, Some(&dir_a), "logs").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 52);
        let _ = fs::remove_dir_all(&dir_a);
    }

    #[test]
    fn test_config_dir_matches_normalizes() {
        let dir = temp_dir();
        assert!(config_dir_matches(Some(&dir.to_string_lossy()), &dir));
        assert!(!config_dir_matches(Some("/definitely/not/here"), &dir));
        assert!(!config_dir_matches(None, &dir));
        let _ = fs::remove_dir_all(&dir);
    }
}
