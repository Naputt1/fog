//! Cross-platform local IPC transport for fog instances.
//!
//! On Unix the endpoint is a Unix domain socket at the path returned by
//! [`super::socket_path`]. On Windows it is a named pipe derived from that
//! path's file name, and the path itself is written as an empty marker file so
//! instance discovery ([`super::find_instances`]) stays platform-agnostic.
//!
//! Both platforms expose the same small surface: [`Listener`], [`Stream`] and
//! [`connect`], with read timeouts so callers can poll for client liveness.

use std::io;
use std::path::Path;

#[cfg(unix)]
mod imp {
    use std::io::{self, Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;
    use std::time::Duration;

    /// A bound local IPC endpoint.
    pub struct Listener(UnixListener);

    /// A connected local IPC stream.
    pub struct Stream(UnixStream);

    impl Listener {
        pub fn bind(path: &Path) -> io::Result<Self> {
            Ok(Self(UnixListener::bind(path)?))
        }

        pub fn incoming(&self) -> impl Iterator<Item = io::Result<Stream>> + '_ {
            self.0.incoming().map(|r| r.map(Stream))
        }

        pub fn accept(&self) -> io::Result<Stream> {
            self.0.accept().map(|(stream, _addr)| Stream(stream))
        }
    }

    impl Stream {
        pub fn connect(path: &Path) -> io::Result<Self> {
            Ok(Self(UnixStream::connect(path)?))
        }

        pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
            self.0.set_read_timeout(timeout)
        }

        pub fn try_clone(&self) -> io::Result<Self> {
            Ok(Self(self.0.try_clone()?))
        }

