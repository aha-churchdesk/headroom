//! The daemon's state machine: events in, effects out.
//!
//! Every input — a client message, a dropped connection, a once-a-second
//! sample — goes through here and comes back as effects: messages to send,
//! signals to deliver, files to save. The daemon only carries them out.
//! Keeping the sockets and `kill(2)` outside is what lets the tests below
//! drive whole job lifecycles with scripted time and memory.

use std::collections::{BTreeMap, HashMap, VecDeque};

use crate::admission::{self, Blocked};
use crate::config::Config;
use crate::guard::{self, Action, GuardState};
use crate::model::{
    Bytes, FrozenBy, HistoryEntry, Job, JobId, JobSpec, JobState, Millis, Outcome, Snapshot, Tuned,
};
use crate::profile::{Profiles, Run, Scaling};
use crate::protocol::{Control, DaemonMsg, Listing};

pub type ConnId = u64;

/// History entries shown, and kept whatever their age.
pub const HISTORY: usize = 50;
/// Older entries are kept for a day, for the popover's summary of it…
pub const HISTORY_DAY_S: u64 = 86_400;
/// …up to this many in all.
pub const HISTORY_MAX: usize = 2_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sig {
    Stop,
    Cont,
    Term,
    Kill,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Send(ConnId, DaemonMsg),
    Signal { pgid: i32, sig: Sig },
    SaveProfiles,
    SaveHistory,
    SaveConfig,
}

/// One process group's measured usage, from the sampler.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    pub footprint: Bytes,
    pub cpu_pct: f64,
    pub procs: u32,
}

/// A decision about how a running job ends, made before it has ended.
#[derive(Clone, Debug, PartialEq)]
enum Fate {
    Requeue(String),
    GiveUp(String),
    ManualKill,
}

#[derive(Clone, Debug)]
struct Entry {
    job: Job,
    conn: ConnId,
    fate: Option<Fate>,
    /// Last position report sent, so clients only hear about changes.
    told: Option<(usize, usize, Option<String>)>,
}

pub struct Scheduler {
    pub cfg: Config,
    pub profiles: Profiles,
    pub history: VecDeque<HistoryEntry>,
    pub held: bool,
    jobs: BTreeMap<JobId, Entry>,
    /// Queued job ids in queue order: pinned first, then the user's order.
    order: Vec<JobId>,
    next_id: JobId,
    guard: GuardState,
    snap: Snapshot,
    blocked: Option<Blocked>,
    /// Process groups owed a SIGKILL once their grace period runs out.
    kills: Vec<(i32, Millis)>,
    /// The latest time any event carried, for listings.
    clock: Millis,
}

fn reply(conn: ConnId, r: Result<(), String>) -> Effect {
    Effect::Send(
        conn,
        r.map_or_else(|msg| DaemonMsg::Error { msg }, |()| DaemonMsg::Ok),
    )
}

impl Scheduler {
    pub fn new(cfg: Config, profiles: Profiles, history: VecDeque<HistoryEntry>) -> Self {
        Self {
            cfg,
            profiles,
            history,
            held: false,
            jobs: BTreeMap::new(),
            order: Vec::new(),
            next_id: 1,
            guard: GuardState::default(),
            snap: Snapshot::default(),
            blocked: None,
            kills: Vec::new(),
            clock: 0,
        }
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snap
    }

    fn by_conn(&self, conn: ConnId) -> Option<JobId> {
        self.jobs
            .iter()
            .find(|(_, e)| e.conn == conn)
            .map(|(&id, _)| id)
    }

    /// Process groups of running and frozen jobs, for the sampler.
    pub fn pgids(&self) -> Vec<i32> {
        let mut v: Vec<i32> = self.jobs.values().filter_map(|e| e.job.pgid).collect();
        v.extend(self.kills.iter().map(|(p, _)| *p));
        v.sort_unstable();
        v.dedup();
        v
    }

    // ── client messages ───────────────────────────────────────────────

    pub fn enqueue(&mut self, conn: ConnId, spec: JobSpec, now: Millis) -> Vec<Effect> {
        self.clock = self.clock.max(now);
        if self.by_conn(conn).is_some() {
            return vec![reply(conn, Err("this connection already has a job".into()))];
        }
        let est = self
            .profiles
            .estimate(&spec.key, &spec.fallback_key, self.cfg.default_estimate);
        let id = self.next_id;
        self.next_id += 1;
        let scaling = self.scaling_for(&spec);
        let median = self.median_for(&spec);
        let mut job = Job::queued(id, spec, est.bytes, est.samples, now);
        (job.scaling, job.median_ms) = (scaling, median);
        self.jobs.insert(
            id,
            Entry {
                job,
                conn,
                fate: None,
                told: None,
            },
        );
        self.order.push(id);
        self.reorder();
        self.settle(now)
    }

    pub fn started(&mut self, conn: ConnId, pgid: i32) -> Vec<Effect> {
        let Some(id) = self.by_conn(conn) else {
            return vec![];
        };
        let e = self.jobs.get_mut(&id).expect("by_conn");
        e.job.pgid = Some(pgid);
        // Killed between grant and start: the wrapper may have spawned anyway.
        if e.fate.is_some() {
            return vec![Effect::Signal {
                pgid,
                sig: Sig::Term,
            }];
        }
        vec![]
    }

    pub fn exited(
        &mut self,
        conn: ConnId,
        code: Option<i32>,
        signal: Option<i32>,
        now: Millis,
        wall: u64,
    ) -> Vec<Effect> {
        self.clock = self.clock.max(now);
        let Some(id) = self.by_conn(conn) else {
            return vec![];
        };
        let mut e = self.jobs.remove(&id).expect("by_conn");
        let mut fx = vec![];
        if let Some(pgid) = e.job.pgid {
            self.kills.retain(|(p, _)| *p != pgid);
        }
        let duration_ms = e.job.started_at.map_or(0, |t| now.saturating_sub(t));
        if e.job.started_at.is_some() && e.job.pgid.is_some() {
            let flagged = e.job.was_frozen || e.fate.is_some() || signal.is_some();
            let run = Run {
                peak: e.job.peak,
                duration_ms,
                exit: code,
                flagged,
                at: wall,
                workers: e.job.tuned.map(|t| t.workers).or(e
                    .job
                    .spec
                    .tune
                    .map(|t| t.default_workers)),
            };
            self.profiles
                .record(&e.job.spec.key, &e.job.spec.fallback_key, run);
            fx.push(Effect::SaveProfiles);
        }
        let (outcome, why) = match e.fate.take() {
            Some(Fate::Requeue(why)) => {
                self.push_history(
                    &e.job,
                    duration_ms,
                    code,
                    wall,
                    Outcome::Requeued,
                    Some(why),
                );
                self.requeue(e, now);
                fx.push(Effect::SaveHistory);
                fx.extend(self.settle(now));
                return fx;
            }
            Some(Fate::GiveUp(why)) => (Outcome::GaveUp, Some(why)),
            Some(Fate::ManualKill) => (Outcome::ManualKill, Some("killed by you".into())),
            None => (Outcome::Exited, None),
        };
        self.push_history(&e.job, duration_ms, code, wall, outcome, why);
        fx.push(Effect::SaveHistory);
        fx.extend(self.settle(now));
        fx
    }

