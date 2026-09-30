//! File descriptor passing over local IPC (SCM_RIGHTS on Unix).
//!
//! Used to hand a live PTY master fd from one fog instance to another so the
//! replacing instance can keep reading its output without killing it.
//!
//! Windows has no equivalent transfer (ConPTY handles are process-local and
//! cannot be duplicated), so live-service handoff is Unix-only and the Windows
//! entry points report [`io::ErrorKind::Unsupported`].

use std::io;

#[cfg(unix)]
use crate::ipc::transport::Stream;

/// Opaque handoff handle.
///
/// On Unix this is a duplicated PTY master file descriptor; on Windows live
/// handoff is unsupported and it is an uninhabited placeholder.
#[cfg(unix)]
pub type Fd = std::os::unix::io::RawFd;
#[cfg(windows)]
#[derive(Debug, Clone, Copy)]
pub struct Fd;

#[cfg(unix)]
const CMSG_BUF: usize = 64;

/// Byte buffer aligned enough to hold a `cmsghdr` (and any fd payload).
#[cfg(unix)]
#[repr(align(8))]
struct AlignBuf([u8; CMSG_BUF]);

/// Sends `fd` over `stream` using an SCM_RIGHTS control message.
///
/// # Errors
/// Returns an error if the sendmsg call fails.
#[cfg(unix)]
pub fn send_fd(stream: &Stream, fd: Fd) -> io::Result<()> {
    use std::mem;
    use std::os::unix::io::RawFd;
    let mut cmsg = AlignBuf([0u8; CMSG_BUF]);
    let mut byte = b'X';
    let mut iovec = libc::iovec {
        iov_base: (&mut byte as *mut u8).cast(),
        iov_len: 1,
    };
    // SAFETY: zeroed msghdr is fully initialized below before use.
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iovec;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg.0.as_mut_ptr().cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(mem::size_of::<RawFd>() as u32) as _ };

    let cmsg_ptr = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    if cmsg_ptr.is_null() {
        return Err(io::Error::other("control buffer too small to send fd"));
    }
    unsafe {
        (*cmsg_ptr).cmsg_level = libc::SOL_SOCKET;
        (*cmsg_ptr).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg_ptr).cmsg_len = libc::CMSG_LEN(mem::size_of::<RawFd>() as u32) as _;
        *(libc::CMSG_DATA(cmsg_ptr).cast::<RawFd>()) = fd;
    }

    let ret = unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, 0) };
    if ret >= 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Receives a file descriptor sent over `stream` via SCM_RIGHTS.
///
/// # Errors
/// Returns an error if the recvmsg call fails or no fd was received.
#[cfg(unix)]
pub fn recv_fd(stream: &Stream) -> io::Result<Fd> {
    use std::mem;
    use std::os::unix::io::RawFd;
    let mut cmsg = AlignBuf([0u8; CMSG_BUF]);
    let mut byte = 0u8;
    let mut iovec = libc::iovec {
        iov_base: (&mut byte as *mut u8).cast(),
        iov_len: 1,
    };
    // SAFETY: zeroed msghdr is fully initialized below before use.
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iovec;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg.0.as_mut_ptr().cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(mem::size_of::<RawFd>() as u32) as _ };

    let ret = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    if msg.msg_controllen == 0 {
        return Err(io::Error::other("no control message received"));
    }
    let cmsg_ptr = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    if cmsg_ptr.is_null() {
        return Err(io::Error::other("no fd received"));
    }
    let fd = unsafe { *(libc::CMSG_DATA(cmsg_ptr).cast::<RawFd>()) };
    if fd < 0 {
        return Err(io::Error::other("received invalid fd"));
    }
    Ok(fd)
}

/// Closes a handoff handle. No-op on Windows.
///
/// # Safety
/// On Unix the caller must own `fd` and not use it afterwards.
#[cfg(unix)]
pub fn close(fd: Fd) {
    // SAFETY: the caller owns the descriptor.
    unsafe { libc::close(fd) };
}

/// Closes a handoff handle. No-op on Windows.
#[cfg(windows)]
pub fn close(_fd: Fd) {}

/// Live-service handoff via fd passing is unsupported on Windows.
///
/// # Errors
/// Always returns [`io::ErrorKind::Unsupported`].
#[cfg(windows)]
pub fn send_fd(_stream: &crate::ipc::transport::Stream, _fd: Fd) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "fd passing is unsupported on Windows",
    ))
}

/// Live-service handoff via fd passing is unsupported on Windows.
///
/// # Errors
/// Always returns [`io::ErrorKind::Unsupported`].
#[cfg(windows)]
pub fn recv_fd(_stream: &crate::ipc::transport::Stream) -> io::Result<Fd> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "fd passing is unsupported on Windows",
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};

    #[test]
    fn test_send_recv_fd_roundtrip() {
        let path = std::env::temp_dir().join(format!("fog-fds-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let stream = crate::ipc::transport::Stream::from_unix(stream);
            recv_fd(&stream).unwrap()
        });

        let client = UnixStream::connect(&path).unwrap();
        let client = crate::ipc::transport::Stream::from_unix(client);
        let fd = unsafe { libc::dup(1) };
        send_fd(&client, fd).unwrap();

        let received = server.join().unwrap();
        assert_ne!(received, -1);
        unsafe { libc::close(fd) };
        unsafe { libc::close(received) };
        let _ = std::fs::remove_file(&path);
    }
}
