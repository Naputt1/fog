//! Process and signal helpers, portable across Unix and Windows.
//!
//! POSIX signals are abstracted behind [`Signal`] so callers never reference
//! platform-specific constants. On Unix a signal is delivered with `kill(2)`;
//! on Windows the equivalent is `TerminateProcess`, which has no graceful
//! variant, so both [`Signal::Term`] and [`Signal::Kill`] terminate.

use std::io;

/// A termination request, abstracting over POSIX signals.
///
/// `Term` is the polite request (`SIGTERM`) and `Kill` the forceful one
/// (`SIGKILL`). On Windows both map to `TerminateProcess`, which is immediate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Polite termination request (`SIGTERM`).
    Term,
    /// Forceful termination (`SIGKILL`).
    Kill,
}

/// Returns `true` if `name` is found on the `PATH`.
pub fn command_exists(name: &str) -> bool {
    #[cfg(unix)]
    let mut cmd = {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", &format!("command -v {name} >/dev/null 2>&1")]);
        c
    };
    #[cfg(windows)]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", &format!("where {name} >NUL 2>NUL")]);
        c
    };
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

/// Configures `cmd` to run detached from this process's controlling terminal
/// and session, so it survives the parent exiting.
///
/// On Unix this calls `setsid` in a `pre_exec` hook; on Windows it sets the
/// `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` creation flags.
pub fn detach_command(cmd: &mut std::process::Command) {
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
}

