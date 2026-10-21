//! `headroom run`: wait for a slot, run the command, report how it went.
//!
//! The wrapper holds its socket connection for the job's whole life; that
//! connection is the lease. It never blocks the work it was asked to do:
//! with no daemon, a daemon that doesn't answer, or one that goes away
//! mid-wait, the command runs directly. Headroom is a traffic light, not a gate
//! that can jam shut.

use std::io::{BufReader, IsTerminal};
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use headroom_core::classify::{describe_in, profile_keys};
use headroom_core::model::JobSpec;
use headroom_core::protocol::{ClientMsg, DaemonMsg, read_msg, write_msg};
use headroom_core::shell;
use headroom_core::store::Store;
use headroom_core::tweaks::{self, Knob, Tunable};

use crate::context::{git_context, git_hook_installed, tunable};

/// Set in a job's environment. A `headroom run` inside a job runs directly:
/// queueing it would wait behind its own parent.
pub const JOB_ENV: &str = "HEADROOM_JOB";
/// Exit code when the wait cap runs out (EX_TEMPFAIL: try again later).
pub const EXIT_STILL_QUEUED: i32 = 75;
pub const EXIT_CANCELLED: i32 = 130;
pub const EXIT_GAVE_UP: i32 = 137;

#[derive(Clone, Debug, Default)]
pub struct RunOpts {
    pub sh: Option<String>,
    pub argv: Vec<String>,
    pub background: bool,
    pub session: Option<String>,
    pub agent: Option<String>,
    pub wait_cap: Option<u64>,
    pub low: bool,
    pub label: Option<String>,
}

static SIGNALLED: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(sig: libc::c_int) {
    SIGNALLED.store(sig, Ordering::Relaxed);
}

fn note(msg: &str) {
    eprintln!("headroom: {} {msg}", clock());
}

