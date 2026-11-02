//! The popover's state: the JSON `ui/index.html` renders.
//!
//! This is the contract agreed in the prototype. The page only formats what
//! it is given; every number and every reason is decided here or in core.
//! Ids go out as strings because the page reads them back from DOM data
//! attributes, which are strings.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use headroom_core::classify::{command_key, is_exact_key};
use headroom_core::config::Config;
use headroom_core::model::{Bytes, HistoryEntry, Job, JobState, Outcome, Pressure};
use headroom_core::profile::{ESTIMATE_SAMPLES, Profile, Profiles, Run};
use headroom_core::protocol::Listing;
use headroom_core::scheduler::{HISTORY, HISTORY_DAY_S, HISTORY_MAX};
use headroom_core::tally::{Day, TALLY_DAYS, Tally};
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
    pub others: &'a [AppUsage],
    /// Per agent: installed | not_installed | stale | absent.
    pub hooks: &'a [(&'static str, &'static str)],
    pub classify: Option<Value>,
    /// The profile the page has open. Only it carries what ran inside its
    /// runs: those lists hold whole command lines, too much to send for
    /// every profile every second.
    pub open_profile: Option<&'a str>,
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

/// A job that spent less than this in the queue started right away; the
/// wrapper doesn't mention the wait either.
const WAITED_MS: u64 = 1000;

/// A run that ended for good, as the Commands page counts runs: a run the
/// guard killed and requeued is not one, its retry is.
fn finished(h: &HistoryEntry) -> bool {
    h.has_run() && h.outcome != Outcome::Requeued
}

/// What finished in the day before `wall`, from the history (newest
/// first). The history keeps at most `HISTORY_MAX` runs; when all of them
/// are inside the day the counts are a floor, and `more` says so.
///
/// Runs and failures count as the Commands page counts them; `requeued`
/// says how many runs were killed and run again on top.
///
/// Most jobs never wait, so wait percentiles over every run would read 0s.
/// They are over the runs that waited, and `waited` says how many did.
fn last_day(history: &[HistoryEntry], wall: u64) -> Value {
    let since = wall.saturating_sub(HISTORY_DAY_S);
    let recent: Vec<&HistoryEntry> = history
        .iter()
        .filter(|h| h.ended_at >= since && h.ended_at <= wall)
        .collect();
    let count = |f: &dyn Fn(&HistoryEntry) -> bool| recent.iter().filter(|h| f(h)).count();
    let mut waits: Vec<u64> = recent
        .iter()
        .filter(|h| finished(h) && h.waited_ms >= WAITED_MS)
        .map(|h| h.waited_ms)
        .collect();
    waits.sort_unstable();
    json!({
        "runs": count(&finished),
        "failed": count(&|h| finished(h) && match h.outcome {
            Outcome::Exited => h.exit != Some(0),
            Outcome::GaveUp => true,
            _ => false,
        }),
        "requeued": count(&|h| h.has_run() && h.outcome == Outcome::Requeued),
        "waited": waits.len(),
        "waited_s": secs(waits.iter().sum()),
        "longest_wait_s": secs(waits.last().copied().unwrap_or(0)),
        "wait_p50_s": secs(percentile(&waits, 50)),
        "wait_p95_s": secs(percentile(&waits, 95)),
        "went_first": count(&|h| finished(h) && h.ahead_of.is_some()),
        "more": history.len() >= HISTORY_MAX && recent.len() == history.len(),
    })
}

/// The Commands page's windows, in days.
const WINDOWS: [u64; 3] = [7, 30, TALLY_DAYS];

/// Where a command's profile is in the state's `profiles`. A prefix keeps
/// it apart from exact and fallback keys, which can read the same.
fn command_profile_key(command: &str) -> String {
    format!("cmd:{command}")
}

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

/// Per window: every command that ran in it, most time running first, and
/// the totals over them, so the rows add up to the totals.
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
                "rows": rows.iter().map(|(k, d)| {
                    let mut v = day_json(d);
                    v["key"] = json!(k);
                    v["profile"] = json!(command_profile_key(k));
                    v
                }).collect::<Vec<_>>(),
            })
        })
        .collect()
}

/// Whether a command line is `own`, the job's normalised command, with its
/// arguments: a `git commit` under the shell that started it.
fn is_own(cmd: &str, own: &str) -> bool {
    let own: Vec<&str> = own.split_whitespace().collect();
    let words: Vec<&str> = cmd.split_whitespace().collect();
    words.len() >= own.len() && words[..own.len()] == own[..]
}

/// The largest process inside a job, unless that is the job's own command,
/// which says nothing new.
fn inner(j: &Job) -> Option<&str> {
    j.now.as_deref().filter(|n| !is_own(n, &j.spec.cmd))
}

/// Words after the program that still name what runs, such as `eslint`
/// in `yarn eslint src/a.ts`.
const PART_WORDS: usize = 2;
/// Characters of an inner command's example line: enough to recognise it.
/// A linter's line can list hundreds of files.
const EXAMPLE_CHARS: usize = 300;

/// `line` cut to `EXAMPLE_CHARS`, marked when cut.
fn example(line: &str) -> String {
    match line.char_indices().nth(EXAMPLE_CHARS) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_string(),
    }
}

