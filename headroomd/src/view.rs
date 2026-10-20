//! The popover's state: the JSON `ui/index.html` renders.
//!
//! This is the contract agreed in the prototype. The page only formats what
//! it is given; every number and every reason is decided here or in core.
//! Ids go out as strings because the page reads them back from DOM data
//! attributes, which are strings.

use std::collections::{BTreeMap, HashSet};

use headroom_core::config::Config;
use headroom_core::model::{Bytes, Job, JobState, Outcome, Pressure};
use headroom_core::profile::Profiles;
use headroom_core::protocol::Listing;
use serde_json::{Value, json};

/// One unmanaged app's memory, for the "Other apps" page.
#[derive(Clone, Debug, PartialEq)]
pub struct AppUsage {
    pub name: String,
    pub bytes: Bytes,
    pub procs: u32,
    /// The app bundle, or the executable for a bare command like `node`.
    pub path: String,
    /// Its largest processes, with what they run and who started them.
    pub top: Vec<ProcInfo>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProcInfo {
    pub pid: i32,
    pub bytes: Bytes,
    /// The command line, or the executable when it can't be read.
    pub cmd: String,
    /// Working directory, when it says something (not `/`).
    pub cwd: Option<String>,
    /// Parent, grandparent, … up to the first app or launchd.
    pub started_by: Vec<String>,
}

/// Start-at-login as the page shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Login {
    Off,
    On,
    /// Installed with `brew services`; changed there, not here.
    Brew,
}

pub struct Extras<'a> {
    pub cfg: &'a Config,
    pub profiles: &'a Profiles,
    pub others: &'a [AppUsage],
    /// Per agent: installed | not_installed | stale | absent.
    pub hooks: &'a [(&'static str, &'static str)],
    pub classify: Option<Value>,
    pub dismissed: &'a HashSet<String>,
    /// Unix seconds now, and when this app started (older guard actions
    /// are history, not news).
    pub wall: u64,
    pub since: u64,
    pub login: Login,
    /// The queue runs in another headroomd; quitting this one leaves it.
    pub attached: bool,
}

fn secs(ms: u64) -> u64 {
    ms / 1000
}

fn job_json(j: &Job, now: u64, cfg: &Config) -> Value {
    let state = match j.state {
        JobState::Queued => "queued",
        JobState::Running => "running",
        JobState::Frozen => "frozen",
    };
    json!({
        "id": j.id.to_string(),
        "label": j.spec.label,
        "cmd": j.spec.cmd,
        "full_cmd": j.spec.full_cmd,
        "cwd": j.spec.cwd,
        "session": j.spec.session,
        "key": j.spec.key,
        "state": state,
        "pinned": j.pinned,
        "low": j.spec.low,
        "estimate": j.estimate,
        "samples": j.samples,
        "footprint": j.footprint,
        "peak": j.peak,
        "runaway_at": j.runaway_at(cfg.runaway_factor, cfg.runaway_cap),
        "elapsed_s": j.started_at.map_or(0, |t| secs(now.saturating_sub(t))),
        "frozen_s": j.frozen_at.map_or(0, |t| secs(now.saturating_sub(t))),
        "waited_s": secs(now.saturating_sub(j.enqueued_at)),
        "cpu_pct": j.cpu_pct,
        "procs": j.procs,
        "retry": j.retry,
        "tuned": j.tuned,
        "skipped": j.skipped,
        "strict_in_s": j.skipped_since.map(|t| {
            cfg.backfill_wait_s.saturating_sub(secs(now.saturating_sub(t)))
        }),
    })
}

fn outcome_guard(o: &Outcome) -> Option<&'static str> {
    match o {
        Outcome::Exited => None,
        Outcome::Requeued => Some("requeued"),
        Outcome::GaveUp => Some("gave_up"),
        Outcome::ManualKill => Some("manual_kill"),
        Outcome::Cancelled => Some("cancelled"),
        Outcome::Disconnected => Some("disconnected"),
    }
}

