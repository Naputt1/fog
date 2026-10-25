//! PTY output readers: ingesting raw output into the parser and tee files.

use crate::fds::Fd;
use crate::process;
use std::{
    collections::VecDeque,
    fs,
    io::{self, Seek, SeekFrom, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

/// Largest log fog keeps per service.
///
/// Raw PTY output is unbounded by nature: one redraw per HMR update, a progress
/// bar, or an error loop can emit hundreds of megabytes per hour. Past this
/// size the tee discards the file and continues from the top, which keeps the
/// newest output (what `fog logs` tails) without ever filling the disk.
pub(super) const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

/// Bounded append sink for one service's raw PTY output.
pub(super) struct LogTee {
    file: fs::File,
    written: u64,
    cap: u64,
}

impl LogTee {
    /// Opens `<log_dir>/<name>.log`, replacing any previous run's file. The
    /// service name is sanitized so a name containing a `/` cannot escape the
    /// directory.
    pub(super) fn create(log_dir: &std::path::Path, name: &str) -> io::Result<Self> {
        let safe_name: String = name
            .chars()
            .map(|c| if c == '/' { '_' } else { c })
            .collect();
        Ok(Self {
            file: fs::File::create(log_dir.join(format!("{safe_name}.log")))?,
            written: 0,
            cap: MAX_LOG_BYTES,
        })
    }

    /// Appends one line, bounded like [`Self::write`].
    pub(super) fn write_line(&mut self, line: &str) {
        self.write(format!("{line}\n").as_bytes());
    }

    /// Appends `buf`, discarding earlier output once the cap is reached so the
    /// file never grows past it.
    fn write(&mut self, buf: &[u8]) {
        if self.written + buf.len() as u64 > self.cap {
            self.reset();
        }
        if self.file.write_all(buf).is_ok() {
            self.written += buf.len() as u64;
        }
    }

    /// Truncates the file and marks where earlier output was dropped.
    fn reset(&mut self) {
        self.written = 0;
        if self.file.set_len(0).is_err() || self.file.seek(SeekFrom::Start(0)).is_err() {
            return;
        }
        let marker = format!(
            "[fog] log truncated after reaching {} MiB; earlier output discarded\n",
            self.cap >> 20
        );
        if self.file.write_all(marker.as_bytes()).is_ok() {
            self.written = marker.len() as u64;
        }
    }
}

/// Notice shown for a borrowed (shared/reused) service, both in its tab and
/// in its log file.
pub(super) fn borrow_notice(name: &str) -> String {
    format!("♻ reusing already-running '{name}'; start skipped (press R to take over)")
}

/// Creates a pipe used to signal a reader thread to stop. Returns
/// `(read_end, write_end)`.
#[cfg(unix)]
pub(super) fn make_stop_pipe() -> io::Result<(Fd, Fd)> {
    let mut fds = [-1i32, -1];
    let ret = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if ret != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok((fds[0], fds[1]))
    }
}

/// Feeds one chunk of PTY output into the shared parser, bumps the generation
/// counter, appends to the bounded raw-output queue (re-render / scrollback),
/// and tees it to the optional log file. Shared by every platform's reader.
fn ingest(
    parser: &Mutex<vt100::Parser>,
    generation: &AtomicUsize,
    raw_output: &Mutex<VecDeque<Vec<u8>>>,
    tee: &mut Option<LogTee>,
    buf: &[u8],
) {
    if let Ok(mut p) = parser.lock() {
        p.process(buf);
    }
    generation.fetch_add(1, Ordering::Relaxed);
    {
        let mut q = raw_output.lock().expect("mutex poisoned");
        if q.len() >= 500 {
            q.pop_front();
        }
        q.push_back(buf.to_vec());
    }
    if let Some(tee) = tee.as_mut() {
        tee.write(buf);
    }
}

/// Spawns a thread that reads PTY output from `fd` and feeds the parser,
/// stopping when the PTY reaches EOF or the `stop` pipe becomes readable.
///
/// When `tee` is `Some`, each raw chunk read is also appended to it (bounded by
/// [`MAX_LOG_BYTES`]), capturing service output to `<log_dir>/<name>.log`.
///
/// The thread owns `fd`, `stop`, and `tee` and closes them on exit.
#[cfg(unix)]
pub(super) fn spawn_reader(
    parser: Arc<Mutex<vt100::Parser>>,
    generation: Arc<AtomicUsize>,
    raw_output: Arc<Mutex<VecDeque<Vec<u8>>>>,
    fd: Fd,
    stop: Fd,
    mut tee: Option<LogTee>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut pfds = [
            libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stop,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            // SAFETY: pfds is a valid array of pollfd structs.
            let r = unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) };
            if r < 0 {
                break;
            }
            if pfds[1].revents != 0 {
                break;
            }
            if pfds[0].revents & libc::POLLIN != 0 {
                // SAFETY: fd is a valid, owned descriptor opened for reading.
                let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                if n <= 0 {
                    break;
                }
                ingest(
                    &parser,
                    &generation,
                    &raw_output,
                    &mut tee,
                    &buf[..n as usize],
                );
            } else if pfds[0].revents != 0 {
                break;
            }
        }
        // SAFETY: this thread owns these fds.
        unsafe {
            libc::close(fd);
            libc::close(stop);
        }
    })
}