/// The local time of day as HH:MM:SS.
fn clock() -> String {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    if unsafe { libc::localtime_r(&now, &mut tm) }.is_null() {
        return "--:--:--".to_string();
    }
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// How long a queued job keeps quiet when only the numbers in its status
/// changed. The daemon sends a new status whenever free memory moves, which
/// can be every second.
const RESAY_AFTER: Duration = Duration::from_secs(30);

/// Which queue updates get a line on stderr: a move in the queue at once,
/// anything else at most once per [`RESAY_AFTER`].
#[derive(Default)]
struct Chatter {
    said: String,
    pos: Option<usize>,
    at: Option<Instant>,
}

impl Chatter {
    fn say(&mut self, pos: usize, line: String, now: Instant) -> Option<String> {
        let moved = self.pos != Some(pos);
        let due = self.at.is_none_or(|t| now.duration_since(t) >= RESAY_AFTER);
        if line == self.said || !(moved || due) {
            return None;
        }
        (self.said, self.pos, self.at) = (line.clone(), Some(pos), Some(now));
        Some(line)
    }

    fn said_anything(&self) -> bool {
        self.at.is_some()
    }
}

fn code_of(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

impl RunOpts {
    fn line(&self) -> String {
        self.sh.clone().unwrap_or_else(|| shell::join(&self.argv))
    }

    fn command(&self, low: bool, knob: Option<&Knob>) -> Command {
        let (prog, mut args): (String, Vec<String>) = match &self.sh {
            Some(line) => {
                let shell = std::env::var("SHELL")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("/bin/zsh".into());
                let line = knob.map_or_else(|| line.clone(), |k| tweaks::apply_line(line, k));
                (shell, vec!["-c".into(), line])
            }
            None => (self.argv[0].clone(), self.argv[1..].to_vec()),
        };
        if let (None, Some(Knob::Args(extra))) = (&self.sh, knob) {
            args.extend(extra.iter().cloned());
        }
        let mut c = if low {
            // Background QoS clamp: inherited by every child of the job.
            let mut c = Command::new("/usr/sbin/taskpolicy");
            c.args(["-c", "background"]).arg(prog).args(args);
            c
        } else {
            let mut c = Command::new(prog);
            c.args(args);
            c
        };
        if let Some(Knob::Env(var, value)) = knob {
            c.env(var, value);
        }
        c
    }
}

/// Run without the queue, in our own process group, as if Headroom weren't there.
fn direct(o: &RunOpts) -> i32 {
    match o.command(false, None).status() {
        Ok(s) => code_of(s),
        Err(e) => {
            note(&format!("can't start: {e}"));
            127
        }
    }
}

pub fn run(o: RunOpts) -> i32 {
    if o.sh.as_deref().is_none_or(|s| s.trim().is_empty()) && o.argv.is_empty() {
        eprintln!(
            "usage: headroom run [--background] [--wait-cap SECS] [--low] (--sh 'LINE' | -- CMD ARGS…)"
        );
        return 2;
    }
    if std::env::var_os(JOB_ENV).is_some() {
        return direct(&o);
    }
    let Ok(store) = Store::open() else {
        return direct(&o);
    };
    let Ok(stream) = UnixStream::connect(store.socket()) else {
        note("scheduler isn't running; running directly");
        return direct(&o);
    };
    let cfg = store.load_config();
    let line = o.line();
    let cwd = std::env::current_dir().unwrap_or_default();
    let git = git_context(&cwd);
    let (cmd, program) = describe_in(&line, &cfg, &|d, h| git_hook_installed(&cwd, d, h));
    let (key, fallback_key) = profile_keys(&git.repo, &git.subdir, &cmd, &program);
    let tune = tunable(&line, &cwd);
    let spec = JobSpec {
        full_cmd: line,
        cmd,
        key,
        fallback_key,
        label: o.label.clone().unwrap_or(git.label),
        cwd: cwd.display().to_string(),
        session: o.session.clone(),
        agent: o.agent.clone(),
        low: o.low,
        background: o.background,
        tune,
        pid: Some(std::process::id() as i32),
    };
    let wait_cap = (!o.background).then(|| o.wait_cap.unwrap_or(cfg.wait_cap_s));
    match Session::open(stream, spec) {
        Some(s) => s.drive(&o, wait_cap, tune),
        None => {
            note("scheduler didn't answer; running directly");
            direct(&o)
        }
    }
}

struct Session {
    writer: UnixStream,
    rx: Receiver<Option<DaemonMsg>>,
    /// A message read ahead while checking that the daemon answers.
    peeked: Option<Option<DaemonMsg>>,
}

impl Session {
    /// Enqueue and wait briefly for the first answer; `None` means fail open.
    fn open(stream: UnixStream, spec: JobSpec) -> Option<Self> {
        let mut writer = stream.try_clone().ok()?;
        write_msg(&mut writer, &ClientMsg::Enqueue(spec)).ok()?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(stream);
            loop {
                let msg = read_msg::<_, DaemonMsg>(&mut r).ok().flatten();
                let end = msg.is_none();
                if tx.send(msg).is_err() || end {
                    return;
                }
            }
        });
        let first = rx.recv_timeout(Duration::from_millis(500)).ok().flatten()?;
        Some(Session {
            writer,
            rx,
            peeked: Some(Some(first)),
        })
    }

    fn recv(&mut self, d: Duration) -> Result<Option<DaemonMsg>, RecvTimeoutError> {
        match self.peeked.take() {
            Some(m) => Ok(m),
            None => self.rx.recv_timeout(d),
        }
    }

    fn send(&mut self, m: &ClientMsg) {
        let _ = write_msg(&mut self.writer, m);
    }

    fn drive(mut self, o: &RunOpts, wait_cap: Option<u64>, tune: Option<Tunable>) -> i32 {
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            unsafe {
                libc::signal(
                    sig,
                    on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
                )
            };
        }
        let mut daemon_gone = false;
        let mut queued_since = Instant::now();
        let mut chatter = Chatter::default();
        let mut pos = (0usize, None::<String>);
        let mut waited_total = Duration::ZERO;

        loop {
            // ── waiting for a slot ──
            let grant = loop {
                if let Some(cap) = wait_cap {
                    let waited = queued_since.elapsed();
                    if waited >= Duration::from_secs(cap) {
                        let why = pos
                            .1
                            .as_deref()
                            .map(|r| format!(" ({r})"))
                            .unwrap_or_default();
                        note(&format!(
                            "still queued at #{} after {}s{why}. Nothing ran. Try again later, or run it in the background.",
                            pos.0,
                            waited.as_secs()
                        ));
                        return EXIT_STILL_QUEUED;
                    }
                }
                let sig = SIGNALLED.swap(0, Ordering::Relaxed);
                if sig != 0 {
                    return 128 + sig;
                }
                match self.recv(Duration::from_millis(200)) {
                    Ok(Some(DaemonMsg::Queued {
                        pos: p,
                        of,
                        reason,
                        estimate,
                        ..
                    })) => {
                        pos = (p, reason.clone());
                        let gb = estimate as f64 / (1u64 << 30) as f64;
                        let said = format!(
                            "queued #{p} of {of}{} (needs ~{gb:.1} GB)",
                            reason.map(|r| format!(" · {r}")).unwrap_or_default()
                        );
                        if let Some(line) = chatter.say(p, said, Instant::now()) {
                            note(&line);
                        }
                    }
                    Ok(Some(DaemonMsg::Granted {
                        id, low, workers, ..
                    })) => break (id, low, workers),
                    Ok(Some(DaemonMsg::Cancelled)) => {
                        note("cancelled from the queue. Nothing ran.");
                        return EXIT_CANCELLED;
                    }
                    Ok(Some(DaemonMsg::Killed { why, .. })) => {
                        note(&format!("killed before it started: {why}"));
                        return EXIT_GAVE_UP;
                    }
                    Ok(Some(DaemonMsg::Error { msg })) => {
                        note(&format!("{msg}; running directly"));
                        return direct(o);
                    }
                    // Part of a job already running: a turbo task's shim.
                    Ok(Some(DaemonMsg::Inside { .. })) => return direct(o),
                    Ok(Some(_)) => {}
                    Ok(None) | Err(RecvTimeoutError::Disconnected) => {
                        note("scheduler went away; running directly");
                        return direct(o);
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                }
            };
            waited_total += queued_since.elapsed();
            if waited_total >= Duration::from_secs(1) && chatter.said_anything() {
                note(&format!(
                    "starting after {}s in the queue",
                    waited_total.as_secs()
                ));
            }

            // ── running ──
            let (id, low, workers) = grant;
            let knob = workers.zip(tune).map(|(n, t)| {
                note(&format!(
                    "starting with {n} {} workers instead of {} so it fits in memory",
                    t.tool.name(),
                    t.default_workers
                ));
                t.knob(n)
            });
            let mut cmd = o.command(low, knob.as_ref());
            cmd.process_group(0).env(JOB_ENV, id.to_string());
            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    note(&format!("can't start: {e}"));
                    self.send(&ClientMsg::Exited {
                        code: Some(127),
                        signal: None,
                    });
                    return 127;
                }
            };
            let pgid = child.id() as i32;
            self.send(&ClientMsg::Started { pid: pgid, pgid });
            let tty = Foreground::take(pgid);
            let (status, killed) = self.supervise(&mut child, pgid, &mut daemon_gone);
            drop(tty);
            self.send(&ClientMsg::Exited {
                code: status.code(),
                signal: status.signal(),
            });

            match killed {
                Some((why, true)) => {
                    note(&format!(
                        "killed to free memory ({why}); requeued, will run again"
                    ));
                    queued_since = Instant::now();
                    chatter = Chatter::default();
                    continue;
                }
                Some((why, false)) if why != "killed by you" => {
                    note(&format!("gave up: killed twice to free memory ({why})"));
                    return EXIT_GAVE_UP;
                }
                _ => return code_of(status),
            }
        }
    }

    /// Wait for the child, relaying the daemon's notices and our signals.
    fn supervise(
        &mut self,
        child: &mut Child,
        pgid: i32,
        daemon_gone: &mut bool,
    ) -> (ExitStatus, Option<(String, bool)>) {
        let mut killed = None;
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                return (status, killed);
            }
            let sig = SIGNALLED.swap(0, Ordering::Relaxed);
            if sig != 0 {
                unsafe { libc::killpg(pgid, sig) };
            }
            if *daemon_gone {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            match self.recv(Duration::from_millis(100)) {
                Ok(Some(DaemonMsg::Frozen { why, .. })) => note(&format!(
                    "paused{}",
                    why.map(|w| format!(": {w}")).unwrap_or_default()
                )),
                Ok(Some(DaemonMsg::Thawed)) => note("resumed"),
                Ok(Some(DaemonMsg::Killed { why, requeued })) => {
                    if why == "killed by you" {
                        note("killed from the queue");
                    }
                    killed = Some((why, requeued));
                }
                Ok(Some(_)) | Err(RecvTimeoutError::Timeout) => {}
                // The job keeps running without a scheduler; nothing more to hear.
                Ok(None) | Err(RecvTimeoutError::Disconnected) => *daemon_gone = true,
            }
        }
    }
}

