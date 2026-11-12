//! `fog kill`: choose target instances and stop them, rendering the per-service
//! shutdown checklist while a graceful stop drains.
//!
//! A wedged instance still has to die, so `--force` escalates from the IPC kill
//! flag to SIGTERM and then SIGKILL; the process-tree signal is separate from
//! the direct PID signal because a non-group-leader target would miss one.

use crate::cli::presentation::color_enabled;
use crate::cli::progress::StopProgress;
use crate::cli::{Cli, die};
use crate::ipc;
use std::collections::HashMap;
use std::io::{self, IsTerminal, Write, stdout};
use std::path::Path;
use std::time::Duration;

use super::resolve_targets;

/// How long a graceful `fog kill` waits for the instance to drain its services
/// (rendering per-service progress) before suggesting `--force`. Services tear
/// down sequentially, so this must cover the whole teardown, not one service.
pub(crate) const KILL_GRACE_TIMEOUT: Duration = Duration::from_secs(30);

/// Asks the instance at `path` to stop and waits for it to exit.
///
/// The instance is first asked to shut down gracefully over IPC (so services
/// tear down cleanly). A wedged instance whose event loop never consumes the
/// kill flag will not exit on its own, so with `force` we escalate: SIGTERM to
/// the process tree, then SIGKILL, which cannot be caught or blocked.
///
/// With `progress`, the wait renders each service's shutdown state
/// (docker-style) from the same IPC status the instance publishes as it tears
/// down.
///
/// Returns `true` once the process is no longer alive.
pub(crate) fn stop_instance(
    pid: u32,
    path: &Path,
    force: bool,
    progress: bool,
    out: &mut impl Write,
) -> bool {
    if let Err(e) = ipc::send_kill(path) {
        eprintln!("warning: could not reach instance {pid}: {e}");
    }

    let mut view =
        progress.then(|| StopProgress::new(pid, stdout().is_terminal(), color_enabled()));
    if let Some(v) = view.as_mut() {
        let _ = v.tick(path, out);
    }

    // A graceful kill waits for the services to drain, rendering progress as
    // they do. `--force` keeps the short grace so it can escalate quickly.
    let grace = if force {
        Duration::from_millis(2500)
    } else {
        KILL_GRACE_TIMEOUT
    };
    if wait_for_pid_exit(pid, grace, path, view.as_mut(), out) {
        finish_view(view.as_mut(), out);
        return true;
    }

    if !force {
        finish_view(view.as_mut(), out);
        eprintln!("warning: instance {pid} did not stop; retry with `fog kill --force {pid}`");
        return false;
    }

    // SIGTERM first for a chance at a clean tree teardown, then SIGKILL. A
    // fog instance registers a SIGTERM handler (the `ctrlc` termination
    // feature), so a wedged loop can ignore the former but never the latter.
    signal_instance(pid, crate::process::Signal::Term);
    if wait_for_pid_exit(pid, Duration::from_millis(1000), path, view.as_mut(), out) {
        finish_view(view.as_mut(), out);
        return true;
    }
    signal_instance(pid, crate::process::Signal::Kill);
    let exited = wait_for_pid_exit(pid, Duration::from_millis(2000), path, view.as_mut(), out);
    finish_view(view.as_mut(), out);
    if !exited {
        eprintln!("warning: instance {pid} survived SIGKILL");
    }
    exited
}

/// Finalizes a shutdown progress view, if one is active.
fn finish_view(view: Option<&mut StopProgress>, out: &mut impl Write) {
    if let Some(v) = view {
        let _ = v.finish(out);
    }
}

/// Signals the instance's whole process tree and the process itself.
///
/// [`crate::process::signal_tree`] targets the group, which reaches the leader
/// (fog is normally its own group leader), but a target that is not a group
/// leader would be missed — so hit the PID directly too.
fn signal_instance(pid: u32, signal: crate::process::Signal) {
    crate::process::signal_tree(pid, signal);
    // Also signal the process itself: a non-group-leader target is missed by
    // the group kill above.
    let _ = crate::process::kill_process(pid, signal);
}

