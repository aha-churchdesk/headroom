//! When a queued job may start.
//!
//! Memory decides, not a job count: a 200 MB lint and a 6 GB e2e run are not
//! the same "one job". A job is admitted when its estimate fits in what is
//! available, minus the headroom kept free, minus what the running jobs are
//! still expected to grow into.
//!
//! The queue is strict head-of-line: if the head doesn't fit, nothing behind
//! it starts, so a big job can't be starved by a stream of small ones.

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::model::{Bytes, Job, JobId, JobState, Pressure, Snapshot};

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
    pub blocked: Option<Blocked>,
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
    let cpu_limit = snap.perf_cores as f64 * cfg.cpu_factor;

    for job in queue {
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
        if !(idle || job.pinned || job.estimate <= free) {
            d.blocked = Some(Blocked::Memory {
                need: job.estimate,
                free,
            });
            break;
        }
        let cpu_ok = idle || job.low() || snap.perf_cores == 0 || load < cpu_limit;
        if !cpu_ok {
            d.blocked = Some(Blocked::Cpu {
                load,
                cores: snap.perf_cores,
            });
            break;
        }
        d.admit.push(job.id);
        growth += job.estimate;
        load += 1.0;
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
            perf_cores: 10,
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
        decide(s, &r, &q, held, &Config::default())
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
            perf_cores: 0,
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
