//! What to do when the machine runs out of memory anyway.
//!
//! Admission works from estimates, and estimates are sometimes wrong. The
//! guard steps in on sustained pressure, in escalating steps:
//!
//! 1. **Freeze** (SIGSTOP) the most expendable running job. It stops growing
//!    immediately, but keeps its memory (macOS compresses it).
//! 2. **Kill** it (SIGTERM, then SIGKILL) if pressure stays critical, which
//!    actually frees memory, and requeue it once.
//! 3. **Thaw** guard-frozen jobs, newest first, once pressure is back to normal.
//!
//! Only jobs Headroom started are ever touched, and never pinned ones. One
//! freeze or thaw per window, so a single spike can't freeze everything.

use crate::config::{Config, KillMode};
use crate::model::{FrozenBy, Job, JobId, JobState, Millis, Pressure, Snapshot};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GuardState {
    critical_since: Option<Millis>,
    normal_since: Option<Millis>,
    last_step: Option<Millis>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Freeze {
        id: JobId,
        why: String,
    },
    Thaw {
        id: JobId,
    },
    Kill {
        id: JobId,
        requeue: bool,
        why: String,
    },
}

fn gb(b: u64) -> String {
    format!("{:.1} GB", b as f64 / crate::model::GIB as f64)
}

fn secs(ms: Millis) -> Millis {
    ms / 1000
}