    /// The client went away. A running job's processes are ended with it.
    pub fn disconnected(&mut self, conn: ConnId, now: Millis, wall: u64) -> Vec<Effect> {
        self.clock = self.clock.max(now);
        let Some(id) = self.by_conn(conn) else {
            return vec![];
        };
        let e = self.jobs.remove(&id).expect("by_conn");
        self.order.retain(|&q| q != id);
        let mut fx = vec![];
        if let Some(pgid) = e.job.pgid {
            fx.extend(self.terminate(pgid, e.job.state == JobState::Frozen, now));
        }
        let dur = e.job.started_at.map_or(0, |t| now.saturating_sub(t));
        self.push_history(&e.job, dur, None, wall, Outcome::Disconnected, None);
        fx.push(Effect::SaveHistory);
        fx.extend(self.settle(now));
        fx
    }

    pub fn control(&mut self, conn: ConnId, ctl: Control, now: Millis, wall: u64) -> Vec<Effect> {
        self.clock = self.clock.max(now);
        let mut fx = vec![];
        let r = self.apply_control(ctl, now, wall, &mut fx);
        fx.push(reply(conn, r));
        fx.extend(self.settle(now));
        fx
    }

    fn apply_control(
        &mut self,
        ctl: Control,
        now: Millis,
        wall: u64,
        fx: &mut Vec<Effect>,
    ) -> Result<(), String> {
        let missing = |id: JobId| format!("no job {id}");
        match ctl {
            Control::Hold { on } => self.held = on,
            Control::Pin { id, on } => {
                self.jobs
                    .get_mut(&id)
                    .ok_or_else(|| missing(id))?
                    .job
                    .pinned = on;
                self.reorder();
            }
            Control::Low { id, on } => {
                self.jobs
                    .get_mut(&id)
                    .ok_or_else(|| missing(id))?
                    .job
                    .spec
                    .low = on
            }
            Control::Top { id } => self.move_to(id, 0)?,
            Control::Move { id, to } => self.move_to(id, to)?,
            Control::Cancel { id } => {
                let e = self.jobs.get(&id).ok_or_else(|| missing(id))?;
                if e.job.state != JobState::Queued {
                    return Err(format!("job {id} is already running; kill it instead"));
                }
                let e = self.jobs.remove(&id).expect("checked");
                self.order.retain(|&q| q != id);
                fx.push(Effect::Send(e.conn, DaemonMsg::Cancelled));
                self.push_history(
                    &e.job,
                    0,
                    None,
                    wall,
                    Outcome::Cancelled,
                    Some("cancelled by you".into()),
                );
                fx.push(Effect::SaveHistory);
            }
            Control::Kill { id } => {
                let state = self.jobs.get(&id).ok_or_else(|| missing(id))?.job.state;
                if state == JobState::Queued {
                    return self.apply_control(Control::Cancel { id }, now, wall, fx);
                }
                let e = self.jobs.get_mut(&id).expect("checked");
                e.fate = Some(Fate::ManualKill);
                let (conn, pgid) = (e.conn, e.job.pgid);
                fx.push(Effect::Send(
                    conn,
                    DaemonMsg::Killed {
                        why: "killed by you".into(),
                        requeued: false,
                    },
                ));
                if let Some(pgid) = pgid {
                    fx.extend(self.terminate(pgid, state == JobState::Frozen, now));
                }
            }
            Control::Freeze { id } => {
                let e = self.jobs.get_mut(&id).ok_or_else(|| missing(id))?;
                if e.job.state != JobState::Running {
                    return Err(format!("job {id} is not running"));
                }
                let pgid = e
                    .job
                    .pgid
                    .ok_or_else(|| format!("job {id} has not started yet"))?;
                freeze(e, FrozenBy::User, now);
                fx.push(Effect::Signal {
                    pgid,
                    sig: Sig::Stop,
                });
                fx.push(Effect::Send(
                    e.conn,
                    DaemonMsg::Frozen {
                        by: FrozenBy::User,
                        why: None,
                    },
                ));
            }
            Control::Resume { id } => {
                let e = self.jobs.get_mut(&id).ok_or_else(|| missing(id))?;
                if e.job.state != JobState::Frozen {
                    return Err(format!("job {id} is not frozen"));
                }
                fx.extend(thaw(e));
            }
            Control::Configure { config } => {
                self.cfg = *config;
                self.reestimate(None);
                fx.push(Effect::SaveConfig);
            }
            Control::ResetProfile { key } => {
                if !self.profiles.reset(&key) {
                    return Err(format!("no profile for {key}"));
                }
                fx.push(Effect::SaveProfiles);
                self.reestimate(Some(&key));
            }
        }
        Ok(())
    }

    pub fn list(&self) -> Listing {
        let mut running: Vec<Job> = self
            .jobs
            .values()
            .filter(|e| e.job.state != JobState::Queued)
            .map(|e| e.job.clone())
            .collect();
        running.sort_by_key(|j| j.started_at);
        Listing {
            now: self.clock,
            snapshot: self.snap.clone(),
            held: self.held,
            running,
            queued: self
                .order
                .iter()
                .map(|id| self.jobs[id].job.clone())
                .collect(),
            blocked: self.blocked.clone(),
            history: self.history.iter().cloned().collect(),
        }
    }

    // ── time ──────────────────────────────────────────────────────────

