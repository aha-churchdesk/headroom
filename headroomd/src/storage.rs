//! Watching the disk the user's files are on, and saying so in a macOS
//! notification when it fills up. When to warn is `headroom_core::storage`.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use headroom_core::storage::{Alarm, notice, used_pct};

use crate::worktrees;

pub struct Watch {
    path: PathBuf,
    alarm: Alarm,
}

impl Watch {
    /// Watch the volume `path` is on: the home folder, where repositories
    /// and their builds live.
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            alarm: Alarm::default(),
        }
    }

    /// Read the disk. The notice to post, as (title, body), if this reading
    /// crossed a threshold. An unreadable disk is skipped, not warned about.
    pub fn check(&mut self) -> Option<(String, String)> {
        let d = worktrees::disk(&self.path)?;
        let threshold = self.alarm.check(used_pct(d.total, d.free))?;
        Some(notice(threshold, d.total, d.free))
    }
}

/// Post a notification without waiting for it. Through osascript: the
/// Homebrew build runs outside an app bundle, and the native notification
/// API needs one. Title and body go in as arguments, never as script text.
pub fn notify(title: &str, body: &str) -> std::io::Result<()> {
    let mut child = Command::new("/usr/bin/osascript")
        .args([
            "-e",
            "on run argv",
            "-e",
            "display notification (item 2 of argv) with title (item 1 of argv)",
            "-e",
            "end run",
            title,
            body,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Reap it off the event loop.
    std::thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_reading_warns_only_if_the_disk_is_past_the_first_threshold() {
        let path = std::env::temp_dir();
        let d = worktrees::disk(&path).unwrap();
        let mut w = Watch::new(path);
        let notice = w.check();
        assert_eq!(notice.is_some(), used_pct(d.total, d.free) >= 70.0);
        if let Some((title, _)) = notice {
            assert!(title.starts_with("Disk is "), "{title}");
        }
        assert_eq!(w.check(), None, "the same disk a second later is not news");
    }

    #[test]
    fn a_disk_that_cannot_be_read_is_skipped() {
        let mut w = Watch::new("/nonexistent/folder".into());
        assert_eq!(w.check(), None);
    }
}
