//! What a command has needed before, and so what it will probably need now.
//!
//! Each profile keeps a command's last runs. The estimate is the p90 of
//! their peak footprints: high enough that one run in ten exceeds it, so the
//! queue rarely admits a job that then pushes the machine into swap.

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};

use crate::model::{Bytes, Part};

/// Runs kept per key.
pub const KEEP: usize = 20;
/// Clean runs needed before a key's own history is trusted outright.
pub const TRUSTED: usize = 3;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub peak: Bytes,
    pub duration_ms: u64,
    pub exit: Option<i32>,
    /// Frozen, killed or cut short. Its peak is only a lower bound on what
    /// the command needs, and its duration means nothing.
    pub flagged: bool,
    /// Unix seconds.
    pub at: u64,
    /// Workers the run used, for a tool Headroom can tune (see `tweaks`).
    #[serde(default)]
    pub workers: Option<u32>,
    /// What ran inside it (see `model::Part`).
    #[serde(default)]
    pub parts: Vec<Part>,
}

/// Peak memory as a function of worker count: `base + per_worker × N`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scaling {
    pub base: Bytes,
    pub per_worker: Bytes,
}

impl Scaling {
    pub fn at(&self, workers: u32) -> Bytes {
        self.base + self.per_worker * workers as Bytes
    }
}

/// Share of a peak assumed to be fixed cost while every run so far used the
/// same worker count. Until a second count has been seen there is nothing to
/// fit, and without a first tuned run there never would be.
const ASSUMED_BASE: f64 = 0.25;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub runs: VecDeque<Run>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Estimate {
    pub bytes: Bytes,
    /// Clean runs of the exact key behind it; 0 means no history of its own.
    pub samples: u32,
}

/// Nearest-rank p90. `None` for no values.
fn p90(values: &mut [Bytes]) -> Option<Bytes> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let rank = (values.len() * 9).div_ceil(10);
    Some(values[rank - 1])
}

impl Profile {
    fn clean_peaks(&self) -> Vec<Bytes> {
        self.runs
            .iter()
            // A command can finish between samples. Its zero peak means
            // unmeasured memory, while its duration is still useful.
            .filter(|r| !r.flagged && r.peak > 0)
            .map(|r| r.peak)
            .collect()
    }

    /// Highest peak of a flagged run. A run killed at 7 GB needed at least
    /// 7 GB, so it may raise an estimate; a run frozen early proves nothing
    /// low, so it never lowers one.
    fn flagged_floor(&self) -> Bytes {
        self.runs
            .iter()
            .filter(|r| r.flagged)
            .map(|r| r.peak)
            .max()
            .unwrap_or(0)
    }

    /// The key's own p90 when it has enough clean runs to stand alone.
    fn trusted(&self) -> Option<Bytes> {
        let mut peaks = self.clean_peaks();
        if peaks.len() < TRUSTED {
            return None;
        }
        p90(&mut peaks).map(|p| p.max(self.flagged_floor()))
    }

    pub fn p90(&self) -> Option<Bytes> {
        p90(&mut self.clean_peaks())
    }

    pub fn median_ms(&self) -> Option<u64> {
        let mut d: Vec<u64> = self
            .runs
            .iter()
            .filter(|r| !r.flagged)
            .map(|r| r.duration_ms)
            .collect();
        if d.is_empty() {
            return None;
        }
        d.sort_unstable();
        Some(d[(d.len() - 1) / 2])
    }

    /// How peak memory grows with workers, from clean runs that recorded
    /// their worker count. With two or more counts seen in at least
    /// `TRUSTED` runs: the least-squares slope, with the base raised until
    /// every run lies on or under the line, so it never underestimates a run
    /// it has seen. Otherwise an assumption, from the p90 at the count seen
    /// most often.
    pub fn scaling(&self) -> Option<Scaling> {
        let pts: Vec<(u32, Bytes)> = self
            .runs
            .iter()
            .filter(|r| !r.flagged && r.peak > 0)
            .filter_map(|r| r.workers.filter(|&n| n > 0).map(|n| (n, r.peak)))
            .collect();
        let mut counts: BTreeMap<u32, usize> = BTreeMap::new();
        for (n, _) in &pts {
            *counts.entry(*n).or_default() += 1;
        }
        if counts.len() >= 2 && pts.len() >= TRUSTED {
            let len = pts.len() as f64;
            let mx = pts.iter().map(|p| p.0 as f64).sum::<f64>() / len;
            let my = pts.iter().map(|p| p.1 as f64).sum::<f64>() / len;
            let sxx: f64 = pts.iter().map(|p| (p.0 as f64 - mx).powi(2)).sum();
            let sxy: f64 = pts
                .iter()
                .map(|p| (p.0 as f64 - mx) * (p.1 as f64 - my))
                .sum();
            let per = (sxy / sxx).max(0.0);
            let base = pts
                .iter()
                .map(|p| p.1 as f64 - per * p.0 as f64)
                .fold(0.0, f64::max);
            return Some(Scaling {
                base: base.ceil() as Bytes,
                per_worker: per.ceil() as Bytes,
            });
        }
        let (&n0, _) = counts.iter().max_by_key(|(n, c)| (**c, **n))?;
        let mut peaks: Vec<Bytes> = pts.iter().filter(|p| p.0 == n0).map(|p| p.1).collect();
        let peak = p90(&mut peaks)? as f64;
        Some(Scaling {
            base: (peak * ASSUMED_BASE).ceil() as Bytes,
            per_worker: (peak * (1.0 - ASSUMED_BASE) / n0 as f64).ceil() as Bytes,
        })
    }

