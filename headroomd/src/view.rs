//! The popover's state: the JSON `ui/index.html` renders.
//!
//! This is the contract agreed in the prototype. The page only formats what
//! it is given; every number and every reason is decided here or in core.
//! Ids go out as strings because the page reads them back from DOM data
//! attributes, which are strings.

use std::collections::{BTreeMap, HashSet};

use headroom_core::config::Config;
use headroom_core::model::{Bytes, HistoryEntry, Job, JobState, Outcome, Pressure};
use headroom_core::profile::Profiles;
use headroom_core::protocol::Listing;
use headroom_core::scheduler::{HISTORY, HISTORY_DAY_S, HISTORY_MAX};
use headroom_core::tally::{Day, TALLY_DAYS, Tally};
use headroom_core::trend::{TREND_S, Trend};
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
    pub tally: &'a Tally,
    pub trend: &'a Trend,
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

/// Nearest-rank percentile of `sorted`, ascending; 0 when empty.
fn percentile(sorted: &[u64], pct: usize) -> u64 {
    match sorted.len() {
        0 => 0,
        n => sorted[(n * pct).div_ceil(100).max(1) - 1],
    }
}

/// What finished in the day before `wall`, from the history (newest
/// first). The history keeps at most `HISTORY_MAX` runs; when all of them
/// are inside the day the counts are a floor, and `more` says so.
fn last_day(history: &[HistoryEntry], wall: u64) -> Value {
    let since = wall.saturating_sub(HISTORY_DAY_S);
    let recent: Vec<&HistoryEntry> = history.iter().filter(|h| h.ended_at >= since).collect();
    let count = |f: &dyn Fn(&HistoryEntry) -> bool| recent.iter().filter(|h| f(h)).count();
    let mut waits: Vec<u64> = recent.iter().map(|h| h.waited_ms).collect();
    waits.sort_unstable();
    json!({
        "runs": count(&|h| h.outcome != Outcome::Cancelled),
        "failed": count(&|h| h.outcome == Outcome::Exited && h.exit != Some(0)),
        "killed": count(&|h| matches!(h.outcome, Outcome::Requeued | Outcome::GaveUp)),
        "waited_s": secs(recent.iter().map(|h| h.waited_ms).sum()),
        "longest_wait_s": secs(waits.last().copied().unwrap_or(0)),
        "wait_p50_s": secs(percentile(&waits, 50)),
        "wait_p99_s": secs(percentile(&waits, 99)),
        "shrunk": count(&|h| h.tuned.is_some()),
        "went_first": count(&|h| h.ahead_of.is_some()),
        "more": history.len() >= HISTORY_MAX && recent.len() == history.len(),
    })
}

/// The Commands page's windows, in days.
const WINDOWS: [u64; 3] = [7, 30, TALLY_DAYS];
/// Commands listed per window. The page re-sorts these by runs, memory or
/// waiting, so a command that is cheap per run but frequent may be missing
/// from those orders past this many.
const COMMANDS: usize = 50;

fn day_json(d: &Day) -> Value {
    json!({
        "runs": d.runs,
        "failed": d.failed,
        "run_s": secs(d.run_ms),
        "wait_s": secs(d.wait_ms),
        "gb_h": d.mib_s as f64 / 1024.0 / 3600.0,
        "peak": d.peak,
    })
}

/// Per window: the commands that ran in it, most time running first, and
/// the totals over every command.
fn commands(t: &Tally, wall: u64) -> Value {
    WINDOWS
        .iter()
        .map(|&days| {
            let rows = t.window(wall, days);
            let mut total = Day::default();
            for (_, d) in &rows {
                total.add(d);
            }
            json!({
                "days": days,
                "total": day_json(&total),
                "count": rows.len(),
                "rows": rows.iter().take(COMMANDS).map(|(k, d)| {
                    let mut v = day_json(d);
                    v["key"] = json!(k);
                    v
                }).collect::<Vec<_>>(),
            })
        })
        .collect()
}

/// The graphs' windows in seconds, and the points each is drawn from.
const GRAPHS: [(u64, usize); 2] = [(3_600, 120), (TREND_S, 288)];

