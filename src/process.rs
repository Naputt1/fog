use std::io;

/// Sends a signal to the entire process group of the given PID.
///
/// Negating the PID targets the process group, which is standard POSIX semantics.
///
/// # Arguments
/// * `pid` - The process ID (group leader).
/// * `signal` - The signal number to send (e.g. `SIGTERM`, `SIGKILL`).
///
/// # Errors
/// Returns an error if the kill syscall fails.
pub fn kill_process_group(pid: u32, signal: i32) -> io::Result<()> {
    debug_assert!(
        pid > 0,
        "kill_process_group: pid must be positive, got {}",
        pid
    );
    // SAFETY: pid is a valid process id from portable_pty. Negating pid
    // targets the entire process group, which is standard POSIX semantics.
    let ret = unsafe { libc::kill(-(pid as libc::pid_t), signal) };
    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Waits for a child process without blocking.
///
/// # Arguments
/// * `pid` - The child process ID to wait for.
///
/// # Returns
/// * `Ok(Some(status))` if the child has exited.
/// * `Ok(None)` if the child is still running.
/// * `Err(e)` if the waitpid call failed.
pub fn waitpid_nohang(pid: u32) -> io::Result<Option<i32>> {
    debug_assert!(pid > 0, "waitpid_nohang: pid must be positive, got {}", pid);
    let mut status: i32 = 0;
    // SAFETY:
    // - pid is a valid child process id from portable_pty
    // - WNOHANG ensures this call never blocks
    // - status is a valid pointer to a i32 on the stack
    let ret = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
    if ret == pid as libc::pid_t {
        Ok(Some(status))
    } else if ret == 0 {
        Ok(None)
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Returns `true` if a process with the given PID exists and is signalable.
///
/// Uses `kill(pid, 0)` which performs no signal delivery but reports whether
/// the process exists.
pub fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    if is_zombie(pid) {
        return false;
    }
    let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if ret == 0 {
        return true;
    }
    // EPERM means the process exists but we lack permission to signal it.
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Returns `true` if the process is a zombie. `kill(pid, 0)` reports zombies
/// as alive, which would make fog wait forever on an unreaped child.
#[cfg(target_os = "linux")]
fn is_zombie(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        // The comm field may contain spaces and parens; everything after the
        // final `)` is `state ppid pgrp ...`, with state first.
        .and_then(|s| s.rsplit_once(')').map(|(_, rest)| rest.to_string()))
        .and_then(|rest| rest.split_whitespace().next().map(str::to_string))
        .map(|state| state == "Z")
        .unwrap_or(false)
}

/// Attempts to send a signal to a process group, ignoring any errors.
///
/// # Arguments
/// * `pid` - The process ID (group leader).
/// * `signal` - The signal number to send.
pub fn try_kill_process_group(pid: u32, signal: i32) {
    debug_assert!(
        pid > 0,
        "try_kill_process_group: pid must be positive, got {}",
        pid
    );
    let _ = kill_process_group(pid, signal);
}

/// Returns `true` if the given process has any child processes.
///
/// On macOS this uses `proc_listchildpids`.
///
/// # Arguments
/// * `pid` - The parent process ID to check.
#[cfg(target_os = "macos")]
pub fn has_child_processes(pid: u32) -> bool {
    unsafe {
        let byte_count = libc::proc_listchildpids(pid as libc::pid_t, std::ptr::null_mut(), 0);
        byte_count > 0
    }
}

/// Returns `true` if the given process has any child processes.
///
/// On Linux this reads `/proc/<pid>/task/<pid>/children`.
///
/// # Arguments
/// * `pid` - The parent process ID to check.
#[cfg(target_os = "linux")]
pub fn has_child_processes(pid: u32) -> bool {
    let path = format!("/proc/{pid}/task/{pid}/children");
    std::fs::read_to_string(&path)
        .ok()
        .is_some_and(|s| !s.trim().is_empty())
}

/// Returns `true` if the given process has any child processes.
///
/// This is a no-op returning `false` on platforms other than macOS and Linux.
///
/// # Arguments
/// * `pid` - The parent process ID (ignored on non-macOS, non-Linux).
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn has_child_processes(_pid: u32) -> bool {
    false
}

/// Collects all descendant PIDs of the given PID (BFS, excludes `pid` itself).
#[cfg(target_os = "macos")]
pub fn descendant_pids(pid: u32) -> Vec<u32> {
    use std::collections::VecDeque;

    let mut result = Vec::new();
    let mut queue = VecDeque::new();
    queue.push_back(pid as libc::pid_t);

    while let Some(current_pid) = queue.pop_front() {
        // SAFETY:
        // First call queries the buffer size needed (null pointer, 0 length).
        // Second call fills the pre-allocated buffer with child PIDs.
        unsafe {
            let byte_count = libc::proc_listchildpids(current_pid, std::ptr::null_mut(), 0);
            if byte_count > 0 {
                let pid_count = byte_count as usize / std::mem::size_of::<libc::pid_t>();
                let mut children: Vec<libc::pid_t> = vec![0; pid_count];
                libc::proc_listchildpids(
                    current_pid,
                    children.as_mut_ptr() as *mut libc::c_void,
                    byte_count,
                );
                for &child_pid in &children {
                    if child_pid > 0 {
                        result.push(child_pid as u32);
                        queue.push_back(child_pid);
                    }
                }
            }
        }
    }
    result
}

/// Collects all descendant PIDs of the given PID via `/proc` PPid mapping.
///
/// Snapshot the tree BEFORE signaling the leader: once the leader exits,
/// its children are reparented (usually to pid 1) and a post-mortem scan
/// finds nothing.
#[cfg(target_os = "linux")]
pub fn descendant_pids(pid: u32) -> Vec<u32> {
    use std::collections::VecDeque;
    use std::fs;
    fn get_ppid(pid: u32) -> Option<u32> {
        let path = format!("/proc/{}/status", pid);
        let content = fs::read_to_string(&path).ok()?;
        for line in content.lines() {
            if let Some(ppid_str) = line.strip_prefix("PPid:\t") {
                return ppid_str.trim().parse().ok();
            }
        }
        None
    }

    let mut result = Vec::new();
    let entries = match fs::read_dir("/proc") {
        Ok(e) => e,
        Err(_) => return result,
    };

    let mut children_map: std::collections::HashMap<u32, Vec<u32>> =
        std::collections::HashMap::new();

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let child_pid: u32 = match name_str.parse() {
            Ok(pid) => pid,
            Err(_) => continue,
        };
        if let Some(ppid) = get_ppid(child_pid) {
            children_map.entry(ppid).or_default().push(child_pid);
        }
    }

    let mut queue = VecDeque::new();
    queue.push_back(pid);

    while let Some(current) = queue.pop_front() {
        if let Some(children) = children_map.get(&current) {
            for &child in children {
                result.push(child);
                queue.push_back(child);
            }
        }
    }

    result
}

/// Collects all descendant PIDs of the given PID.
///
/// This is a no-op returning an empty vec on platforms other than macOS and Linux.
///
/// # Arguments
/// * `pid` - The parent process ID (ignored on non-macOS, non-Linux).
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn descendant_pids(_pid: u32) -> Vec<u32> {
    vec![]
}

/// Kills all descendant processes of the given PID recursively with `SIGKILL`.
///
/// # Arguments
/// * `pid` - The parent process ID whose descendants should be killed.
pub fn kill_descendants(pid: u32) {
    debug_assert!(
        pid > 0,
        "kill_descendants: pid must be positive, got {}",
        pid
    );
    for child_pid in descendant_pids(pid) {
        // SAFETY: child_pid was just observed in the process table.
        let _ = unsafe { libc::kill(child_pid as libc::pid_t, libc::SIGKILL) };
    }
}

/// Sends a signal to a whole process tree: descendants first, then the group.
///
/// The descendant list is snapshotted BEFORE signaling the leader. A
/// backgrounded grandchild in its own process group (shell job control)
/// survives `kill(-leader, sig)`; and once the leader exits, orphans are
/// reparented so a post-mortem scan finds nothing. Signaling the snapshot
/// first closes that race.
///
/// # Arguments
/// * `pid` - The process ID (group leader / tree root).
/// * `signal` - The signal number to send (e.g. `SIGTERM`, `SIGKILL`).
pub fn signal_tree(pid: u32, signal: i32) {
    debug_assert!(pid > 0, "signal_tree: pid must be positive, got {}", pid);
    for child_pid in descendant_pids(pid) {
        // SAFETY: child_pid was just observed in the process table.
        let _ = unsafe { libc::kill(child_pid as libc::pid_t, signal) };
    }
    try_kill_process_group(pid, signal);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_try_kill_nonexistent_pid() {
        try_kill_process_group(999_999, libc::SIGTERM);
    }

    #[test]
    fn test_kill_process_group_nonexistent_pid() {
        let result = kill_process_group(999_999, libc::SIGTERM);
        assert!(result.is_err());
    }

    #[test]
    fn test_waitpid_nohang_nonexistent_pid() {
        let result = waitpid_nohang(999_999);
        assert!(result.is_err());
    }
}
