use super::types::{HandoffItem, KillResponse, StatusResponse};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// How long the reclaiming side waits for the replaced instance to prepare
/// and send its handoffs before giving up.
pub(crate) const HANDOFF_PREPARE_TIMEOUT_SECS: u64 = 30;

/// The handoff message sent over the wire before each transferred fd.
#[derive(Debug, Serialize)]
struct HandoffMsg {
    r#type: String,
    name: String,
    pid: u32,
}

/// The handoff message as received by the reclaiming client.
#[derive(Debug, Deserialize)]
struct HandoffMsgReply {
    name: String,
    pid: u32,
}

/// The final response to a kill/reclaim request.
#[derive(Debug, Deserialize)]
struct KillReply {
    ok: bool,
    #[serde(default)]
    reason: String,
}

/// Outcome of a reclaim attempt against a single instance.
pub struct ReclaimOutcome {
    /// Live services successfully transferred. The PTY master fds are owned
    /// by the caller.
    pub handoffs: Vec<HandoffItem>,
    /// True if the transfer was cut short (connection dropped or an fd could
    /// not be received) after some handoffs were delivered.
    pub incomplete: bool,
    /// Present when the reclaim failed outright and no handoffs are usable.
    pub error: Option<String>,
}

impl ReclaimOutcome {
    fn failed(error: impl Into<String>) -> Self {
        Self {
            handoffs: Vec::new(),
            incomplete: false,
            error: Some(error.into()),
        }
    }
}

/// Reads a single newline-terminated line directly from the stream without
/// buffering, so fd-passing control messages are not swallowed by a reader.
fn read_line_nobuf(mut stream: &UnixStream) -> io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before response",
            ));
        }
        line.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&line).into_owned())
}