/// Advance the guard by one sample. `jobs` are the running and frozen jobs.
pub fn step(
    gs: &mut GuardState,
    now: Millis,
    snap: &Snapshot,
    jobs: &[&Job],
    cfg: &Config,
) -> Vec<Action> {
    match snap.pressure {
        Pressure::Critical => {
            gs.critical_since.get_or_insert(now);
            gs.normal_since = None;
        }
        Pressure::Normal => {
            gs.normal_since.get_or_insert(now);
            gs.critical_since = None;
        }
        Pressure::Warn => {
            gs.critical_since = None;
            gs.normal_since = None;
        }
    }
    let guard_frozen =
        |j: &&&Job| j.state == JobState::Frozen && j.frozen_by == Some(FrozenBy::Guard);

    if cfg.kill_mode == KillMode::Off {
        // Turning the guard off releases whatever it was holding.
        return jobs
            .iter()
            .filter(guard_frozen)
            .map(|j| Action::Thaw { id: j.id })
            .collect();
    }

    let mut out = Vec::new();
    let runaway = |j: &Job| j.footprint > j.runaway_at(cfg.runaway_factor, cfg.runaway_cap);

    if cfg.kill_mode == KillMode::FreezeKill {
        for j in jobs.iter().filter(|j| !j.pinned) {
            let requeue = j.retry < 1;
            // Frozen long enough and the pressure that froze it hasn't gone.
            // A runaway only needs warn: it is the problem, not a bystander.
            let need = if runaway(j) {
                Pressure::Warn
            } else {
                Pressure::Critical
            };
            if guard_frozen(&j)
                && snap.pressure >= need
                && j.frozen_at
                    .is_some_and(|t| now.saturating_sub(t) >= cfg.kill_after_s * 1000)
            {
                out.push(Action::Kill {
                    id: j.id,
                    requeue,
                    why: format!(
                        "memory still {} {} s after freezing; peak {} vs est {}",
                        format!("{:?}", snap.pressure).to_lowercase(),
                        cfg.kill_after_s,
                        gb(j.peak),
                        gb(j.estimate)
                    ),
                });
                continue;
            }
            let swap_growth = snap.swap_used.saturating_sub(j.swap_at_start);
            if j.state == JobState::Running
                && snap.pressure >= Pressure::Warn
                && swap_growth > cfg.swap_kill_growth
            {
                out.push(Action::Kill {
                    id: j.id,
                    requeue,
                    why: format!("swap grew {} since it started", gb(swap_growth)),
                });
            }
        }
        if !out.is_empty() {
            return out;
        }
    }

    let window = cfg.freeze_after_s * 1000;
    let cooling = gs.last_step.is_some_and(|t| now.saturating_sub(t) < window);
    if cooling {
        return out;
    }

    let candidates = || {
        jobs.iter()
            .filter(|j| j.state == JobState::Running && !j.pinned)
    };

    // A runaway is frozen as soon as memory is tight at all.
    if snap.pressure >= Pressure::Warn
        && let Some(j) = candidates()
            .filter(|j| runaway(j))
            .max_by_key(|j| j.footprint)
    {
        gs.last_step = Some(now);
        out.push(Action::Freeze {
            id: j.id,
            why: format!("runaway: {} vs est {}", gb(j.footprint), gb(j.estimate)),
        });
        return out;
    }

    if gs
        .critical_since
        .is_some_and(|t| now.saturating_sub(t) >= window)
    {
        // Most expendable first: low priority, then the most recently started
        // (it has the least work to lose).
        if let Some(j) = candidates().max_by_key(|j| (j.low(), j.started_at)) {
            gs.last_step = Some(now);
            out.push(Action::Freeze {
                id: j.id,
                why: format!(
                    "memory critical for {} s",
                    secs(now - gs.critical_since.unwrap_or(now))
                ),
            });
        }
        return out;
    }

    if gs
        .normal_since
        .is_some_and(|t| now.saturating_sub(t) >= cfg.thaw_after_s * 1000)
        && let Some(j) = jobs.iter().filter(guard_frozen).max_by_key(|j| j.frozen_at)
    {
        gs.last_step = Some(now);
        out.push(Action::Thaw { id: j.id });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{GIB, JobSpec};

    fn snap(p: Pressure) -> Snapshot {
        Snapshot {
            pressure: p,
            mem_total: 36 * GIB,
            cores: 10,
            ..Snapshot::default()
        }
    }

    fn running(id: JobId, started: Millis) -> Job {
        let mut j = Job::queued(id, JobSpec::default(), 2 * GIB, 5, 0);
        j.state = JobState::Running;
        j.started_at = Some(started);
        j.footprint = GIB;
        j.peak = GIB;
        j
    }

    fn frozen(id: JobId, at: Millis) -> Job {
        let mut j = running(id, 0);
        j.state = JobState::Frozen;
        j.frozen_by = Some(FrozenBy::Guard);
        j.frozen_at = Some(at);
        j
    }

    /// Feed the same reading every second from `from` to `to` inclusive,
    /// returning every action with the second it happened.
    fn drive(
        gs: &mut GuardState,
        p: Pressure,
        jobs: &[Job],
        from: u64,
        to: u64,
        cfg: &Config,
    ) -> Vec<(u64, Action)> {
        let refs: Vec<&Job> = jobs.iter().collect();
        (from..=to)
            .flat_map(|s| {
                step(gs, s * 1000, &snap(p), &refs, cfg)
                    .into_iter()
                    .map(move |a| (s, a))
            })
            .collect()
    }

    #[test]
    fn sustained_critical_freezes_the_newest_job_after_the_window() {
        let cfg = Config::default();
        let mut gs = GuardState::default();
        let jobs = [running(1, 0), running(2, 5_000)];
        let acts = drive(&mut gs, Pressure::Critical, &jobs, 0, 10, &cfg);
        assert_eq!(acts.len(), 1);
        assert_eq!(acts[0].0, 10);
        assert!(matches!(acts[0].1, Action::Freeze { id: 2, .. }));
    }

    #[test]
    fn low_priority_is_frozen_before_newer_normal_jobs() {
        let mut gs = GuardState::default();
        let mut low = running(1, 0);
        low.spec.low = true;
        let acts = drive(
            &mut gs,
            Pressure::Critical,
            &[low, running(2, 9_000)],
            0,
            10,
            &Config::default(),
        );
        assert!(matches!(acts[0].1, Action::Freeze { id: 1, .. }));
    }

    #[test]
    fn flapping_pressure_inside_the_window_freezes_nothing() {
        let cfg = Config::default();
        let mut gs = GuardState::default();
        let jobs = [running(1, 0)];
        let mut acts = drive(&mut gs, Pressure::Critical, &jobs, 0, 8, &cfg);
        acts.extend(drive(&mut gs, Pressure::Warn, &jobs, 9, 9, &cfg));
        acts.extend(drive(&mut gs, Pressure::Critical, &jobs, 10, 18, &cfg));
        assert!(acts.is_empty(), "{acts:?}");
    }

    #[test]
    fn one_freeze_per_window() {
        let cfg = Config::default();
        let mut gs = GuardState::default();
        let jobs = [running(1, 0), running(2, 1), running(3, 2)];
        let acts = drive(&mut gs, Pressure::Critical, &jobs, 0, 25, &cfg);
        let at: Vec<u64> = acts.iter().map(|(s, _)| *s).collect();
        assert_eq!(at, vec![10, 20]);
    }

    #[test]
    fn frozen_job_is_killed_and_requeued_if_pressure_stays_critical() {
        let cfg = Config::default();
        let mut gs = GuardState::default();
        let acts = drive(&mut gs, Pressure::Critical, &[frozen(1, 0)], 0, 30, &cfg);
        assert_eq!(acts.len(), 1);
        assert_eq!(acts[0].0, 30);
        assert!(
            matches!(&acts[0].1, Action::Kill { id: 1, requeue: true, why } if why.contains("critical 30 s"))
        );
    }

    #[test]
    fn second_kill_gives_up() {
        let mut j = frozen(1, 0);
        j.retry = 1;
        let acts = drive(
            &mut GuardState::default(),
            Pressure::Critical,
            &[j],
            30,
            30,
            &Config::default(),
        );
        assert!(matches!(acts[0].1, Action::Kill { requeue: false, .. }));
    }

    #[test]
    fn freeze_mode_never_kills() {
        let cfg = Config {
            kill_mode: KillMode::Freeze,
            ..Config::default()
        };
        let acts = drive(
            &mut GuardState::default(),
            Pressure::Critical,
            &[frozen(1, 0)],
            0,
            120,
            &cfg,
        );
        assert!(acts.iter().all(|(_, a)| !matches!(a, Action::Kill { .. })));
    }

    #[test]
    fn normal_pressure_thaws_the_newest_guard_freeze_first() {
        let cfg = Config::default();
        let mut gs = GuardState::default();
        let jobs = [frozen(1, 0), frozen(2, 5_000)];
        let acts = drive(&mut gs, Pressure::Normal, &jobs, 100, 115, &cfg);
        assert_eq!(acts, vec![(115, Action::Thaw { id: 2 })]);
    }

    #[test]
    fn user_freezes_are_never_thawed_by_the_guard() {
        let mut j = frozen(1, 0);
        j.frozen_by = Some(FrozenBy::User);
        let cfg = Config::default();
        assert!(
            drive(
                &mut GuardState::default(),
                Pressure::Normal,
                &[j.clone()],
                0,
                60,
                &cfg
            )
            .is_empty()
        );
        assert!(
            drive(
                &mut GuardState::default(),
                Pressure::Critical,
                &[j.clone()],
                0,
                60,
                &cfg
            )
            .is_empty()
        );
        let off = Config {
            kill_mode: KillMode::Off,
            ..cfg
        };
        assert!(
            drive(
                &mut GuardState::default(),
                Pressure::Normal,
                &[j],
                0,
                1,
                &off
            )
            .is_empty()
        );
    }

    #[test]
    fn pinned_jobs_are_exempt() {
        let mut j = running(1, 0);
        j.pinned = true;
        j.footprint = 50 * GIB;
        let mut f = frozen(2, 0);
        f.pinned = true;
        let acts = drive(
            &mut GuardState::default(),
            Pressure::Critical,
            &[j, f],
            0,
            120,
            &Config::default(),
        );
        assert!(acts.is_empty(), "{acts:?}");
    }

    #[test]
    fn runaway_is_frozen_first_under_warn_regardless_of_priority() {
        let mut big = running(1, 0);
        big.footprint = 9 * GIB; // est 2 GB → runaway above max(4, 8) GB
        let mut low = running(2, 5_000);
        low.spec.low = true;
        let acts = drive(
            &mut GuardState::default(),
            Pressure::Warn,
            &[big, low],
            0,
            0,
            &Config::default(),
        );
        assert!(matches!(&acts[0].1, Action::Freeze { id: 1, why } if why.starts_with("runaway")));
    }

    #[test]
    fn runaway_threshold_boundary() {
        let mut j = running(1, 0);
        j.footprint = 8 * GIB;
        let none = drive(
            &mut GuardState::default(),
            Pressure::Warn,
            &[j.clone()],
            0,
            0,
            &Config::default(),
        );
        assert!(none.is_empty());
        j.footprint += 1;
        let one = drive(
            &mut GuardState::default(),
            Pressure::Warn,
            &[j],
            0,
            0,
            &Config::default(),
        );
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn frozen_runaway_is_killed_under_warn() {
        let mut j = frozen(1, 0);
        j.footprint = 10 * GIB;
        let acts = drive(
            &mut GuardState::default(),
            Pressure::Warn,
            &[j],
            30,
            30,
            &Config::default(),
        );
        assert!(matches!(acts[0].1, Action::Kill { id: 1, .. }));
    }

    #[test]
    fn swap_growth_kills_outright() {
        let mut j = running(1, 0);
        j.swap_at_start = GIB;
        let s = Snapshot {
            swap_used: 5 * GIB + 1,
            ..snap(Pressure::Warn)
        };
        let acts = step(&mut GuardState::default(), 0, &s, &[&j], &Config::default());
        assert!(
            matches!(&acts[0], Action::Kill { id: 1, requeue: true, why } if why.starts_with("swap grew"))
        );
        // Exactly the limit is tolerated.
        let s = Snapshot {
            swap_used: 5 * GIB,
            ..s
        };
        assert!(step(&mut GuardState::default(), 0, &s, &[&j], &Config::default()).is_empty());
    }

    #[test]
    fn turning_the_guard_off_releases_its_freezes() {
        let cfg = Config {
            kill_mode: KillMode::Off,
            ..Config::default()
        };
        let acts = step(
            &mut GuardState::default(),
            0,
            &snap(Pressure::Critical),
            &[&frozen(1, 0)],
            &cfg,
        );
        assert_eq!(acts, vec![Action::Thaw { id: 1 }]);
    }

    #[test]
    fn nothing_to_do_without_jobs() {
        let acts = drive(
            &mut GuardState::default(),
            Pressure::Critical,
            &[],
            0,
            60,
            &Config::default(),
        );
        assert!(acts.is_empty());
    }
}