/// Guard actions worth a banner: jobs frozen by the guard now, and kills
/// since the app started that the user hasn't dismissed.
fn banners(l: &Listing, x: &Extras) -> Vec<Value> {
    let mut out = Vec::new();
    for j in &l.running {
        if j.state == JobState::Frozen && j.guard_note.is_some() {
            let id = format!("f{}", j.id);
            if !x.dismissed.contains(&id) {
                out.push(json!({
                    "id": id, "kind": "frozen", "job_id": j.id.to_string(),
                    "label": j.spec.label, "cmd": j.spec.cmd,
                    "why": format!("{}. Frozen jobs stop growing but keep their memory; kill frees it.",
                        j.guard_note.as_deref().unwrap_or("memory critical")),
                }));
            }
        }
    }
    for h in &l.history {
        let kind = match h.outcome {
            Outcome::Requeued => "requeued",
            Outcome::GaveUp => "gave_up",
            _ => continue,
        };
        let id = format!("h{}-{}", h.id, h.ended_at);
        if h.ended_at < x.since || x.dismissed.contains(&id) {
            continue;
        }
        let why = match h.outcome {
            Outcome::Requeued => format!(
                "{}. Requeued at the front (retry 1/1).",
                h.why.as_deref().unwrap_or("")
            ),
            _ => format!(
                "{}. The agent was told: exit 137.",
                h.why.as_deref().unwrap_or("killed twice")
            ),
        };
        out.push(json!({
            "id": id, "kind": kind, "job_id": h.id.to_string(),
            "label": h.label, "cmd": h.cmd, "why": why,
        }));
    }
    out
}

