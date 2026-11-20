//! Ending processes Headroom doesn't manage, from the "Other apps" page.
//!
//! The page lists processes a few seconds old, and pids are reused, so each
//! process is named by its pid and start time and only signalled while both
//! still match. SIGTERM first, so it can save and exit; SIGKILL after the
//! grace period for whatever is still the same process.

use std::time::Duration;

use crate::sample;

/// One process: its pid and when it started.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Target {
    pub pid: i32,
    pub start: u64,
}

impl Target {
    /// The process now at `pid`, if it may be inspected.
    pub fn of(pid: i32) -> Option<Self> {
        sample::process_start(pid).map(|start| Self { pid, start })
    }

    /// Whether `pid` is still this process.
    pub fn alive(&self) -> bool {
        sample::process_start(self.pid) == Some(self.start)
    }
}

/// What a kill did: how many processes were sent SIGTERM, and why any were
/// not. A process that already exited is neither.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub signalled: Vec<Target>,
    pub refused: Vec<String>,
}

/// Why `t` must not be signalled from the page, if it must not.
fn refuse(t: &Target, jobs: &[i32]) -> Option<String> {
    if t.pid <= 1 {
        return Some(format!("pid {} is the system's", t.pid));
    }
    if t.pid == std::process::id() as i32 {
        return Some("that is this menu bar app".into());
    }
    if jobs.contains(&t.pid) {
        return Some(format!(
            "pid {} is a Headroom job: kill it from the queue",
            t.pid
        ));
    }
    if sample::exe_path(t.pid).is_some_and(|p| sample::is_headroom(&p)) {
        return Some(format!("pid {} is part of Headroom", t.pid));
    }
    None
}

/// SIGTERM each target still alive, then SIGKILL, after `grace`, those
/// still the same process. `jobs` are the pids of Headroom's jobs, which
/// are never signalled from here.
pub fn term(targets: &[Target], jobs: &[i32], grace: Duration) -> Outcome {
    let mut out = Outcome::default();
    for t in targets {
        if !t.alive() {
            continue;
        }
        if let Some(why) = refuse(t, jobs) {
            out.refused.push(why);
            continue;
        }
        if unsafe { libc::kill(t.pid, libc::SIGTERM) } == 0 {
            out.signalled.push(*t);
            continue;
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => {}
            Some(libc::EPERM) => out
                .refused
                .push(format!("pid {} belongs to another user", t.pid)),
            _ => out.refused.push(format!(
                "pid {}: {}",
                t.pid,
                std::io::Error::last_os_error()
            )),
        }
    }
    if !out.signalled.is_empty() {
        let owed = out.signalled.clone();
        std::thread::spawn(move || {
            std::thread::sleep(grace);
            for t in owed.iter().filter(|t| t.alive()) {
                unsafe { libc::kill(t.pid, libc::SIGKILL) };
            }
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command};
    use std::time::Instant;

    fn wait_gone(c: &mut Child, within: Duration) -> Option<std::process::ExitStatus> {
        let until = Instant::now() + within;
        while Instant::now() < until {
            if let Some(st) = c.try_wait().unwrap() {
                return Some(st);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    fn signal_of(st: std::process::ExitStatus) -> Option<i32> {
        use std::os::unix::process::ExitStatusExt;
        st.signal()
    }

    #[test]
    fn a_process_that_quits_on_sigterm_gets_only_that() {
        let mut c = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let t = Target::of(c.id() as i32).unwrap();
        let out = term(&[t], &[], Duration::from_secs(30));
        assert_eq!(out.signalled, vec![t]);
        assert!(out.refused.is_empty());
        let st = wait_gone(&mut c, Duration::from_secs(5)).expect("exited");
        assert_eq!(signal_of(st), Some(libc::SIGTERM));
    }

    #[test]
    fn a_process_that_ignores_sigterm_is_killed_after_the_grace_period() {
        let mut c = Command::new("/bin/sh")
            .args(["-c", "trap '' TERM; while :; do sleep 0.05; done"])
            .spawn()
            .unwrap();
        // Let the shell install its trap before it is signalled.
        std::thread::sleep(Duration::from_millis(200));
        let t = Target::of(c.id() as i32).unwrap();
        let out = term(&[t], &[], Duration::from_millis(300));
        assert_eq!(out.signalled, vec![t]);
        assert!(
            wait_gone(&mut c, Duration::from_millis(150)).is_none(),
            "still there during the grace period"
        );
        let st = wait_gone(&mut c, Duration::from_secs(5)).expect("killed");
        assert_eq!(signal_of(st), Some(libc::SIGKILL));
    }

    #[test]
    fn a_reused_pid_is_not_signalled() {
        let mut c = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let real = Target::of(c.id() as i32).unwrap();
        let stale = Target {
            start: real.start.wrapping_sub(1),
            ..real
        };
        let out = term(&[stale], &[], Duration::from_millis(10));
        assert_eq!(out, Outcome::default(), "another process: left alone");
        std::thread::sleep(Duration::from_millis(100));
        assert!(c.try_wait().unwrap().is_none(), "the real one still runs");
        c.kill().unwrap();
        c.wait().unwrap();
    }

    #[test]
    fn an_exited_process_is_neither_signalled_nor_an_error() {
        let mut c = Command::new("/usr/bin/true").spawn().unwrap();
        let t = Target::of(c.id() as i32);
        c.wait().unwrap();
        // Reaped before it could be named, or named and now gone: either way
        // nothing is sent.
        if let Some(t) = t {
            assert_eq!(term(&[t], &[], Duration::ZERO), Outcome::default());
        }
    }

    #[test]
    fn headroom_jobs_this_app_and_the_system_are_refused() {
        let mut c = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let job = Target::of(c.id() as i32).unwrap();
        let me = Target::of(std::process::id() as i32).unwrap();
        let out = term(&[job, me], &[job.pid], Duration::ZERO);
        assert!(out.signalled.is_empty());
        assert_eq!(out.refused.len(), 2);
        assert!(out.refused[0].contains("Headroom job"), "{:?}", out.refused);
        assert!(out.refused[1].contains("this menu bar app"));
        assert!(c.try_wait().unwrap().is_none(), "the job still runs");
        c.kill().unwrap();
        c.wait().unwrap();
        // launchd can be named but never signalled.
        if let Some(launchd) = Target::of(1) {
            let out = term(&[launchd], &[], Duration::ZERO);
            assert!(out.signalled.is_empty());
        }
    }

    #[test]
    fn another_users_process_is_refused_with_a_reason() {
        // A root daemon that is always there on macOS, readable or not.
        let Some(pid) = sample::all_pids().into_iter().find(|&p| {
            p > 1
                && sample::exe_path(p).is_some_and(|x| x.ends_with("/syslogd"))
                && Target::of(p).is_some()
        }) else {
            return; // its start time isn't readable here: nothing to name it by
        };
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let out = term(&[Target::of(pid).unwrap()], &[], Duration::ZERO);
        assert!(out.signalled.is_empty());
        assert_eq!(out.refused.len(), 1);
        assert!(out.refused[0].contains("another user"), "{:?}", out.refused);
    }
}
