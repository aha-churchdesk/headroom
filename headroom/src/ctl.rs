//! `headroom ls` and the queue controls, over the daemon's socket.

use headroom_core::classify::{Class, classify_in};

use crate::context::git_hook_installed;
use headroom_core::model::{Bytes, GIB, Job, JobState, Millis};
use headroom_core::protocol::{ClientMsg, Control, DaemonMsg, Listing};
use headroom_core::store::Store;

fn ask(msg: &ClientMsg) -> Result<DaemonMsg, String> {
    let store = Store::open().map_err(|e| e.to_string())?;
    headroom_core::protocol::request(&store.socket(), msg)
}

fn gb(b: Bytes) -> String {
    format!("{:.1}", b as f64 / GIB as f64)
}

fn dur(ms: Millis) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}", s / 60, s % 60)
    } else {
        format!("{}h{:02}", s / 3600, s % 3600 / 60)
    }
}

pub fn render(l: &Listing) -> String {
    let m = &l.snapshot;
    let mut out = format!(
        "memory   {} / {} GB used · {} GB available · swap {} GB · pressure {}\n",
        gb(m.mem_used),
        gb(m.mem_total),
        gb(m.mem_available),
        gb(m.swap_used),
        format!("{:?}", m.pressure).to_uppercase()
    );
    if l.held {
        out += "held     new jobs wait until `headroom hold off`\n";
    }
    let marks = |j: &Job| {
        let mut v = vec![];
        if j.pinned {
            v.push("pinned");
        }
        if j.spec.low {
            v.push("low");
        }
        if j.state == JobState::Frozen {
            v.push("frozen");
        }
        if v.is_empty() {
            String::new()
        } else {
            format!("  [{}]", v.join(", "))
        }
    };
    out += &format!("running  {}\n", l.running.len());
    for j in &l.running {
        let elapsed = j.started_at.map_or(0, |t| l.now.saturating_sub(t));
        out += &format!(
            "  {:>4}  {:<16} {:<22} {:>5} / est {} GB  {:>6}  CPU {:>3.0}%{}\n",
            j.id,
            j.spec.label,
            j.spec.cmd,
            gb(j.footprint),
            gb(j.estimate),
            dur(elapsed),
            j.cpu_pct,
            marks(j)
        );
    }
    let why = l
        .blocked
        .as_ref()
        .map(|b| format!(" · {}", b.describe()))
        .unwrap_or_default();
    out += &format!(
        "queued   {}{}\n",
        l.queued.len(),
        if l.queued.is_empty() { "" } else { &why }
    );
    for (i, j) in l.queued.iter().enumerate() {
        let new = if j.samples == 0 { " (new)" } else { "" };
        out += &format!(
            "  {:>4}  {}. {:<13} {:<22} est {} GB{new}  waited {}{}\n",
            j.id,
            i + 1,
            j.spec.label,
            j.spec.cmd,
            gb(j.estimate),
            dur(l.now.saturating_sub(j.enqueued_at)),
            marks(j)
        );
    }
    out
}

pub fn ls() -> i32 {
    match ask(&ClientMsg::List) {
        Ok(DaemonMsg::Jobs(l)) => {
            print!("{}", render(&l));
            0
        }
        Ok(other) => {
            eprintln!("headroom: unexpected answer {other:?}");
            1
        }
        Err(e) => {
            eprintln!("headroom: {e}");
            1
        }
    }
}

pub fn control(ctl: Control) -> i32 {
    match ask(&ClientMsg::Control(ctl)) {
        Ok(DaemonMsg::Ok) => 0,
        Ok(DaemonMsg::Error { msg }) => {
            eprintln!("headroom: {msg}");
            1
        }
        Ok(other) => {
            eprintln!("headroom: unexpected answer {other:?}");
            1
        }
        Err(e) => {
            eprintln!("headroom: {e}");
            1
        }
    }
}

/// `headroom classify LINE`: what the hook would do with a command.
pub fn explain(line: &str) -> i32 {
    let cfg = Store::open().map(|s| s.load_config()).unwrap_or_default();
    let cwd = std::env::current_dir().unwrap_or_default();
    match classify_in(line, &cfg, &|d, h| git_hook_installed(&cwd, d, h)) {
        Class::Heavy { cmd, .. } => println!("heavy: queued, command type `{cmd}`"),
        Class::Excluded { why } => println!("long-lived ({why}): never queued"),
        Class::Wrapped => println!("already wrapped: left alone"),
        Class::Light => println!("light: runs immediately"),
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use headroom_core::model::{JobSpec, Pressure, Snapshot};

    #[test]
    fn durations_read_naturally() {
        assert_eq!(dur(0), "0s");
        assert_eq!(dur(59_999), "59s");
        assert_eq!(dur(60_000), "1m00");
        assert_eq!(dur(134_000), "2m14");
        assert_eq!(dur(3_600_000), "1h00");
    }

    #[test]
    fn listing_shows_both_lists_and_the_reason() {
        let spec = JobSpec {
            label: "sc-49123".into(),
            cmd: "vitest run".into(),
            ..JobSpec::default()
        };
        let mut r = Job::queued(1, spec.clone(), 4 * GIB, 5, 0);
        r.state = JobState::Running;
        r.started_at = Some(1_000);
        r.pinned = true;
        let q = Job::queued(2, spec, 2 * GIB, 0, 10_000);
        let l = Listing {
            now: 135_000,
            snapshot: Snapshot {
                pressure: Pressure::Warn,
                mem_total: 24 * GIB,
                ..Snapshot::default()
            },
            held: true,
            running: vec![r],
            queued: vec![q],
            blocked: Some(headroom_core::admission::Blocked::Held),
            history: vec![],
        };
        let s = render(&l);
        assert!(s.contains("pressure WARN"), "{s}");
        assert!(s.contains("held "), "{s}");
        assert!(s.contains("2m14") && s.contains("[pinned]"), "{s}");
        assert!(s.contains("queued   1 · held by you"), "{s}");
        assert!(s.contains("(new)  waited 2m05"), "{s}");
    }
}
