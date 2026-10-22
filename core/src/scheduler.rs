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
    Bytes, FrozenBy, HistoryEntry, Job, JobId, JobSpec, JobState, Millis, Outcome, Part, Pressure,
    Snapshot, Tuned, merge_parts,
};
use crate::profile::{Estimate, Profiles, Run, Scaling};
use crate::protocol::{Control, DaemonMsg, Listing};
use crate::tally::Tally;

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
    Int,
    Hup,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Send(ConnId, DaemonMsg),
    Signal { pgid: i32, sig: Sig },
    ContinueWrapper { pid: i32 },
    SaveProfiles,
    SaveHistory,
    SaveConfig,
}

/// One job's measured usage, from the sampler: its process group and
/// every descendant (see `proctree`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub footprint: Bytes,
    pub cpu_pct: f64,
    pub procs: u32,
    /// The largest process inside it, when that isn't the one it started:
    /// what a `git commit` or a `turbo run` is busy with.
    pub now: Option<String>,
    /// The process groups its processes are in, for signals.
    pub groups: Vec<i32>,
    /// The commands running inside it now, other than the first process.
    pub parts: Vec<Part>,
}

/// A decision about how a running job ends, made before it has ended.
#[derive(Clone, Debug, PartialEq)]
enum Fate {
    Requeue(String),
    GiveUp(String),
    ManualKill,
    Signalled,
}

#[derive(Clone, Debug)]
struct Entry {
    job: Job,
    conn: ConnId,
    fate: Option<Fate>,
    /// Last position report sent, so clients only hear about changes.
    told: Option<(usize, usize, Option<String>)>,
    terminal_stopped: bool,
    terminal_wake: bool,
}

pub struct Scheduler {
    pub cfg: Config,
    pub profiles: Profiles,
    pub history: VecDeque<HistoryEntry>,
    /// Per-command totals; saved with the history.
    pub tally: Tally,
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
            tally: Tally::default(),
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

    /// Daemon shutdown releases its own pauses, preserving shell job control.
    pub fn shutdown_thaws(&self) -> Vec<i32> {
        self.jobs
            .values()
            .filter(|e| e.job.state == JobState::Frozen && !e.terminal_stopped)
            .filter_map(|e| e.job.pgid)
            .collect()
    }

    /// Terminations already owed when the daemon stops accepting work.
    pub fn shutdown_kills(&self) -> Vec<(i32, Millis)> {
        self.kills.clone()
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
        let est = self.estimate_for(&spec);
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
                terminal_stopped: false,
                terminal_wake: false,
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
            return self.terminate(pgid, false, self.clock);
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
        // The wrapper reports its first child's exit, not the death of all
        // its descendants. Keep any pending kill until its grace expires.
        let duration_ms = e.job.started_at.map_or(0, |t| now.saturating_sub(t));
        let flagged = e.job.was_frozen || e.fate.is_some() || signal.is_some();
        if self.record_run(&e.job, duration_ms, code, flagged, wall) {
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
            Some(Fate::Signalled) => (Outcome::Exited, None),
            None => (Outcome::Exited, None),
        };
        self.push_history(&e.job, duration_ms, code, wall, outcome, why);
        fx.push(Effect::SaveHistory);
        fx.extend(self.settle(now));
        fx
    }

    /// Forward wrapper cancellation while retaining descendant cleanup after exit.
    pub fn signalled(&mut self, conn: ConnId, sig: Sig, now: Millis) -> Vec<Effect> {
        self.clock = self.clock.max(now);
        if !matches!(sig, Sig::Int | Sig::Term | Sig::Hup) {
            return vec![];
        }
        let Some(e) = self.by_conn(conn).and_then(|id| self.jobs.get_mut(&id)) else {
            return vec![];
        };
        // Cancellation overrides a guard requeue, but preserves the child's
        // actual exit status and the usual history outcome.
        e.fate = Some(Fate::Signalled);
        let Some(pgid) = e.job.pgid else {
            return vec![];
        };
        let frozen = e.job.state == JobState::Frozen;
        let mut fx = self.terminate(pgid, frozen, now);
        fx[0] = Effect::Signal { pgid, sig };
        fx
    }