/// Sends a signal to a single process.
///
/// # Arguments
/// * `pid` - The target process ID.
/// * `signal` - The termination strength to request.
///
/// # Errors
/// Returns an error if the underlying call fails.
#[cfg(unix)]
pub fn kill_process(pid: u32, signal: Signal) -> io::Result<()> {
    debug_assert!(pid > 0, "kill_process: pid must be positive, got {}", pid);
    // SAFETY: pid is a valid process id.
    let ret = unsafe { libc::kill(pid as libc::pid_t, signal_number(signal)) };
    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Terminates a single process.
///
/// Windows has no graceful signal, so `signal` only affects nothing here; the
/// process is terminated immediately with `TerminateProcess`.
///
/// # Errors
/// Returns an error if the process could not be opened or terminated.
#[cfg(windows)]
pub fn kill_process(pid: u32, _signal: Signal) -> io::Result<()> {
    debug_assert!(pid > 0, "kill_process: pid must be positive, got {}", pid);
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
    // SAFETY: OpenProcess/TerminateProcess/CloseHandle operate on an owned handle.
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let ok = TerminateProcess(handle, 1);
        CloseHandle(handle);
        if ok != 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

/// Maps a [`Signal`] to its POSIX signal number.
#[cfg(unix)]
fn signal_number(signal: Signal) -> i32 {
    match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    }
}

/// Sends a signal to the entire process group of the given PID.
///
/// Negating the PID targets the process group, which is standard POSIX
/// semantics. On Windows there is no process group kill, so this terminates
/// the whole process tree (descendants first, then the process itself).
///
/// # Arguments
/// * `pid` - The process ID (group leader).
/// * `signal` - The termination strength to request.
///
/// # Errors
/// Returns an error if the kill call fails.
#[cfg(unix)]
pub fn kill_process_group(pid: u32, signal: Signal) -> io::Result<()> {
    debug_assert!(
        pid > 0,
        "kill_process_group: pid must be positive, got {}",
        pid
    );
    // SAFETY: pid is a valid process id from portable_pty. Negating pid
    // targets the entire process group, which is standard POSIX semantics.
    let ret = unsafe { libc::kill(-(pid as libc::pid_t), signal_number(signal)) };
    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Terminates the process tree rooted at `pid` (descendants first, then the
/// process itself). On Windows there is no process-group concept to signal.
///
/// # Errors
/// Returns an error if the root process could not be terminated.
#[cfg(windows)]
pub fn kill_process_group(pid: u32, signal: Signal) -> io::Result<()> {
    debug_assert!(
        pid > 0,
        "kill_process_group: pid must be positive, got {}",
        pid
    );
    // Windows has no POSIX process groups, so terminate the whole tree:
    // descendants first (while the parent still records their PPID), then the
    // process itself.
    for child_pid in descendant_pids(pid) {
        let _ = kill_process(child_pid, signal);
    }
    kill_process(pid, signal)
}

/// Waits for a child process without blocking.
///
/// # Arguments
/// * `pid` - The child process ID to wait for.
///
/// # Returns
/// * `Ok(Some(status))` if the child has exited.
/// * `Ok(None)` if the child is still running.
/// * `Err(e)` if the query failed (the process no longer exists).
#[cfg(unix)]
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

/// Reports whether a process has exited, mirroring `waitpid(pid, WNOHANG)`.
///
/// # Returns
/// * `Ok(Some(exit_code))` if the process has exited.
/// * `Ok(None)` if it is still running.
/// * `Err(e)` if the process could not be queried (it no longer exists).
#[cfg(windows)]
pub fn waitpid_nohang(pid: u32) -> io::Result<Option<i32>> {
    debug_assert!(pid > 0, "waitpid_nohang: pid must be positive, got {}", pid);
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: the handle is owned by this function and closed before returning.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if code == STILL_ACTIVE as u32 {
            Ok(None)
        } else {
            Ok(Some(code as i32))
        }
    }
}

/// Returns `true` if a process with the given PID exists and is signalable.
///
/// Uses `kill(pid, 0)` which performs no signal delivery but reports whether
/// the process exists.
#[cfg(unix)]
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

/// Returns `true` if a process with the given PID exists and has not exited.
#[cfg(windows)]
pub fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    matches!(waitpid_nohang(pid), Ok(None))
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
/// * `signal` - The termination strength to request.
pub fn try_kill_process_group(pid: u32, signal: Signal) {
    debug_assert!(
        pid > 0,
        "try_kill_process_group: pid must be positive, got {}",
        pid
    );
    let _ = kill_process_group(pid, signal);
}

/// Snapshot of `(pid, parent_pid)` pairs for every process on the system.
#[cfg(windows)]
fn process_snapshot() -> Vec<(u32, u32)> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    let mut out = Vec::new();
    // SAFETY: the snapshot handle is owned here and closed before returning;
    // PROCESSENTRY32W is fully initialized (dwSize) before use.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE || snapshot.is_null() {
            return out;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                out.push((entry.th32ProcessID, entry.th32ParentProcessID));
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
    }
    out
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
/// On Windows this scans a Toolhelp32 process snapshot for entries whose parent
/// is `pid`.
///
/// # Arguments
/// * `pid` - The parent process ID to check.
#[cfg(windows)]
pub fn has_child_processes(pid: u32) -> bool {
    process_snapshot().iter().any(|(_, ppid)| *ppid == pid)
}

/// Returns `true` if the given process has any child processes.
///
/// This is a no-op returning `false` on platforms other than macOS, Linux and
/// Windows.
///
/// # Arguments
/// * `pid` - The parent process ID (ignored on unsupported platforms).
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
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

/// Collects all descendant PIDs of the given PID via a Toolhelp32 snapshot.
///
/// Unlike POSIX, Windows does not reparent orphans, so the recorded parent PID
/// stays stable even after the parent exits — a post-mortem scan still finds
/// the tree.
#[cfg(windows)]
pub fn descendant_pids(pid: u32) -> Vec<u32> {
    use std::collections::{HashMap, VecDeque};

    let mut children_map: HashMap<u32, Vec<u32>> = HashMap::new();
    for (child, parent) in process_snapshot() {
        children_map.entry(parent).or_default().push(child);
    }

    let mut result = Vec::new();
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
/// This is a no-op returning an empty vec on platforms other than macOS, Linux
/// and Windows.
///
/// # Arguments
/// * `pid` - The parent process ID (ignored on unsupported platforms).
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
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
        let _ = kill_process(child_pid, Signal::Kill);
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
/// * `signal` - The termination strength to request.
#[cfg(unix)]
pub fn signal_tree(pid: u32, signal: Signal) {
    debug_assert!(pid > 0, "signal_tree: pid must be positive, got {}", pid);
    for child_pid in descendant_pids(pid) {
        let _ = kill_process(child_pid, signal);
    }
    try_kill_process_group(pid, signal);
}

/// Sends a signal to a whole process tree.
///
/// Windows has no process-group signal, so this is a tree termination:
/// descendants first, then the process itself.
///
/// # Arguments
/// * `pid` - The process tree root.
/// * `signal` - The termination strength to request.
#[cfg(windows)]
pub fn signal_tree(pid: u32, signal: Signal) {
    debug_assert!(pid > 0, "signal_tree: pid must be positive, got {}", pid);
    let _ = kill_process_group(pid, signal);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_try_kill_nonexistent_pid() {
        try_kill_process_group(999_999, Signal::Term);
    }

    #[test]
    fn test_kill_process_group_nonexistent_pid() {
        let result = kill_process_group(999_999, Signal::Term);
        assert!(result.is_err());
    }

    #[test]
    fn test_waitpid_nohang_nonexistent_pid() {
        let result = waitpid_nohang(999_999);
        assert!(result.is_err());
    }
}
