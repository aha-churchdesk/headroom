//! One instance at a time.
//!
//! Two daemons would each run their own queue, both admitting jobs against
//! the same free memory, which is the overcommit Headroom exists to prevent.
//! It becomes likely the moment a login item exists: launchd starts one, and
//! opening the app from Spotlight starts another.
//!
//! An advisory `flock` held for the process lifetime is enough — it is
//! released automatically when the process dies, including on a crash or a
//! force quit, so a stale lock cannot lock you out.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::Path;

/// Holds the lock. Dropping it (or exiting) releases it.
pub struct InstanceLock(#[allow(dead_code)] File);

/// Take the lock, or return `None` if another instance already holds it.
pub fn acquire(dir: &Path) -> Option<InstanceLock> {
    acquire_named(dir, "instance.lock")
}

/// The same, for a lock of another name: the menu bar keeps its own, so a
/// second app window can't open beside a daemon that runs headless.
pub fn acquire_named(dir: &Path, name: &str) -> Option<InstanceLock> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(name))
        .ok()?;
    // LOCK_NB so we fail immediately rather than blocking behind the running
    // instance forever.
    let ok = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    ok.then_some(InstanceLock(file))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("headroom-lock-{name}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn the_first_instance_gets_the_lock() {
        let dir = tmpdir("first");
        assert!(acquire(&dir).is_some());
    }

    #[test]
    fn a_second_instance_is_turned_away() {
        let dir = tmpdir("second");
        let _held = acquire(&dir).expect("first");
        // Within one process flock is per file description, so a fresh open
        // is genuinely a second contender.
        assert!(
            acquire(&dir).is_none(),
            "two daemons would admit against the same memory"
        );
    }

    /// A crash must not leave the app unable to start again.
    #[test]
    fn the_lock_is_released_when_the_holder_goes_away() {
        let dir = tmpdir("released");
        drop(acquire(&dir).expect("first"));
        assert!(acquire(&dir).is_some(), "no stale lock survives");
    }

    #[test]
    fn named_locks_are_independent_of_each_other() {
        let dir = tmpdir("named");
        let _daemon = acquire(&dir).expect("daemon");
        let _ui = acquire_named(&dir, "ui.lock").expect("the menu bar beside a daemon");
        assert!(
            acquire_named(&dir, "ui.lock").is_none(),
            "but only one menu bar"
        );
    }
}