/// What an inner command line runs, without its arguments: the program's
/// name and up to `PART_WORDS` plain words after it, stopping at the first
/// flag, path, file or assignment. `eslint.js --fix a.ts b.ts` is
/// `eslint.js`, `yarn eslint ./src` is `yarn eslint`.
fn part_label(cmd: &str) -> String {
    let mut words = cmd.split_whitespace();
    let Some(program) = words.next() else {
        return String::new();
    };
    let plain = |w: &&str| {
        w.len() <= 24
            && !w.starts_with('-')
            && w.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b':')
    };
    let mut label = program.rsplit('/').next().unwrap_or(program).to_string();
    for w in words.take_while(plain).take(PART_WORDS) {
        label.push(' ');
        label.push_str(w);
    }
    label
}

/// What ran inside a command's recent runs, largest first: per inner
/// command (see `part_label`), the runs it was in, its highest peak, the
/// most processes at once and its median time (samples are a second
/// apart). `example` is its largest command line. `own` is the command
/// itself, left out.
///
/// Lines of one label in the same run count once, with their largest
/// numbers: they may have run side by side, so their sums could overstate.
fn profile_parts<'a>(runs: impl IntoIterator<Item = &'a Run>, own: &str) -> Vec<Value> {
    #[derive(Default)]
    struct Seen<'a> {
        runs: u32,
        peak: Bytes,
        example: &'a str,
        procs: u32,
        secs: Vec<u32>,
    }
    let mut by_label: BTreeMap<String, Seen> = BTreeMap::new();
    for r in runs {
        // Per label in this run: peak, its line, processes, samples.
        let mut in_run: BTreeMap<String, (Bytes, &str, u32, u32)> = BTreeMap::new();
        for part in r.parts.iter().filter(|x| !is_own(&x.cmd, own)) {
            let e = in_run.entry(part_label(&part.cmd)).or_insert((0, "", 0, 0));
            if part.peak >= e.0 {
                (e.0, e.1) = (part.peak, part.cmd.as_str());
            }
            e.2 = e.2.max(part.procs);
            e.3 = e.3.max(part.samples);
        }
        for (label, (peak, example, procs, samples)) in in_run {
            let e = by_label.entry(label).or_default();
            e.runs += 1;
            if peak >= e.peak {
                (e.peak, e.example) = (peak, example);
            }
            e.procs = e.procs.max(procs);
            if r.representative() {
                e.secs.push(samples);
            }
        }
    }
    let mut rows: Vec<(String, Seen)> = by_label.into_iter().collect();
    rows.sort_by(|a, b| b.1.peak.cmp(&a.1.peak).then_with(|| a.0.cmp(&b.0)));
    rows.into_iter()
        .map(|(cmd, mut e)| {
            e.secs.sort_unstable();
            json!({
                "cmd": cmd,
                "example": example(e.example),
                "runs": e.runs,
                "peak": e.peak,
                "procs": e.procs,
                "median_s": e.secs.get(e.secs.len() / 2),
            })
        })
        .collect()
}

