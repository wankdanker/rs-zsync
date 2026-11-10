//! `--lock-file`: keep two zsync runs (e.g. overlapping cron jobs) from working at once.
//!
//! Uses `flock(2)` on the file, so a crashed run never leaves a stale lock behind. The file is
//! left in place; waiting polls until `--lock-wait` milliseconds have passed.

use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::time::{Duration, Instant};

pub struct Lock {
    file: File,
    path: String,
}

impl Lock {
    pub fn acquire(path: &str, wait: Duration) -> Result<Lock> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("cannot open lock file {path}"))?;
        let deadline = Instant::now() + wait;
        loop {
            // SAFETY: flock on a valid, owned descriptor.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc == 0 {
                crate::debug!("obtained lock file {path}");
                return Ok(Lock { file, path: path.into() });
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EWOULDBLOCK) && err.kind() != std::io::ErrorKind::Interrupted {
                return Err(err).with_context(|| format!("cannot lock {path}"));
            }
            if Instant::now() >= deadline {
                bail!("EEXIST, lock file {path} is held by another zsync (waited {}ms)", wait.as_millis());
            }
            std::thread::sleep(Duration::from_millis(50).min(deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1))));
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // SAFETY: as above; closing the file would release it too.
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        crate::debug!("released lock file {}", self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_with_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zsync.lock");
        let path = path.to_str().unwrap();
        let first = Lock::acquire(path, Duration::from_millis(0)).unwrap();
        let start = Instant::now();
        let err = Lock::acquire(path, Duration::from_millis(200)).err().unwrap();
        assert!(err.to_string().contains("held by another zsync"));
        assert!(start.elapsed() >= Duration::from_millis(200));
        drop(first);
        Lock::acquire(path, Duration::from_millis(0)).unwrap();
    }

    #[test]
    fn waits_for_release() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zsync.lock").to_str().unwrap().to_string();
        let first = Lock::acquire(&path, Duration::from_millis(0)).unwrap();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            drop(first);
        });
        Lock::acquire(&path, Duration::from_secs(5)).unwrap();
        t.join().unwrap();
    }
}