    /// A child killed by a terminal signal still needs descendant cleanup,
    /// while a guard/manual kill must retain its already decided outcome.
    pub fn child_signalled(&mut self, conn: ConnId, sig: Sig, now: Millis) -> Vec<Effect> {
        if self
            .by_conn(conn)
            .and_then(|id| self.jobs.get(&id))
            .is_none_or(|e| e.fate.is_some())
        {
            return vec![];
        }
        self.signalled(conn, sig, now)
    }

    /// The wrapper owns terminal ordering; the daemon stops/resumes its whole tree.
    pub fn terminal_stopped(&mut self, conn: ConnId, stopped: bool, now: Millis) -> Vec<Effect> {
        self.clock = self.clock.max(now);
        let Some(e) = self.by_conn(conn).and_then(|id| self.jobs.get_mut(&id)) else {
            return vec![];
        };
        e.terminal_stopped = stopped;
        if !stopped {
            e.terminal_wake = false;
        }
        if stopped && e.fate.is_some() {
            // Termination can win just before the wrapper reports its stop.
            // Keep waking it until it acknowledges continuation and can reap.
            e.terminal_wake = true;
            let mut fx = vec![];
            if let Some(pgid) = e.job.pgid {
                fx.push(Effect::Signal {
                    pgid,
                    sig: Sig::Cont,
                });
            }
            if let Some(pid) = e.job.spec.pid {
                fx.push(Effect::ContinueWrapper { pid });
            }
            return fx;
        }
        if stopped && e.job.state != JobState::Queued {
            freeze(e, FrozenBy::User, now);
            return e
                .job
                .pgid
                .map(|pgid| {
                    vec![Effect::Signal {
                        pgid,
                        sig: Sig::Stop,
                    }]
                })
                .unwrap_or_default();
        } else if !stopped
            && e.job.state == JobState::Frozen
            && e.job.frozen_by == Some(FrozenBy::User)
        {
            // The wrapper sends this only after restoring foreground ownership.
            return thaw(e);
        }
        vec![]
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
        if self.record_run(&e.job, dur, None, true, wall) {
            fx.push(Effect::SaveProfiles);
        }
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
                if e.terminal_stopped {
                    e.terminal_wake = true;
                    if let Some(pid) = e.job.spec.pid {
                        fx.push(Effect::ContinueWrapper { pid });
                    }
                }
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
                e.job.spared = e.job.frozen_by == Some(FrozenBy::Guard);
                if e.terminal_stopped {
                    e.terminal_wake = true;
                    if let Some(pid) = e.job.spec.pid {
                        fx.push(Effect::ContinueWrapper { pid });
                    }
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
        let normal = self.snap.pressure == Pressure::Normal;
        for e in self.jobs.values_mut() {
            e.job.spared &= !normal;
            if let Some(u) = e.job.pgid.and_then(|p| usage.get(&p)) {
                e.job.footprint = u.footprint;
                e.job.peak = e.job.peak.max(u.footprint);
                e.job.cpu_pct = u.cpu_pct;
                e.job.procs = u.procs;
                e.job.now.clone_from(&u.now);
                merge_parts(&mut e.job.parts, &u.parts);
            }
        }
        let mut fx = vec![];

        // A control can arrive after the stop notice but before the wrapper
        // actually suspends. Repeat CONT until its continuation acknowledges it.
        for e in self
            .jobs
            .values()
            .filter(|e| e.terminal_stopped && e.terminal_wake)
        {
            if let Some(pid) = e.job.spec.pid {
                fx.push(Effect::ContinueWrapper { pid });
            }
        }

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
        if !self.kills.iter().any(|(p, _)| *p == pgid) {
            self.kills.push((pgid, now + self.cfg.kill_grace_s * 1000));
        }
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

    /// Recorded peaks include runs with fewer workers. Starting untouched
    /// reserves the model's memory at the default count, even with tuning off.
    fn estimate_for(&self, spec: &JobSpec) -> Estimate {
        self.profiles.estimate_for_workers(
            &spec.key,
            &spec.fallback_key,
            self.cfg.default_estimate,
            spec.tune.map(|t| t.default_workers),
        )
    }

    fn median_for(&self, spec: &JobSpec) -> Option<u64> {
        self.profiles
            .get(&spec.key)?
            .median_ms_for_workers(spec.tune.map(|t| t.default_workers))
    }

    /// After the settings or a profile changed: queued jobs' estimates.
    fn reestimate(&mut self, key: Option<&str>) {
        let ids: Vec<JobId> = self
            .jobs
            .values()
            .filter(|e| e.job.state == JobState::Queued)
            .filter(|e| key.is_none_or(|k| e.job.spec.key == k || e.job.spec.fallback_key == k))
            .map(|e| e.job.id)
            .collect();
        for id in ids {
            let spec = self.jobs[&id].job.spec.clone();
            let est = self.estimate_for(&spec);
            let scaling = self.scaling_for(&spec);
            let median = self.median_for(&spec);
            let j = &mut self.jobs.get_mut(&id).expect("queued").job;
            (j.estimate, j.samples, j.scaling) = (est.bytes, est.samples, scaling);
            j.median_ms = median;
            self.jobs.get_mut(&id).expect("queued").told = None;
        }
    }

    fn requeue(&mut self, mut e: Entry, now: Millis) {
        let est = self.estimate_for(&e.job.spec);
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
        j.now = None;
        j.parts.clear();
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

    /// Keep observed memory even when a run was interrupted, then refresh
    /// queued consumers of both the exact profile and its fallback.
    fn record_run(
        &mut self,
        j: &Job,
        duration_ms: u64,
        exit: Option<i32>,
        flagged: bool,
        wall: u64,
    ) -> bool {
        if j.started_at.is_none() || j.pgid.is_none() {
            return false;
        }
        self.profiles.record(
            &j.spec.key,
            &j.spec.fallback_key,
            Run {
                peak: j.peak,
                duration_ms,
                exit,
                flagged,
                at: wall,
                workers: j
                    .tuned
                    .map(|t| t.workers)
                    .or(j.spec.tune.map(|t| t.default_workers)),
                parts: j.parts.clone(),
            },
        );
        self.reestimate(Some(&j.spec.key));
        if j.spec.fallback_key != j.spec.key {
            self.reestimate(Some(&j.spec.fallback_key));
        }
        true
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
        let started = j.started_at.is_some() && j.pgid.is_some();
        let h = HistoryEntry {
            id: j.id,
            label: j.spec.label.clone(),
            cmd: j.spec.cmd.clone(),
            key: j.spec.key.clone(),
            duration_ms: if started { duration_ms } else { 0 },
            started: Some(started),
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
        };
        self.tally.add(&h);
        self.history.push_front(h);
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
            head.skipped += d.backfilled.len() as u32;
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
                e.job.median_ms = self
                    .profiles
                    .get(&e.job.spec.key)
                    .and_then(|p| p.median_ms_for_workers(Some(workers)));
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
                ..Usage::default()
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
                parts: Vec::new(),
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
    fn backfill_uses_the_running_jobs_actual_worker_count() {
        for (candidate_ms, should_backfill) in [(60_000, false), (15_000, true)] {
            let mut s = sched(6);
            let mut active = own("vitest run");
            active.tune = Some(Tunable {
                tool: Tool::Vitest,
                default_workers: 8,
                npm: false,
            });
            let head = own("head");
            let candidate = own("candidate");
            for (sp, peak, workers, duration_ms, count) in [
                (&active, 2 * GIB, Some(2), 120_000, 3),
                (&active, 2 * GIB, Some(8), 30_000, 1),
                (&head, 4 * GIB, None, 90_000, 3),
                (&candidate, 2 * GIB, None, candidate_ms, 3),
            ] {
                for _ in 0..count {
                    s.profiles.record(
                        &sp.key,
                        &sp.fallback_key,
                        Run {
                            peak,
                            workers,
                            duration_ms,
                            exit: Some(0),
                            flagged: false,
                            at: 0,
                            parts: vec![],
                        },
                    );
                }
            }
            assert!(granted(&s.enqueue(1, active, 0), 1));
            assert_eq!(s.list().running[0].median_ms, Some(30_000));
            s.started(1, 100);
            s.tick(snap(4), &usage(100, 2.0), 0);
            assert!(!granted(&s.enqueue(2, head, 0), 2));
            assert_eq!(granted(&s.enqueue(3, candidate, 0), 3), should_backfill);
        }
    }

    #[test]
    fn tuned_runtime_uses_matching_history_or_stays_unknown() {
        for matching in [false, true] {
            let (mut s, sp) = tunable_sched(12);
            if matching {
                s.profiles.record(
                    &sp.key,
                    &sp.fallback_key,
                    Run {
                        peak: 27 * GIB / 4,
                        duration_ms: 30_000,
                        exit: Some(0),
                        flagged: false,
                        at: 0,
                        workers: Some(6),
                        parts: vec![],
                    },
                );
            }
            assert!(granted(&s.enqueue(1, sp, 0), 1));
            let job = s.list().running.into_iter().find(|j| j.id == 2).unwrap();
            assert_eq!(job.tuned.unwrap().workers, 6);
            assert_eq!(job.median_ms, matching.then_some(30_000));
        }
    }

    #[test]
    fn tuned_history_reserves_the_default_worker_count_before_admission() {
        for tune in [true, false] {
            let mut s = sched(8);
            s.cfg.tune = tune;
            let mut sp = own("vitest run");
            sp.tune = Some(Tunable {
                tool: Tool::Vitest,
                default_workers: 10,
                npm: false,
            });
            for i in 0..20 {
                s.profiles.record(
                    &sp.key,
                    &sp.fallback_key,
                    Run {
                        peak: if i < 2 { 8 * GIB } else { 2 * GIB },
                        workers: Some(if i < 2 { 10 } else { 2 }),
                        duration_ms: 60_000,
                        exit: Some(0),
                        flagged: false,
                        at: 0,
                        parts: vec![],
                    },
                );
            }
            assert_eq!(s.profiles.get(&sp.key).unwrap().p90(), Some(2 * GIB));
            s.enqueue(9, spec("blocker"), 0);
            let fx = s.enqueue(1, sp.clone(), 0);
            if tune {
                assert!(sent(&fx, 1).iter().any(|m| {
                    matches!(
                        m,
                        DaemonMsg::Granted {
                            workers: Some(_),
                            ..
                        }
                    )
                }));
            } else {
                assert!(!granted(&fx, 1));
                assert!(s.list().queued[0].estimate >= 8 * GIB);
            }
            // Reconfiguration must also preserve the default-count estimate.
            s.held = true;
            s.enqueue(2, sp, 0);
            s.reestimate(None);
            assert!(s.list().queued.iter().all(|j| j.estimate >= 8 * GIB));
        }
    }

    #[test]
    fn completing_a_run_refreshes_exact_and_fallback_consumers() {
        let mut s = sched(4);
        let sp = spec("build");
        for _ in 0..2 {
            s.profiles.record(
                &sp.key,
                &sp.fallback_key,
                Run {
                    peak: 3 * GIB,
                    workers: None,
                    duration_ms: 1_000,
                    exit: Some(0),
                    flagged: false,
                    at: 0,
                    parts: vec![],
                },
            );
        }
        s.enqueue(1, sp.clone(), 0);
        s.started(1, 100);
        s.control(9, Control::Hold { on: true }, 0, 0);
        s.enqueue(2, sp, 0);
        s.enqueue(3, spec("other-build"), 0);
        assert!(s.list().queued.iter().all(|j| j.estimate == 3 * GIB));
        s.tick(snap(4), &usage(100, 12.0), 1_000);
        let fx = s.exited(1, Some(0), None, 60_000, 100);
        assert!(s.list().queued.iter().all(|j| j.estimate == 12 * GIB));
        assert_eq!(s.list().queued[0].median_ms, Some(1_000));
        assert!(
            sent(&fx, 3).iter().any(|m| {
                matches!(m, DaemonMsg::Queued { estimate, .. } if *estimate == 12 * GIB)
            })
        );
        s.control(9, Control::ResetProfile { key: "/r|x".into() }, 60_000, 100);
        let q = s.list().queued;
        assert_eq!(q[0].estimate, 12 * GIB);
        assert_eq!(q[1].estimate, 3 * GIB);
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
                parts: Vec::new(),
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

    #[test]
    fn a_batch_counts_every_job_against_the_heads_backfill_limit() {
        let mut s = sched(8);
        learned(&mut s, "build", 6, 60);
        learned(&mut s, "e2e", 7, 300);
        learned(&mut s, "lint", 1, 10);
        s.enqueue(1, own("build"), 0);
        s.started(1, 100);
        s.tick(snap(8), &usage(100, 6.0), 1_000);
        s.control(99, Control::Hold { on: true }, 1_000, 0);
        s.enqueue(2, own("e2e"), 1_000);
        for conn in 3..13 {
            s.enqueue(conn, own("lint"), 1_000);
        }
        s.control(99, Control::Hold { on: false }, 1_000, 0);
        assert_eq!(s.list().queued[0].skipped, 3);
        assert_eq!(s.list().running.len(), 4);
        // Another settle cannot spend the same allowance again.
        s.tick(snap(8), &usage(100, 6.0), 2_000);
        assert_eq!(s.list().running.len(), 4);
    }

    fn s_cfg() -> Config {
        Config {
            default_estimate: 3 * GIB,
            ..Config::default()
        }
    }

    #[test]
    fn what_runs_inside_a_job_is_kept_with_its_run() {
        let mut s = sched(20);
        s.enqueue(1, spec("git commit"), 0);
        s.started(1, 100);
        let inside = |gb: u64, procs: u32| {
            let mut u = usage(100, 1.0);
            u.get_mut(&100).unwrap().parts = vec![Part {
                cmd: "tsc --noEmit".into(),
                peak: gb * GIB,
                procs,
                samples: 1,
            }];
            u
        };
        s.tick(snap(20), &inside(2, 1), 1_000);
        s.tick(snap(20), &inside(3, 2), 2_000);
        s.tick(snap(20), &usage(100, 1.0), 3_000);
        let parts = s.list().running[0].parts.clone();
        assert_eq!(parts.len(), 1);
        assert_eq!(
            (parts[0].peak, parts[0].procs, parts[0].samples),
            (3 * GIB, 2, 2)
        );
        s.exited(1, Some(0), None, 4_000, 42);
        let run = &s.profiles.get("/r|git commit").unwrap().runs[0];
        assert_eq!(run.parts, parts, "the run keeps what ran inside it");
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
                parts: Vec::new(),
            }
        );
        assert_eq!(s.history[0].outcome, Outcome::Exited);
        assert_eq!(s.history[0].exit, Some(1));
        let t = s.tally.window(42, 1);
        assert_eq!(t[0].0, "/r|pnpm test");
        assert_eq!((t[0].1.runs, t[0].1.failed, t[0].1.run_ms), (1, 1, 3_000));
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
    fn disconnect_preserves_observed_memory_and_refreshes_queued_profiles() {
        for frozen in [false, true] {
            let mut s = sched(20);
            let mut sp = spec("build");
            sp.tune = Some(Tunable {
                tool: Tool::Vitest,
                default_workers: 4,
                npm: false,
            });
            for _ in 0..3 {
                s.profiles.record(
                    &sp.key,
                    &sp.fallback_key,
                    Run {
                        peak: 3 * GIB,
                        duration_ms: 1_000,
                        exit: Some(0),
                        flagged: false,
                        at: 1,
                        workers: Some(4),
                        parts: vec![],
                    },
                );
            }
            s.enqueue(1, sp.clone(), 0);
            s.started(1, 100);
            s.held = true;
            s.enqueue(2, sp.clone(), 0);
            s.enqueue(3, spec("other-build"), 0);
            let mut sample = usage(100, 8.0);
            sample.get_mut(&100).unwrap().parts.push(Part {
                cmd: "worker".into(),
                peak: 7 * GIB,
                procs: 4,
                samples: 1,
            });
            s.tick(snap(20), &sample, 1_000);
            if frozen {
                s.control(9, Control::Freeze { id: 1 }, 2_000, 2);
            }
            let fx = s.disconnected(1, 60_000, 60);
            assert!(fx.contains(&Effect::SaveProfiles));
            assert!(fx.contains(&Effect::SaveHistory));
            let profile = s.profiles.get(&sp.key).unwrap();
            let run = profile.runs.back().unwrap();
            assert!(run.flagged);
            assert_eq!(
                (run.peak, run.duration_ms, run.exit),
                (8 * GIB, 60_000, None)
            );
            assert_eq!((run.workers, run.parts.len()), (Some(4), 1));
            assert_eq!(profile.median_ms(), Some(1_000));
            assert_eq!(profile.samples(), 3);
            assert_eq!(
                s.profiles.get(&sp.fallback_key).unwrap().runs.back(),
                Some(run)
            );
            assert!(s.list().queued.iter().all(|j| j.estimate >= 8 * GIB));
            assert!(sent(&fx, 3).iter().any(|m| matches!(m,
                DaemonMsg::Queued { estimate, .. } if *estimate == 8 * GIB
            )));
        }
    }

    #[test]
    fn disconnect_counts_only_confirmed_starts_even_at_zero_duration() {
        for (held, started) in [(true, false), (false, false), (false, true)] {
            let mut s = sched(20);
            s.held = held;
            s.enqueue(1, spec("a"), 0);
            if started {
                s.started(1, 100);
            }
            // An unconfirmed grant can last a while without a child ever starting.
            let now = if started { 0 } else { 5_000 };
            let fx = s.disconnected(1, now, 10);
            let h = &s.history[0];
            assert_eq!(h.started, Some(started));
            assert_eq!(h.has_run(), started);
            assert_eq!(h.duration_ms, 0);
            assert_eq!(fx.contains(&Effect::SaveProfiles), started);
            assert_eq!(s.profiles.get("/r|a").is_some(), started);
            let runs: u32 = s.tally.window(10, 1).iter().map(|(_, d)| d.runs).sum();
            assert_eq!(runs, u32::from(started));
            assert!(s.tally.window(10, 1).iter().all(|(_, d)| d.run_ms == 0));
        }
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
    fn leader_exit_keeps_escalation_for_surviving_descendants() {
        let mut s = sched(20);
        s.enqueue(1, spec("a"), 0);
        s.started(1, 100);
        s.control(9, Control::Kill { id: 1 }, 1_000, 0);
        s.exited(1, None, Some(15), 2_000, 0);
        assert_eq!(s.pgids(), vec![100]);
        // The sampler associates remembered descendants with their old root.
        let mut surviving = usage(100, 0.1);
        surviving.get_mut(&100).unwrap().groups = vec![300];
        let fx = s.tick(snap(20), &surviving, 11_000);
        assert_eq!(signals(&fx), vec![(100, Sig::Kill)]);
        assert!(s.pgids().is_empty());
    }

    #[test]
    fn wrapper_signals_keep_the_original_signal_and_cleanup_deadline_after_exit() {
        for sig in [Sig::Int, Sig::Term, Sig::Hup] {
            let mut s = sched(20);
            s.enqueue(1, spec("a"), 0);
            s.started(1, 100);
            assert_eq!(signals(&s.signalled(1, sig, 1_000)), vec![(100, sig)]);
            s.signalled(1, sig, 9_000);
            s.exited(1, None, Some(15), 9_100, 0);
            assert_eq!(s.history[0].outcome, Outcome::Exited);
            assert_eq!(s.pgids(), vec![100]);
            assert_eq!(
                signals(&s.tick(snap(20), &usage(100, 0.1), 11_000)),
                vec![(100, Sig::Kill)]
            );
        }
    }

    #[test]
    fn terminal_stops_pause_the_whole_tree_as_a_user_pause() {
        let mut s = sched(20);
        s.enqueue(1, spec("a"), 0);
        s.started(1, 100);
        assert_eq!(
            signals(&s.terminal_stopped(1, true, 1_000)),
            vec![(100, Sig::Stop)]
        );
        let job = &s.list().running[0];
        assert_eq!(job.state, JobState::Frozen);
        assert_eq!(job.frozen_by, Some(FrozenBy::User));
        assert!(
            s.shutdown_thaws().is_empty(),
            "shell pauses survive daemon shutdown"
        );
        let mut critical = snap(20);
        critical.pressure = Pressure::Critical;
        assert!(signals(&s.tick(critical, &usage(100, 0.1), 60_000)).is_empty());
        assert_eq!(
            signals(&s.terminal_stopped(1, false, 61_000)),
            vec![(100, Sig::Cont)]
        );
        assert_eq!(s.list().running[0].state, JobState::Running);
        assert_eq!(s.list().running[0].frozen_by, None);
        s.control(9, Control::Freeze { id: 1 }, 62_000, 0);
        assert_eq!(
            s.shutdown_thaws(),
            vec![100],
            "daemon pauses are released on shutdown"
        );
    }

    #[test]
    fn a_child_signal_exit_does_not_replace_the_guards_requeue() {
        let mut s = sched(20);
        s.enqueue(1, spec("a"), 0);
        s.started(1, 100);
        s.apply_guard(
            Action::Kill {
                id: 1,
                requeue: true,
                why: "pressure".into(),
            },
            1_000,
        );
        assert!(s.child_signalled(1, Sig::Term, 1_100).is_empty());
        s.exited(1, None, Some(15), 1_100, 0);
        assert_eq!(s.history[0].outcome, Outcome::Requeued);
        assert_eq!(s.list().running[0].retry, 1);
    }

    #[test]
    fn terminal_control_retries_wrapper_wakes_until_continuation_is_acknowledged() {
        for kill in [false, true] {
            let mut s = sched(20);
            let mut job = spec("a");
            job.pid = Some(99);
            s.enqueue(1, job, 0);
            s.started(1, 100);
            s.terminal_stopped(1, true, 1_000);
            let control = if kill {
                Control::Kill { id: 1 }
            } else {
                Control::Resume { id: 1 }
            };
            let wake = Effect::ContinueWrapper { pid: 99 };
            assert!(s.control(9, control, 1_001, 0).contains(&wake));
            assert!(s.tick(snap(20), &usage(100, 0.1), 2_000).contains(&wake));
            s.terminal_stopped(1, false, 2_001);
            assert!(!s.tick(snap(20), &usage(100, 0.1), 3_000).contains(&wake));
        }
    }

    #[test]
    fn a_stop_notice_after_kill_still_wakes_the_wrapper_and_terminating_tree() {
        let mut s = sched(20);
        let mut job = spec("a");
        job.pid = Some(99);
        s.enqueue(1, job, 0);
        s.started(1, 100);
        s.control(9, Control::Kill { id: 1 }, 1_000, 0);
        let fx = s.terminal_stopped(1, true, 1_001);
        assert_eq!(signals(&fx), vec![(100, Sig::Cont)]);
        assert!(fx.contains(&Effect::ContinueWrapper { pid: 99 }));
        assert!(
            s.tick(snap(20), &usage(100, 0.1), 2_000)
                .contains(&Effect::ContinueWrapper { pid: 99 })
        );
        s.terminal_stopped(1, false, 2_001);
        s.exited(1, None, Some(15), 2_002, 0);
        assert_eq!(s.history[0].outcome, Outcome::ManualKill);
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
        assert_eq!(
            signals(&s.tick(snap(20), &usage(100, 0.1), 10_000)),
            vec![(100, Sig::Kill)]
        );
    }

    #[test]
    fn repeated_kills_keep_the_original_grace_deadline() {
        let mut s = sched(20);
        s.enqueue(1, spec("a"), 0);
        s.started(1, 100);
        s.control(9, Control::Kill { id: 1 }, 0, 0);
        s.control(9, Control::Kill { id: 1 }, 9_000, 0);
        assert_eq!(
            signals(&s.tick(snap(20), &usage(100, 0.1), 10_000)),
            vec![(100, Sig::Kill)]
        );
        assert!(signals(&s.tick(snap(20), &usage(100, 0.1), 19_000)).is_empty());
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
                parts: Vec::new(),
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

    #[test]
    fn a_job_resumed_from_a_guard_freeze_stays_running_until_memory_recovers() {
        let mut s = sched(20);
        s.enqueue(1, spec("a"), 0);
        s.started(1, 100);
        let critical = Snapshot {
            pressure: Pressure::Critical,
            ..snap(1)
        };
        let mut t = 0;
        let mut drive = |s: &mut Scheduler, snap: &Snapshot, secs: u64| {
            let mut fx = vec![];
            for _ in 0..secs {
                t += 1000;
                fx.extend(s.tick(snap.clone(), &usage(100, 1.0), t));
            }
            fx
        };
        drive(&mut s, &critical, 11);
        assert_eq!(s.list().running[0].frozen_by, Some(FrozenBy::Guard));
        s.control(9, Control::Resume { id: 1 }, 0, 0);
        // Still critical for a minute: neither frozen again nor killed.
        let fx = drive(&mut s, &critical, 60);
        assert!(signals(&fx).is_empty(), "{fx:?}");
        assert_eq!(s.list().running[0].state, JobState::Running);
        // Once memory has been normal, the guard may freeze it again.
        drive(&mut s, &snap(20), 1);
        let fx = drive(&mut s, &critical, 11);
        assert_eq!(signals(&fx), vec![(100, Sig::Stop)]);
    }

    #[test]
    fn resuming_a_freeze_of_your_own_leaves_the_guard_in_charge() {
        let mut s = sched(20);
        s.enqueue(1, spec("a"), 0);
        s.started(1, 100);
        s.control(9, Control::Freeze { id: 1 }, 0, 0);
        s.control(9, Control::Resume { id: 1 }, 0, 0);
        let critical = Snapshot {
            pressure: Pressure::Critical,
            ..snap(1)
        };
        let fx: Vec<Effect> = (1..=11)
            .flat_map(|t| s.tick(critical.clone(), &usage(100, 1.0), t * 1000))
            .collect();
        assert_eq!(signals(&fx), vec![(100, Sig::Stop)]);
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
