//! Per-(project, script) owner lock used to make concurrent `fog` startups
//! deterministic.
//!
//! When two instances try to start the same script in the same git project
//! (e.g. two worktrees, or a human plus an agent), the owner lock ensures
//! exactly one of them performs the reclaim of any existing instance. The
//! lock is held only for the startup critical section (scan → reclaim → spawn
//! services) and released once the instance is serving, so a later worktree
//! switch can still replace it.
//!
//! [`flock`] is used because it is released automatically when the holding
//! process dies, so stale locks are impossible. The lock file lives in the
//! temp directory, namespaced by a hash of the (project, script) identity.

use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// FNV-1a 64-bit hash of the `(project, script)` identity, hex-encoded.
///
/// Deterministic across processes so two fog instances derive the same lock
/// path.
pub fn identity_hash(project: &str, script: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in format!("{project}\u{1}{script}").bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Returns the lock file path for a given (project, script) identity.
pub fn owner_lock_path(project: &str, script: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fog-owner-{}.lock", identity_hash(project, script)))
}

/// Current epoch time in milliseconds.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Metadata about the process currently holding the owner lock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HolderInfo {
    /// PID of the holding fog process.
    pub pid: u32,
    /// Epoch milliseconds when the holder started.
    pub started_at: u64,
    /// Project identity the holder serves.
    pub project: String,
    /// Script name the holder runs.
    pub script: String,
}

/// Result of a non-blocking acquire attempt.
#[derive(Debug)]
pub enum AcquireResult {
    /// This process now holds the lock.
    Locked(OwnerLock),
    /// Another process holds the lock (with its metadata, if readable).
    HeldBy(Option<HolderInfo>),
}

/// A held owner lock. Dropping it releases the underlying `flock`.
#[derive(Debug)]
pub struct OwnerLock {
    file: File,
}

impl OwnerLock {
    /// Attempts to acquire the owner lock for `(project, script)` without
    /// blocking.
    pub fn try_acquire(project: &str, script: &str) -> io::Result<AcquireResult> {
        let path = owner_lock_path(project, script);
        // Not `truncate(true)` here on purpose: opening with truncate would
        // wipe a held lock's payload before we even attempt `flock`.
        #[allow(clippy::suspicious_open_options)]
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)?;
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if ret == 0 {
            write_payload(&file, project, script)?;
            Ok(AcquireResult::Locked(OwnerLock { file }))
        } else {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                Ok(AcquireResult::HeldBy(read_payload(&path).ok().flatten()))
            } else {
                Err(err)
            }
        }
    }

    /// Acquires the owner lock, retrying until `timeout` elapses.
    ///
    /// Returns `None` on timeout. A holder that crashes mid-start releases its
    /// lock automatically, so this succeeds early instead of waiting the full
    /// timeout.
    pub fn acquire_with_timeout(
        project: &str,
        script: &str,
        timeout: Duration,
    ) -> io::Result<Option<OwnerLock>> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match Self::try_acquire(project, script)? {
                AcquireResult::Locked(lock) => return Ok(Some(lock)),
                AcquireResult::HeldBy(_) => {
                    if std::time::Instant::now() >= deadline {
                        return Ok(None);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }
}

impl Drop for OwnerLock {
    fn drop(&mut self) {
        // SAFETY: `file` is a valid open descriptor owned by this struct.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn write_payload(file: &File, project: &str, script: &str) -> io::Result<()> {
    let info = HolderInfo {
        pid: std::process::id(),
        started_at: now_ms(),
        project: project.to_string(),
        script: script.to_string(),
    };
    let json = serde_json::to_string(&info).unwrap_or_default();
    let mut file = file.try_clone()?;
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(json.as_bytes())?;
    file.flush()
}

fn read_payload(path: &Path) -> io::Result<Option<HolderInfo>> {
    let mut contents = String::new();
    let mut file = File::open(path)?;
    file.read_to_string(&mut contents)?;
    Ok(serde_json::from_str(contents.trim()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_identity(tag: &str) -> (String, String) {
        (
            format!("{}-{}-{}", tag, std::process::id(), tag.len()),
            "dev".to_string(),
        )
    }

    #[test]
    fn test_identity_hash_stable_and_distinct() {
        let a = identity_hash("proj-a", "dev");
        let b = identity_hash("proj-a", "dev");
        let c = identity_hash("proj-a", "test");
        let d = identity_hash("proj-b", "dev");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn test_lock_contention_and_release() {
        let (project, script) = unique_identity("contend");
        let a = OwnerLock::try_acquire(&project, &script).expect("open lock");
        let lock_a = match a {
            AcquireResult::Locked(l) => l,
            _ => panic!("first acquire should succeed"),
        };
        // A second, independently opened descriptor must conflict.
        match OwnerLock::try_acquire(&project, &script).expect("open lock") {
            AcquireResult::HeldBy(info) => {
                let info = info.expect("holder metadata readable");
                assert_eq!(info.pid, std::process::id());
                assert_eq!(info.script, script);
                assert_eq!(info.project, project);
                assert!(info.started_at > 0);
            }
            AcquireResult::Locked(_) => panic!("second acquire must be denied"),
        }
        // Releasing lets another acquire succeed.
        drop(lock_a);
        let b = OwnerLock::try_acquire(&project, &script).expect("open lock");
        match b {
            AcquireResult::Locked(_) => {}
            AcquireResult::HeldBy(_) => panic!("lock should be free after drop"),
        }
    }

    #[test]
    fn test_acquire_with_timeout_when_held() {
        let (project, script) = unique_identity("timeout");
        let a = OwnerLock::try_acquire(&project, &script).expect("open lock");
        let lock_a = match a {
            AcquireResult::Locked(l) => l,
            _ => panic!("first acquire should succeed"),
        };
        let start = std::time::Instant::now();
        let got = OwnerLock::acquire_with_timeout(&project, &script, Duration::from_millis(150))
            .expect("timeout acquire");
        assert!(got.is_none());
        assert!(start.elapsed() >= Duration::from_millis(100));
        drop(lock_a);
    }
}