    /// One sample: measure, guard, deliver owed kills, admit.
    pub fn tick(
        &mut self,
        snap: Snapshot,
        usage: &HashMap<i32, Usage>,
        now: Millis,
    ) -> Vec<Effect> {
        self.clock = self.clock.max(now);
        self.snap = snap;
        for e in self.jobs.values_mut() {
            if let Some(u) = e.job.pgid.and_then(|p| usage.get(&p)) {
                e.job.footprint = u.footprint;
                e.job.peak = e.job.peak.max(u.footprint);
                e.job.cpu_pct = u.cpu_pct;
                e.job.procs = u.procs;
            }
        }
        let mut fx = vec![];

        // Only jobs with a process group can be frozen or killed, and only
        // jobs not already on their way out.
        let watched: Vec<&Job> = self
            .jobs
            .values()
            .filter(|e| e.job.pgid.is_some() && e.fate.is_none() && e.job.state != JobState::Queued)
            .map(|e| &e.job)
            .collect();
        let actions = guard::step(&mut self.guard, now, &self.snap, &watched, &self.cfg);
        for a in actions {
            fx.extend(self.apply_guard(a, now));
        }

        let (due, later): (Vec<_>, Vec<_>) = self.kills.iter().partition(|(_, t)| *t <= now);
        self.kills = later;
        for (pgid, _) in due {
            // Only if something is left to kill: a process group id can be reused.
            if usage.get(&pgid).is_some_and(|u| u.procs > 0) {
                fx.push(Effect::Signal {
                    pgid,
                    sig: Sig::Kill,
                });
            }
        }
        fx.extend(self.settle(now));
        fx
    }

    fn apply_guard(&mut self, a: Action, now: Millis) -> Vec<Effect> {
        let id = match &a {
            Action::Freeze { id, .. } | Action::Thaw { id } | Action::Kill { id, .. } => *id,
        };
        let Some(e) = self.jobs.get_mut(&id) else {
            return vec![];
        };
        let Some(pgid) = e.job.pgid else {
            return vec![];
        };
        match a {
            Action::Freeze { why, .. } => {
                freeze(e, FrozenBy::Guard, now);
                e.job.guard_note = Some(why.clone());
                vec![
                    Effect::Signal {
                        pgid,
                        sig: Sig::Stop,
                    },
                    Effect::Send(
                        e.conn,
                        DaemonMsg::Frozen {
                            by: FrozenBy::Guard,
                            why: Some(why),
                        },
                    ),
                ]
            }
            Action::Thaw { .. } => thaw(e),
            Action::Kill { requeue, why, .. } => {
                let frozen = e.job.state == JobState::Frozen;
                e.fate = Some(if requeue {
                    Fate::Requeue(why.clone())
                } else {
                    Fate::GiveUp(why.clone())
                });
                let mut fx = vec![Effect::Send(
                    e.conn,
                    DaemonMsg::Killed {
                        why,
                        requeued: requeue,
                    },
                )];
                fx.extend(self.terminate(pgid, frozen, now));
                fx
            }
        }
    }

    // ── internals ─────────────────────────────────────────────────────

    /// SIGTERM now, SIGKILL after the grace period. A stopped process can't
    /// act on SIGTERM, so a frozen group is continued first.
    fn terminate(&mut self, pgid: i32, frozen: bool, now: Millis) -> Vec<Effect> {
        self.kills.push((pgid, now + self.cfg.kill_grace_s * 1000));
        let mut fx = vec![Effect::Signal {
            pgid,
            sig: Sig::Term,
        }];
        if frozen {
            fx.push(Effect::Signal {
                pgid,
                sig: Sig::Cont,
            });
        }
        fx
    }

    /// How a job's memory scales with workers, if it may be tuned at all.
    fn scaling_for(&self, spec: &JobSpec) -> Option<Scaling> {
        let tune = spec.tune?;
        let off = |s: &String| *s == tune.tool.name() || spec.key.starts_with(&format!("{s}|"));
        if !self.cfg.tune || self.cfg.tune_off.iter().any(off) {
            return None;
        }
        self.profiles.get(&spec.key)?.scaling()
    }

    fn median_for(&self, spec: &JobSpec) -> Option<u64> {
        self.profiles.get(&spec.key)?.median_ms()
    }

    /// After the settings or a profile changed: queued jobs' estimates.
    fn reestimate(&mut self, key: Option<&str>) {
        let ids: Vec<JobId> = self
            .jobs
            .values()
            .filter(|e| e.job.state == JobState::Queued)
            .filter(|e| key.is_none_or(|k| e.job.spec.key == k))
            .map(|e| e.job.id)
            .collect();
        for id in ids {
            let spec = self.jobs[&id].job.spec.clone();
            let est =
                self.profiles
                    .estimate(&spec.key, &spec.fallback_key, self.cfg.default_estimate);
            let scaling = self.scaling_for(&spec);
            let median = self.median_for(&spec);
            let j = &mut self.jobs.get_mut(&id).expect("queued").job;
            (j.estimate, j.samples, j.scaling) = (est.bytes, est.samples, scaling);
            j.median_ms = median;
        }
    }

    fn requeue(&mut self, mut e: Entry, now: Millis) {
        let est = self.profiles.estimate(
            &e.job.spec.key,
            &e.job.spec.fallback_key,
            self.cfg.default_estimate,
        );
        let scaling = self.scaling_for(&e.job.spec);
        let median = self.median_for(&e.job.spec);
        let j = &mut e.job;
        (j.median_ms, j.ahead_of) = (median, None);
        j.state = JobState::Queued;
        j.retry += 1;
        j.estimate = est.bytes;
        j.samples = est.samples;
        (j.scaling, j.tuned) = (scaling, None);
        (j.footprint, j.peak, j.cpu_pct, j.procs) = (0, 0, 0.0, 0);
        (j.started_at, j.frozen_at, j.frozen_by, j.pgid) = (None, None, None, None);
        j.was_frozen = false;
        // Its next wait counts from here; the first one is in its history.
        j.enqueued_at = now;
        e.told = None;
        let id = j.id;
        self.jobs.insert(id, e);
        // Front of the unpinned jobs: it already waited its turn once.
        self.order.insert(0, id);
        self.reorder();
    }

    fn push_history(
        &mut self,
        j: &Job,
        duration_ms: u64,
        exit: Option<i32>,
        wall: u64,
        outcome: Outcome,
        why: Option<String>,
    ) {
        self.history.push_front(HistoryEntry {
            id: j.id,
            label: j.spec.label.clone(),
            cmd: j.spec.cmd.clone(),
            key: j.spec.key.clone(),
            duration_ms,
            peak: j.peak,
            estimate: j.estimate,
            exit,
            ended_at: wall,
            outcome,
            why,
            tuned: j.tuned,
            ahead_of: j.ahead_of.clone(),
            waited_ms: j
                .started_at
                .unwrap_or(self.clock)
                .saturating_sub(j.enqueued_at),
        });
        let mut i = 0;
        self.history.retain(|h| {
            i += 1;
            i <= HISTORY || h.ended_at + HISTORY_DAY_S >= wall
        });
        self.history.truncate(HISTORY_MAX);
    }

