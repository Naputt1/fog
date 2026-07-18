use serde::Deserialize;
use std::fmt;
use std::{
    collections::VecDeque,
    fs,
    io::{self, Read},
    os::unix::{
        io::{AsRawFd, FromRawFd, OwnedFd},
        process::CommandExt,
    },
    path::Path,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
};

const MAX_LINES: usize = 2000;

#[derive(Deserialize)]
pub struct Service {
    pub path: String,
    pub cmd: String,

    #[serde(skip)]
    child: Option<Child>,
    #[serde(skip)]
    handler: Option<JoinHandle<()>>,
    #[serde(skip)]
    output: Arc<Mutex<VecDeque<String>>>,
    #[serde(skip)]
    signal_write: Option<OwnedFd>,
    #[serde(skip)]
    pub stopped: bool,
}

impl fmt::Debug for Service {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Service")
            .field("path", &self.path)
            .field("cmd", &self.cmd)
            .field("child", &self.child)
            .field("handler", &self.handler)
            .field("output", &self.output)
            .field("signal_write", &self.signal_write)
            .field("stopped", &self.stopped)
            .finish()
    }
}

impl Service {
    pub fn run(&mut self) -> Result<(), std::io::Error> {
        let part: Vec<&str> = self.cmd.split_whitespace().collect();

        let mut raw = [0i32; 2];
        if unsafe { libc::pipe(raw.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let signal_read = unsafe { OwnedFd::from_raw_fd(raw[0]) };
        let signal_write = unsafe { OwnedFd::from_raw_fd(raw[1]) };
        self.signal_write = Some(signal_write);

        let mut child = Command::new(part[0]);
        let child = child
            .args(&part[1..])
            .current_dir(&self.path)
            .stdout(Stdio::piped())
            .process_group(0);
        unsafe {
            child.pre_exec(|| {
                libc::dup2(1, 2);
                Ok(())
            });
        }
        let mut child = child.spawn()?;
        let mut stdout = child.stdout.take().unwrap();

        self.child = Some(child);

        let output_mutex = self.output.clone();
        let handler = thread::spawn(move || {
            let stdout_fd = stdout.as_raw_fd();
            let signal_fd = signal_read.as_raw_fd();
            let mut fds = [
                libc::pollfd {
                    fd: stdout_fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: signal_fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let mut buffer = [0; 1024];
            let mut current_line = String::new();
            let mut after_cr = false;
            loop {
                let ret = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
                if ret < 0 {
                    break;
                }
                if fds[1].revents != 0 {
                    break;
                }
                if fds[0].revents & libc::POLLIN != 0 {
                    match stdout.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(n) => {
                            let mut output = output_mutex.lock().unwrap();
                            for &byte in &buffer[..n] {
                                match byte {
                                    b'\r' => {
                                        output.push_back(std::mem::take(&mut current_line));
                                        if output.len() > MAX_LINES {
                                            output.pop_front();
                                        }
                                        after_cr = true;
                                    }
                                    b'\n' => {
                                        if !after_cr {
                                            output.push_back(std::mem::take(&mut current_line));
                                            if output.len() > MAX_LINES {
                                                output.pop_front();
                                            }
                                        }
                                        after_cr = false;
                                    }
                                    _ => {
                                        after_cr = false;
                                        current_line.push(byte as char);
                                    }
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
                if fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                    break;
                }
            }
            if !current_line.is_empty() {
                let mut output = output_mutex.lock().unwrap();
                output.push_back(std::mem::take(&mut current_line));
                if output.len() > MAX_LINES {
                    output.pop_front();
                }
            }
        });
        self.handler = Some(handler);

        Ok(())
    }

    pub fn tail(&self, n: usize, offset: usize) -> Vec<String> {
        let output = self.output.lock().unwrap();
        let len = output.len();
        let end = len.saturating_sub(offset);
        let start = end.saturating_sub(n);
        output.range(start..end).cloned().collect()
    }

    pub fn total_lines(&self) -> usize {
        self.output.lock().unwrap().len()
    }

    fn kill_inner(&mut self) {
        if let Some(ref w) = self.signal_write {
            let byte: [u8; 1] = [0];
            unsafe {
                libc::write(w.as_raw_fd(), byte.as_ptr() as *const libc::c_void, 1);
            }
        }
        self.signal_write = None;

        if let Some(child) = &self.child {
            let pid = child.id() as libc::pid_t;
            let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
            kill_descendants(pid);
        }

        if let Some(handler) = self.handler.take() {
            let _ = handler.join();
        }

        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
        }
    }

    pub fn kill(&mut self) {
        if self.child.is_none() {
            return;
        }
        self.kill_inner();
        self.stopped = true;
    }

    pub fn restart(&mut self) -> Result<(), std::io::Error> {
        self.kill_inner();
        self.stopped = false;
        self.run()
    }
}

#[cfg(target_os = "macos")]
fn kill_descendants(pid: libc::pid_t) {
    use std::collections::VecDeque;

    let mut queue = VecDeque::new();
    queue.push_back(pid);

    while let Some(current_pid) = queue.pop_front() {
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
fn kill_descendants(_pid: libc::pid_t) {}

impl Drop for Service {
    fn drop(&mut self) {
        let path = Path::new(&self.path);
        let name: String = path.file_name().unwrap().to_string_lossy().into_owned();

        let _ = fs::create_dir_all("temp");

        if let Ok(output) = self.output.lock() {
            let text = output.iter().cloned().collect::<Vec<_>>().join("\n");
            _ = fs::write(format!("temp/{}.txt", name), &text);
        }

        self.kill_inner();
    }
}