    pub fn samples(&self) -> u32 {
        self.runs
            .iter()
            .filter(|r| !r.flagged && r.peak > 0)
            .count() as u32
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Profiles(pub BTreeMap<String, Profile>);

impl Profiles {
    /// Record a finished run under its key and its fallback key, so the
    /// coarser key learns from every command in the repository.
    pub fn record(&mut self, key: &str, fallback: &str, run: Run) {
        for k in [key, fallback] {
            let p = self.0.entry(k.to_string()).or_default();
            p.runs.push_back(run.clone());
            while p.runs.len() > KEEP {
                p.runs.pop_front();
            }
            if key == fallback {
                break;
            }
        }
    }

    /// Blend toward the best available prior while a key has few runs:
    /// with one clean run the key's own peak weighs a third, with three it
    /// stands alone.
    pub fn estimate(&self, key: &str, fallback: &str, default: Bytes) -> Estimate {
        let prior = self
            .0
            .get(fallback)
            .filter(|_| fallback != key)
            .and_then(Profile::trusted)
            .unwrap_or(default);
        let Some(own) = self.0.get(key) else {
            return Estimate {
                bytes: prior,
                samples: 0,
            };
        };
        let n = own.samples();
        if let Some(t) = own.trusted() {
            return Estimate {
                bytes: t,
                samples: n,
            };
        }
        let blended = match own.p90() {
            Some(p) => {
                let n = n as u128;
                ((p as u128 * n + prior as u128 * (TRUSTED as u128 - n)) / TRUSTED as u128) as Bytes
            }
            None => prior,
        };
        Estimate {
            bytes: blended.max(own.flagged_floor()),
            samples: n,
        }
    }

    /// Reserve for the command's default workers, since observed peaks may
    /// come from runs with fewer workers. This floor also applies when worker
    /// tuning is disabled; the command then starts with its default count.
    pub fn estimate_for_workers(
        &self,
        key: &str,
        fallback: &str,
        default: Bytes,
        workers: Option<u32>,
    ) -> Estimate {
        let mut est = self.estimate(key, fallback, default);
        if let Some(workers) = workers
            && let Some(scaling) = self.get(key).and_then(Profile::scaling)
        {
            est.bytes = est.bytes.max(scaling.at(workers));
        }
        est
    }

    pub fn reset(&mut self, key: &str) -> bool {
        self.0.remove(key).is_some()
    }

    pub fn get(&self, key: &str) -> Option<&Profile> {
        self.0.get(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::GIB;

    const DEF: Bytes = 3 * GIB / 2;

    fn run(gb: f64) -> Run {
        Run {
            peak: (gb * GIB as f64) as Bytes,
            duration_ms: 1000,
            exit: Some(0),
            flagged: false,
            at: 0,
            workers: None,
            parts: Vec::new(),
        }
    }

    fn flagged(gb: f64) -> Run {
        Run {
            flagged: true,
            exit: None,
            ..run(gb)
        }
    }

    fn at(gb: f64, workers: u32) -> Run {
        Run {
            workers: Some(workers),
            ..run(gb)
        }
    }

    fn profile(runs: Vec<Run>) -> Profile {
        Profile { runs: runs.into() }
    }

    #[test]
    fn no_scaling_without_runs_that_recorded_workers() {
        assert_eq!(profile(vec![]).scaling(), None);
        assert_eq!(profile(vec![run(4.0), run(4.0)]).scaling(), None);
        let flagged_only = Run {
            workers: Some(8),
            ..flagged(9.0)
        };
        assert_eq!(
            profile(vec![flagged_only]).scaling(),
            None,
            "a killed run proves nothing"
        );
    }

    #[test]
    fn one_worker_count_seen_assumes_a_quarter_is_fixed_cost() {
        let s = profile(vec![at(8.0, 8)]).scaling().unwrap();
        assert_eq!((gb(s.base), gb(s.per_worker)), (2.0, 0.75));
        assert_eq!(gb(s.at(8)), 8.0, "it reproduces the run it saw");
        // Two counts but too few runs to fit: the count seen most often wins.
        let s = profile(vec![at(8.0, 8), at(8.0, 8), at(3.0, 2)]).scaling();
        assert!(s.is_some());
        let s = profile(vec![at(8.0, 8), at(4.0, 4)]).scaling().unwrap();
        assert_eq!(gb(s.at(8)), 8.0, "ties go to the larger count");
    }

    #[test]
    fn two_counts_over_enough_runs_are_fitted_and_never_undershoot() {
        // 1 GB + 0.5 GB a worker, with one noisy run above the line.
        let s = profile(vec![at(5.0, 8), at(3.0, 4), at(2.2, 2)])
            .scaling()
            .unwrap();
        for (n, peak) in [(8, 5.0), (4, 3.0), (2, 2.2)] {
            assert!(gb(s.at(n)) >= peak, "{n} workers: {} < {peak}", gb(s.at(n)));
        }
        assert!(
            (gb(s.per_worker) - 0.48).abs() < 0.05,
            "{}",
            gb(s.per_worker)
        );
    }

    #[test]
    fn memory_that_does_not_grow_with_workers_gets_no_slope() {
        let s = profile(vec![at(4.0, 8), at(4.0, 4), at(4.2, 2)])
            .scaling()
            .unwrap();
        assert_eq!(s.per_worker, 0);
        assert_eq!(gb(s.base), 4.2);
    }

    fn gb(b: Bytes) -> f64 {
        (b as f64 / GIB as f64 * 100.0).round() / 100.0
    }

    #[test]
    fn unknown_command_gets_the_default() {
        let p = Profiles::default();
        assert_eq!(
            p.estimate("k", "f", DEF),
            Estimate {
                bytes: DEF,
                samples: 0
            }
        );
    }

    #[test]
    fn three_clean_runs_stand_alone() {
        let mut p = Profiles::default();
        for g in [3.0, 4.0, 3.5] {
            p.record("k", "f", run(g));
        }
        let e = p.estimate("k", "f", DEF);
        assert_eq!(gb(e.bytes), 4.0);
        assert_eq!(e.samples, 3);
    }

    #[test]
    fn p90_ignores_one_outlier_in_twenty() {
        let mut p = Profiles::default();
        for _ in 0..19 {
            p.record("k", "f", run(2.0));
        }
        p.record("k", "f", run(9.0));
        assert_eq!(gb(p.estimate("k", "f", DEF).bytes), 2.0);
    }

    #[test]
    fn default_workers_raise_tuned_history_without_lowering_other_floors() {
        let mut p = Profiles::default();
        for i in 0..20 {
            p.record("k", "f", if i < 2 { at(8.0, 10) } else { at(2.0, 2) });
        }
        assert_eq!(p.estimate("k", "f", DEF).bytes, 2 * GIB);
        let est = p.estimate_for_workers("k", "f", DEF, Some(10));
        assert!(est.bytes >= 8 * GIB);
        assert_eq!(est.samples, 20);
        assert_eq!(
            p.estimate_for_workers("k", "f", DEF, None),
            p.estimate("k", "f", DEF)
        );
        p.record("k", "f", flagged(12.0));
        assert_eq!(
            p.estimate_for_workers("k", "f", DEF, Some(10)).bytes,
            12 * GIB
        );
        assert_eq!(
            p.estimate_for_workers("unknown", "f", DEF, Some(10)),
            p.estimate("unknown", "f", DEF),
            "the fallback mixes commands, so its scaling is not used"
        );
    }

    #[test]
    fn one_run_blends_a_third_of_the_way_from_the_default() {
        let mut p = Profiles::default();
        p.record("k", "k", run(4.5));
        // (4.5 + 1.5 + 1.5) / 3
        assert_eq!(gb(p.estimate("k", "k", DEF).bytes), 2.5);
    }

    #[test]
    fn a_trusted_fallback_is_the_prior_for_a_new_key() {
        let mut p = Profiles::default();
        for g in [6.0, 6.0, 6.0] {
            p.record("repo|apps/web|vitest run", "repo|vitest", run(g));
        }
        let e = p.estimate("repo|apps/api|vitest run", "repo|vitest", DEF);
        assert_eq!(gb(e.bytes), 6.0);
        assert_eq!(e.samples, 0);
    }

    #[test]
    fn a_killed_run_raises_but_never_lowers_the_estimate() {
        let mut p = Profiles::default();
        for g in [2.0, 2.0, 2.0] {
            p.record("k", "f", run(g));
        }
        p.record("k", "f", flagged(7.1));
        assert_eq!(gb(p.estimate("k", "f", DEF).bytes), 7.1);

        let mut q = Profiles::default();
        for g in [4.0, 4.0, 4.0] {
            q.record("k", "f", run(g));
        }
        q.record("k", "f", flagged(0.3));
        assert_eq!(gb(q.estimate("k", "f", DEF).bytes), 4.0);
    }

    #[test]
    fn flagged_runs_do_not_count_as_samples() {
        let mut p = Profiles::default();
        p.record("k", "f", flagged(1.0));
        let e = p.estimate("k", "f", DEF);
        assert_eq!(e.samples, 0);
        assert_eq!(e.bytes, DEF);
    }

    #[test]
    fn unsampled_runs_keep_the_prior_and_duration_without_learning_zero_memory() {
        let mut p = Profiles::default();
        for duration_ms in [10, 30, 20] {
            p.record(
                "k",
                "f",
                Run {
                    duration_ms,
                    ..at(0.0, 8)
                },
            );
        }
        assert_eq!(
            p.estimate("k", "f", DEF),
            Estimate {
                bytes: DEF,
                samples: 0
            }
        );
        let own = p.get("k").unwrap();
        assert_eq!(own.p90(), None);
        assert_eq!(own.scaling(), None);
        assert_eq!(own.median_ms(), Some(20));
        assert!(own.runs.iter().all(|r| !r.flagged));
    }

    #[test]
    fn unsampled_worker_counts_do_not_change_measured_peaks_or_scaling() {
        let mut p = Profiles::default();
        for _ in 0..3 {
            p.record("k", "f", at(8.0, 8));
        }
        let scaling = p.get("k").unwrap().scaling();
        for _ in 0..16 {
            p.record(
                "k",
                "f",
                Run {
                    duration_ms: 10,
                    ..at(0.0, 2)
                },
            );
        }
        assert_eq!(
            p.estimate("k", "f", DEF),
            Estimate {
                bytes: 8 * GIB,
                samples: 3
            }
        );
        assert_eq!(p.estimate("new", "f", DEF).bytes, 8 * GIB);
        assert_eq!(p.get("k").unwrap().scaling(), scaling);
        assert_eq!(p.get("k").unwrap().median_ms(), Some(10));
        // If all remembered attempts were unmeasured, return to the prior.
        for _ in 0..KEEP {
            p.record("k", "f", at(0.0, 2));
        }
        assert_eq!(
            p.estimate("k", "f", DEF),
            Estimate {
                bytes: DEF,
                samples: 0
            }
        );
    }

    #[test]
    fn only_the_last_twenty_runs_are_kept() {
        let mut p = Profiles::default();
        for _ in 0..20 {
            p.record("k", "f", run(8.0));
        }
        for _ in 0..20 {
            p.record("k", "f", run(1.0));
        }
        assert_eq!(p.get("k").unwrap().runs.len(), KEEP);
        assert_eq!(gb(p.estimate("k", "f", DEF).bytes), 1.0);
    }

    #[test]
    fn same_key_and_fallback_records_once() {
        let mut p = Profiles::default();
        p.record("k", "k", run(1.0));
        assert_eq!(p.get("k").unwrap().runs.len(), 1);
    }

    #[test]
    fn reset_forgets_and_reports_whether_there_was_anything() {
        let mut p = Profiles::default();
        p.record("k", "f", run(1.0));
        assert!(p.reset("k"));
        assert!(!p.reset("k"));
        assert_eq!(p.estimate("k", "nope", DEF).samples, 0);
    }

    #[test]
    fn median_duration_of_clean_runs() {
        let mut p = Profiles::default();
        for d in [10, 30, 20] {
            p.record(
                "k",
                "f",
                Run {
                    duration_ms: d,
                    ..run(1.0)
                },
            );
        }
        p.record(
            "k",
            "f",
            Run {
                duration_ms: 999,
                ..flagged(1.0)
            },
        );
        assert_eq!(p.get("k").unwrap().median_ms(), Some(20));
    }

    #[test]
    fn p90_rank_boundaries() {
        assert_eq!(p90(&mut []), None);
        assert_eq!(p90(&mut [5]), Some(5));
        assert_eq!(p90(&mut [1, 2]), Some(2));
        let mut ten: Vec<Bytes> = (1..=10).collect();
        assert_eq!(p90(&mut ten), Some(9));
        let mut eleven: Vec<Bytes> = (1..=11).collect();
        assert_eq!(p90(&mut eleven), Some(10));
    }
}
