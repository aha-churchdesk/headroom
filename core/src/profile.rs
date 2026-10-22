//! What a command has needed before, and so what it will probably need now.
//!
//! Each profile keeps a command's last runs. Successful uninterrupted runs
//! supply observed peak percentiles and typical durations. Failed or
//! interrupted runs can raise the memory reservation through measured peaks,
//! but cannot establish a typical successful workload.

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};

use crate::model::{Bytes, Part};

/// Runs kept per key.
pub const KEEP: usize = 20;
/// Successful uninterrupted runs needed before a key's history is trusted outright.
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
    /// Successful measured runs of the exact key; 0 means no representative memory history.
    pub samples: u32,
}

/// Observed p10 and p90 peaks at one worker count. This describes history;
/// it is neither a prediction interval nor the scheduler's reservation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct MemoryRange {
    pub lower: Bytes,
    pub upper: Bytes,
    pub samples: u32,
    pub workers: Option<u32>,
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

impl Run {
    /// Successful and uninterrupted. Failed exits can have done less work than
    /// a successful run, so their time and peak cannot describe a typical run.
    pub fn representative(&self) -> bool {
        !self.flagged && self.exit == Some(0)
    }
}

impl Profile {
    fn clean_peaks(&self) -> Vec<Bytes> {
        self.runs
            .iter()
            // A command can finish between samples. Its zero peak means
            // unmeasured memory, while its duration is still useful.
            .filter(|r| r.representative() && r.peak > 0)
            .map(|r| r.peak)
            .collect()
    }

    /// Highest peak of an unsuccessful or interrupted run. A run that failed
    /// at 7 GB needed at least 7 GB, so it may raise an estimate. A run frozen
    /// early proves no lower requirement, so it never lowers the estimate.
    fn observed_floor(&self) -> Bytes {
        self.runs
            .iter()
            .filter(|r| !r.representative())
            .map(|r| r.peak)
            .max()
            .unwrap_or(0)
    }

    /// The key's p90 with enough successful measured runs to stand alone,
    /// raised to any observed lower bound from unsuccessful attempts.
    fn trusted(&self) -> Option<Bytes> {
        let mut peaks = self.clean_peaks();
        if peaks.len() < TRUSTED {
            return None;
        }
        p90(&mut peaks).map(|p| p.max(self.observed_floor()))
    }

    pub fn p90(&self) -> Option<Bytes> {
        p90(&mut self.clean_peaks())
    }

    /// Nearest-rank p10/p90 of at least three successful uninterrupted runs
    /// measured with exactly these workers. Unknown counts match only unknown counts;
    /// worker scaling and interrupted runs cannot supply observed bounds.
    pub fn memory_range(&self, workers: Option<u32>) -> Option<MemoryRange> {
        let mut peaks: Vec<Bytes> = self
            .runs
            .iter()
            .filter(|r| r.representative() && r.peak > 0 && r.workers == workers)
            .map(|r| r.peak)
            .collect();
        if peaks.len() < TRUSTED {
            return None;
        }
        peaks.sort_unstable();
        Some(MemoryRange {
            lower: peaks[peaks.len().div_ceil(10) - 1],
            upper: peaks[(peaks.len() * 9).div_ceil(10) - 1],
            samples: peaks.len() as u32,
            workers,
        })
    }

    pub fn median_ms(&self) -> Option<u64> {
        self.median_ms_matching(|_| true)
    }

    /// Successful uninterrupted runtime at exactly this worker count. Unknown
    /// counts match only unknown counts; memory scaling cannot predict runtime.
    pub fn median_ms_for_workers(&self, workers: Option<u32>) -> Option<u64> {
        self.median_ms_matching(|r| r.workers == workers)
    }

    fn median_ms_matching(&self, matches: impl Fn(&Run) -> bool) -> Option<u64> {
        let mut d: Vec<u64> = self
            .runs
            .iter()
            .filter(|r| r.representative() && matches(r))
            .map(|r| r.duration_ms)
            .collect();
        if d.is_empty() {
            return None;
        }
        d.sort_unstable();
        Some(d[(d.len() - 1) / 2])
    }

    /// How peak memory grows with workers, from successful uninterrupted runs
    /// that recorded their worker count. With two or more counts seen in at least
    /// `TRUSTED` runs: the least-squares slope, with the base raised until
    /// every run lies on or under the line, so it never underestimates a run
    /// it has seen. Otherwise an assumption, from the p90 at the count seen
    /// most often. Unsuccessful or interrupted peaks raise the base to preserve
    /// their lower bounds, but never participate in fitting the slope.
    pub fn scaling(&self) -> Option<Scaling> {
        let mut scaling = self.clean_scaling()?;
        for run in self.runs.iter().filter(|r| !r.representative()) {
            // With no worker count, no part of this lower bound can safely
            // be attributed to workers. Keep it as a fixed cost instead.
            let variable = scaling
                .per_worker
                .saturating_mul(run.workers.unwrap_or(0) as Bytes);
            scaling.base = scaling.base.max(run.peak.saturating_sub(variable));
        }
        Some(scaling)
    }