pub fn state(l: &Listing, x: &Extras) -> Value {
    let s = &l.snapshot;
    let cfg = x.cfg;
    let mut keys: Vec<&str> = l
        .running
        .iter()
        .chain(&l.queued)
        .map(|j| j.spec.key.as_str())
        .collect();
    keys.extend(l.history.iter().map(|h| h.key.as_str()));
    let profiles: BTreeMap<&str, Value> = keys
        .into_iter()
        .filter_map(|k| {
            let p = x.profiles.get(k)?;
            let peaks: Vec<Bytes> = p
                .runs
                .iter()
                .filter(|r| !r.flagged)
                .map(|r| r.peak)
                .collect();
            let est = x.profiles.estimate(k, "", cfg.default_estimate);
            Some((
                k,
                json!({
                    "peaks": peaks,
                    "p90": est.bytes,
                    "median_s": p.median_ms().map_or(0, secs),
                    "samples": p.samples(),
                }),
            ))
        })
        .collect();
    let history: Vec<Value> = l
        .history
        .iter()
        .map(|h| {
            json!({
                "id": h.id.to_string(),
                "label": h.label,
                "cmd": h.cmd,
                "key": h.key,
                "duration_s": secs(h.duration_ms),
                "peak": h.peak,
                "estimate": h.estimate,
                "exit": h.exit,
                "ended_ago_s": x.wall.saturating_sub(h.ended_at),
                "tuned": h.tuned,
                "ahead_of": h.ahead_of,
                "guard": outcome_guard(&h.outcome).map(|a| json!({ "action": a, "why": h.why })),
            })
        })
        .collect();
    let hooks: serde_json::Map<String, Value> = x
        .hooks
        .iter()
        .map(|(a, st)| (a.to_string(), Value::String(st.to_string())))
        .collect();
    json!({
        "system": {
            "pressure": s.pressure,
            "mem_total": s.mem_total,
            "mem_used": s.mem_used,
            "mem_available": s.mem_available,
            "swap_used": s.swap_used,
            "cpu_pct": s.cpu_pct,
        },
        "held": l.held,
        "hooks": hooks,
        "queue_reason": l.blocked,
        "running": l.running.iter().map(|j| job_json(j, l.now, cfg)).collect::<Vec<_>>(),
        "queued": l.queued.iter().map(|j| job_json(j, l.now, cfg)).collect::<Vec<_>>(),
        "banners": banners(l, x),
        "history": history,
        "profiles": profiles,
        "others": x.others.iter().map(|o| json!({
            "name": o.name,
            "bytes": o.bytes,
            "procs": o.procs,
            "path": o.path,
            "top": o.top.iter().map(|p| json!({
                "pid": p.pid,
                "bytes": p.bytes,
                "cmd": p.cmd,
                "cwd": p.cwd,
                "started_by": p.started_by,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "settings": {
            "headroom": cfg.headroom,
            "kill_mode": cfg.kill_mode,
            "wait_cap_s": cfg.wait_cap_s,
            "default_estimate": cfg.default_estimate,
            "start_at_login": x.login != Login::Off,
            "tune": cfg.tune,
            "backfill": cfg.backfill,
            "backfill_skips": cfg.backfill_skips,
            "backfill_wait_s": cfg.backfill_wait_s,
            "login_managed_by_brew": x.login == Login::Brew,
            "patterns": cfg.patterns,
            "excluded": cfg.excluded,
        },
        "classify": x.classify,
        "attached": x.attached,
    })
}

/// The menu bar item: glyph fill, glyph colour (`None` = template), title, tooltip.
pub fn tray(l: &Listing) -> (u32, Option<[u8; 3]>, String, String) {
    let s = &l.snapshot;
    let segs = crate::shell::glyph_segments(s.mem_used, s.mem_total);
    let ink = match s.pressure {
        Pressure::Normal => None,
        Pressure::Warn => Some([245, 158, 11]),
        Pressure::Critical => Some([239, 68, 68]),
    };
    let (n, q) = (l.running.len(), l.queued.len());
    let mut title = if n + q > 0 {
        format!("{n}·{q}")
    } else {
        String::new()
    };
    if l.running.iter().any(|j| j.state == JobState::Frozen) {
        title += " ❄︎";
    }
    if l.held {
        title += " ⏸︎";
    }
    let gb = |b: Bytes| b as f64 / (1u64 << 30) as f64;
    let tip = format!(
        "{} · {:.1}/{:.0} GB · swap {:.1} GB · {n} running · {q} queued",
        match s.pressure {
            Pressure::Normal => "Normal",
            Pressure::Warn => "Warn",
            Pressure::Critical => "Critical",
        },
        gb(s.mem_used),
        gb(s.mem_total),
        gb(s.swap_used)
    );
    (segs, ink, title.trim().to_string(), tip)
}

#[cfg(test)]
mod tests {
    use super::*;
    use headroom_core::model::{FrozenBy, GIB, HistoryEntry, JobSpec, Snapshot};

    fn listing() -> Listing {
        let spec = JobSpec {
            label: "sc-1".into(),
            cmd: "vitest run".into(),
            key: "/r|vitest run".into(),
            ..JobSpec::default()
        };
        let mut run = Job::queued(7, spec.clone(), 4 * GIB, 3, 1_000);
        run.state = JobState::Frozen;
        run.frozen_by = Some(FrozenBy::Guard);
        run.guard_note = Some("memory critical for 10 s".into());
        run.started_at = Some(2_000);
        run.frozen_at = Some(60_000);
        let q = Job::queued(8, spec, 2 * GIB, 0, 50_000);
        Listing {
            now: 72_000,
            snapshot: Snapshot {
                pressure: Pressure::Critical,
                mem_total: 36 * GIB,
                mem_used: 35 * GIB,
                ..Snapshot::default()
            },
            held: true,
            running: vec![run],
            queued: vec![q],
            blocked: Some(headroom_core::admission::Blocked::Pressure {
                level: Pressure::Critical,
            }),
            history: vec![
                HistoryEntry {
                    id: 5,
                    label: "sc-2".into(),
                    cmd: "pnpm build".into(),
                    key: "/r|pnpm build".into(),
                    duration_ms: 212_000,
                    peak: 7 * GIB,
                    estimate: 2 * GIB,
                    exit: None,
                    ended_at: 1_000,
                    outcome: Outcome::Requeued,
                    why: Some("memory still critical".into()),
                    tuned: None,
                    ahead_of: None,
                },
                HistoryEntry {
                    id: 4,
                    label: "sc-2".into(),
                    cmd: "pnpm build".into(),
                    key: "/r|pnpm build".into(),
                    duration_ms: 1,
                    peak: 0,
                    estimate: 0,
                    exit: Some(0),
                    ended_at: 10,
                    outcome: Outcome::Exited,
                    why: None,
                    tuned: None,
                    ahead_of: None,
                },
            ],
        }
    }

    fn extras<'a>(cfg: &'a Config, p: &'a Profiles, d: &'a HashSet<String>) -> Extras<'a> {
        Extras {
            cfg,
            profiles: p,
            others: &[],
            hooks: &[("claude", "installed"), ("codex", "absent")],
            classify: None,
            dismissed: d,
            wall: 1_100,
            since: 500,
            login: Login::Brew,
            attached: false,
        }
    }

    #[test]
    fn state_matches_the_prototype_contract() {
        let (cfg, p, d) = (Config::default(), Profiles::default(), HashSet::new());
        let v = state(&listing(), &extras(&cfg, &p, &d));
        assert_eq!(v["system"]["pressure"], "critical");
        assert_eq!(
            v["queue_reason"],
            json!({ "kind": "pressure", "level": "critical" })
        );
        let r = &v["running"][0];
        assert_eq!(
            (r["id"].as_str(), r["state"].as_str()),
            (Some("7"), Some("frozen"))
        );
        assert_eq!(
            (r["elapsed_s"].as_u64(), r["frozen_s"].as_u64()),
            (Some(70), Some(12))
        );
        assert_eq!(v["queued"][0]["waited_s"], 22);
        assert_eq!(v["hooks"]["codex"], "absent");
        assert_eq!(v["settings"]["kill_mode"], "freeze_kill");
        assert_eq!(v["settings"]["login_managed_by_brew"], true);
        assert_eq!(v["history"][0]["guard"]["action"], "requeued");
        assert_eq!(v["history"][0]["ended_ago_s"], 100);
        assert!(v["history"][1]["guard"].is_null());
    }

    #[test]
    fn banners_cover_current_freezes_and_recent_kills_until_dismissed() {
        let (cfg, p) = (Config::default(), Profiles::default());
        let d = HashSet::new();
        let v = state(&listing(), &extras(&cfg, &p, &d));
        let kinds: Vec<&str> = v["banners"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, vec!["frozen", "requeued"]);
        assert!(
            v["banners"][0]["why"]
                .as_str()
                .unwrap()
                .starts_with("memory critical for 10 s.")
        );

        let d: HashSet<String> = ["f7".to_string(), "h5-1000".to_string()].into();
        let v = state(&listing(), &extras(&cfg, &p, &d));
        assert!(v["banners"].as_array().unwrap().is_empty());
    }

    #[test]
    fn kills_from_before_the_app_started_are_history_not_news() {
        let (cfg, p, d) = (Config::default(), Profiles::default(), HashSet::new());
        let mut x = extras(&cfg, &p, &d);
        x.since = 2_000;
        let v = state(&listing(), &x);
        assert_eq!(
            v["banners"].as_array().unwrap().len(),
            1,
            "only the live freeze"
        );
    }

    #[test]
    fn tray_shows_counts_freeze_hold_and_pressure_colour() {
        let (segs, ink, title, tip) = tray(&listing());
        assert_eq!(segs, 4);
        assert_eq!(ink, Some([239, 68, 68]));
        assert_eq!(title, "1·1 ❄︎ ⏸︎");
        assert!(tip.starts_with("Critical · 35.0/36 GB"));
        let idle = Listing {
            running: vec![],
            queued: vec![],
            held: false,
            ..listing()
        };
        let (_, _, title, _) = tray(&Listing {
            snapshot: Snapshot::default(),
            ..idle
        });
        assert_eq!(title, "");
    }
}
