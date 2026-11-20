//! Persistent log file for Finder/Dock launches.
//!
//! Everything in Vox logs through println!/eprintln!, which vanishes when the
//! app isn't started from a terminal. Redirecting fd 1/2 once at startup keeps
//! that code untouched and also captures the Python daemons, which inherit
//! stderr. Every failure here is silent: logging must never take the app down.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

const MAX_BYTES: u64 = 5 * 1024 * 1024;

fn log_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Logs/Vox/vox.log"))
}

/// Move an oversized log to `<name>.1`, replacing any previous one. Returns
/// whether it rotated.
pub fn rotate_if_needed(path: &Path, max_bytes: u64) -> bool {
    let too_big = fs::metadata(path).map(|m| m.len() > max_bytes).unwrap_or(false);
    if !too_big {
        return false;
    }
    let mut rotated = path.as_os_str().to_owned();
    rotated.push(".1");
    fs::rename(path, PathBuf::from(rotated)).is_ok()
}

/// Send stdout and stderr to the log file — but only when stderr is not a
/// terminal, so `npm run dev` keeps printing to the console.
pub fn init() {
    // SAFETY: isatty only inspects a file descriptor.
    if unsafe { libc::isatty(2) } == 1 {
        return;
    }
    let Some(path) = log_path() else { return };
    let Some(dir) = path.parent() else { return };
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    rotate_if_needed(&path, MAX_BYTES);
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = writeln!(file, "\n=== Vox {} started (unix {secs}) ===", env!("CARGO_PKG_VERSION"));

    use std::os::fd::AsRawFd;
    let fd = file.as_raw_fd();
    // SAFETY: fd is a valid open file; dup2 gives fds 1 and 2 their own
    // reference, so dropping `file` afterwards is fine.
    unsafe {
        libc::dup2(fd, 1);
        libc::dup2(fd, 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_only_when_over_the_limit() {
        let dir = std::env::temp_dir().join(format!("vox-logfile-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let log = dir.join("vox.log");
        let old = dir.join("vox.log.1");

        // Missing file: nothing to do.
        assert!(!rotate_if_needed(&log, 10));

        // At the limit is not over it.
        fs::write(&log, [b'a'; 10]).unwrap();
        assert!(!rotate_if_needed(&log, 10));
        assert!(log.exists());

        // Over the limit rotates, replacing a stale .1.
        fs::write(&old, "stale").unwrap();
        fs::write(&log, [b'b'; 11]).unwrap();
        assert!(rotate_if_needed(&log, 10));
        assert!(!log.exists());
        assert_eq!(fs::read(&old).unwrap(), vec![b'b'; 11]);

        let _ = fs::remove_dir_all(&dir);
    }
}
