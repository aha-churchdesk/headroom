//! When a queued job may start.
//!
//! Memory decides, not a job count: a 200 MB lint and a 6 GB e2e run are not
//! the same "one job". A job is admitted when its estimate fits in what is
//! available, minus the headroom kept free, minus what the running jobs are
//! still expected to grow into.
//!
//! The queue is strict head-of-line: if the head doesn't fit, nothing behind
//! it starts, so a big job can't be starved by a stream of small ones.
//!
//! A head that doesn't fit may still start with fewer workers, when its tool
//! can be told so and its profile says how memory scales with workers.
//!
//! Or a later job may start first, if it fits now and its profile says it
//! will be done before the head could start anyway (EASY backfilling). The
//! head is passed over a bounded number of times, for a bounded time; then
//! the queue is strict again until it starts.

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
    /// Admitted jobs that start with fewer workers: (id, workers, estimate).
    pub tuned: Vec<(JobId, u32, Bytes)>,
    /// Admitted jobs that start ahead of `skipped`, the head waiting on memory.
    pub backfilled: Vec<JobId>,
    pub skipped: Option<JobId>,
    pub blocked: Option<Blocked>,
}

/// When the head could start, in ms from now: running jobs are expected to
/// end at their median run time, each freeing what it holds or is still
/// expected to grow into. `None` if any end can't be predicted, or if even
/// all of them ending wouldn't make room.
fn shadow(
    running: &[&Job],
    admitted: &[(&Job, Bytes)],
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
                ends.push((
                    j.median_ms?.saturating_sub(elapsed),
                    j.estimate.max(j.footprint),
                ));
            }
        }
    }
    for (j, est) in admitted {
        ends.push((j.median_ms?, *est));
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

/// The most workers, below the tool's default and not below the floor, at
/// which the job's estimate fits in `free`, and that estimate.
pub fn shrink(job: &Job, free: Bytes, cfg: &Config) -> Option<(u32, Bytes)> {
    if !cfg.tune || job.pinned {
        return None;
    }
    let (scaling, tune) = (job.scaling?, job.spec.tune?);
    (cfg.tune_floor.max(1)..tune.default_workers)
        .rev()
        .map(|n| (n, scaling.at(n)))
        .find(|&(_, est)| est <= free)
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

    let mut admitted: Vec<(&Job, Bytes)> = Vec::new();
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
        let tuned = if fits { None } else { shrink(job, free, cfg) };
        if !fits && tuned.is_none() {
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
        let est = match tuned {
            Some((n, est)) => {
                d.tuned.push((job.id, n, est));
                est
            }
            None => job.estimate,
        };
        growth += est;
        admitted.push((job, est));
        load += 1.0;
    }

    let Some((i, mut free)) = waiting_head else {
        return d;
    };
    let head = queue[i];
    if strict(head, cfg, now) {
        return d;
    }
    let Some(head_starts_in) = shadow(running, &admitted, head.estimate, free, now) else {
        return d;
    };
    for job in &queue[i + 1..] {
        // A job with no history makes no promise about when it ends.
        let Some(takes) = job.median_ms.filter(|_| job.samples > 0) else {
            continue;
        };
        let cpu_ok = job.low() || snap.cores == 0 || load < cpu_limit;
        if takes <= head_starts_in && job.estimate <= free && cpu_ok {
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
    use crate::profile::Scaling;
    use crate::tweaks::{Tool, Tunable};

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

    /// A vitest job estimated at 6 GB, needing 1 GB plus 0.5 GB a worker.
    fn tunable(id: JobId) -> Job {
        let mut j = job(id, 6.0);
        j.spec.tune = Some(Tunable {
            tool: Tool::Vitest,
            default_workers: 10,
            npm: false,
        });
        j.scaling = Some(Scaling {
            base: GIB,
            per_worker: GIB / 2,
        });
        j
    }

    /// Something of ours running, so the idle rule doesn't admit everything.
    fn busy() -> Vec<Job> {
        vec![running(9, 0.0, 0.0)]
    }

    #[test]
    fn a_job_that_does_not_fit_starts_with_the_most_workers_that_do() {
        // 6 GB available − 2 headroom = 4 GB: 1 + 0.5 × 6 = 4 GB fits exactly.
        let d = run(&snap(6.0), &busy(), &[tunable(1), job(2, 0.5)], false);
        assert_eq!(d.admit, vec![1]);
        assert_eq!(d.tuned, vec![(1, 6, 4 * GIB)]);
        // The tuned estimate is what it reserves: nothing is left for job 2.
        assert_eq!(
            d.blocked,
            Some(Blocked::Memory {
                need: GIB / 2,
                free: 0
            })
        );
        // One byte less and it takes one worker fewer.
        let d = run(&snap(6.0 - 1.0 / GIB as f64), &busy(), &[tunable(1)], false);
        assert_eq!(d.tuned.first().map(|t| t.1), Some(5));
    }

    #[test]
    fn below_the_floor_it_waits_as_before() {
        // The floor, 2 workers, needs 2 GB: 4 GB available leaves exactly that.
        let d = run(&snap(4.0), &busy(), &[tunable(1)], false);
        assert_eq!(d.tuned, vec![(1, 2, 2 * GIB)]);
        let d = run(&snap(3.9), &busy(), &[tunable(1)], false);
        assert!(d.admit.is_empty());
        assert_eq!(
            d.blocked,
            Some(Blocked::Memory {
                need: 6 * GIB,
                free: (1.9 * GIB as f64) as Bytes
            })
        );
    }

    #[test]
    fn no_tuning_without_a_profile_a_tool_or_permission() {
        let mut no_profile = tunable(1);
        no_profile.scaling = None;
        let mut no_tool = tunable(1);
        no_tool.spec.tune = None;
        let mut pinned = tunable(1);
        pinned.pinned = true;
        for j in [no_profile, no_tool] {
            assert!(run(&snap(5.0), &busy(), &[j], false).admit.is_empty());
        }
        let off = Config {
            tune: false,
            ..Config::default()
        };
        let d = decide(&snap(5.0), &[&busy()[0]], &[&tunable(1)], false, &off, 0);
        assert!(d.admit.is_empty());
        // Pinned jobs skip the memory check, so they start untouched.
        let d = run(&snap(5.0), &busy(), &[pinned], false);
        assert_eq!((d.admit, d.tuned), (vec![1], vec![]));
    }

    #[test]
    fn a_job_that_fits_is_never_tuned() {
        let d = run(&snap(9.0), &busy(), &[tunable(1)], false);
        assert_eq!((d.admit, d.tuned), (vec![1], vec![]));
    }

    #[test]
    fn a_default_at_or_below_the_floor_leaves_nothing_to_try() {
        let mut j = tunable(1);
        j.spec.tune = Some(Tunable {
            tool: Tool::Vitest,
            default_workers: 2,
            npm: false,
        });
        assert!(run(&snap(3.0), &busy(), &[j], false).admit.is_empty());
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
    fn no_promise_no_backfill() {
        let (s, r, head) = backfill_case(0, None);
        // A new command has no run time to go by.
        let mut new = short(2, 1.0, 1_000);
        new.samples = 0;
        assert!(
            at(&s, std::slice::from_ref(&r), &[head.clone(), new], 0)
                .admit
                .is_empty()
        );
        // Nor can it be planned if a running job's end is unknown.
        let mut unknown = r.clone();
        unknown.median_ms = None;
        let q = [head.clone(), short(2, 1.0, 1_000)];
        assert!(at(&s, &[unknown], &q, 0).admit.is_empty());
        // A frozen job won't end by itself: the head has no start to plan for.
        let mut frozen = r;
        frozen.state = JobState::Frozen;
        assert!(at(&s, &[frozen], &q, 0).admit.is_empty());
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
    fn stops_at_the_first_job_that_does_not_fit() {
        let d = run(
            &snap(8.0),
            &[],
            &[job(1, 3.0), job(2, 4.0), job(3, 0.5)],
            false,
        );
        // 6 GB usable: job 1 takes 3, job 2 needs 4 of the remaining 3.
        assert_eq!(d.admit, vec![1]);
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
