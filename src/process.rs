use std::io;

pub fn kill_process_group(pid: u32, signal: i32) -> io::Result<()> {
    // SAFETY: pid is a valid process id from portable_pty. Negating pid
    // targets the entire process group, which is standard POSIX semantics.
    let ret = unsafe { libc::kill(-(pid as libc::pid_t), signal) };
    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

pub fn waitpid_nohang(pid: u32) -> io::Result<Option<i32>> {
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

pub fn try_kill_process_group(pid: u32, signal: i32) {
    let _ = kill_process_group(pid, signal);
}

#[cfg(target_os = "macos")]
pub fn kill_descendants(pid: u32) {
    use std::collections::VecDeque;

    let mut queue = VecDeque::new();
    queue.push_back(pid as libc::pid_t);

    while let Some(current_pid) = queue.pop_front() {
        // SAFETY:
        // First call queries the buffer size needed (null pointer, 0 length).
        // Second call fills the pre-allocated buffer with child PIDs.
        // Each returned PID > 0 is a valid child process to kill.
        unsafe {
            let byte_count =
                libc::proc_listchildpids(current_pid, std::ptr::null_mut(), 0);
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
                        libc::kill(child_pid, libc::SIGKILL);
                        queue.push_back(child_pid);
                    }
                }
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn kill_descendants(_pid: u32) {}