    /// Pinned jobs first, otherwise keeping the current order.
    fn reorder(&mut self) {
        let jobs = &self.jobs;
        self.order.sort_by_key(|id| !jobs[id].job.pinned);
    }

    fn move_to(&mut self, id: JobId, to: usize) -> Result<(), String> {
        let from = self
            .order
            .iter()
            .position(|&q| q == id)
            .ok_or_else(|| format!("job {id} is not queued"))?;
        let pinned = self
            .order
            .iter()
            .filter(|q| self.jobs[q].job.pinned)
            .count();
        self.order.remove(from);
        // A job stays within its band: pinned jobs above, the rest below.
        let to = if self.jobs[&id].job.pinned {
            to.min(pinned - 1)
        } else {
            to.clamp(pinned, self.order.len())
        };
        self.order.insert(to.min(self.order.len()), id);
        Ok(())
    }

    /// Admit what fits, then tell queued clients where they stand.
    fn settle(&mut self, now: Millis) -> Vec<Effect> {
        let mut fx = vec![];
        let running: Vec<&Job> = self
            .jobs
            .values()
            .filter(|e| e.job.state != JobState::Queued)
            .map(|e| &e.job)
            .collect();
        let queue: Vec<&Job> = self.order.iter().map(|id| &self.jobs[id].job).collect();
        let d = admission::decide(&self.snap, &running, &queue, self.held, &self.cfg, now);
        self.blocked = d.blocked;
        if let Some(h) = d.skipped {
            let head = &mut self.jobs.get_mut(&h).expect("queued").job;
            head.skipped += 1;
            head.skipped_since.get_or_insert(now);
            let name = format!("{} · {}", head.spec.label, head.spec.cmd);
            for id in &d.backfilled {
                self.jobs.get_mut(id).expect("queued").job.ahead_of = Some(name.clone());
            }
        }
        for id in d.admit {
            self.order.retain(|&q| q != id);
            let swap = self.snap.swap_used;
            let e = self.jobs.get_mut(&id).expect("queued");
            e.job.state = JobState::Running;
            e.job.started_at = Some(now);
            e.job.swap_at_start = swap;
            if let Some(&(_, workers, est)) = d.tuned.iter().find(|t| t.0 == id) {
                let default = e.job.spec.tune.map_or(workers, |t| t.default_workers);
                e.job.tuned = Some(Tuned { workers, default });
                e.job.estimate = est;
            }
            fx.push(Effect::Send(
                e.conn,
                DaemonMsg::Granted {
                    id,
                    retry: e.job.retry,
                    low: e.job.spec.low,
                    workers: e.job.tuned.map(|t| t.workers),
                },
            ));
        }
        let reason = self.blocked.as_ref().map(Blocked::describe);
        let of = self.order.len();
        for (pos, id) in self.order.iter().enumerate() {
            let e = self.jobs.get_mut(id).expect("queued");
            let now_told = (pos + 1, of, reason.clone());
            if e.told.as_ref() != Some(&now_told) {
                e.told = Some(now_told);
                fx.push(Effect::Send(
                    e.conn,
                    DaemonMsg::Queued {
                        id: *id,
                        pos: pos + 1,
                        of,
                        estimate: e.job.estimate,
                        samples: e.job.samples,
                        reason: reason.clone(),
                    },
                ));
            }
        }
        fx
    }
}

fn freeze(e: &mut Entry, by: FrozenBy, now: Millis) {
    e.job.state = JobState::Frozen;
    e.job.frozen_by = Some(by);
    e.job.frozen_at = Some(now);
    e.job.was_frozen = true;
}