        /// Raw descriptor, used for `SCM_RIGHTS` fd passing during handoff.
        pub fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
            use std::os::unix::io::AsRawFd;
            self.0.as_raw_fd()
        }

        /// Wraps an existing Unix stream (used by tests and fd-passing code).
        pub fn from_unix(stream: UnixStream) -> Self {
            Self(stream)
        }

        /// Borrows the underlying Unix stream.
        pub fn as_unix(&self) -> &UnixStream {
            &self.0
        }
    }

    impl Read for Stream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.read(buf)
        }
    }

    impl Write for Stream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.0.flush()
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::OsStr;
    use std::io::{self, Read, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{
        CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY,
        ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FlushFileBuffers, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
        ReadFile, WriteFile,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_UNLIMITED_INSTANCES, PeekNamedPipe, WaitNamedPipeW,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    /// Size of the named-pipe buffers, in bytes.
    const PIPE_BUFFER: u32 = 64 * 1024;
    /// How long [`Stream::connect`] keeps retrying while the server starts.
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

    /// `\\.\pipe\fog-<pid>.sock`, derived from the marker file name.
    fn pipe_name(path: &Path) -> Vec<u16> {
        let file = path.file_name().unwrap_or_default();
        let name = format!(r"\\.\pipe\{}", file.to_string_lossy());
        let mut wide: Vec<u16> = OsStr::new(&name).encode_wide().collect();
        wide.push(0);
        wide
    }

    /// Creates one listenable named-pipe instance.
    fn create_instance(name: &[u16]) -> io::Result<HANDLE> {
        // SAFETY: `name` is a valid null-terminated UTF-16 string.
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                // PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT are all 0.
                0,
                PIPE_UNLIMITED_INSTANCES,
                PIPE_BUFFER,
                PIPE_BUFFER,
                0,
                std::ptr::null(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(handle)
        }
    }

    /// Blocks until a client connects to `handle`, or fails.
    fn connect_instance(handle: HANDLE) -> io::Result<()> {
        // SAFETY: `handle` is a pipe instance created by `create_instance`.
        let ok = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) };
        if ok != 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32) {
            return Ok(());
        }
        // SAFETY: the instance is unusable; close it.
        unsafe { CloseHandle(handle) };
        Err(err)
    }

    /// A bound named-pipe endpoint. The next instance is pre-created so a
    /// client connecting right after `bind` never races the accept loop.
    pub struct Listener {
        name: Vec<u16>,
        pending: Mutex<HANDLE>,
    }

    // SAFETY: a pipe HANDLE is a kernel object whose ownership is unique to
    // this struct; it is safe to move between threads.
    unsafe impl Send for Listener {}
    unsafe impl Sync for Listener {}

    impl Listener {
        pub fn bind(path: &Path) -> io::Result<Self> {
            // Marker file so `find_instances` can discover this endpoint.
            std::fs::write(path, b"")?;
            let name = pipe_name(path);
            match create_instance(&name) {
                Ok(first) => Ok(Self {
                    name,
                    pending: Mutex::new(first),
                }),
                Err(e) => {
                    let _ = std::fs::remove_file(path);
                    Err(e)
                }
            }
        }

        pub fn incoming(&self) -> impl Iterator<Item = io::Result<Stream>> + '_ {
            std::iter::from_fn(move || Some(self.accept()))
        }

        pub fn accept(&self) -> io::Result<Stream> {
            let handle = {
                let mut pending = self.pending.lock().expect("pipe mutex poisoned");
                let current = *pending;
                // Pre-create the next instance (best-effort; on failure the
                // following accept creates one on demand).
                *pending = create_instance(&self.name).unwrap_or(INVALID_HANDLE_VALUE);
                current
            };
            let handle = if handle == INVALID_HANDLE_VALUE {
                create_instance(&self.name)?
            } else {
                handle
            };
            connect_instance(handle)?;
            Ok(Stream::from_handle(handle))
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let pending = *self.pending.lock().expect("pipe mutex poisoned");
            if pending != INVALID_HANDLE_VALUE {
                // SAFETY: pending is a pipe instance owned by this listener.
                unsafe { CloseHandle(pending) };
            }
        }
    }

    /// A connected named-pipe stream.
    pub struct Stream {
        handle: HANDLE,
        read_timeout: Option<Duration>,
    }

    // SAFETY: see `Listener`.
    unsafe impl Send for Stream {}

    impl Stream {
        fn from_handle(handle: HANDLE) -> Self {
            Self {
                handle,
                read_timeout: None,
            }
        }

        pub fn connect(path: &Path) -> io::Result<Self> {
            let name = pipe_name(path);
            let deadline = Instant::now() + CONNECT_TIMEOUT;
            loop {
                // SAFETY: `name` is a valid null-terminated UTF-16 string.
                let handle = unsafe {
                    CreateFileW(
                        name.as_ptr(),
                        GENERIC_READ | GENERIC_WRITE,
                        0,
                        std::ptr::null(),
                        OPEN_EXISTING,
                        FILE_ATTRIBUTE_NORMAL,
                        std::ptr::null_mut(),
                    )
                };
                if handle != INVALID_HANDLE_VALUE {
                    return Ok(Self::from_handle(handle));
                }
                let err = io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(c) if c == ERROR_PIPE_BUSY as i32 => {
                        // SAFETY: `name` is a valid pipe name.
                        unsafe { WaitNamedPipeW(name.as_ptr(), 200) };
                        continue;
                    }
                    Some(c) if c == ERROR_FILE_NOT_FOUND as i32 && Instant::now() < deadline => {
                        // The server has not created its instance yet.
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    _ => return Err(err),
                }
            }
        }

        pub fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
            self.read_timeout = timeout;
            Ok(())
        }

        pub fn try_clone(&self) -> io::Result<Self> {
            let mut new_handle: HANDLE = std::ptr::null_mut();
            // SAFETY: both handles are valid; the new handle is owned by the
            // returned Stream.
            let ok = unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    self.handle,
                    GetCurrentProcess(),
                    &mut new_handle,
                    0,
                    0,
                    DUPLICATE_SAME_ACCESS,
                )
            };
            if ok == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(Self {
                    handle: new_handle,
                    read_timeout: self.read_timeout,
                })
            }
        }
    }

    impl Drop for Stream {
        fn drop(&mut self) {
            if self.handle != INVALID_HANDLE_VALUE {
                // SAFETY: the handle is owned by this stream.
                unsafe { CloseHandle(self.handle) };
            }
        }
    }

    impl Read for Stream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if let Some(timeout) = self.read_timeout {
                // Named pipes have no native read timeout; poll for available
                // bytes so callers can use timeouts as a liveness probe.
                let deadline = Instant::now() + timeout;
                loop {
                    let mut avail: u32 = 0;
                    // SAFETY: the handle is valid and the out-pointer is ours.
                    let ok = unsafe {
                        PeekNamedPipe(
                            self.handle,
                            std::ptr::null_mut(),
                            0,
                            std::ptr::null_mut(),
                            &mut avail,
                            std::ptr::null_mut(),
                        )
                    };
                    if ok == 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if avail > 0 {
                        break;
                    }
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "named pipe read timed out",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            let mut read: u32 = 0;
            // SAFETY: `buf` is valid for `buf.len()` bytes.
            let ok = unsafe {
                ReadFile(
                    self.handle,
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    &mut read,
                    std::ptr::null_mut(),
                )
            };
            if ok != 0 {
                Ok(read as usize)
            } else {
                Err(io::Error::last_os_error())
            }
        }
    }

    impl Write for Stream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut written: u32 = 0;
            // SAFETY: `buf` is valid for `buf.len()` bytes.
            let ok = unsafe {
                WriteFile(
                    self.handle,
                    buf.as_ptr(),
                    buf.len() as u32,
                    &mut written,
                    std::ptr::null_mut(),
                )
            };
            if ok != 0 {
                Ok(written as usize)
            } else {
                Err(io::Error::last_os_error())
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            // SAFETY: the handle is owned by this stream.
            let ok = unsafe { FlushFileBuffers(self.handle) };
            if ok != 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }
    }
}

#[cfg(any(unix, windows))]
pub use imp::{Listener, Stream};

/// Connects to the endpoint at `path`.
pub fn connect(path: &Path) -> io::Result<Stream> {
    Stream::connect(path)
}

/// A boxed stream usable on both platforms for asynchronous callers.
pub type BoxedAsyncStream = Box<dyn AsyncStream + Unpin + Send>;

/// Marker for a stream that is both async-readable and async-writable.
pub trait AsyncStream: tokio::io::AsyncRead + tokio::io::AsyncWrite {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite> AsyncStream for T {}

/// Asynchronously connects to the endpoint at `path`.
#[cfg(unix)]
pub async fn connect_async(path: &Path) -> io::Result<BoxedAsyncStream> {
    Ok(Box::new(tokio::net::UnixStream::connect(path).await?))
}

/// Asynchronously connects to the endpoint at `path`.
#[cfg(windows)]
pub async fn connect_async(path: &Path) -> io::Result<BoxedAsyncStream> {
    let file = path.file_name().unwrap_or_default();
    let name = format!(r"\\.\pipe\{}", file.to_string_lossy());
    let client = tokio::net::windows::named_pipe::ClientOptions::new().open(name)?;
    Ok(Box::new(client))
}
