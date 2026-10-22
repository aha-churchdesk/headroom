//! When a queued job may start.
//!
//! Memory decides, not a job count: a 200 MB lint and a 6 GB e2e run are not
//! the same "one job". A job is admitted when its estimate fits in what is
//! available, minus the headroom kept free, minus what the running jobs are
//! still expected to grow into.
//!
//! The queue gives its head first claim on memory. Later jobs can pass a
//! waiting head only within the backfill limits below.
//!
//! A later job may start first if it fits now. When runtimes are known,
//! it must finish before the head could start anyway (EASY backfilling).
//! Without a predicted runtime or release window, fitting jobs may still
//! pass opportunistically, which can delay the head. Every pass counts
//! toward the same bounded number and time; then the queue is strict again.

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::model::{Bytes, Job, JobId, JobState, Millis, Pressure, Snapshot};

/// Why the head of the queue is waiting. Shown to the user and the agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Blocked {
    Memory { need: Bytes, free: Bytes },
    Pressure { level: Pressure },
    Cpu { load: f64, cores: u32 },
    Held,
}

impl Blocked {
    pub fn describe(&self) -> String {
        let gb = |b: Bytes| format!("{:.1}", b as f64 / crate::model::GIB as f64).replace(".0", "");
        match self {
            Blocked::Memory { need, free } => {
                format!("next needs {} GB, {} GB free", gb(*need), gb(*free))
            }
            Blocked::Pressure { level } => {
                format!("memory pressure {}", format!("{level:?}").to_uppercase())
            }
            Blocked::Cpu { load, cores } => format!("CPU busy, load {load:.0} / {cores} cores"),
            Blocked::Held => "held by you".to_string(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Decision {
    pub admit: Vec<JobId>,
    /// Admitted jobs that start ahead of `skipped`, the head waiting on memory.
    pub backfilled: Vec<JobId>,
    pub skipped: Option<JobId>,
    pub blocked: Option<Blocked>,
}

/// When the head could start, in ms from now: running jobs are expected to
/// end at their median run time, each freeing what it holds or is still
/// expected to grow into. One already past its median is expected to run as
/// long again as it has so far. `None` if any end can't be predicted, or if
/// even all of them ending wouldn't make room.
fn shadow(
    running: &[&Job],
    admitted: &[(Option<u64>, Bytes)],
    need: Bytes,
    free: Bytes,
    now: Millis,
) -> Option<u64> {
    let mut ends: Vec<(u64, Bytes)> = Vec::new();
    for j in running {
        match j.state {
            // A frozen job holds its memory until someone acts on it.
            JobState::Frozen => continue,
            _ => {
                let elapsed = j.started_at.map_or(0, |t| now.saturating_sub(t));
                let median = j.median_ms?;
                let left = if elapsed < median {
                    median - elapsed
                } else {
                    elapsed
                };
                ends.push((left, j.estimate.max(j.footprint)));
            }
        }
    }
    for (median, est) in admitted {
        ends.push(((*median)?, *est));
    }
    ends.sort_unstable();
    let mut free = free;
    for (at, freed) in ends {
        free += freed;
        if free >= need {
            return Some(at);
        }
    }
    None
}

/// Has the head been passed over enough?
fn strict(head: &Job, cfg: &Config, now: Millis) -> bool {
    !cfg.backfill
        || head.skipped >= cfg.backfill_skips
        || head
            .skipped_since
            .is_some_and(|t| now.saturating_sub(t) >= cfg.backfill_wait_s * 1000)
}

/// Memory the running jobs are still expected to take on top of what they
/// hold now. A frozen job can't grow.
pub fn pending_growth(running: &[&Job]) -> Bytes {
    running
        .iter()
        .filter(|j| j.state == JobState::Running)
        .map(|j| j.estimate.saturating_sub(j.footprint))
        .sum()
}

/// Decide which queued jobs start now. `running` holds running and frozen
/// jobs; `queue` is in queue order (pinned first).
pub fn decide(
    snap: &Snapshot,
    running: &[&Job],
    queue: &[&Job],
    held: bool,
    cfg: &Config,
    now: Millis,
) -> Decision {
    let mut d = Decision::default();
    if queue.is_empty() {
        return d;
    }
    if held {
        d.blocked = Some(Blocked::Held);
        return d;
    }
    let mut growth = pending_growth(running);
    // Jobs admitted this round haven't reached the load average yet; count
    // each as a busy core so one round can't overshoot.
    let mut load = snap.load1;
    let cpu_limit = snap.cores as f64 * cfg.cpu_factor;

    let mut admitted: Vec<(Option<u64>, Bytes)> = Vec::new();
    let mut waiting_head: Option<(usize, Bytes)> = None;
    for (i, job) in queue.iter().enumerate() {
        // With nothing of ours running, holding back can't help: an
        // estimate bigger than the machine would otherwise wait forever.
        let idle = running.is_empty() && d.admit.is_empty();
        // Warn is where a busy Mac with some swap sits for hours, so it only
        // stops jobs from piling up: one may still start on an idle queue.
        // Critical stops everything; a new job would only add to it.
        let pressure_ok = match snap.pressure {
            Pressure::Normal => true,
            Pressure::Warn => idle,
            Pressure::Critical => false,
        };
        if !pressure_ok {
            d.blocked = Some(Blocked::Pressure {
                level: snap.pressure,
            });
            break;
        }
        let free = snap
            .mem_available
            .saturating_sub(cfg.headroom)
            .saturating_sub(growth);
        let fits = idle || job.pinned || job.estimate <= free;
        if !fits {
            d.blocked = Some(Blocked::Memory {
                need: job.estimate,
                free,
            });
            waiting_head = Some((i, free));
            break;
        }
        let cpu_ok = idle || job.low() || snap.cores == 0 || load < cpu_limit;
        if !cpu_ok {
            d.blocked = Some(Blocked::Cpu {
                load,
                cores: snap.cores,
            });
            break;
        }
        d.admit.push(job.id);
        growth += job.estimate;
        admitted.push((job.median_ms, job.estimate));
        load += 1.0;
    }

    let Some((i, mut free)) = waiting_head else {
        return d;
    };
    let head = queue[i];
    if strict(head, cfg, now) {
        return d;
    }
    let head_starts_in = shadow(running, &admitted, head.estimate, free, now);
    let remaining = cfg.backfill_skips.saturating_sub(head.skipped) as usize;
    for job in &queue[i + 1..] {
        if d.backfilled.len() >= remaining {
            break;
        }
        // Preserve the finish window whenever both sides are known. A
        // successful run can supply runtime even if its memory was unmeasured.
        // Otherwise use the remaining skip allowance without promising that
        // the head will not be delayed.
        let time_ok = match (job.median_ms, head_starts_in) {
            (Some(takes), Some(window)) => takes <= window,
            _ => true,
        };
        let cpu_ok = job.low() || snap.cores == 0 || load < cpu_limit;
        if time_ok && job.estimate <= free && cpu_ok {
            d.admit.push(job.id);
            d.backfilled.push(job.id);
            free -= job.estimate;
            load += 1.0;
        }
    }
    if !d.backfilled.is_empty() {
        d.skipped = Some(head.id);
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{GIB, JobSpec};

    fn snap(avail_gb: f64) -> Snapshot {
        Snapshot {
            pressure: Pressure::Normal,
            mem_total: 36 * GIB,
            mem_used: 36 * GIB - (avail_gb * GIB as f64) as Bytes,
            mem_available: (avail_gb * GIB as f64) as Bytes,
            swap_used: 0,
            load1: 2.0,
            cores: 10,
            cpu_pct: 20.0,
        }
    }

    fn job(id: JobId, est_gb: f64) -> Job {
        Job::queued(id, JobSpec::default(), (est_gb * GIB as f64) as Bytes, 3, 0)
    }

    fn running(id: JobId, est_gb: f64, now_gb: f64) -> Job {
        let mut j = job(id, est_gb);
        j.state = JobState::Running;
        j.footprint = (now_gb * GIB as f64) as Bytes;
        j
    }

    fn run(s: &Snapshot, r: &[Job], q: &[Job], held: bool) -> Decision {
        let r: Vec<&Job> = r.iter().collect();
        let q: Vec<&Job> = q.iter().collect();
        decide(s, &r, &q, held, &Config::default(), 0)
    }

    /// 6 GB available, 2 kept free; a 3 GB job running that is done in 60 s.
    /// The 5 GB head can start once it ends.
    fn backfill_case(skipped: u32, since: Option<Millis>) -> (Snapshot, Job, Job) {
        let mut r = running(9, 3.0, 3.0);
        r.median_ms = Some(60_000);
        let mut head = job(1, 5.0);
        (head.skipped, head.skipped_since) = (skipped, since);
        (snap(6.0), r, head)
    }

    fn short(id: JobId, est_gb: f64, takes_ms: u64) -> Job {
        let mut j = job(id, est_gb);
        j.median_ms = Some(takes_ms);
        j
    }

    fn at(s: &Snapshot, r: &[Job], q: &[Job], now: Millis) -> Decision {
        let r: Vec<&Job> = r.iter().collect();
        let q: Vec<&Job> = q.iter().collect();
        decide(s, &r, &q, false, &Config::default(), now)
    }

    #[test]
    fn a_short_job_that_fits_starts_ahead_of_a_head_waiting_on_memory() {
        let (s, r, head) = backfill_case(0, None);
        let d = at(&s, &[r], &[head, short(2, 2.0, 30_000)], 0);
        assert_eq!(
            (d.admit, d.backfilled, d.skipped),
            (vec![2], vec![2], Some(1))
        );
        assert_eq!(
            d.blocked,
            Some(Blocked::Memory {
                need: 5 * GIB,
                free: 4 * GIB
            }),
            "the head still waits, and says why"
        );
    }

    #[test]
    fn it_must_be_done_by_the_time_the_head_could_start() {
        let (s, r, head) = backfill_case(0, None);
        let exactly = at(
            &s,
            std::slice::from_ref(&r),
            &[head.clone(), short(2, 2.0, 60_000)],
            0,
        );
        assert_eq!(exactly.backfilled, vec![2]);
        let over = at(
            &s,
            std::slice::from_ref(&r),
            &[head.clone(), short(2, 2.0, 60_001)],
            0,
        );
        assert!(over.admit.is_empty());
        // 20 s into the running job, the head can start in 40 s.
        let mut started = r;
        started.started_at = Some(0);
        let later = at(&s, &[started], &[head, short(2, 2.0, 45_000)], 20_000);
        assert!(later.admit.is_empty());
    }

    #[test]
    fn a_job_past_its_usual_time_is_expected_to_run_as_long_again() {
        let (s, mut r, head) = backfill_case(0, None);
        r.started_at = Some(0);
        let q = |takes| [head.clone(), short(2, 2.0, takes)];
        let backfilled = |now, takes| at(&s, std::slice::from_ref(&r), &q(takes), now).backfilled;
        // 80 s into a job that usually takes 60 s: 80 s more.
        assert_eq!(backfilled(80_000, 80_000), vec![2]);
        assert!(backfilled(80_000, 80_001).is_empty());
        // At its usual time exactly, as long again; a second before, one left.
        assert_eq!(backfilled(60_000, 60_000), vec![2]);
        assert_eq!(backfilled(59_000, 1_000), vec![2]);
        assert!(backfilled(59_000, 1_001).is_empty());
    }

    #[test]
    fn backfilled_jobs_must_fit_now_one_after_another() {
        let (s, r, head) = backfill_case(0, None);
        let q = [
            head,
            short(2, 3.0, 1_000),
            short(3, 2.0, 1_000),
            short(4, 1.0, 1_000),
        ];
        // 4 GB free: 3 GB fits, then 2 GB doesn't, then 1 GB does.
        assert_eq!(at(&s, &[r], &q, 0).backfilled, vec![2, 4]);
    }

    #[test]
    fn one_batch_cannot_exceed_the_remaining_backfill_allowance() {
        for skipped in 0..=3 {
            let (s, r, head) = backfill_case(skipped, None);
            let mut q = vec![head];
            q.extend((2..12).map(|id| short(id, 0.1, 1_000)));
            assert_eq!(at(&s, &[r], &q, 0).backfilled.len(), (3 - skipped) as usize);
        }
    }

    #[test]
    fn missing_runtime_on_either_side_allows_bounded_backfill() {
        let (s, r, head) = backfill_case(0, None);
        for running_runtime in [Some(60_000), None] {
            for candidate_runtime in [Some(60_000), Some(60_001), None] {
                let mut running = r.clone();
                running.median_ms = running_runtime;
                let mut candidate = job(2, 1.0);
                candidate.median_ms = candidate_runtime;
                // Even without a memory sample, a known runtime must obey
                // a known release window. A successful quick run can have
                // elapsed time but finish between memory samples.
                candidate.samples = 0;
                let d = at(&s, &[running], &[head.clone(), candidate], 0);
                let expected = match (running_runtime, candidate_runtime) {
                    (Some(window), Some(takes)) => takes <= window,
                    _ => true,
                };
                assert_eq!(!d.backfilled.is_empty(), expected);
                assert_eq!(d.skipped, expected.then_some(head.id));
            }
        }
        // Frozen memory has no predicted release, but smaller jobs can still
        // use available memory within the same starvation limits.
        let mut frozen = r;
        frozen.state = JobState::Frozen;
        let q = [head, short(2, 1.0, 600_000)];
        assert_eq!(at(&s, &[frozen], &q, 0).backfilled, vec![2]);
    }

    #[test]
    fn opportunistic_jobs_share_the_remaining_memory_and_skip_budget() {
        let mut r = running(9, 2.0, 2.0);
        r.median_ms = None;
        // 8.5 GB available minus 2 GB headroom leaves 6.5 GB. The 8 GB
        // head waits, while later unknown-duration jobs share that budget.
        let q = [
            job(1, 8.0),
            job(2, 3.0),
            job(3, 4.0),
            job(4, 3.5),
            job(5, 0.1),
        ];
        let d = at(&snap(8.5), &[r], &q, 0);
        assert_eq!(d.admit, vec![2, 4]);
        assert_eq!(d.backfilled, vec![2, 4]);
        assert_eq!(d.skipped, Some(1));
        assert_eq!(
            d.blocked,
            Some(Blocked::Memory {
                need: 8 * GIB,
                free: 13 * GIB / 2
            })
        );

        for skipped in 0..=3 {
            let (s, mut r, mut head) = backfill_case(skipped, Some(0));
            r.median_ms = None;
            let mut q = vec![head.clone()];
            q.extend((2..12).map(|id| job(id, 0.1)));
            assert_eq!(
                at(&s, std::slice::from_ref(&r), &q, 299_999)
                    .backfilled
                    .len(),
                (3 - skipped) as usize
            );
            assert!(
                at(&s, std::slice::from_ref(&r), &q, 300_000)
                    .admit
                    .is_empty()
            );
            // A later decision sees the passes recorded by settle and must
            // not gain a fresh allowance just because runtime is unknown.
            head.skipped = 3;
            q[0] = head;
            assert!(at(&s, &[r], &q, 0).admit.is_empty());
        }
    }

    #[test]
    fn opportunistic_backfill_keeps_hold_pressure_cpu_and_setting_gates() {
        let (mut s, mut r, head) = backfill_case(0, None);
        r.median_ms = None;
        let later = job(2, 1.0);
        let cfg = Config::default();
        for pressure in [Pressure::Warn, Pressure::Critical] {
            s.pressure = pressure;
            assert!(
                decide(&s, &[&r], &[&head, &later], false, &cfg, 0)
                    .admit
                    .is_empty()
            );
        }
        s.pressure = Pressure::Normal;
        assert!(
            decide(&s, &[&r], &[&head, &later], true, &cfg, 0)
                .admit
                .is_empty()
        );
        let off = Config {
            backfill: false,
            ..cfg.clone()
        };
        assert!(
            decide(&s, &[&r], &[&head, &later], false, &off, 0)
                .admit
                .is_empty()
        );
        s.load1 = s.cores as f64 * cfg.cpu_factor;
        assert!(
            decide(&s, &[&r], &[&head, &later], false, &cfg, 0)
                .admit
                .is_empty()
        );
        s.load1 -= 1.0;
        let another = job(3, 1.0);
        assert_eq!(
            decide(&s, &[&r], &[&head, &later, &another], false, &cfg, 0).admit,
            vec![2]
        );
        // Low-priority work keeps its existing CPU exception.
        let mut low = another;
        low.spec.low = true;
        assert_eq!(
            decide(&s, &[&r], &[&head, &later, &low], false, &cfg, 0).admit,
            vec![2, 3]
        );
        // A CPU-blocked pinned head is never passed by this memory-only path.
        let mut pinned = head;
        pinned.pinned = true;
        s.load1 += 1.0;
        assert!(
            decide(&s, &[&r], &[&pinned, &low], false, &cfg, 0)
                .admit
                .is_empty()
        );
    }

    #[test]
    fn opportunistic_backfill_respects_running_growth() {
        // 8 GB minus 2 GB headroom minus 3 GB pending growth leaves 3 GB.
        let r = running(9, 4.0, 1.0);
        let d = at(
            &snap(8.0),
            std::slice::from_ref(&r),
            &[job(1, 4.0), job(2, 3.0), job(3, 0.1)],
            0,
        );
        assert_eq!(d.admit, vec![2]);
    }

    #[test]
    fn jobs_admitted_this_round_reserve_their_full_estimate_and_runtime() {
        let mut running = short(9, 2.0, 30_000);
        running.state = JobState::Running;
        running.footprint = 2 * GIB;
        for first_runtime in [Some(120_000), None] {
            let mut first = short(1, 4.0, 120_000);
            first.median_ms = first_runtime;
            for candidate_runtime in [120_000, 120_001] {
                // 6 GB usable before the first admission, 2 GB afterward.
                // The 5 GB head needs the first job's 4 GB to be released.
                let d = at(
                    &snap(8.0),
                    std::slice::from_ref(&running),
                    &[first.clone(), job(2, 5.0), short(3, 1.0, candidate_runtime)],
                    0,
                );
                let may_pass = first_runtime.is_none() || candidate_runtime <= 120_000;
                assert_eq!(d.admit, if may_pass { vec![1, 3] } else { vec![1] });
                assert_eq!(d.skipped, may_pass.then_some(2));
                assert_eq!(
                    d.blocked,
                    Some(Blocked::Memory {
                        need: 5 * GIB,
                        free: 2 * GIB
                    })
                );
            }
        }
    }

    #[test]
    fn a_head_passed_over_three_times_or_for_five_minutes_is_waited_for() {
        let q = |head: Job| [head, short(2, 1.0, 1_000)];
        let (s, r, head) = backfill_case(2, Some(0));
        assert_eq!(
            at(&s, std::slice::from_ref(&r), &q(head), 0).backfilled,
            vec![2]
        );
        let (s, r, head) = backfill_case(3, Some(0));
        assert!(at(&s, &[r], &q(head), 0).admit.is_empty());
        let (s, r, head) = backfill_case(1, Some(0));
        assert_eq!(
            at(&s, std::slice::from_ref(&r), &q(head.clone()), 299_999).backfilled,
            vec![2]
        );
        assert!(at(&s, &[r], &q(head), 300_000).admit.is_empty());
    }

    #[test]
    fn backfill_can_be_switched_off() {
        let (s, r, head) = backfill_case(0, None);
        let off = Config {
            backfill: false,
            ..Config::default()
        };
        let d = decide(&s, &[&r], &[&head, &short(2, 1.0, 1_000)], false, &off, 0);
        assert!(d.admit.is_empty());
    }

    #[test]
    fn admits_everything_that_fits_in_order() {
        let d = run(
            &snap(12.0),
            &[],
            &[job(1, 3.0), job(2, 3.0), job(3, 3.0)],
            false,
        );
        // 12 − 2 headroom = 10: three 3 GB jobs fit.
        assert_eq!(d.admit, vec![1, 2, 3]);
        assert_eq!(d.blocked, None);
    }

    #[test]
    fn passes_a_memory_blocked_head_with_a_fitting_unknown_duration_job() {
        let d = run(
            &snap(8.0),
            &[],
            &[job(1, 3.0), job(2, 4.0), job(3, 0.5)],
            false,
        );
        // 6 GB usable: job 1 takes 3, job 2 needs 4 of the remaining 3.
        assert_eq!(d.admit, vec![1, 3]);
        assert_eq!(d.backfilled, vec![3]);
        assert_eq!(d.skipped, Some(2));
        assert_eq!(
            d.blocked,
            Some(Blocked::Memory {
                need: 4 * GIB,
                free: 3 * GIB
            })
        );
    }

    #[test]
    fn running_jobs_reserve_what_they_have_yet_to_grow() {
        // Running job expects 4 GB and holds 1: 3 GB still reserved.
        let d = run(&snap(8.0), &[running(9, 4.0, 1.0)], &[job(1, 3.5)], false);
        assert_eq!(d.admit, Vec::<JobId>::new());
        assert_eq!(
            d.blocked,
            Some(Blocked::Memory {
                need: (3.5 * GIB as f64) as Bytes,
                free: 3 * GIB
            })
        );
    }

    #[test]
    fn a_job_past_its_estimate_reserves_nothing_more() {
        let d = run(&snap(5.0), &[running(9, 2.0, 6.0)], &[job(1, 3.0)], false);
        assert_eq!(d.admit, vec![1]);
    }

    #[test]
    fn frozen_jobs_do_not_grow() {
        let mut f = running(9, 6.0, 1.0);
        f.state = JobState::Frozen;
        let d = run(&snap(5.0), &[f], &[job(1, 3.0)], false);
        assert_eq!(d.admit, vec![1]);
    }

    #[test]
    fn estimate_exactly_at_the_limit_fits_and_one_byte_over_does_not() {
        let mut exact = job(1, 0.0);
        exact.estimate = 4 * GIB;
        assert_eq!(
            run(&snap(6.0), &[running(9, 0.0, 0.0)], &[exact.clone()], false).admit,
            vec![1]
        );
        exact.estimate += 1;
        assert!(
            run(&snap(6.0), &[running(9, 0.0, 0.0)], &[exact], false)
                .admit
                .is_empty()
        );
    }

    #[test]
    fn idle_queue_admits_a_head_bigger_than_the_machine() {
        let d = run(&snap(4.0), &[], &[job(1, 64.0), job(2, 0.1)], false);
        assert_eq!(d.admit, vec![1]);
        assert!(matches!(d.blocked, Some(Blocked::Memory { .. })));
    }

    #[test]
    fn critical_pressure_holds_everything_even_when_idle() {
        let s = Snapshot {
            pressure: Pressure::Critical,
            ..snap(30.0)
        };
        let d = run(&s, &[], &[job(1, 0.1)], false);
        assert!(d.admit.is_empty());
        assert_eq!(
            d.blocked,
            Some(Blocked::Pressure {
                level: Pressure::Critical
            })
        );
    }

    #[test]
    fn warn_pressure_lets_one_job_start_on_an_idle_queue_only() {
        let s = Snapshot {
            pressure: Pressure::Warn,
            ..snap(30.0)
        };
        let d = run(&s, &[], &[job(1, 0.1), job(2, 0.1)], false);
        assert_eq!(d.admit, vec![1]);
        assert_eq!(
            d.blocked,
            Some(Blocked::Pressure {
                level: Pressure::Warn
            })
        );
        let d = run(&s, &[running(9, 0.1, 0.1)], &[job(1, 0.1)], false);
        assert!(d.admit.is_empty());
    }

    #[test]
    fn hold_blocks_and_says_so() {
        let d = run(&snap(30.0), &[], &[job(1, 0.1)], true);
        assert!(d.admit.is_empty());
        assert_eq!(d.blocked, Some(Blocked::Held));
    }

    #[test]
    fn empty_queue_has_no_reason() {
        assert_eq!(run(&snap(1.0), &[], &[], true), Decision::default());
    }

    #[test]
    fn pinned_skips_the_memory_check_but_not_pressure() {
        let mut p = job(1, 20.0);
        p.pinned = true;
        let busy = [running(9, 1.0, 1.0)];
        let d = run(&snap(4.0), &busy, &[p.clone()], false);
        assert_eq!(d.admit, vec![1]);
        let s = Snapshot {
            pressure: Pressure::Warn,
            ..snap(4.0)
        };
        assert!(run(&s, &busy, &[p], false).admit.is_empty());
    }

    #[test]
    fn cpu_saturation_holds_normal_jobs_but_not_low_ones() {
        let s = Snapshot {
            load1: 16.0,
            ..snap(30.0)
        };
        let busy = [running(9, 0.0, 0.0)];
        let d = run(&s, &busy, &[job(1, 0.1)], false);
        assert_eq!(
            d.blocked,
            Some(Blocked::Cpu {
                load: 16.0,
                cores: 10
            })
        );
        let mut low = job(2, 0.1);
        low.spec.low = true;
        assert_eq!(run(&s, &busy, &[low], false).admit, vec![2]);
    }

    #[test]
    fn one_round_cannot_overshoot_the_cpu() {
        // limit 15, load 13: two admissions, then the third sees load 15.
        let s = Snapshot {
            load1: 13.0,
            ..snap(30.0)
        };
        let d = run(
            &s,
            &[running(9, 0.0, 0.0)],
            &[job(1, 0.1), job(2, 0.1), job(3, 0.1)],
            false,
        );
        assert_eq!(d.admit, vec![1, 2]);
    }

    #[test]
    fn unknown_core_count_disables_the_cpu_rule() {
        let s = Snapshot {
            load1: 99.0,
            cores: 0,
            ..snap(30.0)
        };
        assert_eq!(
            run(&s, &[running(9, 0.0, 0.0)], &[job(1, 0.1)], false).admit,
            vec![1]
        );
    }

    #[test]
    fn available_below_headroom_counts_as_nothing_free() {
        let d = run(&snap(1.0), &[running(9, 0.0, 0.0)], &[job(1, 0.1)], false);
        assert_eq!(
            d.blocked,
            Some(Blocked::Memory {
                need: job(1, 0.1).estimate,
                free: 0
            })
        );
    }

    #[test]
    fn reasons_read_as_sentences() {
        assert_eq!(
            Blocked::Memory {
                need: 42 * GIB / 10,
                free: 29 * GIB / 10
            }
            .describe(),
            "next needs 4.2 GB, 2.9 GB free"
        );
        assert_eq!(
            Blocked::Memory {
                need: 4 * GIB,
                free: 0
            }
            .describe(),
            "next needs 4 GB, 0 GB free"
        );
        assert_eq!(
            Blocked::Pressure {
                level: Pressure::Critical
            }
            .describe(),
            "memory pressure CRITICAL"
        );
        assert_eq!(
            Blocked::Cpu {
                load: 16.2,
                cores: 10
            }
            .describe(),
            "CPU busy, load 16 / 10 cores"
        );
        assert_eq!(Blocked::Held.describe(), "held by you");
    }
}