/// Waits until `pid` is no longer alive, up to `timeout`, polling the
/// instance's shutdown progress while it drains.
fn wait_for_pid_exit(
    pid: u32,
    timeout: Duration,
    path: &Path,
    mut progress: Option<&mut StopProgress>,
    out: &mut impl Write,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(p) = progress.as_deref_mut() {
            let _ = p.tick(path, out);
        }
        if !crate::process::is_pid_alive(pid) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub(crate) fn cmd_kill(pid: Option<u32>, all: bool, cli: &Cli) -> io::Result<()> {
    let instances = ipc::find_instances()?;

    if instances.is_empty() {
        die(
            "no running fog instances",
            Some("start one with `fog <script>`"),
            1,
        );
    }

    let targets = resolve_targets(&instances, pid, all, cli, "kill");

    // Snapshot identity before anything stops: sockets disappear with their
    // process, and the auto-start links are needed to explain what remains.
    let mut statuses: HashMap<u32, ipc::StatusResponse> = HashMap::new();
    let mut socket_paths: HashMap<u32, std::path::PathBuf> = HashMap::new();
    for (instance_pid, path) in &instances {
        if let Ok(status) = ipc::query_status(path) {
            statuses.insert(*instance_pid, status);
            socket_paths.insert(*instance_pid, path.clone());
        }
    }

    let mut to_kill: Vec<(u32, std::path::PathBuf)> = targets.clone();
    if cli.with_dependents {
        for (target_pid, _) in &targets {
            for child in descendants(*target_pid, &statuses) {
                if let Some(path) = socket_paths.get(&child)
                    && !to_kill.iter().any(|(p, _)| p == &child)
                {
                    to_kill.push((child, path.clone()));
                }
            }
        }
    }

    let killed: Vec<u32> = to_kill.iter().map(|(p, _)| *p).collect();
    for (target_pid, path) in &to_kill {
        let target_pid = *target_pid;
        if stop_instance(target_pid, path, cli.force, true, &mut stdout()) {
            println!("stopped fog instance {target_pid}");
        } else {
            eprintln!("warning: fog instance {target_pid} is still running");
        }
    }

    // Instances this run created indirectly and chose to keep: name them so
    // they do not linger invisibly behind their parent.
    for (instance_pid, path) in ipc::find_instances()? {
        if killed.contains(&instance_pid) {
            continue;
        }
        let Ok(status) = ipc::query_status(&path) else {
            continue;
        };
        let Some(parent) = status.autostarted_by else {
            continue;
        };
        if !chain_reaches(parent, &killed, &statuses) {
            continue;
        }
        println!(
            "{} (pid {instance_pid}) was started by this instance and is still running; \
             fog kill {instance_pid}",
            instance_label(&status)
        );
    }

    Ok(())
}

/// Every instance transitively cold-started by `root`, from the pre-kill
/// snapshot.
fn descendants(root: u32, statuses: &HashMap<u32, ipc::StatusResponse>) -> Vec<u32> {
    let mut found = Vec::new();
    let mut queue = vec![root];
    while let Some(parent) = queue.pop() {
        for (instance_pid, status) in statuses {
            if status.autostarted_by == Some(parent) && !found.contains(instance_pid) {
                found.push(*instance_pid);
                queue.push(*instance_pid);
            }
        }
    }
    found
}

/// Whether `parent` is one of `killed`, following auto-start links through the
/// pre-kill snapshot (so killed parents still link to their children).
fn chain_reaches(
    parent: u32,
    killed: &[u32],
    statuses: &HashMap<u32, ipc::StatusResponse>,
) -> bool {
    let mut current = Some(parent);
    while let Some(instance_pid) = current {
        if killed.contains(&instance_pid) {
            return true;
        }
        current = statuses
            .get(&instance_pid)
            .and_then(|status| status.autostarted_by);
    }
    false
}

/// `repo:branch:script` label for an instance, for human-facing messages.
fn instance_label(status: &ipc::StatusResponse) -> String {
    let repo = status
        .project
        .as_deref()
        .map(crate::project::display_name)
        .unwrap_or_else(|| "-".to_string());
    let branch = status.branch.as_deref().unwrap_or("-");
    format!("{repo}:{branch}:{}", status.script)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::commands::test_status;
    use std::process::{Command, Stdio};

    #[test]
    fn test_kill_dependents_follow_autostart_links() {
        let mut parent = test_status(Vec::new(), None);
        parent.pid = 10;
        let mut child = test_status(Vec::new(), None);
        child.pid = 11;
        child.autostarted_by = Some(10);
        child.autostarted_by_label = Some("a:main:web".to_string());
        let mut grandchild = test_status(Vec::new(), None);
        grandchild.pid = 12;
        grandchild.autostarted_by = Some(11);
        let mut unrelated = test_status(Vec::new(), None);
        unrelated.pid = 13;
        let map: HashMap<u32, ipc::StatusResponse> =
            [(10, parent), (11, child), (12, grandchild), (13, unrelated)]
                .into_iter()
                .collect();

        let mut found = descendants(10, &map);
        found.sort_unstable();
        assert_eq!(found, vec![11, 12]);
        assert!(chain_reaches(11, &[10], &map));
        assert!(chain_reaches(12, &[10], &map));
        assert!(!chain_reaches(13, &[10], &map));
        assert!(!chain_reaches(10, &[99], &map));
        assert_eq!(
            instance_label(map.get(&11).unwrap()),
            "-:-:dev",
            "test_status leaves project/branch unset"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_stop_instance_force_kills_unresponsive_pid() {
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("sleep 30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = cmd.spawn().expect("spawn sleeper");
        let pid = child.id();
        // Reap concurrently: otherwise the killed child lingers as a zombie and
        // `is_pid_alive` (bare `kill(pid, 0)`) keeps reporting it as alive.
        let reaper = std::thread::spawn(move || {
            let _ = child.wait();
        });
        // No instance socket exists, so the graceful IPC attempt fails; force
        // must still terminate the process.
        let bogus = std::env::temp_dir().join(format!("fog-nonexistent-{pid}.sock"));
        let mut out: Vec<u8> = Vec::new();
        let stopped = stop_instance(pid, &bogus, true, false, &mut out);
        assert!(stopped, "force stop must terminate the process");
        reaper.join().expect("reaper thread");
        assert!(!crate::process::is_pid_alive(pid));
    }
}