/// While a job runs in its own process group, hand it the terminal so
/// Ctrl-C and keyboard input reach it directly. Restored on drop.
struct Foreground {
    ours: libc::pid_t,
}

impl Foreground {
    fn take(pgid: i32) -> Option<Self> {
        if !std::io::stdin().is_terminal() {
            return None;
        }
        unsafe {
            let ours = libc::getpgrp();
            if libc::tcgetpgrp(0) != ours {
                return None; // we're in the background ourselves
            }
            // Taking the terminal back later happens from the background.
            libc::signal(libc::SIGTTOU, libc::SIG_IGN);
            (libc::tcsetpgrp(0, pgid) == 0).then_some(Foreground { ours })
        }
    }
}

impl Drop for Foreground {
    fn drop(&mut self) {
        unsafe { libc::tcsetpgrp(0, self.ours) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(pos: usize, free: &str) -> String {
        format!("queued #{pos} of 2 · next needs 1.5 GB, {free} GB free")
    }

    #[test]
    fn a_status_that_only_changes_its_numbers_is_said_twice_a_minute() {
        let t = Instant::now();
        let mut c = Chatter::default();
        assert!(!c.said_anything());
        assert!(c.say(2, line(2, "0.5"), t).is_some());
        assert!(c.said_anything());
        for s in 1..30 {
            let free = format!("0.{}", s % 9);
            assert_eq!(c.say(2, line(2, &free), t + Duration::from_secs(s)), None);
        }
        assert!(c.say(2, line(2, "0.4"), t + RESAY_AFTER).is_some());
    }

    #[test]
    fn a_move_in_the_queue_is_said_at_once() {
        let t = Instant::now();
        let mut c = Chatter::default();
        c.say(2, line(2, "0.5"), t);
        let moved = c.say(1, line(1, "0.5"), t + Duration::from_millis(10));
        assert_eq!(moved, Some(line(1, "0.5")));
    }

    #[test]
    fn the_same_status_is_never_repeated() {
        let t = Instant::now();
        let mut c = Chatter::default();
        c.say(1, line(1, "0.5"), t);
        assert_eq!(c.say(1, line(1, "0.5"), t + Duration::from_secs(600)), None);
    }

    #[test]
    fn the_clock_reads_as_hours_minutes_and_seconds() {
        let c = clock();
        let parts: Vec<u32> = c.split(':').map(|p| p.parse().unwrap()).collect();
        assert_eq!(c.len(), 8, "{c}");
        assert!(
            parts.len() == 3 && parts[0] < 24 && parts[1] < 60 && parts[2] < 61,
            "{c}"
        );
    }
}