fn job_json(j: &Job, now: u64, cfg: &Config, profiles: &Profiles) -> Value {
    let state = match j.state {
        JobState::Queued => "queued",
        JobState::Running => "running",
        JobState::Frozen => "frozen",
    };
    let workers = j
        .tuned
        .map(|t| t.workers)
        .or_else(|| j.spec.tune.as_ref().map(|t| t.default_workers));
    let memory_range = profiles
        .get(&j.spec.key)
        .and_then(|p| p.memory_range(workers));
    json!({
        "id": j.id.to_string(),
        "label": j.spec.label,
        "cmd": j.spec.cmd,
        "full_cmd": j.spec.full_cmd,
        "cwd": j.spec.cwd,
        "session": j.spec.session,
        "agent": j.spec.agent,
        "key": j.spec.key,
        "state": state,
        "pinned": j.pinned,
        "low": j.spec.low,
        "estimate": j.estimate,
        "memory_range": memory_range,
        "runtime_estimate_s": j.median_ms.map(secs),
        "growth_reserved": headroom_core::admission::pending_growth(&[j]),
        "samples": j.samples,
        "samples_required": ESTIMATE_SAMPLES,
        "footprint": j.footprint,
        "peak": j.peak,
        "runaway_at": j.runaway_at(cfg.runaway_factor, cfg.runaway_cap),
        "elapsed_s": j.started_at.map_or(0, |t| secs(now.saturating_sub(t))),
        "frozen_s": j.frozen_at.map_or(0, |t| secs(now.saturating_sub(t))),
        "frozen_by": j.frozen_by,
        "waited_s": secs(j.started_at.unwrap_or(now).saturating_sub(j.enqueued_at)),
        "cpu_pct": j.cpu_pct,
        "procs": j.procs,
        "now": inner(j),
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

/// A profile as the page shows it. `parts` are what ran inside its runs,
/// when it is the open one (see `Extras::open_profile`).
fn profile_json(p: &Profile, estimate: Bytes, parts: Option<Vec<Value>>) -> Value {
    let peaks: Vec<Bytes> = p
        .runs
        .iter()
        .filter(|r| r.representative() && r.peak > 0)
        .map(|r| r.peak)
        .collect();
    json!({
        "peaks": peaks,
        "p90": estimate,
        "median_s": p.median_ms().map(secs),
        "samples": p.samples(),
        "runs": p.runs.len(),
        "parts": parts,
    })
}

/// The command at the end of a key, which its parts leave out.
fn own_command(key: &str) -> &str {
    key.rsplit('|').next().unwrap_or("")
}

/// Each of `commands` over every invocation profiled for it, oldest run
/// first, keyed by `command_profile_key`. A command with no exact keys,
/// from before they existed, is its own old profile. Each key keeps its
/// last `KEEP` runs, so `runs` counts what is kept, not every run.
fn command_profiles(
    profiles: &Profiles,
    commands: &BTreeSet<String>,
    default: Bytes,
    open: Option<&str>,
) -> Vec<(String, Value)> {
    let mut runs: BTreeMap<&str, Vec<&Run>> = BTreeMap::new();
    for (k, p) in &profiles.0 {
        if !is_exact_key(k) {
            continue;
        }
        if let Some(c) = commands.get(&command_key(k)) {
            runs.entry(c).or_default().extend(&p.runs);
        }
    }
    for c in commands {
        if let (false, Some(p)) = (runs.contains_key(c.as_str()), profiles.get(c)) {
            runs.insert(c, p.runs.iter().collect());
        }
    }
    runs.into_iter()
        .map(|(c, mut rs)| {
            rs.sort_by_key(|r| r.at);
            // The statistics need no parts; copying them would copy most of
            // the profile store every second.
            let merged = Profile {
                runs: rs
                    .iter()
                    .map(|r| Run {
                        peak: r.peak,
                        duration_ms: r.duration_ms,
                        exit: r.exit,
                        flagged: r.flagged,
                        at: r.at,
                        workers: r.workers,
                        parts: Vec::new(),
                    })
                    .collect(),
            };
            let est = Profiles(BTreeMap::from([(c.to_string(), merged)]));
            let p = est.get(c).expect("just added");
            let key = command_profile_key(c);
            let parts = (open == Some(key.as_str())).then(|| profile_parts(rs, own_command(c)));
            let mut v = profile_json(p, est.estimate(c, "", default).bytes, parts);
            v["command"] = json!(c);
            (key, v)
        })
        .collect()
}

pub fn state(l: &Listing, x: &Extras) -> Value {
    let s = &l.snapshot;
    let cfg = x.cfg;
    let growth = headroom_core::admission::pending_growth(&l.running.iter().collect::<Vec<_>>());
    // A job's own workload: its estimate comes from exactly this key.
    let mut profiles: BTreeMap<String, Value> = l
        .running
        .iter()
        .chain(&l.queued)
        .filter_map(|j| {
            let k = j.spec.key.as_str();
            let p = x.profiles.get(k)?;
            // This summarizes observed peaks. Scheduling can reserve more at
            // a known default worker count, which cannot be read from a key.
            let est = x.profiles.estimate(k, "", cfg.default_estimate);
            let parts = (x.open_profile == Some(k)).then(|| profile_parts(&p.runs, own_command(k)));
            Some((k.to_string(), profile_json(p, est.bytes, parts)))
        })
        .collect();
    // Every command on the Commands and History pages, over all its
    // invocations. The longest window holds every shorter one.
    let mut listed: BTreeSet<String> = x
        .tally
        .window(x.wall, TALLY_DAYS)
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    listed.extend(l.history.iter().map(|h| command_key(&h.key)));
    profiles.extend(command_profiles(
        x.profiles,
        &listed,
        cfg.default_estimate,
        x.open_profile,
    ));
    let history: Vec<Value> = l
        .history
        .iter()
        .map(|h| {
            json!({
                "id": h.id.to_string(),
                "label": h.label,
                "cmd": h.cmd,
                "key": h.key,
                "profile": command_profile_key(&command_key(&h.key)),
                "duration_s": secs(h.duration_ms),
                "peak": h.peak,
                "estimate": h.estimate,
                "exit": h.exit,
                "ended_at": h.ended_at,
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
        "version": env!("CARGO_PKG_VERSION"),
        "system": {
            "pressure": s.pressure,
            "mem_total": s.mem_total,
            "mem_used": s.mem_used,
            "mem_available": s.mem_available,
            "swap_used": s.swap_used,
            "cpu_pct": s.cpu_pct,
        },
        "held": l.held,
        "queue_budget": {
            "growth": growth,
            "available": s.mem_available.saturating_sub(cfg.headroom).saturating_sub(growth),
        },
        "hooks": hooks,
        "queue_reason": l.blocked,
        "running": l.running.iter().map(|j| job_json(j, l.now, cfg, x.profiles)).collect::<Vec<_>>(),
        "queued": l.queued.iter().map(|j| job_json(j, l.now, cfg, x.profiles)).collect::<Vec<_>>(),
        "banners": banners(l, x),
        "history": history,
        "last_day": last_day(&l.history, x.wall),
        "commands": commands(x.tally, x.wall),
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
    use headroom_core::model::{FrozenBy, GIB, JobSpec, Snapshot};

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
                    started: None,
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
                    started: None,
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
            others: &[],
            hooks: &[("claude", "installed"), ("codex", "absent")],
            classify: None,
            open_profile: None,
            dismissed: d,
            wall: 1_100,
            since: 500,
            login: Login::Brew,
            attached: false,
        }
    }

    #[test]
    fn estimate_progress_uses_the_attempts_sample_count_and_core_threshold() {
        let cfg = Config::default();
        let profiles = Profiles::default();
        let mut job = listing().running.remove(0);
        for samples in 0..=ESTIMATE_SAMPLES as u32 + 1 {
            job.samples = samples;
            let v = job_json(&job, 100_000, &cfg, &profiles);
            assert_eq!(v["samples"], samples);
            assert_eq!(v["samples_required"], ESTIMATE_SAMPLES);
        }
    }

    #[test]
    fn queue_budget_reserves_growth_only_for_running_jobs_and_never_goes_negative() {
        let mut l = listing();
        let cfg = Config::default();
        let profiles = Profiles::default();
        let dismissed = HashSet::new();
        l.snapshot.mem_available = cfg.headroom + 6 * GIB;
        l.running[0].estimate = 12 * GIB;
        l.running[0].footprint = 2 * GIB;
        let v = state(&l, &extras(&cfg, &profiles, &dismissed));
        assert_eq!(v["queue_budget"]["growth"], 0);
        assert_eq!(v["queue_budget"]["available"], 6 * GIB);
        assert_eq!(v["running"][0]["frozen_by"], "guard");
        l.running[0].state = JobState::Running;
        let v = state(&l, &extras(&cfg, &profiles, &dismissed));
        assert_eq!(v["queue_budget"]["growth"], 10 * GIB);
        assert_eq!(v["queue_budget"]["available"], 0);
    }

    #[test]
    fn a_command_page_lists_what_ran_inside_its_runs() {
        use headroom_core::model::Part;
        use headroom_core::profile::Run;
        let part = |cmd: &str, gb: u64, procs: u32, samples: u32| Part {
            cmd: cmd.into(),
            peak: gb << 30,
            procs,
            samples,
        };
        let run = |parts: Vec<Part>| Run {
            peak: 4 << 30,
            duration_ms: 60_000,
            exit: Some(0),
            flagged: false,
            at: 1,
            workers: None,
            parts,
        };
        let mut p = Profiles::default();
        let key = "/r|git push";
        // An older run, from before parts were kept, has none.
        p.record(key, key, run(vec![]));
        p.record(
            key,
            key,
            run(vec![
                part("tsc --noEmit", 3, 1, 40),
                part("git push origin x", 1, 1, 60),
            ]),
        );
        p.record(
            key,
            key,
            run(vec![
                part("tsc --noEmit", 4, 1, 50),
                part("eslint .", 1, 2, 20),
            ]),
        );
        p.record(key, key, run(vec![part("tsc --noEmit", 2, 1, 45)]));
        let rows = profile_parts(&p.get(key).unwrap().runs, "git push");
        assert_eq!(rows.len(), 2, "git itself is the command, not part of it");
        assert_eq!(rows[0]["cmd"], "tsc");
        assert_eq!(rows[0]["example"], "tsc --noEmit");
        assert_eq!(
            (rows[0]["runs"].clone(), rows[0]["peak"].clone()),
            (json!(3), json!(4u64 << 30))
        );
        assert_eq!(rows[0]["median_s"], 45);
        assert_eq!(
            (rows[1]["cmd"].clone(), rows[1]["procs"].clone()),
            (json!("eslint"), json!(2))
        );
        // Only runs from before: nothing to list.
        let mut old = Profiles::default();
        old.record(key, key, run(vec![]));
        assert!(profile_parts(&old.get(key).unwrap().runs, "git push").is_empty());
        // Many inner commands: all of them, largest first.
        let many: Vec<Part> = (0..12)
            .map(|i| part(&format!("c{i}"), i + 1, 1, 1))
            .collect();
        let mut big = Profiles::default();
        big.record(key, key, run(many));
        let rows = profile_parts(&big.get(key).unwrap().runs, "git push");
        assert_eq!(rows.len(), 12);
        assert_eq!(rows[0]["cmd"], "c11");
        assert_eq!(rows[11]["cmd"], "c0");
    }

    #[test]
    fn an_inner_command_is_named_without_its_arguments() {
        for (line, label) in [
            ("eslint.js --fix ./a.ts ./b.ts", "eslint.js"),
            ("yarn eslint ./src", "yarn eslint"),
            ("corepack yarn biome check src/x.ts", "corepack yarn biome"),
            ("git commit -m wip", "git commit"),
            ("rustc --crate-name foo --edition=2024 src/lib.rs", "rustc"),
            ("/opt/homebrew/bin/node forks.js", "node"),
            ("cross-env.js NODE_ENV=test mocha", "cross-env.js"),
            ("vitest run", "vitest run"),
            ("fallow audit:deps", "fallow audit:deps"),
            ("tsc", "tsc"),
            ("", ""),
            ("   ", ""),
        ] {
            assert_eq!(part_label(line), label, "{line:?}");
        }
        // Long example lines are cut, short ones kept whole.
        let long = format!("eslint.js {}", "ä".repeat(400));
        let cut = example(&long);
        assert_eq!(cut.chars().count(), EXAMPLE_CHARS + 1);
        assert!(cut.ends_with('…'));
        let exact = "x".repeat(EXAMPLE_CHARS);
        assert_eq!(example(&exact), exact);
        assert_eq!(example(""), "");
        // A word too long to be a subcommand is an argument.
        assert_eq!(part_label(&format!("tool {}", "a".repeat(25))), "tool");
        assert_eq!(part_label(&format!("tool {}", "a".repeat(24))).len(), 29);
    }

    #[test]
    fn the_same_tool_with_other_arguments_is_one_inner_command() {
        use headroom_core::model::Part;
        let part = |cmd: &str, gb: u64, procs: u32, samples: u32| Part {
            cmd: cmd.into(),
            peak: gb << 30,
            procs,
            samples,
        };
        let run = |parts: Vec<Part>| Run {
            peak: 4 << 30,
            duration_ms: 60_000,
            exit: Some(0),
            flagged: false,
            at: 1,
            workers: None,
            parts,
        };
        let runs = [
            // Two batches in one run: one run, its largest numbers.
            run(vec![
                part("eslint.js --fix a.ts", 1, 1, 10),
                part("eslint.js --fix b.ts c.ts", 3, 2, 30),
            ]),
            run(vec![part("eslint.js --fix d.ts", 2, 4, 20)]),
        ];
        let rows = profile_parts(&runs, "git commit");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["cmd"], "eslint.js");
        assert_eq!(rows[0]["runs"], 2, "counted once per run");
        assert_eq!(rows[0]["peak"], 3u64 << 30);
        assert_eq!(
            rows[0]["example"], "eslint.js --fix b.ts c.ts",
            "the largest line"
        );
        assert_eq!(rows[0]["procs"], 4);
        // Over the runs, each its longest batch: 30 and 20, upper middle.
        assert_eq!(rows[0]["median_s"], 30);
        // The command itself, with any arguments, stays out.
        let own = [run(vec![part("git commit -m wip", 1, 1, 1)])];
        assert!(profile_parts(&own, "git commit").is_empty());
    }

    #[test]
    fn inner_runtime_ignores_unsuccessful_runs_but_keeps_their_observed_peaks() {
        use headroom_core::model::Part;
        use headroom_core::profile::Run;
        let p = Profile {
            runs: [(30, false, 0), (600, false, 1), (900, true, 0)]
                .into_iter()
                .map(|(samples, flagged, exit)| Run {
                    peak: samples as Bytes,
                    duration_ms: samples as u64 * 1000,
                    exit: Some(exit),
                    flagged,
                    at: 0,
                    workers: None,
                    parts: vec![Part {
                        cmd: "tsc --noEmit".into(),
                        peak: samples as Bytes,
                        procs: 1,
                        samples,
                    }],
                })
                .collect(),
        };
        let rows = profile_parts(&p.runs, "git push");
        assert_eq!(p.median_ms(), Some(30_000));
        assert_eq!(rows[0]["median_s"], 30);
        assert_eq!(rows[0]["runs"], 3);
        assert_eq!(rows[0]["peak"], 900);
        let flagged = Profile {
            runs: p.runs.into_iter().filter(|r| !r.representative()).collect(),
        };
        let rows = profile_parts(&flagged.runs, "git push");
        assert!(rows[0]["median_s"].is_null());
        assert_eq!(rows[0]["runs"], 2);
        assert_eq!(rows[0]["peak"], 900);
    }

    #[test]
    fn the_running_row_names_what_runs_inside_unless_it_is_the_command_itself() {
        let mut j = listing().running[0].clone();
        assert_eq!(inner(&j), None, "nothing measured yet");
        j.now = Some("vitest run --pool=forks src/a.test.ts".into());
        assert_eq!(inner(&j), None, "the job's own command, with its arguments");
        j.spec.cmd = "git commit".into();
        j.now = Some("git commit -q -F msg-2186.txt".into());
        assert_eq!(inner(&j), None);
        j.now = Some("tsc --noEmit".into());
        assert_eq!(inner(&j), Some("tsc --noEmit"));
        // Shorter than the command, or only sharing its first word.
        j.now = Some("git".into());
        assert_eq!(inner(&j), Some("git"));
        j.now = Some("git hook run pre-commit".into());
        assert_eq!(inner(&j), Some("git hook run pre-commit"));
    }

    #[test]
    fn profile_charts_exclude_unmeasured_and_interrupted_peaks_but_keep_run_counts() {
        use headroom_core::profile::Run;
        let (cfg, mut p, d) = (Config::default(), Profiles::default(), HashSet::new());
        let key = "/r|vitest run";
        for (peak, flagged, duration_ms, exit) in [
            (0, false, 1_000, 0),
            (4 * GIB, false, 3_000, 0),
            (8 * GIB, true, 9_000, 0),
            (6 * GIB, false, 500, 1),
        ] {
            p.record(
                key,
                key,
                Run {
                    peak,
                    flagged,
                    duration_ms,
                    exit: Some(exit),
                    at: 0,
                    workers: Some(2),
                    parts: vec![],
                },
            );
        }
        let v = state(&listing(), &extras(&cfg, &p, &d));
        let profile = &v["profiles"][key];
        assert_eq!(profile["peaks"], json!([4 * GIB]));
        assert_eq!(profile["samples"], 1);
        assert_eq!(profile["runs"], 4);
        assert_eq!(profile["median_s"], 1, "unmeasured durations still count");
        assert_eq!(
            profile["p90"],
            8 * GIB,
            "interrupted peaks remain a lower bound"
        );

        let mut unmeasured = Profiles::default();
        unmeasured.record(
            key,
            key,
            Run {
                peak: 0,
                flagged: false,
                duration_ms: 1_000,
                exit: Some(0),
                at: 0,
                workers: None,
                parts: vec![],
            },
        );
        let v = state(&listing(), &extras(&cfg, &unmeasured, &d));
        assert_eq!(v["profiles"][key]["peaks"], json!([]));
        assert_eq!(v["profiles"][key]["samples"], 0);
        assert_eq!(v["profiles"][key]["runs"], 1);

        let mut failed = Profiles::default();
        failed.record(
            key,
            key,
            Run {
                peak: 6 * GIB,
                flagged: false,
                duration_ms: 500,
                exit: Some(1),
                at: 0,
                workers: None,
                parts: vec![],
            },
        );
        let v = state(&listing(), &extras(&cfg, &failed, &d));
        assert!(v["profiles"][key]["median_s"].is_null());
        assert_eq!(v["profiles"][key]["peaks"], json!([]));
        assert_eq!(v["profiles"][key]["p90"], 6 * GIB);
    }

    #[test]
    fn memory_ranges_use_the_exact_profile_and_actual_worker_count() {
        use headroom_core::model::Tuned;
        use headroom_core::profile::Run;
        use headroom_core::tweaks::{Tool, Tunable};

        let (cfg, mut p, d) = (Config::default(), Profiles::default(), HashSet::new());
        let mut l = listing();
        let key = l.running[0].spec.key.clone();
        let fallback = "/r|vitest";
        l.running[0].spec.fallback_key = fallback.into();
        let run = |peak, workers| Run {
            peak,
            workers,
            flagged: false,
            exit: Some(0),
            duration_ms: 1000,
            at: 1,
            parts: vec![],
        };
        // A populated fallback must not provide a range for an unseen key.
        for peak in [20, 30, 40] {
            p.record("other command", fallback, run(peak * GIB, None));
        }
        let v = state(&l, &extras(&cfg, &p, &d));
        assert!(v["running"][0]["memory_range"].is_null());
        for (workers, peaks) in [
            (None, [1, 2, 3]),
            (Some(8), [8, 9, 10]),
            (Some(2), [2, 3, 4]),
        ] {
            for peak in peaks {
                p.record(&key, fallback, run(peak * GIB, workers));
            }
        }
        let range = |lower: u64, upper: u64, workers: Option<u32>| {
            json!({
                "lower": lower * GIB, "upper": upper * GIB, "samples": 3, "workers": workers,
            })
        };
        let v = state(&l, &extras(&cfg, &p, &d));
        assert_eq!(v["running"][0]["memory_range"], range(1, 3, None));
        assert_eq!(v["queued"][0]["memory_range"], range(1, 3, None));
        l.running[0].spec.tune = Some(Tunable {
            tool: Tool::Vitest,
            default_workers: 8,
            npm: false,
        });
        let v = state(&l, &extras(&cfg, &p, &d));
        assert_eq!(v["running"][0]["memory_range"], range(8, 10, Some(8)));
        l.running[0].tuned = Some(Tuned {
            workers: 2,
            default: 8,
        });
        let v = state(&l, &extras(&cfg, &p, &d));
        assert_eq!(v["running"][0]["memory_range"], range(2, 4, Some(2)));
        assert_eq!(v["running"][0]["estimate"], 4 * GIB);
        assert_eq!(v["running"][0]["samples"], 3);
        l.running[0].tuned = Some(Tuned {
            workers: 4,
            default: 8,
        });
        let v = state(&l, &extras(&cfg, &p, &d));
        assert!(v["running"][0]["memory_range"].is_null());
    }

    #[test]
    fn runtime_display_uses_the_jobs_scheduler_estimate_not_a_fresh_profile_median() {
        use headroom_core::profile::Run;

        let cfg = Config::default();
        let mut profiles = Profiles::default();
        let mut job = listing().running[0].clone();
        profiles.record(
            &job.spec.key,
            &job.spec.fallback_key,
            Run {
                peak: GIB,
                duration_ms: 900_000,
                exit: Some(0),
                flagged: false,
                at: 1,
                workers: None,
                parts: vec![],
            },
        );
        // A running attempt retains its admission estimate when history changes.
        job.median_ms = Some(45_000);
        assert_eq!(
            job_json(&job, 100_000, &cfg, &profiles)["runtime_estimate_s"],
            45
        );
        // No threshold hides an estimate based on one run, or a subsecond run.
        job.median_ms = Some(500);
        assert_eq!(
            job_json(&job, 100_000, &cfg, &profiles)["runtime_estimate_s"],
            0
        );
        // History at another worker count must not invent a missing job estimate.
        job.median_ms = None;
        assert!(job_json(&job, 100_000, &cfg, &profiles)["runtime_estimate_s"].is_null());

        job.state = JobState::Running;
        job.estimate = 4 * GIB;
        job.footprint = GIB;
        assert_eq!(
            job_json(&job, 100_000, &cfg, &profiles)["growth_reserved"],
            3 * GIB
        );
        job.state = JobState::Frozen;
        assert_eq!(
            job_json(&job, 100_000, &cfg, &profiles)["growth_reserved"],
            0
        );
        job.state = JobState::Running;
        job.footprint = 5 * GIB;
        assert_eq!(
            job_json(&job, 100_000, &cfg, &profiles)["growth_reserved"],
            0
        );
    }

    #[test]
    fn state_matches_the_prototype_contract() {
        let (cfg, p, d) = (Config::default(), Profiles::default(), HashSet::new());
        let v = state(&listing(), &extras(&cfg, &p, &d));
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
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
        assert_eq!(v["history"][0]["ended_at"], 1000);
        assert_eq!(v["history"][0]["ended_ago_s"], 100);
        assert!(v["history"][1]["guard"].is_null());
        // The requeued run is counted when its retry ends.
        assert_eq!(v["last_day"]["runs"], 1);
        assert_eq!(v["last_day"]["requeued"], 1);
    }

    #[test]
    fn job_origin_preserves_the_reported_agent_for_running_and_queued_jobs() {
        let cfg = Config::default();
        let profiles = Profiles::default();
        let dismissed = HashSet::new();
        let mut l = listing();
        for agent in [
            None,
            Some(""),
            Some("codex"),
            Some("claude"),
            Some("pi"),
            Some("custom <agent>"),
        ] {
            for j in l.running.iter_mut().chain(&mut l.queued) {
                j.spec.agent = agent.map(str::to_owned);
                // Session and command text must never be used to guess an origin.
                j.spec.session = Some("codex-session".into());
                j.spec.cmd = "claude test".into();
            }
            let v = state(&l, &extras(&cfg, &profiles, &dismissed));
            for group in ["running", "queued"] {
                assert!(v[group][0].get("agent").is_some());
                assert_eq!(v[group][0]["agent"], json!(agent));
            }
        }
    }

    #[test]
    fn queue_wait_stops_growing_when_a_job_is_granted() {
        let cfg = Config::default();
        let profiles = Profiles::default();
        let mut job = Job::queued(1, JobSpec::default(), 0, 0, 1_000);
        assert_eq!(job_json(&job, 6_000, &cfg, &profiles)["waited_s"], 5);
        job.started_at = Some(6_000);
        job.state = JobState::Running;
        assert_eq!(job_json(&job, 16_000, &cfg, &profiles)["waited_s"], 5);
        job.state = JobState::Frozen;
        job.frozen_at = Some(16_000);
        assert_eq!(job_json(&job, 26_000, &cfg, &profiles)["waited_s"], 5);
    }

    fn ended(at: u64, outcome: Outcome, exit: Option<i32>, waited_ms: u64) -> HistoryEntry {
        HistoryEntry {
            id: at,
            label: "r".into(),
            cmd: "c".into(),
            key: "/r|c".into(),
            duration_ms: 1,
            started: None,
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
            started: None,
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
            parts: Vec::new(),
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
        assert_eq!(v["commands"][1]["rows"][0]["profile"], "cmd:/r|old");
        assert_eq!(v["profiles"]["cmd:/r|old"]["samples"], 1);
        assert_eq!(v["profiles"]["cmd:/r|old"]["command"], "/r|old");
        // Not in any window, not sent: the 90-day page stops there too.
        x.wall += 200 * 86_400;
        assert!(
            state(&listing(), &x)["profiles"]
                .get("cmd:/r|old")
                .is_none()
        );
    }

    #[test]
    fn commands_lists_every_command_and_the_rows_add_up_to_the_totals() {
        let mut t = Tally::default();
        let wall = 100 * 86_400;
        for i in 0..205 {
            t.add(&HistoryEntry {
                key: format!("/r|c{i}"),
                ..ended(wall, Outcome::Exited, Some(i % 2), 0)
            });
        }
        let v = commands(&t, wall);
        let rows = v[0]["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 205);
        assert_eq!(v[0]["count"], 205);
        let sum = |f: &str| rows.iter().map(|r| r[f].as_u64().unwrap()).sum::<u64>();
        assert_eq!(sum("runs"), v[0]["total"]["runs"].as_u64().unwrap());
        assert_eq!(sum("failed"), v[0]["total"]["failed"].as_u64().unwrap());
    }

    fn digest_key(c: char, cmd: &str) -> String {
        format!("/r|v2:{}|{cmd}", c.to_string().repeat(64))
    }

    #[test]
    fn a_command_is_one_row_with_one_profile_over_its_invocations() {
        use headroom_core::model::Part;
        let (cfg, d) = (Config::default(), HashSet::new());
        let run = |gb: u64, at: u64, exit: i32, parts: Vec<Part>| Run {
            peak: gb << 30,
            duration_ms: 60_000,
            exit: Some(exit),
            flagged: false,
            at,
            workers: None,
            parts,
        };
        let tsc = |procs| Part {
            cmd: "tsc --noEmit".into(),
            peak: 1 << 30,
            procs,
            samples: 30,
        };
        let (a, b) = (digest_key('a', "git commit"), digest_key('b', "git commit"));
        let mut p = Profiles::default();
        // The fallback "/r|git commit" collects these too; it is no invocation.
        p.record(&b, "/r|git commit", run(3, 20, 0, vec![tsc(1)]));
        p.record(&a, "/r|git commit", run(1, 10, 0, vec![tsc(2)]));
        p.record(&a, "/r|git commit", run(5, 30, 1, vec![]));
        let wall = 100 * 86_400;
        let mut t = Tally::default();
        let mut l = listing();
        l.history.clear();
        for k in [&a, &b, &a] {
            let h = HistoryEntry {
                key: k.clone(),
                ..ended(wall, Outcome::Exited, Some(0), 0)
            };
            t.add(&h);
            l.history.push(h);
        }
        let mut x = extras(&cfg, &p, &d);
        (x.tally, x.wall) = (&t, wall);
        let closed = state(&l, &x);
        assert!(
            closed["profiles"]["cmd:/r|git commit"]["parts"].is_null(),
            "only when open"
        );
        x.open_profile = Some("cmd:/r|git commit");
        let v = state(&l, &x);
        let week = &v["commands"][0];
        assert_eq!(week["count"], 1);
        assert_eq!(week["rows"][0]["key"], "/r|git commit");
        assert_eq!(week["rows"][0]["runs"], week["total"]["runs"]);
        assert_eq!(week["rows"][0]["runs"], 3);
        let prof = &v["profiles"]["cmd:/r|git commit"];
        assert_eq!(prof["command"], "/r|git commit");
        assert_eq!(
            prof["runs"], 3,
            "the fallback's copies are not counted again"
        );
        assert_eq!(prof["samples"], 2);
        assert_eq!(
            prof["peaks"],
            json!([1u64 << 30, 3u64 << 30]),
            "oldest first"
        );
        // The failed 5 GB run is a lower bound on what it needs.
        assert_eq!(prof["p90"], 5u64 << 30);
        assert_eq!(prof["parts"][0]["runs"], 2);
        assert_eq!(prof["parts"][0]["procs"], 2);
        // History rows open the same profile.
        for h in v["history"].as_array().unwrap() {
            assert_eq!(h["profile"], "cmd:/r|git commit");
        }
        // Exact profiles of past runs are not sent.
        assert!(v["profiles"].get(&a).is_none());
    }

    #[test]
    fn a_command_with_nothing_profiled_has_no_profile() {
        let (cfg, p, d) = (Config::default(), Profiles::default(), HashSet::new());
        let wall = 100 * 86_400;
        let mut t = Tally::default();
        t.add(&HistoryEntry {
            key: digest_key('a', "git commit"),
            ..ended(wall, Outcome::Cancelled, None, 5_000)
        });
        let mut x = extras(&cfg, &p, &d);
        (x.tally, x.wall) = (&t, wall);
        let v = state(&listing(), &x);
        assert_eq!(v["commands"][0]["rows"][0]["runs"], 0);
        assert_eq!(v["commands"][0]["rows"][0]["wait_s"], 5);
        assert!(v["profiles"].get("cmd:/r|git commit").is_none());
    }

    #[test]
    fn last_day_sums_what_finished_in_it() {
        let wall = 100_000;
        let mut first = ended(80_000, Outcome::Exited, Some(1), 0);
        first.ahead_of = Some("r · e2e".into());
        let h = vec![
            ended(99_990, Outcome::Requeued, None, 30_000),
            ended(90_000, Outcome::Exited, Some(0), 90_000),
            first,
            ended(70_000, Outcome::Cancelled, None, 20_000),
            ended(65_000, Outcome::Exited, None, 0),
            ended(64_500, Outcome::Exited, Some(0), WAITED_MS - 1),
            ended(64_000, Outcome::GaveUp, None, 0),
            ended(wall - HISTORY_DAY_S, Outcome::Exited, Some(0), 1_000),
            ended(wall - HISTORY_DAY_S - 1, Outcome::Exited, Some(2), 500_000),
        ];
        assert_eq!(
            last_day(&h, wall),
            json!({
                "runs": 6, "failed": 3, "requeued": 1,
                "waited": 2, "waited_s": 91, "longest_wait_s": 90,
                "wait_p50_s": 1, "wait_p95_s": 90,
                "went_first": 1, "more": false,
            })
        );
    }

    #[test]
    fn last_day_counts_started_disconnects_and_ignores_future_entries() {
        let mut queued = ended(100_000, Outcome::Disconnected, None, 90_000);
        queued.duration_ms = 0;
        queued.started = Some(false);
        let mut granted = queued.clone();
        granted.duration_ms = 10_000;
        let mut instant = ended(100_000, Outcome::Disconnected, None, 5_000);
        instant.duration_ms = 0;
        instant.started = Some(true);
        let mut legacy_queued = queued.clone();
        legacy_queued.started = None;
        let legacy_run = ended(100_000, Outcome::Disconnected, None, 10_000);
        let future = ended(100_001, Outcome::Exited, Some(1), 60_000);
        let h = [queued, granted, instant, legacy_queued, legacy_run, future];
        let v = last_day(&h, 100_000);
        assert_eq!(v["runs"], 2);
        assert_eq!(v["failed"], 0);
        assert_eq!(v["waited"], 2);
        assert_eq!(v["waited_s"], 15);
        assert_eq!(v["wait_p50_s"], 5);
        assert_eq!(v["wait_p95_s"], 10);
        assert_eq!(last_day(&h, 100_000 - HISTORY_DAY_S - 1)["runs"], 0);
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
            (
                quiet["runs"].as_u64(),
                quiet["waited"].as_u64(),
                quiet["wait_p95_s"].as_u64()
            ),
            (Some(0), Some(0), Some(0))
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
        let early = [
            ended(0, Outcome::Exited, Some(0), 0),
            ended(30, Outcome::Exited, Some(0), 0),
        ];
        assert_eq!(last_day(&early, 30)["runs"], 2);
    }

    #[test]
    fn the_history_page_shows_every_run_kept_and_the_summary_counts_the_day() {
        let (cfg, p, d) = (Config::default(), Profiles::default(), HashSet::new());
        let mut l = listing();
        l.history = (0..HISTORY as u64 + 10)
            .map(|i| ended(1_000 - i, Outcome::Exited, Some(0), 0))
            .collect();
        let v = state(&l, &extras(&cfg, &p, &d));
        assert_eq!(v["history"].as_array().unwrap().len(), HISTORY + 10);
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