    fn clean_scaling(&self) -> Option<Scaling> {
        let pts: Vec<(u32, Bytes)> = self
            .runs
            .iter()
            .filter(|r| r.representative() && r.peak > 0)
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
            .filter(|r| r.representative() && r.peak > 0)
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
    /// with one successful measured run the key's own peak weighs a third;
    /// with three it stands alone.
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
            bytes: blended.max(own.observed_floor()),
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
    fn unsuccessful_runs_are_not_representative_samples_or_runtime() {
        for exit in [Some(1), Some(101), Some(124), Some(137), None] {
            let failed = Run {
                exit,
                duration_ms: 1,
                ..at(0.1, 4)
            };
            let mut p = profile(vec![at(2.0, 4), at(4.0, 4), failed.clone()]);
            assert_eq!(p.samples(), 2);
            assert_eq!(p.trusted(), None);
            assert_eq!(p.memory_range(Some(4)), None);
            assert_eq!(p.median_ms(), Some(1000));
            assert_eq!(p.median_ms_for_workers(Some(4)), Some(1000));
            p.runs.push_back(at(3.0, 4));
            assert_eq!(p.memory_range(Some(4)).unwrap().samples, 3);
            assert_eq!(p.memory_range(Some(4)).unwrap().lower, 2 * GIB);
            assert_eq!(p.p90(), Some(4 * GIB));

            let only_failed = profile(vec![failed; 3]);
            assert_eq!(only_failed.samples(), 0);
            assert_eq!(only_failed.p90(), None);
            assert_eq!(only_failed.median_ms(), None);
            assert_eq!(only_failed.median_ms_for_workers(Some(4)), None);
            assert_eq!(only_failed.scaling(), None);
        }
    }

    #[test]
    fn failed_peaks_raise_memory_floors_without_becoming_fresh_evidence() {
        for successes in [0, 1, 3] {
            let mut p = Profiles::default();
            for _ in 0..successes {
                p.record("k", "f", at(2.0, 4));
            }
            p.record(
                "k",
                "f",
                Run {
                    exit: Some(101),
                    ..at(8.0, 4)
                },
            );
            assert_eq!(p.estimate("k", "f", DEF).bytes, 8 * GIB);
            assert_eq!(p.estimate("k", "f", DEF).samples, successes);
            assert!(p.estimate_for_workers("k", "f", DEF, Some(4)).bytes >= 8 * GIB);
            let own = p.get("k").unwrap();
            assert_eq!(own.p90(), (successes > 0).then_some(2 * GIB));
            assert_eq!(own.trusted(), (successes >= 3).then_some(8 * GIB));
            if successes >= 3 {
                assert_eq!(p.estimate("new", "f", DEF).bytes, 8 * GIB);
            }
        }
        let mut p = Profiles::default();
        for _ in 0..3 {
            p.record("k", "f", run(4.0));
        }
        p.record(
            "k",
            "f",
            Run {
                exit: Some(1),
                ..run(0.1)
            },
        );
        assert_eq!(p.estimate("k", "f", DEF).bytes, 4 * GIB);
        assert_eq!(p.estimate("k", "f", DEF).samples, 3);
    }

    #[test]
    fn unsuccessful_peaks_raise_scaling_without_fitting_the_slope() {
        for workers in [Some(2), None] {
            let mut p = profile(vec![at(8.0, 8), at(5.0, 4), at(3.5, 2)]);
            let before = p.scaling().unwrap();
            p.runs.push_back(Run {
                exit: Some(1),
                workers,
                ..run(9.0)
            });
            let after = p.scaling().unwrap();
            assert_eq!(after.per_worker, before.per_worker);
            assert_eq!(p.samples(), 3);
            assert!(after.at(workers.unwrap_or(0)) >= 9 * GIB);
            for n in 1..=8 {
                assert!(after.at(n) >= before.at(n));
            }
        }
    }

    #[test]
    fn memory_range_uses_nearest_ranks_including_small_sample_edges() {
        for (count, lower, upper) in [(3, 1, 3), (9, 1, 9), (10, 1, 9), (11, 2, 10), (20, 2, 18)] {
            let p = profile(
                (1..=count)
                    .rev()
                    .map(|peak| Run { peak, ..run(1.0) })
                    .collect(),
            );
            assert_eq!(
                p.memory_range(None),
                Some(MemoryRange {
                    lower,
                    upper,
                    samples: count as u32,
                    workers: None,
                })
            );
        }
    }

    #[test]
    fn memory_range_requires_three_comparable_measured_complete_runs() {
        let mut p = profile(vec![]);
        assert_eq!(p.memory_range(None), None);
        p.runs.extend([run(2.0), run(3.0), run(0.0), flagged(90.0)]);
        assert_eq!(p.memory_range(None), None);
        p.runs.extend([at(8.0, 8), at(9.0, 8), at(10.0, 8)]);
        assert_eq!(
            p.memory_range(None),
            None,
            "known counts cannot fill unknown history"
        );
        assert_eq!(p.memory_range(Some(4)), None, "no scaled bounds");
        assert_eq!(
            p.memory_range(Some(8)),
            Some(MemoryRange {
                lower: 8 * GIB,
                upper: 10 * GIB,
                samples: 3,
                workers: Some(8),
            })
        );
        p.runs.push_back(run(4.0));
        assert_eq!(
            p.memory_range(None),
            Some(MemoryRange {
                lower: 2 * GIB,
                upper: 4 * GIB,
                samples: 3,
                workers: None,
            })
        );
        p.runs.extend([
            at(1.0, 4),
            at(2.0, 4),
            at(0.0, 4),
            Run {
                flagged: true,
                ..at(9.0, 4)
            },
        ]);
        assert_eq!(p.memory_range(Some(4)), None);
        assert_eq!(p.memory_range(Some(8)).unwrap().samples, 3);
    }

    #[test]
    fn memory_range_trims_outliers_and_allows_equal_bounds() {
        let mut runs = vec![at(2.0, 4); KEEP - 2];
        runs.extend([at(0.1, 4), at(99.0, 4)]);
        let p = profile(runs);
        assert_eq!(
            p.memory_range(Some(4)),
            Some(MemoryRange {
                lower: 2 * GIB,
                upper: 2 * GIB,
                samples: KEEP as u32,
                workers: Some(4),
            })
        );
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

    #[test]
    fn flagged_peaks_raise_worker_predictions_without_fitting_incomplete_runs() {
        for clean in [
            vec![at(8.0, 8), at(8.0, 8), at(8.0, 8)],
            vec![at(8.0, 8), at(5.0, 4), at(3.5, 2)],
        ] {
            let mut p = profile(clean);
            let before = p.scaling().unwrap();
            p.runs.push_back(Run {
                flagged: true,
                duration_ms: 99_000,
                ..at(7.0, 2)
            });
            let after = p.scaling().unwrap();
            assert_eq!(after.per_worker, before.per_worker);
            assert_eq!(after.at(2), 7 * GIB);
            for n in 1..=8 {
                assert!(after.at(n) >= before.at(n));
            }
            assert_eq!(p.samples(), 3);
            assert_eq!(p.median_ms(), Some(1000));
            assert_eq!(p.p90(), Some(8 * GIB));
            p.runs.push_back(Run {
                flagged: true,
                ..at(0.1, 1)
            });
            assert_eq!(
                p.scaling(),
                Some(after),
                "a low partial peak proves nothing"
            );
        }
    }

    #[test]
    fn flagged_peaks_without_workers_remain_a_floor_at_every_count() {
        for workers in [None, Some(0)] {
            let p = profile(vec![
                at(8.0, 8),
                Run {
                    workers,
                    ..flagged(9.0)
                },
            ]);
            let scaling = p.scaling().unwrap();
            assert_eq!(scaling.per_worker, 3 * GIB / 4);
            for n in 1..=8 {
                assert!(scaling.at(n) >= 9 * GIB);
            }
        }
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
        assert!(p.estimate_for_workers("k", "f", DEF, Some(10)).bytes >= 12 * GIB);
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
    fn runtime_medians_match_workers_without_scaling_or_flagged_samples() {
        let p = profile(
            [
                (Some(2), 120_000, false),
                (Some(2), 120_000, false),
                (Some(2), 120_000, false),
                (Some(8), 30_000, false),
                (Some(8), 900_000, true),
                (Some(4), 600_000, true),
                (None, 45_000, false),
            ]
            .into_iter()
            .map(|(workers, duration_ms, flagged)| Run {
                workers,
                duration_ms,
                flagged,
                ..run(0.0)
            })
            .collect(),
        );
        assert_eq!(p.median_ms(), Some(120_000), "overall history stays pooled");
        assert_eq!(p.median_ms_for_workers(Some(2)), Some(120_000));
        assert_eq!(p.median_ms_for_workers(Some(8)), Some(30_000));
        assert_eq!(p.median_ms_for_workers(Some(4)), None);
        assert_eq!(p.median_ms_for_workers(Some(6)), None);
        assert_eq!(p.median_ms_for_workers(None), Some(45_000));
        assert_eq!(profile(vec![at(2.0, 8)]).median_ms_for_workers(None), None);
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