/// Memory, swap and CPU per window. Each point is
/// `[seconds ago, memory used, swap used, CPU %, pressure 0|1|2]`, oldest
/// first; `step_s` apart where the app was running, further apart where it
/// wasn't.
fn trend(t: &Trend, wall: u64) -> Value {
    GRAPHS
        .iter()
        .map(|&(secs, max)| {
            let (step, pts) = t.window(wall, secs, max);
            json!({
                "window_s": secs,
                "step_s": step,
                "points": pts.iter().map(|p| json!([
                    wall.saturating_sub(p.at),
                    p.mem_used,
                    p.swap_used,
                    p.cpu_pct.round(),
                    p.pressure as u8,
                ])).collect::<Vec<_>>(),
            })
        })
        .collect()
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
    for h in l.history.iter().take(HISTORY) {
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
    keys.extend(l.history.iter().take(HISTORY).map(|h| h.key.as_str()));
    // The Commands page opens these too.
    let listed: Vec<(&str, Day)> = WINDOWS
        .iter()
        .flat_map(|&days| x.tally.window(x.wall, days).into_iter().take(COMMANDS))
        .collect();
    keys.extend(listed.iter().map(|(k, _)| *k));
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
        .take(HISTORY)
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
        "last_day": last_day(&l.history, x.wall),
        "commands": commands(x.tally, x.wall),
        "trend": trend(x.trend, x.wall),
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
    use headroom_core::model::{FrozenBy, GIB, JobSpec, Snapshot, Tuned};

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
                    waited_ms: 0,
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
                    waited_ms: 0,
                },
            ],
        }
    }

    fn extras<'a>(cfg: &'a Config, p: &'a Profiles, d: &'a HashSet<String>) -> Extras<'a> {
        Extras {
            cfg,
            profiles: p,
            tally: Box::leak(Box::default()),
            trend: Box::leak(Box::default()),
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
        assert_eq!(v["last_day"]["runs"], 2);
    }

    fn ended(at: u64, outcome: Outcome, exit: Option<i32>, waited_ms: u64) -> HistoryEntry {
        HistoryEntry {
            id: at,
            label: "r".into(),
            cmd: "c".into(),
            key: "/r|c".into(),
            duration_ms: 1,
            peak: 0,
            estimate: 0,
            exit,
            ended_at: at,
            outcome,
            why: None,
            tuned: None,
            ahead_of: None,
            waited_ms,
        }
    }

    #[test]
    fn commands_lists_each_window_most_time_first_with_totals() {
        let mut t = Tally::default();
        let wall = 100 * 86_400;
        let run = |key: &str, at: u64, ms: u64, exit: i32| HistoryEntry {
            key: key.into(),
            duration_ms: ms,
            peak: 2 << 30,
            ..ended(at, Outcome::Exited, Some(exit), 3_000)
        };
        t.add(&run("/r|lint", wall - 20 * 86_400, 600_000, 0));
        t.add(&run("/r|test", wall - 60, 60_000, 0));
        t.add(&run("/r|test", wall, 60_000, 1));
        let v = commands(&t, wall);
        let days: Vec<u64> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w["days"].as_u64().unwrap())
            .collect();
        assert_eq!(days, [7, 30, TALLY_DAYS]);
        let week = &v[0];
        assert_eq!(week["count"], 1, "lint ran before the week");
        assert_eq!(week["rows"][0]["key"], "/r|test");
        assert_eq!(week["rows"][0]["runs"], 2);
        assert_eq!(week["rows"][0]["failed"], 1);
        assert_eq!(week["rows"][0]["wait_s"], 6);
        // 2 GB for two minutes.
        assert!((week["rows"][0]["gb_h"].as_f64().unwrap() - 4.0 / 60.0).abs() < 1e-9);
        let month = &v[1];
        assert_eq!(month["rows"][0]["key"], "/r|lint", "ten minutes beats two");
        assert_eq!(month["total"]["runs"], 3);
        assert_eq!(month["total"]["run_s"], 720);
        // An empty tally still gives every window, with nothing in it.
        let none = commands(&Tally::default(), wall);
        assert_eq!(none[2]["count"], 0);
        assert_eq!(none[2]["total"]["runs"], 0);
    }

    #[test]
    fn a_command_from_weeks_ago_still_opens_its_profile() {
        let (cfg, d) = (Config::default(), HashSet::new());
        let mut p = Profiles::default();
        let run = headroom_core::profile::Run {
            peak: 3 << 30,
            duration_ms: 60_000,
            exit: Some(0),
            flagged: false,
            at: 1,
            workers: None,
        };
        p.record("/r|old", "/r|old", run);
        let wall = 100 * 86_400;
        let mut t = Tally::default();
        t.add(&HistoryEntry {
            key: "/r|old".into(),
            ..ended(wall - 20 * 86_400, Outcome::Exited, Some(0), 0)
        });
        let mut x = extras(&cfg, &p, &d);
        (x.tally, x.wall) = (&t, wall);
        let v = state(&listing(), &x);
        assert_eq!(v["commands"][1]["rows"][0]["key"], "/r|old");
        assert_eq!(v["profiles"]["/r|old"]["samples"], 1);
        // Not in any window, not sent: the 90-day page stops there too.
        x.wall += 200 * 86_400;
        assert!(state(&listing(), &x)["profiles"].get("/r|old").is_none());
    }

    #[test]
    fn trend_gives_an_hour_and_a_day_of_points_as_seconds_ago() {
        use headroom_core::model::Snapshot;
        let wall = 10 * 86_400;
        let mut t = Trend::default();
        let snap = |gb: u64, pressure| Snapshot {
            mem_used: gb << 30,
            swap_used: 1 << 30,
            cpu_pct: 41.6,
            pressure,
            ..Snapshot::default()
        };
        t.record(&snap(20, Pressure::Normal), wall - 7_200);
        t.record(&snap(30, Pressure::Critical), wall - 60);
        let v = trend(&t, wall);
        assert_eq!(v[0]["window_s"], 3_600);
        assert_eq!(v[0]["step_s"], 30);
        assert_eq!(
            v[0]["points"],
            json!([[60, 30u64 << 30, 1u64 << 30, 42.0, 2]])
        );
        assert_eq!(v[1]["window_s"], TREND_S);
        assert_eq!(v[1]["step_s"], 300);
        assert_eq!(
            v[1]["points"].as_array().unwrap().len(),
            2,
            "two hours ago is in the day"
        );
        assert_eq!(v[1]["points"][0][0], 7_200);
        // Nothing recorded yet: both windows, no points.
        assert_eq!(trend(&Trend::default(), wall)[1]["points"], json!([]));
    }

    #[test]
    fn commands_lists_at_most_fifty_per_window_but_totals_all() {
        let mut t = Tally::default();
        let wall = 100 * 86_400;
        for i in 0..(COMMANDS as u64 + 5) {
            t.add(&HistoryEntry {
                key: format!("/r|c{i}"),
                ..ended(wall, Outcome::Exited, Some(0), 0)
            });
        }
        let v = commands(&t, wall);
        assert_eq!(v[0]["rows"].as_array().unwrap().len(), COMMANDS);
        assert_eq!(v[0]["count"], COMMANDS + 5);
        assert_eq!(v[0]["total"]["runs"], COMMANDS + 5);
    }

    #[test]
    fn last_day_sums_what_finished_in_it() {
        let wall = 100_000;
        let mut shrunk = ended(90_000, Outcome::Exited, Some(0), 90_000);
        shrunk.tuned = Some(Tuned {
            workers: 3,
            default: 9,
        });
        let mut first = ended(80_000, Outcome::Exited, Some(1), 0);
        first.ahead_of = Some("r · e2e".into());
        let h = vec![
            ended(99_990, Outcome::Requeued, None, 30_000),
            shrunk,
            first,
            ended(70_000, Outcome::Cancelled, None, 20_000),
            ended(65_000, Outcome::Exited, None, 0),
            ended(64_000, Outcome::GaveUp, None, 0),
            ended(wall - HISTORY_DAY_S, Outcome::Exited, Some(0), 1_000),
            ended(wall - HISTORY_DAY_S - 1, Outcome::Exited, Some(2), 500_000),
        ];
        assert_eq!(
            last_day(&h, wall),
            json!({
                "runs": 6, "failed": 2, "killed": 2,
                "waited_s": 141, "longest_wait_s": 90,
                "wait_p50_s": 1, "wait_p99_s": 90,
                "shrunk": 1, "went_first": 1, "more": false,
            })
        );
    }

    #[test]
    fn percentiles_take_the_nearest_rank() {
        let hundred: Vec<u64> = (1..=100).collect();
        assert_eq!(
            (percentile(&hundred, 50), percentile(&hundred, 99)),
            (50, 99)
        );
        // One more value: the 99th percentile's rank moves up to it.
        let more: Vec<u64> = (1..=101).collect();
        assert_eq!((percentile(&more, 50), percentile(&more, 99)), (51, 100));
        assert_eq!((percentile(&[7], 50), percentile(&[7], 99)), (7, 7));
        assert_eq!(percentile(&[], 99), 0);
    }

    #[test]
    fn last_day_says_when_the_history_is_too_short_to_cover_it() {
        let quiet = last_day(&[], 5_000);
        assert_eq!(
            (quiet["runs"].as_u64(), quiet["waited_s"].as_u64()),
            (Some(0), Some(0))
        );
        assert_eq!(quiet["more"], false);
        let full: Vec<_> = (0..HISTORY_MAX as u64)
            .map(|i| ended(500_000 - i, Outcome::Exited, Some(0), 0))
            .collect();
        assert_eq!(last_day(&full, 500_000)["more"], true);
        let mut older = full.clone();
        older.last_mut().unwrap().ended_at = 1_000;
        let v = last_day(&older, 500_000);
        assert_eq!(
            (v["more"].as_bool(), v["runs"].as_u64()),
            (Some(false), Some(HISTORY_MAX as u64 - 1))
        );
        // Early in the clock's life the day starts at zero, not before it.
        assert_eq!(last_day(&full[..3], 30)["runs"], 3);
    }

    #[test]
    fn the_history_page_shows_the_last_runs_and_the_summary_counts_the_day() {
        let (cfg, p, d) = (Config::default(), Profiles::default(), HashSet::new());
        let mut l = listing();
        l.history = (0..HISTORY as u64 + 10)
            .map(|i| ended(1_000 - i, Outcome::Exited, Some(0), 0))
            .collect();
        let v = state(&l, &extras(&cfg, &p, &d));
        assert_eq!(v["history"].as_array().unwrap().len(), HISTORY);
        assert_eq!(v["last_day"]["runs"], HISTORY as u64 + 10);
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
