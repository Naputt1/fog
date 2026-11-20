//! Taking over instances that are already running: reusing their services,
//! reclaiming (shutting down) the old process, or reconciling a crashed one.
//!
//! Reclaiming is opt-in per script (`reuse`/`share`), and only ever touches
//! instances on the same branch: a concurrent multi-branch run owns its own
//! services and must not be disturbed.

use crate::cli::presentation::error_label;
use crate::ipc;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use super::wait_for_socket_gone;

/// How long a starter waits for another instance that is mid-start before
/// giving up.
const LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a replacer waits for the old instance to fully exit.
const RECLAIM_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Names of the services a single-instance run hands over: `reuse`-flagged
/// ones plus any named by `--share`.
pub(crate) fn reuse_names(
    script: &crate::config::ScriptConfig,
    config_dir: &Path,
    share: &crate::runtime::SharePolicy,
) -> Vec<String> {
    script
        .service
        .as_ref()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| {
                    let name = crate::runtime::entry_name(e, config_dir);
                    share.shared(&name, e.reuse).then_some(name)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Shuts down existing fog instances running the same script in the same
/// project, so a new instance can take their place. Returns any live services
/// handed over, keyed by service name, together with their PTY master fd.
///
/// Only instances serving the same `branch` are reclaimed; instances on a
/// different branch (concurrent multi-branch runs) are left untouched.
///
/// The caller is expected to hold the per-(project, script, branch) owner lock.
/// Names of every service in a script: the set a detach takeover hands over,
/// regardless of `reuse`/`share` configuration.
pub(crate) fn all_service_names(
    script: &crate::config::ScriptConfig,
    config_dir: &Path,
) -> Vec<String> {
    script
        .service
        .as_ref()
        .map(|entries| {
            entries
                .iter()
                .map(|e| crate::runtime::entry_name(e, config_dir))
                .collect()
        })
        .unwrap_or_default()
}

fn reclaim_existing(
    project: &str,
    script: &str,
    branch: Option<&str>,
    reuse: &[String],
    timeout: Duration,
) -> HashMap<String, ipc::HandoffItem> {
    let mut adopted: HashMap<String, ipc::HandoffItem> = HashMap::new();
    let existing = ipc::find_instances_for(project, script, branch);
    for (pid, path) in &existing {
        eprintln!(
            "replacing existing fog instance (pid {pid}, script {script}, project {project})"
        );
        let outcome = ipc::reclaim(path, reuse, false);
        if let Some(err) = &outcome.error {
            eprintln!("  warning: could not reclaim instance {pid}: {err}, continuing");
        } else if outcome.incomplete {
            eprintln!("  warning: handoff from instance {pid} was incomplete");
        }
        if outcome.handoffs.is_empty() {
            eprintln!("  old instance {pid} has no live services to reuse");
        } else {
            let names: Vec<&str> = outcome.handoffs.iter().map(|h| h.name.as_str()).collect();
            eprintln!("  reusing live services: {}", names.join(", "));
        }
        for handoff in outcome.handoffs {
            // A duplicate service name from another old instance: close the
            // losing fd so it is not leaked (the process itself stays up).
            if let Some(existing) = adopted.get_mut(&handoff.name) {
                // The handle was dupped for transfer and is owned by us.
                crate::fds::close(existing.fd);
                *existing = handoff;
            } else {
                adopted.insert(handoff.name.clone(), handoff);
            }
        }
        if ipc::wait_for_exit(*pid, timeout) {
            eprintln!("  old instance {pid} stopped");
        } else {
            eprintln!("  warning: instance {pid} did not stop within the timeout");
        }
        wait_for_socket_gone(path);
    }
    adopted
}

/// Reclaims one specific instance for a full-session takeover (TUI detach or
/// `fog attach`), adopting every live service it hands over, then waits for it
/// to exit. Returns `None` if the instance could not be taken over.
pub(crate) fn reclaim_takeover(
    from_pid: u32,
    names: &[String],
) -> Option<HashMap<String, ipc::HandoffItem>> {
    let path = ipc::socket_path(from_pid);
    let outcome = ipc::reclaim(&path, names, true);
    if let Some(err) = &outcome.error {
        eprintln!(
            "{} could not take over instance {from_pid}: {err}",
            error_label()
        );
        return None;
    }
    if outcome.incomplete {
        eprintln!("warning: takeover handoff from instance {from_pid} was incomplete");
    }
    let mut adopted: HashMap<String, ipc::HandoffItem> = HashMap::new();
    for handoff in outcome.handoffs {
        // A duplicate name would only come from a misbehaving peer; keep the
        // newest and close the losing fd so it is not leaked.
        if let Some(existing) = adopted.get_mut(&handoff.name) {
            crate::fds::close(existing.fd);
            *existing = handoff;
        } else {
            adopted.insert(handoff.name.clone(), handoff);
        }
    }
    if !ipc::wait_for_exit(from_pid, RECLAIM_WAIT_TIMEOUT) {
        eprintln!("warning: instance {from_pid} did not exit within the timeout");
    }
    wait_for_socket_gone(&path);
    Some(adopted)
}

/// Coordinates with any other fog instance running `script` in `project`, then
/// reclaims it. Returns any handed-over services and the owner lock, which the
/// caller must drop once its own services are up.
///
/// This makes concurrent startups deterministic:
/// - the instance that acquires the lock first performs the reclaim, and
/// - an instance that finds a just-started serving instance backs off with a
///   clear error instead of fighting over ports or shared infra.
pub(crate) fn reconcile_instance(
    project: &str,
    script: &str,
    branch: Option<&str>,
    reuse: &[String],
) -> (
    HashMap<String, ipc::HandoffItem>,
    Option<crate::lock::OwnerLock>,
) {
    let attempt_started = crate::lock::now_ms();

    let lock = match crate::lock::OwnerLock::try_acquire(project, script, branch) {
        Ok(crate::lock::AcquireResult::Locked(lock)) => lock,
        Ok(crate::lock::AcquireResult::HeldBy(holder)) => {
            let pid = holder
                .as_ref()
                .map(|h| format!(" (pid {})", h.pid))
                .unwrap_or_default();
            eprintln!(
                "another fog instance{pid} is starting script '{script}' for this project; waiting up to 30s"
            );
            match crate::lock::OwnerLock::acquire_with_timeout(
                project,
                script,
                branch,
                LOCK_WAIT_TIMEOUT,
            ) {
                Ok(Some(lock)) => lock,
                Ok(None) => {
                    eprintln!(
                        "error: another fog instance is already starting or stuck starting \
                         script '{script}' for this project; check `fog ls` and `fog kill <pid>`"
                    );
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!(
                        "  warning: could not lock project: {e}, proceeding without coordination"
                    );
                    return (
                        reclaim_existing(project, script, branch, reuse, RECLAIM_WAIT_TIMEOUT),
                        None,
                    );
                }
            }
        }
        Err(e) => {
            eprintln!("  warning: could not lock project: {e}, proceeding without coordination");
            return (
                reclaim_existing(project, script, branch, reuse, RECLAIM_WAIT_TIMEOUT),
                None,
            );
        }
    };

    // We now hold the owner lock. An instance that started after we began our
    // startup is a concurrent starter that beat us and is now serving: back
    // off rather than kill a freshly-started instance.
    let instances = ipc::find_instances_with_status(project, script, branch);
    if let Some((pid, _, _)) = instances
        .iter()
        .find(|(_, _, s)| s.started_at > attempt_started)
    {
        eprintln!(
            "error: another fog instance (pid {pid}) just started script '{script}' for this \
             project and now serves it; use `fog kill {pid}` to replace it"
        );
        std::process::exit(1);
    }

    let adopted = reclaim_existing(project, script, branch, reuse, RECLAIM_WAIT_TIMEOUT);
    (adopted, Some(lock))
}