/// Waits for the App to prepare handoffs, then sends each one (metadata line
/// followed by the fd via SCM_RIGHTS) and finally the kill response.
pub(crate) fn send_handoffs(mut stream: UnixStream, state: Arc<super::types::IpcState>) {
    // Wait for the App to prepare the handoffs. An empty result set after
    // preparation legitimately means "no live services to hand over".
    let deadline = std::time::Instant::now() + Duration::from_secs(HANDOFF_PREPARE_TIMEOUT_SECS);
    loop {
        if state
            .handoff_prepared
            .load(std::sync::atomic::Ordering::SeqCst)
            || std::time::Instant::now() >= deadline
        {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }

    let mut results = std::mem::take(&mut *state.handoff_results.lock().expect("mutex poisoned"));

    let mut ok = true;
    while let Some(item) = results.pop() {
        let msg = HandoffMsg {
            r#type: "handoff".to_string(),
            name: item.name,
            pid: item.pid,
        };
        let line = match serde_json::to_string(&msg) {
            Ok(l) => l,
            Err(_) => {
                // SAFETY: the fd was dupped for transfer; close it if unsent.
                unsafe { libc::close(item.fd) };
                continue;
            }
        };
        if writeln!(stream, "{line}")
            .and_then(|_| stream.flush())
            .is_err()
            || crate::fds::send_fd(&stream, item.fd).is_err()
        {
            ok = false;
            // SAFETY: the fd was dupped for transfer; close it if unsent.
            unsafe { libc::close(item.fd) };
            break;
        }
    }
    // Close any fds that were never sent (transfer aborted mid-way).
    for item in results {
        // SAFETY: the fds were dupped for transfer and are owned by us.
        unsafe { libc::close(item.fd) };
    }

    let reason = if ok {
        String::new()
    } else {
        "handoff transfer failed; connection closed".to_string()
    };
    let _ = writeln!(
        stream,
        "{}",
        serde_json::to_string(&KillResponse { ok, reason }).unwrap_or_default()
    );
    let _ = stream.flush();
    state
        .handoff_done
        .store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Requests a graceful shutdown of an instance while reclaiming live services.
///
/// Sends a kill request carrying `reuse` names; the old instance responds with
/// one `handoff` message (plus an SCM_RIGHTS fd) per live service to keep
/// running, then a kill response. Returns the received handoffs.
///
/// If the transfer is cut short, any already-received handoffs are still
/// returned (alongside `incomplete`), and a process whose fd could not be
/// transferred is killed to avoid leaving an orphan behind.
pub fn reclaim(path: &Path, reuse: &[String]) -> ReclaimOutcome {
    let mut stream = match UnixStream::connect(path) {
        Ok(s) => s,
        Err(e) => return ReclaimOutcome::failed(format!("connection failed: {e}")),
    };
    // The replaced instance may take up to HANDOFF_PREPARE_TIMEOUT_SECS to
    // prepare and send its handoffs before replying, so the client's read
    // timeout must cover that window; using the shorter READ_TIMEOUT_SECS
    // would fail the reclaim while the old instance is still mid-handoff.
    if stream
        .set_read_timeout(Some(Duration::from_secs(HANDOFF_PREPARE_TIMEOUT_SECS)))
        .is_err()
    {
        return ReclaimOutcome::failed("could not set read timeout");
    }
    let mut payload = r#"{"type":"kill""#.to_string();
    if !reuse.is_empty() {
        let names = serde_json::to_string(reuse).unwrap_or_else(|_| "[]".to_string());
        payload.push_str(&format!(r#","reuse":{names}"#));
    }
    payload.push_str("}\n");
    if stream.write_all(payload.as_bytes()).is_err() || stream.flush().is_err() {
        return ReclaimOutcome::failed("could not send reclaim request");
    }

    let mut handoffs = Vec::new();
    loop {
        let line = match read_line_nobuf(&stream) {
            Ok(l) => l,
            Err(e) => {
                if handoffs.is_empty() {
                    return ReclaimOutcome::failed(format!(
                        "connection closed before response: {e}"
                    ));
                }
                return ReclaimOutcome {
                    handoffs,
                    incomplete: true,
                    error: Some(format!("connection closed mid-transfer: {e}")),
                };
            }
        };
        if let Ok(reply) = serde_json::from_str::<HandoffMsgReply>(line.trim()) {
            match crate::fds::recv_fd(&stream) {
                Ok(fd) => {
                    handoffs.push(HandoffItem {
                        name: reply.name,
                        pid: reply.pid,
                        fd,
                    });
                }
                Err(e) => {
                    // The old instance extracted this process but we could not
                    // receive its fd: kill it so it does not run on as an
                    // unmanaged orphan.
                    crate::process::try_kill_process_group(reply.pid, libc::SIGTERM);
                    thread::sleep(Duration::from_millis(300));
                    crate::process::try_kill_process_group(reply.pid, libc::SIGKILL);
                    return ReclaimOutcome {
                        handoffs,
                        incomplete: true,
                        error: Some(format!("failed to receive fd for '{}': {e}", reply.name)),
                    };
                }
            }
            continue;
        }
        if let Ok(reply) = serde_json::from_str::<KillReply>(line.trim()) {
            if reply.ok {
                return ReclaimOutcome {
                    handoffs,
                    incomplete: false,
                    error: None,
                };
            }
            let reason = if reply.reason.is_empty() {
                "instance refused to be replaced".to_string()
            } else {
                reply.reason
            };
            return ReclaimOutcome::failed(reason);
        }
        return ReclaimOutcome::failed("unexpected response from fog instance");
    }
}

/// Finds all running instances belonging to the given project that run the
/// given script on the given branch, excluding this process.
///
/// When `branch` is `Some(b)`, only instances serving exactly that branch
/// match. When it is `None`, only branch-less instances match (legacy / non-git
/// runs), so branch-aware instances never reclaim each other.
///
/// Returns a sorted list of `(pid, socket_path)` pairs.
pub fn find_instances_for(
    project: &str,
    script: &str,
    branch: Option<&str>,
) -> Vec<(u32, PathBuf)> {
    find_instances_with_status(project, script, branch)
        .into_iter()
        .map(|(pid, path, _)| (pid, path))
        .collect()
}

/// Like [`find_instances_for`], but also returns each instance's status
/// snapshot (including its `started_at` and branch).
pub fn find_instances_with_status(
    project: &str,
    script: &str,
    branch: Option<&str>,
) -> Vec<(u32, PathBuf, StatusResponse)> {
    let self_pid = std::process::id();
    let mut out = Vec::new();
    let Ok(instances) = super::find_instances() else {
        return out;
    };
    for (pid, path) in instances {
        if pid == self_pid {
            continue;
        }
        match super::query_status(&path) {
            Ok(status) => {
                if status.project.as_deref() != Some(project) || status.script != script {
                    continue;
                }
                let same_branch = match branch {
                    Some(b) => status.branch.as_deref() == Some(b),
                    None => status.branch.is_none(),
                };
                if same_branch {
                    out.push((pid, path, status));
                }
            }
            Err(_) => {
                if !crate::process::is_pid_alive(pid) {
                    let _ = fs::remove_file(&path);
                }
            }
        }
    }
    out.sort_by_key(|(pid, _, _)| *pid);
    out
}

/// Finds every other running instance serving `script` in `project`, on ANY
/// branch. Used to decide whether shared (reuse) infrastructure may be torn
/// down: as long as another branch still runs the same script, the last one
/// must keep the shared resource alive.
pub fn find_instances_any_branch(
    project: &str,
    script: &str,
) -> Vec<(u32, PathBuf, StatusResponse)> {
    let self_pid = std::process::id();
    let mut out = Vec::new();
    let Ok(instances) = super::find_instances() else {
        return out;
    };
    for (pid, path) in instances {
        if pid == self_pid {
            continue;
        }
        match super::query_status(&path) {
            Ok(status) => {
                if status.project.as_deref() == Some(project) && status.script == script {
                    out.push((pid, path, status));
                }
            }
            Err(_) => {
                if !crate::process::is_pid_alive(pid) {
                    let _ = fs::remove_file(&path);
                }
            }
        }
    }
    out.sort_by_key(|(pid, _, _)| *pid);
    out
}

/// Returns the lowest-PID instance serving `script` in `project` on `branch`,
/// if any.
pub fn find_serving(
    project: &str,
    script: &str,
    branch: Option<&str>,
) -> Option<(u32, PathBuf, StatusResponse)> {
    find_instances_with_status(project, script, branch)
        .into_iter()
        .next()
}

/// Waits until the given process has exited, up to `timeout`.
///
/// Returns `true` if the process exited within the timeout.
pub fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !crate::process::is_pid_alive(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    !crate::process::is_pid_alive(pid)
}

/// Terminates the given fog instances, returning how many were signalled.
///
/// Each instance is first asked to shut down gracefully over its IPC socket
/// (the same path `fog kill <pid>` uses, so services tear down cleanly). An
/// instance that has not exited within [`wait_for_exit`]'s grace period is
/// force-stopped with SIGTERM, so a hung or unresponsive instance cannot
/// survive.
pub fn terminate_instances(instances: &[(u32, PathBuf)]) -> usize {
    if instances.is_empty() {
        return 0;
    }
    for (pid, path) in instances {
        let _ = super::send_kill(path);
        if !wait_for_exit(*pid, Duration::from_secs(2)) && crate::process::is_pid_alive(*pid) {
            crate::process::try_kill_process_group(*pid, libc::SIGTERM);
        }
    }
    instances.len()
}