fn thaw(e: &mut Entry) -> Vec<Effect> {
    e.job.guard_note = None;
    e.job.state = JobState::Running;
    e.job.frozen_by = None;
    e.job.frozen_at = None;
    let mut fx = vec![Effect::Send(e.conn, DaemonMsg::Thawed)];
    if let Some(pgid) = e.job.pgid {
        fx.insert(
            0,
            Effect::Signal {
                pgid,
                sig: Sig::Cont,
            },
        );
    }
    fx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::KillMode;
    use crate::model::{GIB, Pressure};
    use crate::tweaks::{Tool, Tunable};

    fn snap(avail_gb: u64) -> Snapshot {
        Snapshot {
            pressure: Pressure::Normal,
            mem_total: 36 * GIB,
            mem_used: (36 - avail_gb) * GIB,
            mem_available: avail_gb * GIB,
            swap_used: 0,
            load1: 1.0,
            cores: 10,
            cpu_pct: 10.0,
        }
    }

    fn spec(cmd: &str) -> JobSpec {
        JobSpec {
            cmd: cmd.into(),
            full_cmd: cmd.into(),
            key: format!("/r|{cmd}"),
            fallback_key: "/r|x".into(),
            label: "repo".into(),
            ..JobSpec::default()
        }
    }

    /// A scheduler with `avail_gb` free and a 3 GB default estimate.
    fn sched(avail_gb: u64) -> Scheduler {
        let cfg = Config {
            default_estimate: 3 * GIB,
            ..Config::default()
        };
        let mut s = Scheduler::new(cfg, Profiles::default(), VecDeque::new());
        s.tick(snap(avail_gb), &HashMap::new(), 0);
        s
    }

    fn sent(fx: &[Effect], conn: ConnId) -> Vec<&DaemonMsg> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::Send(c, m) if *c == conn => Some(m),
                _ => None,
            })
            .collect()
    }

    fn granted(fx: &[Effect], conn: ConnId) -> bool {
        sent(fx, conn)
            .iter()
            .any(|m| matches!(m, DaemonMsg::Granted { .. }))
    }

    fn signals(fx: &[Effect]) -> Vec<(i32, Sig)> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::Signal { pgid, sig } => Some((*pgid, *sig)),
                _ => None,
            })
            .collect()
    }

    fn usage(pgid: i32, gb: f64) -> HashMap<i32, Usage> {
        HashMap::from([(
            pgid,
            Usage {
                footprint: (gb * GIB as f64) as Bytes,
                cpu_pct: 100.0,
                procs: 3,
            },
        )])
    }

    #[test]
    fn a_job_that_fits_is_granted_at_once() {
        let mut s = sched(20);
        let fx = s.enqueue(1, spec("pnpm test"), 0);
        assert!(granted(&fx, 1));
        assert_eq!(s.list().running.len(), 1);
    }

    #[test]
    fn a_job_that_does_not_fit_waits_with_a_reason_and_starts_when_room_appears() {
        let mut s = sched(9); // 7 usable: one 3 GB job fits, its growth reserves 3
        assert!(granted(&s.enqueue(1, spec("a"), 0), 1));
        s.started(1, 100);
        assert!(granted(&s.enqueue(2, spec("b"), 0), 2));
        s.started(2, 200);
        let fx = s.enqueue(3, spec("c"), 0);
        assert!(!granted(&fx, 3));
        assert_eq!(
            sent(&fx, 3),
            vec![&DaemonMsg::Queued {
                id: 3,
                pos: 1,
                of: 1,
                estimate: 3 * GIB,
                samples: 0,
                reason: Some("next needs 3 GB, 1 GB free".into())
            }]
        );
        let fx = s.exited(1, Some(0), None, 5_000, 0);
        assert!(granted(&fx, 3));
    }

    /// `vitest run`, tunable from 9 workers, with three clean 9 GB runs at 9.
    fn tunable_sched(avail_gb: u64) -> (Scheduler, JobSpec) {
        let mut s = sched(avail_gb);
        let mut sp = spec("vitest run");
        // Its own fallback, so the blocker's estimate stays the default.
        sp.fallback_key = "/r|vitest".into();
        sp.tune = Some(Tunable {
            tool: Tool::Vitest,
            default_workers: 9,
            npm: false,
        });
        for _ in 0..3 {
            let run = Run {
                peak: 9 * GIB,
                duration_ms: 1,
                exit: Some(0),
                flagged: false,
                at: 0,
                workers: Some(9),
            };
            s.profiles.record(&sp.key, &sp.fallback_key, run);
        }
        // A 3 GB job running, so the queue isn't idle.
        s.enqueue(9, spec("blocker"), 0);
        s.started(9, 900);
        (s, sp)
    }

    #[test]
    fn a_job_that_does_not_fit_is_granted_with_fewer_workers_and_remembers_it() {
        // 12 − 2 headroom − 3 still to grow = 7 GB: 2.25 + 0.75 × 6 = 6.75 fits.
        let (mut s, sp) = tunable_sched(12);
        let fx = s.enqueue(1, sp.clone(), 0);
        assert!(sent(&fx, 1).contains(&&DaemonMsg::Granted {
            id: 2,
            retry: 0,
            low: false,
            workers: Some(6),
        }));
        let job = &s
            .list()
            .running
            .iter()
            .find(|j| j.id == 2)
            .cloned()
            .unwrap();
        assert_eq!(
            job.tuned,
            Some(Tuned {
                workers: 6,
                default: 9
            })
        );
        assert_eq!(job.estimate, 27 * GIB / 4);
        s.started(1, 100);
        s.tick(snap(12), &usage(100, 5.0), 1_000);
        s.exited(1, Some(0), None, 2_000, 7);
        assert_eq!(s.profiles.get(&sp.key).unwrap().runs[3].workers, Some(6));
        assert_eq!(
            s.history[0].tuned,
            Some(Tuned {
                workers: 6,
                default: 9
            })
        );
    }

    #[test]
    fn an_untuned_run_records_the_tools_default_workers() {
        let (mut s, sp) = tunable_sched(30);
        let fx = s.enqueue(1, sp.clone(), 0);
        assert!(sent(&fx, 1).contains(&&DaemonMsg::Granted {
            id: 2,
            retry: 0,
            low: false,
            workers: None,
        }));
        s.started(1, 100);
        s.exited(1, Some(0), None, 1_000, 7);
        assert_eq!(s.profiles.get(&sp.key).unwrap().runs[3].workers, Some(9));
        assert_eq!(s.history[0].tuned, None);
    }

    #[test]
    fn turning_tuning_off_globally_per_tool_or_per_repo_keeps_it_waiting() {
        for off in [
            Config {
                tune: false,
                ..s_cfg()
            },
            Config {
                tune_off: vec!["vitest".into()],
                ..s_cfg()
            },
            Config {
                tune_off: vec!["/r".into()],
                ..s_cfg()
            },
        ] {
            let (mut s, sp) = tunable_sched(12);
            s.control(
                5,
                Control::Configure {
                    config: Box::new(off.clone()),
                },
                0,
                0,
            );
            let fx = s.enqueue(1, sp, 0);
            assert!(!granted(&fx, 1), "{off:?}");
        }
        // And turning it back on re-plans the job already waiting.
        let (mut s, sp) = tunable_sched(12);
        let off = Config {
            tune: false,
            ..s_cfg()
        };
        s.control(
            5,
            Control::Configure {
                config: Box::new(off),
            },
            0,
            0,
        );
        assert!(!granted(&s.enqueue(1, sp, 0), 1));
        let fx = s.control(
            5,
            Control::Configure {
                config: Box::new(s_cfg()),
            },
            0,
            0,
        );
        assert!(fx.contains(&Effect::SaveConfig));
        let later = s.tick(snap(12), &HashMap::new(), 1_000);
        assert!(granted(&fx, 1) || granted(&later, 1));
    }

    /// Three clean runs of `cmd`, each `gb` GB for `secs` seconds.
    fn learned(s: &mut Scheduler, cmd: &str, gb: u64, secs: u64) {
        let sp = spec(cmd);
        for _ in 0..3 {
            let run = Run {
                peak: gb * GIB,
                duration_ms: secs * 1000,
                exit: Some(0),
                flagged: false,
                at: 0,
                workers: None,
            };
            s.profiles.record(&sp.key, &format!("/r|own-{cmd}"), run);
        }
    }

    fn own(cmd: &str) -> JobSpec {
        JobSpec {
            fallback_key: format!("/r|own-{cmd}"),
            ..spec(cmd)
        }
    }

    #[test]
    fn a_short_job_starts_ahead_of_a_blocked_head_and_history_says_so() {
        // 8 GB available, 2 kept free. A 6 GB build that takes 60 s runs;
        // the 7 GB e2e run must wait for it; a 1 GB lint taking 10 s need not.
        let mut s = sched(8);
        learned(&mut s, "build", 6, 60);
        learned(&mut s, "e2e", 7, 300);
        learned(&mut s, "lint", 1, 10);
        s.enqueue(1, own("build"), 0);
        s.started(1, 100);
        s.tick(snap(8), &usage(100, 6.0), 1_000);
        s.enqueue(2, own("e2e"), 1_000);
        let fx = s.enqueue(3, own("lint"), 1_000);
        assert!(granted(&fx, 3) && !granted(&fx, 2));
        let head = s.list().queued[0].clone();
        assert_eq!(
            (head.id, head.skipped, head.skipped_since),
            (2, 1, Some(1_000))
        );
        s.started(3, 300);
        s.exited(3, Some(0), None, 5_000, 9);
        assert_eq!(s.history[0].ahead_of.as_deref(), Some("repo · e2e"));
    }

    fn s_cfg() -> Config {
        Config {
            default_estimate: 3 * GIB,
            ..Config::default()
        }
    }

    #[test]
    fn exit_records_a_clean_run_and_history() {
        let mut s = sched(20);
        s.enqueue(1, spec("pnpm test"), 0);
        s.started(1, 100);
        s.tick(snap(20), &usage(100, 2.5), 1_000);
        s.tick(snap(20), &usage(100, 1.0), 2_000);
        let fx = s.exited(1, Some(1), None, 3_000, 42);
        assert!(fx.contains(&Effect::SaveProfiles) && fx.contains(&Effect::SaveHistory));
        let p = s.profiles.get("/r|pnpm test").unwrap();
        assert_eq!(
            p.runs[0],
            Run {
                peak: 5 * GIB / 2,
                duration_ms: 3_000,
                exit: Some(1),
                flagged: false,
                at: 42,
                workers: None,
            }
        );
        assert_eq!(s.history[0].outcome, Outcome::Exited);
        assert_eq!(s.history[0].exit, Some(1));
    }

    #[test]
    fn history_records_how_long_each_job_waited() {
        let mut s = sched(9);
        s.enqueue(1, spec("a"), 0);
        s.started(1, 100);
        s.enqueue(2, spec("b"), 0);
        s.started(2, 200);
        s.enqueue(3, spec("c"), 1_000);
        s.enqueue(4, spec("d"), 2_000);
        s.exited(1, Some(0), None, 5_000, 0);
        s.started(3, 300);
        assert_eq!(s.history[0].waited_ms, 0, "a started at once");
        // Cancelled while queued: the wait up to then.
        s.control(9, Control::Cancel { id: 4 }, 5_500, 0);
        assert_eq!(s.history[0].outcome, Outcome::Cancelled);
        assert_eq!(s.history[0].waited_ms, 3_500);
        s.exited(3, Some(0), None, 6_000, 0);
        assert_eq!(
            s.history[0].waited_ms, 4_000,
            "c: queued at 1 s, granted at 5 s"
        );
    }

    #[test]
    fn a_job_that_never_started_records_no_run() {
        let mut s = sched(20);
        s.enqueue(1, spec("a"), 0);
        let fx = s.exited(1, Some(127), None, 10, 0);
        assert!(!fx.contains(&Effect::SaveProfiles));
        assert!(s.profiles.get("/r|a").is_none());
    }

    #[test]
    fn disconnect_of_a_running_job_terminates_its_group_then_kills_it() {
        let mut s = sched(20);
        s.enqueue(1, spec("a"), 0);
        s.started(1, 100);
        let fx = s.disconnected(1, 1_000, 0);
        assert_eq!(signals(&fx), vec![(100, Sig::Term)]);
        assert_eq!(s.history[0].outcome, Outcome::Disconnected);
        // Still alive after the grace period: SIGKILL.
        let fx = s.tick(snap(20), &usage(100, 0.1), 11_000);
        assert_eq!(signals(&fx), vec![(100, Sig::Kill)]);
        // Owed only once.
        assert!(signals(&s.tick(snap(20), &usage(100, 0.1), 12_000)).is_empty());
    }

    #[test]
    fn a_group_that_is_gone_is_not_killed() {
        let mut s = sched(20);
        s.enqueue(1, spec("a"), 0);
        s.started(1, 100);
        s.disconnected(1, 0, 0);
        assert!(signals(&s.tick(snap(20), &HashMap::new(), 11_000)).is_empty());
    }

    #[test]
    fn disconnect_of_a_queued_job_just_frees_its_place() {
        let mut s = sched(4);
        s.enqueue(1, spec("a"), 0); // idle queue: admitted
        s.enqueue(2, spec("b"), 0);
        s.enqueue(3, spec("c"), 0);
        let fx = s.disconnected(2, 0, 0);
        assert!(signals(&fx).is_empty());
        assert_eq!(sent(&fx, 3).len(), 1, "job 3 moves up to #1");
        assert_eq!(s.list().queued.len(), 1);
    }

    #[test]
    fn unknown_connections_are_ignored() {
        let mut s = sched(20);
        assert!(s.exited(9, Some(0), None, 0, 0).is_empty());
        assert!(s.disconnected(9, 0, 0).is_empty());
        assert!(s.started(9, 1).is_empty());
    }

    #[test]
    fn a_second_enqueue_on_one_connection_is_refused() {
        let mut s = sched(20);
        s.enqueue(1, spec("a"), 0);
        let fx = s.enqueue(1, spec("b"), 0);
        assert!(matches!(sent(&fx, 1)[0], DaemonMsg::Error { .. }));
        assert_eq!(s.list().running.len(), 1);
    }

    #[test]
    fn positions_are_only_resent_when_they_change() {
        let mut s = sched(4);
        s.enqueue(1, spec("a"), 0);
        s.started(1, 100);
        s.enqueue(2, spec("b"), 0);
        let fx = s.tick(snap(4), &usage(100, 1.0), 1_000);
        assert!(sent(&fx, 2).is_empty());
    }

    #[test]
    fn guard_freezes_then_kills_and_requeues_on_the_same_connection() {
        let mut s = sched(20);
        s.enqueue(1, spec("a"), 0);
        s.started(1, 100);
        let critical = Snapshot {
            pressure: Pressure::Critical,
            ..snap(1)
        };
        let mut all = vec![];
        for t in 0..=40 {
            all.extend(s.tick(critical.clone(), &usage(100, 7.0), t * 1000));
        }
        assert!(signals(&all).contains(&(100, Sig::Stop)));
        assert!(signals(&all).contains(&(100, Sig::Term)));
        assert!(
            sent(&all, 1)
                .iter()
                .any(|m| matches!(m, DaemonMsg::Killed { requeued: true, .. }))
        );

        // The wrapper reports its child's death; the job goes back in line.
        let fx = s.exited(1, None, Some(15), 41_000, 0);
        let p = s.profiles.get("/r|a").unwrap();
        assert!(p.runs[0].flagged, "a killed run is flagged");
        assert_eq!(s.history[0].outcome, Outcome::Requeued);
        let q = &s.list().queued;
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].retry, 1);
        assert_eq!(q[0].estimate, 7 * GIB, "the kill raised the estimate");
        assert!(
            sent(&fx, 1)
                .iter()
                .any(|m| matches!(m, DaemonMsg::Queued { .. }))
        );

        // Room again: granted on the same connection, retry 1.
        let fx = s.tick(snap(20), &HashMap::new(), 50_000);
        assert!(sent(&fx, 1).contains(&&DaemonMsg::Granted {
            id: 1,
            retry: 1,
            low: false,
            workers: None,
        }));
        // The second run's wait counts from the requeue, not the first enqueue.
        s.started(1, 100);
        s.exited(1, Some(0), None, 60_000, 0);
        assert_eq!((s.history[0].waited_ms, s.history[1].waited_ms), (9_000, 0));
    }

    #[test]
    fn a_second_guard_kill_gives_up() {
        let mut s = sched(20);
        s.enqueue(1, spec("a"), 0);
        s.started(1, 100);
        let critical = Snapshot {
            pressure: Pressure::Critical,
            ..snap(1)
        };
        for round in 0..2 {
            let base = round * 100_000;
            for t in 0..=40 {
                s.tick(critical.clone(), &usage(100, 7.0), base + t * 1000);
            }
            s.exited(1, None, Some(15), base + 41_000, 0);
            if round == 0 {
                s.tick(snap(20), &HashMap::new(), base + 50_000);
                s.started(1, 100);
            }
        }
        assert_eq!(s.history[0].outcome, Outcome::GaveUp);
        assert!(s.list().queued.is_empty() && s.list().running.is_empty());
    }

    #[test]
    fn guard_off_never_signals() {
        let mut s = sched(20);
        s.cfg.kill_mode = KillMode::Off;
        s.enqueue(1, spec("a"), 0);
        s.started(1, 100);
        let critical = Snapshot {
            pressure: Pressure::Critical,
            ..snap(1)
        };
        let all: Vec<Effect> = (0..60)
            .flat_map(|t| s.tick(critical.clone(), &usage(100, 30.0), t * 1000))
            .collect();
        assert!(signals(&all).is_empty());
    }

    #[test]
    fn a_job_granted_but_not_started_is_left_to_its_wrapper() {
        let mut s = sched(20);
        s.enqueue(1, spec("a"), 0);
        let critical = Snapshot {
            pressure: Pressure::Critical,
            ..snap(1)
        };
        let all: Vec<Effect> = (0..60)
            .flat_map(|t| s.tick(critical.clone(), &HashMap::new(), t * 1000))
            .collect();
        assert!(signals(&all).is_empty());
    }

    #[test]
    fn manual_kill_is_not_requeued_and_says_so() {
        let mut s = sched(20);
        s.enqueue(1, spec("a"), 0);
        s.started(1, 100);
        let fx = s.control(9, Control::Kill { id: 1 }, 0, 0);
        assert_eq!(signals(&fx), vec![(100, Sig::Term)]);
        assert!(sent(&fx, 1).contains(&&DaemonMsg::Killed {
            why: "killed by you".into(),
            requeued: false
        }));
        assert!(sent(&fx, 9).contains(&&DaemonMsg::Ok));
        s.exited(1, None, Some(15), 1_000, 0);
        assert_eq!(s.history[0].outcome, Outcome::ManualKill);
        assert!(s.list().queued.is_empty());
    }

    #[test]
    fn killing_a_frozen_job_continues_it_so_it_can_exit() {
        let mut s = sched(20);
        s.enqueue(1, spec("a"), 0);
        s.started(1, 100);
        s.control(9, Control::Freeze { id: 1 }, 0, 0);
        let fx = s.control(9, Control::Kill { id: 1 }, 0, 0);
        assert_eq!(signals(&fx), vec![(100, Sig::Term), (100, Sig::Cont)]);
    }

    #[test]
    fn killing_a_job_between_grant_and_start_terminates_it_on_start() {
        let mut s = sched(20);
        s.enqueue(1, spec("a"), 0);
        s.control(9, Control::Kill { id: 1 }, 0, 0);
        assert_eq!(
            s.started(1, 100),
            vec![Effect::Signal {
                pgid: 100,
                sig: Sig::Term
            }]
        );
    }

    #[test]
    fn cancel_removes_a_queued_job_and_tells_its_client() {
        let mut s = sched(4);
        s.enqueue(1, spec("a"), 0);
        s.enqueue(2, spec("b"), 0);
        let fx = s.control(9, Control::Cancel { id: 2 }, 0, 0);
        assert!(sent(&fx, 2).contains(&&DaemonMsg::Cancelled));
        assert_eq!(s.history[0].outcome, Outcome::Cancelled);
        // A running job can't be cancelled, and a second cancel finds nothing.
        assert!(matches!(
            sent(&s.control(9, Control::Cancel { id: 1 }, 0, 0), 9)[0],
            DaemonMsg::Error { .. }
        ));
        assert!(matches!(
            sent(&s.control(9, Control::Cancel { id: 2 }, 0, 0), 9)[0],
            DaemonMsg::Error { .. }
        ));
    }

    #[test]
    fn kill_on_a_queued_job_cancels_it() {
        let mut s = sched(4);
        s.enqueue(1, spec("a"), 0);
        s.enqueue(2, spec("b"), 0);
        let fx = s.control(9, Control::Kill { id: 2 }, 0, 0);
        assert!(sent(&fx, 2).contains(&&DaemonMsg::Cancelled));
    }

    #[test]
    fn user_freeze_and_resume() {
        let mut s = sched(20);
        s.enqueue(1, spec("a"), 0);
        // Not started yet: nothing to stop.
        assert!(matches!(
            sent(&s.control(9, Control::Freeze { id: 1 }, 0, 0), 9)[0],
            DaemonMsg::Error { .. }
        ));
        s.started(1, 100);
        let fx = s.control(9, Control::Freeze { id: 1 }, 0, 0);
        assert_eq!(signals(&fx), vec![(100, Sig::Stop)]);
        assert_eq!(s.list().running[0].frozen_by, Some(FrozenBy::User));
        // Freezing twice is refused; resume continues it.
        assert!(matches!(
            sent(&s.control(9, Control::Freeze { id: 1 }, 0, 0), 9)[0],
            DaemonMsg::Error { .. }
        ));
        let fx = s.control(9, Control::Resume { id: 1 }, 0, 0);
        assert_eq!(signals(&fx), vec![(100, Sig::Cont)]);
        assert!(sent(&fx, 1).contains(&&DaemonMsg::Thawed));
        assert!(matches!(
            sent(&s.control(9, Control::Resume { id: 1 }, 0, 0), 9)[0],
            DaemonMsg::Error { .. }
        ));
    }

    #[test]
    fn pin_moves_a_job_ahead_and_lets_it_skip_the_memory_check() {
        let mut s = sched(4);
        s.enqueue(1, spec("a"), 0);
        s.started(1, 100);
        s.enqueue(2, spec("b"), 0);
        s.enqueue(3, spec("c"), 0);
        let fx = s.control(9, Control::Pin { id: 3, on: true }, 0, 0);
        assert!(granted(&fx, 3), "pinned skips the memory check");
        assert_eq!(
            s.list().queued.iter().map(|j| j.id).collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn move_keeps_jobs_inside_their_band() {
        let mut s = sched(4);
        s.held = true;
        for c in 1..=4 {
            s.enqueue(c, spec("x"), 0);
        }
        s.control(9, Control::Pin { id: 4, on: true }, 0, 0);
        let order = |s: &Scheduler| s.list().queued.iter().map(|j| j.id).collect::<Vec<_>>();
        assert_eq!(order(&s), vec![4, 1, 2, 3]);
        s.control(9, Control::Top { id: 3 }, 0, 0);
        assert_eq!(
            order(&s),
            vec![4, 3, 1, 2],
            "top stops below the pinned job"
        );
        s.control(9, Control::Move { id: 4, to: 3 }, 0, 0);
        assert_eq!(
            order(&s),
            vec![4, 3, 1, 2],
            "a pinned job can't sink below the unpinned"
        );
        s.control(9, Control::Move { id: 3, to: 99 }, 0, 0);
        assert_eq!(order(&s), vec![4, 1, 2, 3]);
        assert!(matches!(
            sent(&s.control(9, Control::Move { id: 77, to: 0 }, 0, 0), 9)[0],
            DaemonMsg::Error { .. }
        ));
    }

    #[test]
    fn hold_stops_admissions_until_released() {
        let mut s = sched(20);
        s.control(9, Control::Hold { on: true }, 0, 0);
        let fx = s.enqueue(1, spec("a"), 0);
        assert!(!granted(&fx, 1));
        assert!(
            matches!(sent(&fx, 1)[0], DaemonMsg::Queued { reason: Some(r), .. } if r == "held by you")
        );
        let fx = s.control(9, Control::Hold { on: false }, 0, 0);
        assert!(granted(&fx, 1));
    }

    #[test]
    fn low_marks_the_grant() {
        let mut s = sched(20);
        s.held = true;
        s.enqueue(1, spec("a"), 0);
        s.control(9, Control::Low { id: 1, on: true }, 0, 0);
        let fx = s.control(9, Control::Hold { on: false }, 0, 0);
        assert!(sent(&fx, 1).contains(&&DaemonMsg::Granted {
            id: 1,
            retry: 0,
            low: true,
            workers: None,
        }));
    }

    #[test]
    fn reset_profile_reestimates_queued_jobs() {
        let mut s = sched(4);
        s.profiles.record(
            "/r|b",
            "/r|x",
            Run {
                peak: 9 * GIB,
                duration_ms: 1,
                exit: Some(0),
                flagged: false,
                at: 0,
                workers: None,
            },
        );
        s.enqueue(1, spec("a"), 0);
        s.enqueue(2, spec("b"), 0);
        assert!(s.list().queued[0].estimate > 3 * GIB);
        let fx = s.control(9, Control::ResetProfile { key: "/r|b".into() }, 0, 0);
        assert!(fx.contains(&Effect::SaveProfiles));
        assert_eq!(s.list().queued[0].estimate, 3 * GIB);
        assert!(matches!(
            sent(
                &s.control(9, Control::ResetProfile { key: "/r|b".into() }, 0, 0),
                9
            )[0],
            DaemonMsg::Error { .. }
        ));
    }

    #[test]
    fn configure_replaces_settings_and_saves_them() {
        let mut s = sched(20);
        let cfg = Config {
            headroom: 5 * GIB,
            ..s.cfg.clone()
        };
        let fx = s.control(
            9,
            Control::Configure {
                config: Box::new(cfg.clone()),
            },
            0,
            0,
        );
        assert!(fx.contains(&Effect::SaveConfig));
        assert_eq!(s.cfg, cfg);
    }

    #[test]
    fn a_guard_freeze_explains_itself_until_thawed() {
        let mut s = sched(20);
        s.enqueue(1, spec("a"), 0);
        s.started(1, 100);
        let critical = Snapshot {
            pressure: Pressure::Critical,
            ..snap(1)
        };
        for t in 0..=10 {
            s.tick(critical.clone(), &usage(100, 1.0), t * 1000);
        }
        let note = s.list().running[0].guard_note.clone();
        assert!(note.is_some_and(|n| n.contains("critical")));
        for t in 11..=40 {
            s.tick(snap(20), &usage(100, 1.0), t * 1000);
        }
        assert_eq!(s.list().running[0].guard_note, None);
    }

    fn ended_at(s: &mut Scheduler, conn: ConnId, wall: u64) {
        s.enqueue(conn, spec("a"), 0);
        s.exited(conn, Some(0), None, 0, wall);
    }

    #[test]
    fn history_keeps_the_last_runs_and_the_last_day_newest_first() {
        let mut s = sched(20);
        // A busy day: more than HISTORY runs, all of them kept.
        for c in 0..(HISTORY as u64 + 5) {
            ended_at(&mut s, c, 1_000 + c);
        }
        assert_eq!(s.history.len(), HISTORY + 5);
        assert_eq!(s.history[0].ended_at, 1_000 + HISTORY as u64 + 4);
        // A day after 1 005: beyond the last HISTORY, only the run exactly a
        // day old stays.
        let next = 1_005 + HISTORY_DAY_S;
        ended_at(&mut s, 900, next);
        assert_eq!(s.history.len(), HISTORY + 1);
        assert_eq!(s.history.back().unwrap().ended_at, 1_005);
        // Four seconds on, everything past the last HISTORY is over a day old.
        ended_at(&mut s, 901, next + 4);
        assert_eq!(s.history.len(), HISTORY);
        assert_eq!(s.history.back().unwrap().ended_at, 1_007);
    }

    #[test]
    fn history_never_grows_past_its_cap() {
        let mut s = sched(20);
        for c in 0..(HISTORY_MAX as u64 + 3) {
            ended_at(&mut s, c, 5_000);
        }
        assert_eq!(s.history.len(), HISTORY_MAX);
        // The same run recorded again is a new entry, not a replacement.
        ended_at(&mut s, 1, 5_000);
        assert_eq!(s.history.len(), HISTORY_MAX);
    }

    #[test]
    fn sampler_is_told_every_group_it_must_watch() {
        let mut s = sched(20);
        s.enqueue(1, spec("a"), 0);
        s.started(1, 100);
        s.enqueue(2, spec("b"), 0);
        s.started(2, 200);
        s.disconnected(2, 0, 0);
        // 200 is gone from the jobs but still owed a SIGKILL check.
        assert_eq!(s.pgids(), vec![100, 200]);
    }
}