/// Spawns a thread that reads PTY output from a portable-pty reader and feeds
/// the parser, until the PTY reaches EOF or the master is closed.
///
/// Windows uses ConPTY, whose output handle cannot be polled alongside a stop
/// pipe, so there is no explicit cancellation: dropping the master (which
/// closes the pseudoconsole) ends the read. Every teardown path kills the
/// child and drops the master, so the thread always terminates.
#[cfg(windows)]
pub(super) fn spawn_reader_pty(
    parser: Arc<Mutex<vt100::Parser>>,
    generation: Arc<AtomicUsize>,
    raw_output: Arc<Mutex<VecDeque<Vec<u8>>>>,
    mut reader: Box<dyn std::io::Read + Send>,
    mut tee: Option<LogTee>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    ingest(&parser, &generation, &raw_output, &mut tee, &buf[..n]);
                }
                Err(_) => break,
            }
        }
    })
}

/// Builds a `Command` that runs `cmd` through the platform shell.
pub(super) fn shell_command(cmd: &str) -> std::process::Command {
    #[cfg(unix)]
    {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", cmd]);
        c
    }
    #[cfg(windows)]
    {
        let mut c = std::process::Command::new(super::default_shell());
        c.args(["/C", cmd]);
        c
    }
}

/// Polls `waitpid(pid, WNOHANG)` until the child is reaped or `timeout` elapses.
///
/// Returns `true` if the child was reaped, `false` if it was still running (or
/// was already reaped elsewhere) when the timeout expired. Never blocks longer
/// than `timeout`, so a process stuck in an uninterruptible exit state cannot
/// freeze teardown.
pub(super) fn wait_reaped(pid: u32, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match process::waitpid_nohang(pid) {
            Ok(Some(_)) => return true,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    return false;
                }
                thread::sleep(Duration::from_millis(50));
            }
            // ECHILD (already reaped) or another error: nothing left to wait on.
            Err(_) => return false,
        }
    }
}

/// A write-only wrapper around a raw fd (e.g. a PTY master received from
/// another instance). Owns the fd and closes it on drop.
#[cfg(unix)]
pub(super) struct FdWriter {
    pub(super) fd: Fd,
}

#[cfg(unix)]
impl Write for FdWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // SAFETY: fd is a valid, owned descriptor opened for writing.
        let n = unsafe { libc::write(self.fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for FdWriter {
    fn drop(&mut self) {
        // SAFETY: this struct owns the fd.
        unsafe { libc::close(self.fd) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fog-test-tee-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_log_tee_caps_file_and_keeps_newest_output() {
        let dir = temp_dir("cap");
        let path = dir.join("web.log");
        let mut tee = LogTee::create(&dir, "web").unwrap();

        let chunk = vec![b'x'; 4096];
        let chunks = (MAX_LOG_BYTES / chunk.len() as u64) + 40;
        for _ in 0..chunks {
            tee.write(&chunk);
        }

        let content = fs::read(&path).unwrap();
        assert!(
            content.len() as u64 <= MAX_LOG_BYTES,
            "tee must keep the file under its cap, got {} bytes",
            content.len()
        );
        assert!(
            content.starts_with(b"[fog] log truncated after reaching"),
            "dropped output must be recorded in the log"
        );
        assert!(
            content.ends_with(&chunk),
            "the newest output must survive truncation"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
